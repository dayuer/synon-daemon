//! task_executor.rs — 串行任务执行器
//!
//! Console 下发的耗时命令（exec_cmd / skill_install 等）
//! 通过 mpsc channel 入队，串行执行，防止任务风暴压垮节点。
//! 结果通过 resp_tx 回写到 WS write loop。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::claw_manager;
use crate::exec_handler;
use crate::skills_manager;

/// 待执行的任务消息
pub enum TaskMessage {
    ExecCmd {
        req_id: Value,
        command: String,
        allowed_extra: Vec<String>,
    },
    SkillInstall {
        req_id: Value,
        skill_id: String,
        /// 安装源类型（clawhub / openclaw / github / npm / skills.sh / openclaw-bundled）
        source: String,
        /// skills.sh / clawhub 的短名（可选）
        slug: Option<String>,
        /// github 源仓库名（可选）
        github_repo: Option<String>,
    },
    SkillUninstall {
        req_id: Value,
        skill_id: String,
        /// 安装源类型（决定卸载方式）
        source: String,
    },
    SkillUpdate {
        req_id: Value,
        skill_id: String,
    },
    ClawUpgrade {
        req_id: Value,
        version: Option<String>,
    },
    ClawRestart {
        req_id: Value,
    },
}

impl TaskMessage {
    /// 提取 reqId 字符串（用于去重）
    pub fn req_id_str(&self) -> String {
        let val = match self {
            Self::ExecCmd { req_id, .. }
            | Self::SkillInstall { req_id, .. }
            | Self::SkillUninstall { req_id, .. }
            | Self::SkillUpdate { req_id, .. }
            | Self::ClawUpgrade { req_id, .. }
            | Self::ClawRestart { req_id } => req_id,
        };
        val.as_str().unwrap_or("").to_string()
    }
}

/// reqId 去重集合（read loop 和 executor 共享）
pub type InFlight = Arc<Mutex<HashSet<String>>>;

pub fn new_in_flight() -> InFlight {
    Arc::new(Mutex::new(HashSet::new()))
}

/// 尝试标记 reqId 为 in-flight，返回 true 表示成功（首次claim）
pub fn try_claim(set: &InFlight, req_id: &str) -> bool {
    if req_id.is_empty() {
        return true; // 无 reqId 不做去重
    }
    set.lock().unwrap().insert(req_id.to_string())
}

/// 串行执行器主循环
pub async fn run(
    mut task_rx: mpsc::Receiver<TaskMessage>,
    resp_tx: mpsc::Sender<Message>,
    in_flight: InFlight,
) {
    while let Some(task) = task_rx.recv().await {
        let req_id_s = task.req_id_str();
        let response = execute(task).await;

        if resp_tx.send(Message::Text(response)).await.is_err() {
            warn!("[TaskExecutor] WS write channel 已关闭，退出");
            break;
        }

        // 移除 in-flight 标记
        if !req_id_s.is_empty() {
            if let Ok(mut set) = in_flight.lock() {
                set.remove(&req_id_s);
            }
        }
    }
    info!("[TaskExecutor] 执行器退出");
}

/// 执行单个任务，返回 JSON 响应字符串
async fn execute(task: TaskMessage) -> String {
    match task {
        TaskMessage::ExecCmd { req_id, command, allowed_extra } => {
            info!("[TaskExecutor] 执行命令: {command}");
            let refs: Vec<&str> = allowed_extra.iter().map(|s| s.as_str()).collect();
            let result = exec_handler::exec_allowed(&command, &refs).await;
            json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.code == 0,
                "code": result.code,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }).to_string()
        }
        TaskMessage::SkillInstall { req_id, skill_id, source, slug, github_repo } => {
            info!("[TaskExecutor] 安装技能: {skill_id} (source={source})");
            let result = skills_manager::install_by_source(
                &skill_id,
                &source,
                slug.as_deref(),
                github_repo.as_deref(),
            ).await;
            json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.unwrap_or_else(|e| e.to_string()),
            }).to_string()
        }
        TaskMessage::SkillUninstall { req_id, skill_id, source } => {
            info!("[TaskExecutor] 卸载技能: {skill_id} (source={source})");
            let result = skills_manager::uninstall_by_source(&skill_id, &source).await;
            json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.unwrap_or_else(|e| e.to_string()),
            }).to_string()
        }
        TaskMessage::SkillUpdate { req_id, skill_id } => {
            info!("[TaskExecutor] 更新技能: {skill_id}");
            let result = skills_manager::update(&skill_id).await;
            json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.unwrap_or_else(|e| e.to_string()),
            }).to_string()
        }
        TaskMessage::ClawUpgrade { req_id, version } => {
            info!("[TaskExecutor] 升级 OpenClaw: {:?}", version);
            let result = claw_manager::upgrade(version.as_deref()).await;
            json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.unwrap_or_else(|e| e.to_string()),
            }).to_string()
        }
        TaskMessage::ClawRestart { req_id } => {
            info!("[TaskExecutor] 重启 OpenClaw");
            let result = claw_manager::restart().await;
            json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.err().map(|e| e.to_string()),
            }).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_claim_dedup() {
        let set = new_in_flight();
        assert!(try_claim(&set, "req-1"));   // 首次 claim 成功
        assert!(!try_claim(&set, "req-1"));  // 重复 claim 失败
        assert!(try_claim(&set, "req-2"));   // 不同 reqId 成功
    }

    #[test]
    fn test_try_claim_empty_id() {
        let set = new_in_flight();
        assert!(try_claim(&set, ""));  // 空 reqId 不做去重
        assert!(try_claim(&set, ""));  // 再次也通过
    }

    #[test]
    fn test_in_flight_release() {
        let set = new_in_flight();
        try_claim(&set, "req-1");
        assert!(!try_claim(&set, "req-1")); // 重复
        set.lock().unwrap().remove("req-1");
        assert!(try_claim(&set, "req-1"));  // 释放后可重新 claim
    }
}
