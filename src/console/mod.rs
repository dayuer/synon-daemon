use axum::{
    routing::get,
    Router,
    extract::{ws::{WebSocket, WebSocketUpgrade}, State},
    response::IntoResponse,
};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use futures_util::{StreamExt, SinkExt};
use axum::extract::ws::Message;
use rumqttd::local::LinkTx;

pub mod db;
pub mod session;
pub mod heartbeat;
pub mod task_queue;
pub mod mqtt_broker;
#[allow(dead_code)]
pub mod auth;
#[cfg(all(feature = "ssh-proxy", feature = "console"))]
pub mod ssh_db;
#[cfg(all(feature = "ssh-proxy", feature = "console"))]
pub mod ssh_proxy;

#[derive(Clone)]
pub struct AppState {
    pub db_pool: deadpool_sqlite::Pool,
    pub session: session::SessionState,
    pub mqtt_tx: Arc<Mutex<LinkTx>>,
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

    // ── SSH Proxy Server (feature-gated) ──
    #[cfg(all(feature = "ssh-proxy", feature = "console"))]
    {
        // 初始化 SSH 数据库表
        let ssh_pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = ssh_db::init_tables(&ssh_pool).await {
                tracing::error!("[SSH-Proxy] 初始化 SSH 数据库表失败: {}", e);
            }
        });

        let ssh_pool2 = pool.clone();
        let ssh_shutdown = shutdown_token.clone();
        tokio::spawn(async move {
            ssh_proxy::run_ssh_server(ssh_pool2, ssh_shutdown).await;
        });
    }

    // dispatcher_tx 共享给 state（Web UI 实时下发）和 task_queue（定时调度）
    let shared_dispatcher = Arc::new(Mutex::new(dispatcher_tx));
    let state = AppState {
        db_pool: pool.clone(),
        session: session_state.clone(),
        mqtt_tx: shared_dispatcher.clone(),
    };

    // 构建 Axum 路由
    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/ws", get(ws_handler))
        .with_state(state);

    // 拉起离线任务排队引擎（使用 MQTT dispatcher link 下发指令）
    let db_pool_for_tq = pool.clone();
    let session_for_tq = session_state.clone();
    tokio::spawn(async move {
        task_queue::run_scheduler(db_pool_for_tq, session_for_tq, shared_dispatcher).await;
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

    // 认证状态 — dispatch 等敏感操作必须先通过 auth
    let mut authenticated = false;
    let api_token = std::env::var("CONSOLE_API_TOKEN").unwrap_or_default();

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
                                    let pong = serde_json::json!({ "type": "pong", "ts": crate::util::ts_ms() });
                                    let _ = tx.send(Message::Text(pong.to_string()));
                                }
                                "auth" => {
                                    // Token 认证：客户端需携带 {"type":"auth","token":"..."}
                                    let client_token = json.get("token").and_then(|v| v.as_str()).unwrap_or("");
                                    if api_token.is_empty() || client_token == api_token {
                                        authenticated = true;
                                        let ack = serde_json::json!({ "type": "auth_ok" });
                                        let _ = tx.send(Message::Text(ack.to_string()));
                                    } else {
                                        tracing::warn!("Web UI [{}] 认证失败: token 不匹配", session_id);
                                        let err = serde_json::json!({ "type": "auth_failed", "error": "token 无效" });
                                        let _ = tx.send(Message::Text(err.to_string()));
                                    }
                                }
                                "dispatch" => {
                                    if !authenticated {
                                        let err = serde_json::json!({ "type": "dispatch_error", "error": "未认证，请先发送 auth 消息" });
                                        let _ = tx.send(Message::Text(err.to_string()));
                                    } else {
                                        handle_web_dispatch(&json, &state, &tx).await;
                                    }
                                }
                                _ => {
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

/// 处理 Web UI 的实时指令下发请求
///
/// 期望 JSON 格式:
/// ```json
/// {
///   "type": "dispatch",
///   "nodeId": "node-abc",
///   "cmdType": "exec",        // exec | claw_rpc | skill_install | ...
///   "payload": { ... }        // 透传给 Agent 的完整 payload（需含 reqId）
/// }
/// ```
async fn handle_web_dispatch(
    json: &serde_json::Value,
    state: &AppState,
    tx: &tokio::sync::mpsc::UnboundedSender<Message>,
) {
    let node_id = match json.get("nodeId").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            let err = serde_json::json!({ "type": "dispatch_error", "error": "缺少 nodeId" });
            let _ = tx.send(Message::Text(err.to_string()));
            return;
        }
    };
    let cmd_type = match json.get("cmdType").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            let err = serde_json::json!({ "type": "dispatch_error", "error": "缺少 cmdType" });
            let _ = tx.send(Message::Text(err.to_string()));
            return;
        }
    };
    let payload = json.get("payload").cloned().unwrap_or(serde_json::json!({}));

    // 补充 reqId（如果 payload 中没有）
    let mut payload = payload;
    if payload.get("reqId").is_none() {
        payload["reqId"] = serde_json::json!(format!("ws-{}", uuid::Uuid::new_v4()));
    }

    let topic = format!("synon/cmd/{}/{}", node_id, cmd_type);
    let payload_bytes = payload.to_string().into_bytes();

    let publish_result = state.mqtt_tx.lock().unwrap().publish(topic.clone(), payload_bytes);
    match publish_result {
        Ok(_) => {
            tracing::info!("[WebDispatch] 指令已下发: topic={} reqId={}", topic, payload["reqId"]);
            let ack = serde_json::json!({
                "type": "dispatch_ack",
                "reqId": payload["reqId"],
                "nodeId": node_id,
                "cmdType": cmd_type,
            });
            let _ = tx.send(Message::Text(ack.to_string()));
        }
        Err(e) => {
            tracing::warn!("[WebDispatch] MQTT 下发失败: {}", e);
            let err = serde_json::json!({
                "type": "dispatch_error",
                "reqId": payload["reqId"],
                "error": format!("MQTT 下发失败: {}", e),
            });
            let _ = tx.send(Message::Text(err.to_string()));
        }
    }
}

// 时间戳函数已统一至 crate::util::ts_ms()
