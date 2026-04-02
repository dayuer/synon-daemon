//! self_updater.rs — synon-daemon 自动更新模块
//!
//! 每 24 小时向 Console `/api/mirror/synon-daemon/latest` 查询最新版本，
//! 若有新版本则下载、校验 SHA256、替换自身二进制并重启。
//!
//! 失败时保留旧版本继续运行，下次重试。

use crate::config::DaemonConfig;

use std::env;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Sha256, Digest};
use tokio::time::sleep;
use tracing::{info, warn};

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60; // 24 小时
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

/// Console 返回的版本元数据
#[derive(Debug, Deserialize)]
struct VersionMeta {
    version: String,
    sha256: String,
    url: String,
}

/// 架构标识（编译时决定，与 initnode.sh 的 mirror 文件命名保持一致）
fn arch_tag() -> &'static str {
    if cfg!(target_arch = "x86_64") { "x86_64-musl" }
    else if cfg!(target_arch = "aarch64") { "aarch64-musl" }
    else if cfg!(target_arch = "arm") { "armv7-musl" }
    else if cfg!(target_arch = "mips") || cfg!(target_arch = "mips64") { "mips-musl" }
    else { "x86_64-musl" } // 未知架构回退，Console 端需有对应包
}

/// 运行自动更新循环（阻塞，应在独立 tokio task 中运行）
pub async fn run(config: DaemonConfig, shutdown: tokio_util::sync::CancellationToken) {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    // 启动后等待 5 分钟再做第一次检查（避免启动风暴）
    tokio::select! {
        _ = sleep(Duration::from_secs(5 * 60)) => {}
        _ = shutdown.cancelled() => { return; }
    }

    loop {
        match check_and_update(&config, &client, &shutdown).await {
            Ok(()) => {}
            Err(e) => warn!("[SelfUpdater] 更新检查失败: {e}，将在 24h 后重试"),
        }
        tokio::select! {
            _ = sleep(Duration::from_secs(CHECK_INTERVAL_SECS)) => {}
            _ = shutdown.cancelled() => { break; }
        }
    }
}

/// 单次检查并更新（可失败）
async fn check_and_update(config: &DaemonConfig, client: &Client, shutdown: &tokio_util::sync::CancellationToken) -> Result<()> {
    // 1. 查询最新版本元数据
    let base_url = config.console_url
        .replace("wss://", "https://")
        .replace("ws://", "http://")
        .replace("/ws/daemon", "");

    let meta_url = format!(
        "{}/api/mirror/synon-daemon/latest?arch={}&current={}",
        base_url,
        arch_tag(),
        DAEMON_VERSION,
    );

    info!("[SelfUpdater] 检查更新: {meta_url}");

    let resp = client
        .get(&meta_url)
        .send()
        .await?;

    if resp.status() == 204 {
        info!("[SelfUpdater] 已是最新版本 v{DAEMON_VERSION}，无需更新");
        return Ok(());
    }

    if !resp.status().is_success() {
        return Err(anyhow!("版本查询失败: HTTP {}", resp.status()));
    }

    let meta: VersionMeta = resp.json().await?;
    info!("[SelfUpdater] 发现新版本 v{} (当前 v{DAEMON_VERSION})", meta.version);

    // 2. 下载新版本二进制
    let download_url = if meta.url.starts_with("http") {
        meta.url.clone()
    } else {
        format!("{}/{}", base_url, meta.url.trim_start_matches('/'))
    };

    let binary_data = tokio::time::timeout(
        Duration::from_secs(DOWNLOAD_TIMEOUT_SECS),
        async {
            client
                .get(&download_url)
                .send()
                .await?
                .bytes()
                .await
        },
    ).await
    .map_err(|_| anyhow!("下载超时 ({DOWNLOAD_TIMEOUT_SECS}s)"))?
    .map_err(|e| anyhow!("下载失败: {e}"))?;

    // 3. SHA256 校验
    let mut hasher = Sha256::new();
    hasher.update(&binary_data);
    let actual_sha = format!("{:x}", hasher.finalize());

    if actual_sha != meta.sha256 {
        return Err(anyhow!(
            "SHA256 校验失败: 期望 {} 实际 {}",
            meta.sha256, actual_sha
        ));
    }
    info!("[SelfUpdater] SHA256 校验通过");

    // 4. 写入临时文件，替换自身
    let self_path = env::current_exe()
        .map_err(|e| anyhow!("获取自身路径失败: {e}"))?;
    let tmp_path = self_path.with_extension("new");

    tokio::fs::write(&tmp_path, &binary_data).await
        .map_err(|e| anyhow!("写临时文件失败: {e}"))?;

    // 设置可执行权限
    tokio::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755)).await
        .map_err(|e| anyhow!("设置权限失败: {e}"))?;

    // 原子替换
    tokio::fs::rename(&tmp_path, &self_path).await
        .map_err(|e| anyhow!("替换二进制失败: {e}"))?;

    info!("[SelfUpdater] 新版本 v{} 已就位，正在重启...", meta.version);

    // 5. 触发优雅关闭 → systemd Restart=always 自动拉起新版本
    // 不再使用 unsafe libc::kill，通过 CancellationToken 通知主进程优雅退出
    shutdown.cancel();

    Ok(())
}
