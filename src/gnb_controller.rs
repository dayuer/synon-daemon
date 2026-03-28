//! gnb_controller.rs — 原子写入 address.conf 并触发 GNB 热重载
//!
//! 由 console_ws.rs 的 route_update 处理器调用。
//! 使用 tmp 文件 + fs::rename 原子替换，避免 GNB 读到半写入状态。

use anyhow::{Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::fs;
use std::path::Path;
use tracing::{info, warn};

/// 更新 address.conf 并重载 GNB
///
/// 流程：
///   1. 写入临时文件 address.conf.tmp
///   2. 原子重命名为 address.conf
///   3. 查找 gnb 进程 PID 并发送 SIGHUP（平滑重载）
pub async fn apply_route_update(conf_dir: &Path, address_conf: &str) -> Result<()> {
    let target = conf_dir.join("address.conf");
    let tmp    = conf_dir.join("address.conf.tmp");

    // 1. 写临时文件
    fs::write(&tmp, address_conf).await
        .with_context(|| format!("写入临时 address.conf 失败: {}", tmp.display()))?;

    // 2. 原子重命名
    fs::rename(&tmp, &target).await
        .with_context(|| format!("重命名 address.conf 失败: {} → {}", tmp.display(), target.display()))?;

    info!(
        "address.conf 已更新 ({} 字节) → {}",
        address_conf.len(),
        target.display()
    );

    // 3. 发送 SIGHUP 到 gnb 进程
    match find_gnb_pid() {
        Some(pid) => {
            kill(Pid::from_raw(pid as i32), Signal::SIGHUP)
                .with_context(|| format!("发送 SIGHUP 到 gnb (PID={pid}) 失败"))?;
            info!("SIGHUP 已发送到 gnb (PID={pid})，等待平滑重载...");
        }
        None => {
            warn!("未找到 gnb 进程，跳过 SIGHUP（gnb 下次启动时将读取新配置）");
        }
    }

    Ok(())
}

/// 通过 /proc 查找 gnb 进程 PID
fn find_gnb_pid() -> Option<u32> {
    let proc_dir = std::fs::read_dir("/proc").ok()?;
    for entry in proc_dir.flatten() {
        // 仅处理数字目录（PID）
        let pid_str = entry.file_name();
        let pid: u32 = pid_str.to_str()?.parse().ok()?;

        let comm_path = format!("/proc/{pid}/comm");
        let comm = std::fs::read_to_string(&comm_path).ok()?;
        if comm.trim() == "gnb" {
            return Some(pid);
        }
    }
    None
}
