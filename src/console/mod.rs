use axum::{
    routing::get,
    Router,
    extract::{ws::{WebSocket, WebSocketUpgrade}, State, Query},
    response::IntoResponse,
};
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;
use serde::Deserialize;
use futures_util::{StreamExt, SinkExt};
use axum::extract::ws::Message;

pub mod db;
pub mod session;
pub mod heartbeat;
pub mod task_queue;
pub mod mqtt_broker;

#[derive(Clone)]
pub struct AppState {
    pub db_pool: deadpool_sqlite::Pool,
    pub session: session::SessionState,
}

#[derive(Deserialize)]
pub struct DaemonWsQuery {
    #[serde(rename = "nodeId")]
    pub node_id: String,
    pub token: Option<String>,
}

pub async fn run_server(config_path: String, shutdown_token: CancellationToken) {
    let _ = config_path; // 暂时保留，后续可用于读取一些环境配置

    // 初始化数据库连接池
    let pool = match db::init_db_pool().await {
        Ok(pool) => {
            tracing::info!("数据库连接池初始化成功 (nodes.db)");
            pool
        },
        Err(e) => {
            tracing::error!("初始化 Console 数据库池失败: {}", e);
            return;
        }
    };

    // 初始化会话池
    let session_state = session::SessionState::new();

    let state = AppState {
        db_pool: pool.clone(),
        session: session_state.clone(),
    };

    // 构建 Axum 路由
    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/ws", get(ws_handler))
        .route("/ws/daemon", get(ws_daemon_handler))
        .with_state(state);

    // 拉起 MQTT Broker + 内部消费者桥接
    let (mut broker, link_tx, link_rx, dispatcher_tx) = mqtt_broker::create_broker();
    // Broker 阻塞式启动（独立 OS 线程）
    let mqtt_bind = std::env::var("MQTT_BIND_ADDR").unwrap_or_else(|_| "198.18.0.1".to_string());
    let mqtt_port = std::env::var("MQTT_BIND_PORT").unwrap_or_else(|_| "1883".to_string());
    std::thread::spawn(move || {
        tracing::info!("MQTT Broker 启动，监听 {}:{}", mqtt_bind, mqtt_port);
        if let Err(e) = broker.start() {
            tracing::error!("MQTT Broker 异常退出: {}", e);
        }
    });
    // 消费者桥接（tokio 异步任务）
    let consumer_db = pool.clone();
    let consumer_session = session_state.clone();
    let consumer_shutdown = shutdown_token.clone();
    tokio::spawn(async move {
        mqtt_broker::run_consumer_bridge(
            consumer_db, consumer_session, consumer_shutdown,
            link_tx, link_rx,
        ).await;
    });

    // 拉起离线任务排队引擎（使用 MQTT dispatcher link 下发指令）
    let db_pool_for_tq = pool.clone();
    let session_for_tq = session_state.clone();
    tokio::spawn(async move {
        task_queue::run_scheduler(db_pool_for_tq, session_for_tq, dispatcher_tx).await;
    });

    let port: u16 = std::env::var("CONSOLE_BACKEND_PORT")
        .unwrap_or_else(|_| "3005".to_string())
        .parse()
        .unwrap_or(3005);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    
    tracing::info!("Console Backend 监听于 http://{}", addr);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("绑定端口 {} 失败: {}", port, e);
            return;
        }
    };
    
    // 带有优雅关闭的 Axum 服务
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            tracing::info!("Console HTTP/WS 服务收到关闭信号，正在退出...");
        })
        .await
    {
        tracing::error!("Server 运行异常: {}", e);
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    tracing::info!("收到来自 Web 客户端的 /ws 升级请求");
    ws.on_upgrade(|socket| handle_ui_socket(socket, state))
}

async fn handle_ui_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // 用 mpsc 代理 websocket send
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // 注册到 session 层
    let session_id = state.session.add_ui(tx.clone()).await;
    tracing::info!("Web UI [{}] WebSocket 连接已建立", session_id);

    // 写入线程
    let sid_for_tx = session_id.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = sender.send(msg).await {
                tracing::warn!("Web UI {} 消息写回失败: {}", sid_for_tx, e);
                break;
            }
        }
    });

    // Ping/Pong 保活（30s 间隔）
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.tick().await; // 跳过第一次

    // 主循环：读取 Web 消息 + 定时 Ping
    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                let ping_payload = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .to_be_bytes()
                    .to_vec();
                if tx.send(Message::Ping(ping_payload)).is_err() {
                    break;
                }
            }
            msg = receiver.next() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(text))) => {
                        // Web 客户端发来的 JSON 消息（如指令下发）
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                            let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

                            match msg_type {
                                "ping" => {
                                    // 应用层 ping → 回 pong
                                    let pong = serde_json::json!({ "type": "pong", "ts": chrono_ts_ms() });
                                    let _ = tx.send(Message::Text(pong.to_string()));
                                }
                                "auth" => {
                                    // Bridge 认证消息 → 回 auth_ok
                                    let ack = serde_json::json!({ "type": "auth_ok" });
                                    let _ = tx.send(Message::Text(ack.to_string()));
                                }
                                _ => {
                                    // TODO: 可扩展 — 转发 web 指令给指定 agent
                                    tracing::debug!("Web UI {} 发送: {}", session_id, msg_type);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = tx.send(Message::Pong(p));
                    }
                    _ => {}
                }
            }
        }
    }

    // 断开时清理
    tracing::info!("Web UI [{}] 断开连接", session_id);
    state.session.remove_ui(&session_id).await;
}

