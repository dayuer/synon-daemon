//! main.rs — synon-daemon 入口
//!
//! 用法:
//!   synon-daemon                           # 从默认路径 /opt/gnb/bin/agent.conf 加载
//!   synon-daemon --env /path/to/agent.conf  # 指定配置文件
//!   synon-daemon --help

mod config;
mod console_ws;
mod claw_proxy;
mod gnb_controller;
mod gnb_monitor;
mod heartbeat;
mod watchdog;

use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(name = "synon-daemon", about = "SynonClaw 节点控制面守护进程")]
struct Args {
    /// agent.conf 路径（默认: /opt/gnb/bin/agent.conf）
    #[arg(long, default_value = "/opt/gnb/bin/agent.conf")]
    env: String,

    /// 日志级别 (error|warn|info|debug|trace)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // 初始化日志
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&args.log_level))
        )
        .with_target(false)
        .compact()
        .init();

    tracing::info!("synon-daemon v{} 启动", env!("CARGO_PKG_VERSION"));

    // 加载配置
    let config = match config::DaemonConfig::load(Some(&args.env)) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("配置加载失败: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!("节点 ID: {}", config.node_id);
    tracing::info!("Console: {}", config.console_url);
    tracing::info!("OpenClaw: 127.0.0.1:{}", config.claw_port);

    // 看门狗告警通道
    let (alert_tx, alert_rx) = tokio::sync::mpsc::channel(32);

    // 启动看门狗
    let node_id = config.node_id.clone();
    tokio::spawn(async move {
        watchdog::run(node_id, alert_tx).await;
    });

    // 启动 Console WSS 连接（含自动重连）
    console_ws::run(config, alert_rx).await;
}
