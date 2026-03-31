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

    // 拉起离线任务排队引擎
    let db_pool_for_tq = pool.clone();
    let session_for_tq = session_state.clone();
    tokio::spawn(async move {
        task_queue::run_scheduler(db_pool_for_tq, session_for_tq).await;
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

async fn handle_ui_socket(mut _socket: WebSocket, _state: AppState) {
    tracing::info!("新 UI WebSocket 连接已建立");
    // TODO: 实现推送监控变更等逻辑
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
    let (mut sender, mut _receiver) = socket.split();
    
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
    
    // 主循环，读取 Agent 传上来的信息 (心跳等)
    while let Some(msg) = _receiver.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if msg_type == "heartbeat" {
                        if let Err(e) = heartbeat::ingest(&node_id, &json, &state.db_pool).await {
                            tracing::warn!("Agent {} 心跳处理失败: {}", node_id, e);
                        }
                    } else if msg_type == "hello" {
                        tracing::debug!("Agent {} 发送 hello 握手", node_id);
                        let ack = serde_json::json!({
                            "type": "hello-ack",
                            "ok": true
                        });
                        let _ = tx.send(Message::Text(ack.to_string()));
                    } else if msg_type == "cmd_result" {
                        tracing::debug!("Agent {} 任务回执: {:?}", node_id, json);
                        let task_id = json.get("reqId").and_then(|v| v.as_str()).unwrap_or("");
                        if !task_id.is_empty() {
                            let ok = json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                            let status_str = if ok { "success" } else { "failed" };
                            
                            // 解析回执的 stdout 和 stderr
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
            Ok(Message::Ping(p)) => {
                let _ = tx.send(Message::Pong(p));
            }
            Ok(Message::Close(_)) | Err(_) => {
                break;
            }
            _ => {}
        }
    }

    // 退出时清除会话
    state.session.remove_agent(&node_id).await;
}
