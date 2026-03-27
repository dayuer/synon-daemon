//! claw_manager.rs — OpenClaw 生命周期管理
//!
//! 职责：
//!   - 升级 OpenClaw（npm install -g openclaw@版本）
//!   - 重启 openclaw-gateway systemd 服务
//!   - 查询运行状态（版本 / PID / 是否运行）
//!   - 检查是否有新版本可用

use anyhow::{Context, Result};
use serde::Serialize;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

/// OpenClaw 完整运行状态
#[derive(Debug, Clone, Serialize)]
pub struct ClawFullStatus {
    pub running: bool,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub has_update: bool,
    pub pid: Option<u32>,
    pub port: u16,
}

/// 读取本地安装的 openclaw 版本
pub fn read_local_version() -> Option<String> {
    let output = std::process::Command::new("openclaw")
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // "OpenClaw 2026.3.13 ..." → "2026.3.13"
    text.split_whitespace().nth(1).map(|s| s.to_string())
}

/// 通过 npm registry 查询最新版本（8 秒超时）
pub async fn fetch_latest_version() -> Option<String> {
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        Command::new("npm")
            .args(["show", "openclaw", "version", "--registry=https://registry.npmmirror.com"])
            .output(),
    )
    .await;

    let output = match result {
        Ok(Ok(o)) => o,
        _ => return None,
    };

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// 查询 OpenClaw 是否正在运行（systemctl is-active）
pub async fn is_running() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "openclaw-gateway"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 获取 openclaw-gateway 的 PID
pub async fn get_pid() -> Option<u32> {
    let output = Command::new("systemctl")
        .args(["show", "-p", "MainPID", "--value", "openclaw-gateway"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let pid: u32 = text.parse().ok()?;
    if pid == 0 { None } else { Some(pid) }
}

/// 获取完整状态快照
pub async fn get_full_status(port: u16) -> ClawFullStatus {
    let running = is_running().await;
    let version = read_local_version();
    let latest_version = fetch_latest_version().await;
    let has_update = match (&version, &latest_version) {
        (Some(v), Some(l)) => v != l,
        _ => false,
    };
    let pid = if running { get_pid().await } else { None };
    ClawFullStatus { running, version, latest_version, has_update, pid, port }
}

/// 重启 openclaw-gateway 服务
pub async fn restart() -> Result<()> {
    info!("[ClawManager] 重启 openclaw-gateway...");
    let status = Command::new("systemctl")
        .args(["restart", "openclaw-gateway"])
        .status()
        .await
        .context("systemctl restart openclaw-gateway 失败")?;

    if status.success() {
        info!("[ClawManager] openclaw-gateway 已重启");
        Ok(())
    } else {
        Err(anyhow::anyhow!("openclaw-gateway 重启失败，exit={}", status))
    }
}

/// 升级 OpenClaw
///
/// - `version`: None 表示升级到最新版，Some("2026.3.24") 表示指定版本
pub async fn upgrade(version: Option<&str>) -> Result<String> {
    let pkg = match version {
        Some(v) => format!("openclaw@{v}"),
        None    => "openclaw@latest".to_string(),
    };
    info!("[ClawManager] 升级 OpenClaw: {pkg}");

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        Command::new("npm")
            .args(["install", "-g", &pkg, "--registry=https://registry.npmmirror.com"])
            .output(),
    )
    .await
    .context("升级操作超时 (300s)")?
    .context("npm install 进程启动失败")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        if let Err(e) = restart().await {
            warn!("[ClawManager] 升级后重启失败: {e}");
        }
        info!("[ClawManager] 升级完成: {}", read_local_version().unwrap_or_default());
        Ok(stdout)
    } else {
        Err(anyhow::anyhow!("npm install 失败:\nstdout: {stdout}\nstderr: {stderr}"))
    }
}

/// 构造 upgrade 命令字符串（供白名单校验，预留 Console 端调用）
#[allow(dead_code)]
pub fn upgrade_command(version: Option<&str>) -> String {
    match version {
        Some(v) => format!("npm install -g openclaw@{v} --registry=https://registry.npmmirror.com"),
        None    => "npm install -g openclaw@latest --registry=https://registry.npmmirror.com".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upgrade_command_latest() {
        let cmd = upgrade_command(None);
        assert!(cmd.contains("openclaw@latest"));
        assert!(cmd.contains("npm install -g"));
    }

    #[test]
    fn test_upgrade_command_specific_version() {
        let cmd = upgrade_command(Some("2026.3.24"));
        assert!(cmd.contains("openclaw@2026.3.24"));
        assert!(!cmd.contains("@latest"));
    }

    #[tokio::test]
    async fn test_status_offline_when_no_systemd() {
        // 开发机（macOS）无 systemctl，应安全返回 false 不 panic
        let running = is_running().await;
        let _ = running;
    }

    #[tokio::test]
    async fn test_get_full_status_safe() {
        let status = get_full_status(18789).await;
        assert_eq!(status.port, 18789);
        if status.version.is_none() || status.latest_version.is_none() {
            assert!(!status.has_update);
        }
    }

    #[test]
    fn test_has_update_logic() {
        let local: Option<String> = Some("2026.3.13".into());
        let latest: Option<String> = Some("2026.3.24".into());
        let has_update = match (&local, &latest) {
            (Some(v), Some(l)) => v != l,
            _ => false,
        };
        assert!(has_update);
    }

    #[test]
    fn test_no_update_when_same() {
        let local: Option<String> = Some("2026.3.24".into());
        let latest: Option<String> = Some("2026.3.24".into());
        let has_update = match (&local, &latest) {
            (Some(v), Some(l)) => v != l,
            _ => false,
        };
        assert!(!has_update);
    }
}
