use axum::extract::ws::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub type WsSender = mpsc::UnboundedSender<Message>;

/// 管理所有的 WebSocket 长连接（仅用于 Web UI）
#[derive(Clone)]
pub struct SessionState {
    /// Web UI 连接池: session_id -> 发送通道
    pub ui_clients: Arc<RwLock<HashMap<String, WsSender>>>,
    /// UI 会话自增 ID
    ui_seq: Arc<std::sync::atomic::AtomicU64>,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            ui_clients: Arc::new(RwLock::new(HashMap::new())),
            ui_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    // ─── Web UI 管理 ──────────────────────────────────

    /// 注册一个 Web UI 客户端，返回分配的 session_id
    pub async fn add_ui(&self, sender: WsSender) -> String {
        let seq = self.ui_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session_id = format!("ui-{}", seq);
        tracing::debug!("[Session] Web UI 在线: {}", session_id);
        self.ui_clients.write().await.insert(session_id.clone(), sender);
        session_id
    }

    /// 移除一个 Web UI 客户端
    pub async fn remove_ui(&self, session_id: &str) {
        tracing::debug!("[Session] Web UI 离线: {}", session_id);
        self.ui_clients.write().await.remove(session_id);
    }

    /// 向所有已连接的 Web UI 客户端广播消息
    pub async fn broadcast_to_ui(&self, text: &str) {
        let clients = self.ui_clients.read().await;
        let mut dead: Vec<String> = Vec::new();
        for (id, sender) in clients.iter() {
            if sender.send(Message::Text(text.to_string())).is_err() {
                dead.push(id.clone());
            }
        }
        drop(clients);
        // 清理已断开的连接
        if !dead.is_empty() {
            let mut clients = self.ui_clients.write().await;
            for id in &dead {
                clients.remove(id);
            }
        }
    }

    /// 当前在线 UI 客户端数
    pub async fn ui_count(&self) -> usize {
        self.ui_clients.read().await.len()
    }
}
