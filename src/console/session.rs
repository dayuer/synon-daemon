use axum::extract::ws::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub type WsSender = mpsc::UnboundedSender<Message>;

/// 管理所有的 WebSocket 长连接（区分 Agent 与 Web UI）
#[derive(Clone)]
pub struct SessionState {
    /// 边缘节点连接池: node_id -> 发送通道
    pub agents: Arc<RwLock<HashMap<String, WsSender>>>,
    /// Web UI 连接池: session_id -> 发送通道
    pub ui_clients: Arc<RwLock<HashMap<String, WsSender>>>,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            ui_clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 注册一个 Agent (建立连接)
    pub async fn add_agent(&self, node_id: String, sender: WsSender) {
        tracing::debug!("[Session] Edge Agent 在线: {}", node_id);
        self.agents.write().await.insert(node_id, sender);
    }

    /// 移除一个 Agent (断开连接)
    pub async fn remove_agent(&self, node_id: &str) {
        tracing::debug!("[Session] Edge Agent 离线: {}", node_id);
        self.agents.write().await.remove(node_id);
    }

    /// 向特定的 Agent 投递消息
    pub async fn send_to_agent(&self, node_id: &str, msg: Message) -> Result<(), String> {
        let agents = self.agents.read().await;
        if let Some(sender) = agents.get(node_id) {
            sender.send(msg).map_err(|_| "Agent通道已关闭".to_string())
        } else {
            Err(format!("Agent {} 不在线", node_id))
        }
    }
}