/// 获取当前毫秒时间戳
fn chrono_ts_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn ws_daemon_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<DaemonWsQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    tracing::info!("Agent [{}] 发起握手...", query.node_id);
    
    // TODO: 校验 token ...
    
    ws.on_upgrade(move |socket| handle_daemon_socket(socket, state, query.node_id))
}

async fn handle_daemon_socket(
    socket: WebSocket, 
    state: AppState, 
    node_id: String,
) {
    let (mut sender, mut receiver) = socket.split();
    
    // 用 mpsc 代理 websocket send，允许其他线程下发消息
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    
    // 把当前连接注册进 Session 层
    state.session.add_agent(node_id.clone(), tx.clone()).await;

    // 分离的一个独立线程负责将 mpsc 管道的消息写入真实的 websocket
    let node_id_for_tx = node_id.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = sender.send(msg).await {
                tracing::warn!("Agent {} 消息写回失败: {}", node_id_for_tx, e);
                break;
            }
        }
    });

    tracing::info!("Agent [{}] WebSocket 连接就绪", node_id);
    
    // 定时 Ping（20s 间隔），防止 Agent 端 KeepaliveWatchdog 误判连接死亡
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(20));
    ping_interval.tick().await; // 跳过第一次立即触发

    // 主循环：读取 Agent 消息 + 定时发送 Ping
    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                let ping_payload = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .to_be_bytes()
                    .to_vec();
                if tx.send(Message::Ping(ping_payload)).is_err() {
                    break;
                }
            }
            msg = receiver.next() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                            let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if msg_type == "heartbeat" {
                                if let Err(e) = heartbeat::ingest(&node_id, &json, &state.db_pool).await {
                                    tracing::warn!("Agent {} 心跳处理失败: {}", node_id, e);
                                }
                                // 广播心跳给所有已连接的 Web UI 客户端
                                state.session.broadcast_to_ui(&text).await;
                            } else if msg_type == "hello" {
                                tracing::info!("Agent {} 发送 hello 握手", node_id);
                                let ack = serde_json::json!({
                                    "type": "hello-ack",
                                    "ok": true
                                });
                                let _ = tx.send(Message::Text(ack.to_string()));
                                
                                // 自动注册/更新节点到 nodes 表（确保 Admin UI 可见）
                                let version = json.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let gnb_status = json.get("gnbStatus").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                let claw_status = json.get("clawStatus").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                let nid = node_id.clone();
                                let db = state.db_pool.clone();
                                tokio::spawn(async move {
                                    let nid_log = nid.clone();
                                    if let Ok(c) = db.get().await {
                                        let _ = c.interact(move |conn| {
                                            conn.execute(
                                                "INSERT OR IGNORE INTO nodes (id, name, status, submittedAt) 
                                                 VALUES (?1, ?1, 'online', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                                                rusqlite::params![nid],
                                            )?;
                                            conn.execute(
                                                "UPDATE nodes SET status = 'online', updatedAt = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
                                                rusqlite::params![nid],
                                            )
                                        }).await;
                                    }
                                    tracing::info!("节点 {} 已注册/上线 (v{}, gnb={}, claw={})", nid_log, version, gnb_status, claw_status);
                                });
                            } else if msg_type == "cmd_result" {
                                tracing::debug!("Agent {} 任务回执: {:?}", node_id, json);
                                let task_id = json.get("reqId").and_then(|v| v.as_str()).unwrap_or("");
                                if !task_id.is_empty() {
                                    let ok = json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                                    let status_str = if ok { "success" } else { "failed" };
                                    
                                    let payload_str = json.get("payload")
                                        .and_then(|p| p.as_str())
                                        .unwrap_or("");
                                        
                                    let stderr_str = if !ok {
                                        json.get("stderr").and_then(|e| e.as_str()).unwrap_or(payload_str)
                                    } else {
                                        ""
                                    };
                                    
                                    let stdout_str = if ok { payload_str } else { "" };
                                    
                                    let db_pool_exec = state.db_pool.clone();
                                    let tid = task_id.to_string();
                                    let so = stdout_str.to_string();
                                    let se = stderr_str.to_string();
                                    
                                    tokio::spawn(async move {
                                        if let Ok(c) = db_pool_exec.get().await {
                                            let _ = c.interact(move |conn| {
                                                conn.execute(
                                                    "UPDATE agent_tasks 
                                                     SET status = ?1, 
                                                         resultStdout = ?2, 
                                                         resultStderr = ?3, 
                                                         completedAt = strftime('%Y-%m-%dT%H:%M:%f%z', 'now') 
                                                     WHERE taskId = ?4", 
                                                    rusqlite::params![status_str, so, se, tid]
                                                )
                                            }).await;
                                        }
                                    });
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = tx.send(Message::Pong(p));
                    }
                    _ => {}
                }
            }
        }
    }

    // 退出时标记节点离线并清除会话
    tracing::info!("Agent [{}] 断开连接，标记为 offline", node_id);
    let nid = node_id.clone();
    let db = state.db_pool.clone();
    if let Ok(c) = db.get().await {
        let _ = c.interact(move |conn| {
            conn.execute(
                "UPDATE nodes SET status = 'offline', updatedAt = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
                rusqlite::params![nid],
            )
        }).await;
    }
    state.session.remove_agent(&node_id).await;
}
