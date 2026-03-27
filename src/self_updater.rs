//! self_updater.rs — synon-daemon 自动更新模块
//!
//! 每 24 小时向 Console `/api/mirror/synon-daemon/latest` 查询最新版本，
//! 若有新版本则下载、校验 SHA256、替换自身二进制并重启。
//!
//! 失败时保留旧版本继续运行，下次重试。

use crate::config::DaemonConfig;

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Sha256, Digest};
use tokio::time::sleep;
use tracing::{info, warn, error};

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

/// 架构标识（编译时决定）
fn arch_tag() -> &'static str {
    if cfg!(target_arch = "x86_64") { "x86_64-musl" }
    else if cfg!(target_arch = "aarch64") { "aarch64-musl" }
    else if cfg!(target_arch = "arm") { "armv7-musl" }
    else { "x86_64-musl" }
}

/// 运行自动更新循环（阻塞，应在独立 tokio task 中运行）
pub async fn run(config: DaemonConfig) {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    // 启动后等待 5 分钟再做第一次检查（避免启动风暴）
    sleep(Duration::from_secs(5 * 60)).await;

    loop {
        if let Err(e) = check_and_update(&config, &client).await {
            warn!("[SelfUpdater] 更新检查失败: {e}，将在 24h 后重试");
        }
        sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;
    }
}

/// 单次检查并更新（可失败）
async fn check_and_update(config: &DaemonConfig, client: &Client) -> Result<()> {
    // 1. 查询最新版本元数据
    let meta_url = format!(
        "{}/api/mirror/synon-daemon/latest?arch={}&current={}",
        config.console_url.replace("wss://", "https://").replace("ws://", "http://"),
        arch_tag(),
        DAEMON_VERSION,
    );

    info!("[SelfUpdater] 检查更新: {meta_url}");

    let resp = client
        .get(&meta_url)
        .bearer_auth(&config.token)
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
        format!(
            "{}/{}",
            config.console_url.replace("wss://", "https://").replace("ws://", "http://"),
            meta.url.trim_start_matches('/'),
        )
    };

    let binary_data = tokio::time::timeout(
        Duration::from_secs(DOWNLOAD_TIMEOUT_SECS),
        async {
            client
                .get(&download_url)
                .bearer_auth(&config.token)
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

    fs::write(&tmp_path, &binary_data)
        .map_err(|e| anyhow!("写临时文件失败: {e}"))?;

    // 设置可执行权限
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| anyhow!("设置权限失败: {e}"))?;

    // 原子替换
    fs::rename(&tmp_path, &self_path)
        .map_err(|e| anyhow!("替换二进制失败: {e}"))?;

    info!("[SelfUpdater] 新版本 v{} 已就位，正在重启...", meta.version);

    // 5. 触发重启（systemd 会重新拉起）
    // 使用 kill -SIGTERM 自身，让进程优雅退出
    unsafe {
        libc::kill(libc::getpid(), libc::SIGTERM);
    }

    // 等待信号处理
    sleep(Duration::from_secs(2)).await;

    Ok(())
}
