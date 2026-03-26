//! watchdog.rs — 监控 GNB + OpenClaw 进程，异常时重启并告警

use crate::heartbeat::is_process_running;
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn};

/// 看门狗告警事件（发给 Console）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchdogAlert {
    pub node_id: String,
    pub service: String,
    pub reason: String,
    pub restarted: bool,
    pub ts: u64,
}

/// 看门狗任务：每 10 秒检查一次进程存活
///
/// 发现异常 → 尝试重启 → 通过回调通知上层发送告警
pub async fn run(
    node_id: String,
    alert_tx: tokio::sync::mpsc::Sender<WatchdogAlert>,
) {
    let mut gnb_down_since: Option<Instant> = None;
    let mut claw_down_since: Option<Instant> = None;

    loop {
        sleep(Duration::from_secs(10)).await;

        check_service(
            "gnb",
            "systemctl restart gnb",
            &mut gnb_down_since,
            &node_id,
            &alert_tx,
        ).await;

        check_service(
            "openclaw-gateway",
            "systemctl restart openclaw-gateway",
            &mut claw_down_since,
            &node_id,
            &alert_tx,
        ).await;
    }
}

/// 检查单个服务，超过 30 秒宕机则重启
async fn check_service(
    name: &str,
    restart_cmd: &str,
    down_since: &mut Option<Instant>,
    node_id: &str,
    alert_tx: &tokio::sync::mpsc::Sender<WatchdogAlert>,
) {
    // 进程名匹配（gnb → "gnb"，openclaw → "openclaw"）
    let proc_name = name.split('-').next().unwrap_or(name);
    let running = is_process_running(proc_name);

    if running {
        *down_since = None;
        return;
    }

    // 进程不存在
    let since = down_since.get_or_insert(Instant::now());
    if since.elapsed() < Duration::from_secs(30) {
        // 宕机不满 30 秒，静默观望
        return;
    }

    warn!("[Watchdog] {name} 已停止超过 30 秒，尝试重启...");

    // 尝试通过 systemctl 重启（分离命令，避免阻塞）
    let restarted = restart_via_systemctl(name).await;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let alert = WatchdogAlert {
        node_id: node_id.to_string(),
        service: name.to_string(),
        reason: format!("{name} 进程停止运行超过 30 秒"),
        restarted,
        ts,
    };

    if restarted {
        info!("[Watchdog] {name} 重启成功");
        *down_since = None; // 重置计时
    }

    let _ = alert_tx.try_send(alert);
}

/// 通过 systemctl restart 重启服务（非阻塞，3 秒超时）
async fn restart_via_systemctl(service: &str) -> bool {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("systemctl")
            .args(["restart", service])
            .output(),
    ).await;

    match result {
        Ok(Ok(output)) => output.status.success(),
        _ => false,
    }
}
