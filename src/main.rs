//! main.rs — synon-daemon 入口
//!
//! 用法:
//!   synon-daemon                              # 从默认路径 /opt/gnb/bin/agent.conf 加载
//!   synon-daemon --config /path/to/agent.conf # 指定配置文件
//!   synon-daemon --help

mod config;
mod console_ws;
mod claw_proxy;
mod claw_manager;
mod skills_manager;
mod exec_handler;
mod gnb_controller;
mod gnb_monitor;
mod heartbeat;
mod self_updater;
mod watchdog;
mod task_executor;
#[cfg(feature = "console")]
mod console;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(clap::Subcommand, Debug, Clone, PartialEq)]
pub enum Commands {
    /// 边缘节点模式 (默认行为：连接并接收任务)
    Agent,
    #[cfg(feature = "console")]
    /// 中控台后段服务模式 (WebSocket 监听守护与数据库管理)
    Console,
}

#[derive(Parser, Debug)]
#[command(name = "synon-daemon", version, about = "SynonClaw 控制面守护进程: 可作为 Agent 或 Console 运行")]
struct Args {
    /// 指定子命令 (若为空，则默认为 agent)
    #[command(subcommand)]
    command: Option<Commands>,

    /// agent.conf 路径（默认: /opt/gnb/bin/agent.conf）
    #[arg(long, default_value = "/opt/gnb/bin/agent.conf")]
    config: String,

    /// 日志级别 (error|warn|info|debug|trace)
    #[arg(long, default_value = "info")]
    log_level: String,
}

/// 等待 SIGTERM 或 SIGINT（优雅关闭信号）
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("注册 SIGTERM 失败");
    let mut sigint  = signal(SignalKind::interrupt()).expect("注册 SIGINT 失败");

    tokio::select! {
        _ = sigterm.recv() => tracing::info!("收到 SIGTERM，开始优雅关闭..."),
        _ = sigint.recv()  => tracing::info!("收到 SIGINT，开始优雅关闭..."),
    }
}

/// 监听 SIGHUP → 配置热重载
async fn watch_sighup(config_path: String) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sighup = signal(SignalKind::hangup()).expect("注册 SIGHUP 失败");
    loop {
        sighup.recv().await;
        tracing::info!("收到 SIGHUP，重载配置: {config_path}");
        match config::DaemonConfig::load(Some(&config_path)) {
            Ok(c) => tracing::info!("配置重载成功: Console={} ClawPort={}", c.console_url, c.claw_port),
            Err(e) => tracing::warn!("配置重载失败: {e}（保持当前配置）"),
        }
    }
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

    let exec_cmd = args.command.unwrap_or(Commands::Agent);

    // 如果是 Console 模式，启动后端服务并直接等待关闭（Console模式不需要 agent.conf）
    #[cfg(feature = "console")]
    if exec_cmd == Commands::Console {
        tracing::info!(">>> 进入 Console Backend 模式 <<<");
        // 全局关闭令牌 — 所有子任务通过此 token 感知优雅关闭
        let shutdown = CancellationToken::new();
        let h_console = tokio::spawn(console::run_server(args.config, shutdown.clone()));
        wait_for_shutdown().await;
        shutdown.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), h_console).await;
        tracing::info!("Console Backend 已关闭");
        return;
    }

    // 只有 Agent 模式才加载配置
    let config = match config::DaemonConfig::load(Some(&args.config)) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("配置加载失败: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!("节点 ID: {}", config.node_id);
    tracing::info!("Console: {}", config.console_url);
    tracing::info!("OpenClaw: 127.0.0.1:{}", config.claw_port);

    // 全局关闭令牌 — 所有子任务通过此 token 感知优雅关闭
    let shutdown = CancellationToken::new();

    tracing::info!(">>> 进入 Agent 模式 <<<");
    // Phase 3: sd_notify(READY=1) — 通知 systemd 服务已就绪
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);

    // 看门狗告警通道
    let (alert_tx, alert_rx) = tokio::sync::mpsc::channel(32);

    // 启动看门狗
    let watchdog_token = shutdown.clone();
    let node_id = config.node_id.clone();
    let h_watchdog = tokio::spawn(async move {
        watchdog::run(node_id, alert_tx, watchdog_token).await;
    });

    // 启动自动更新检查（24h 间隔，首次延迟 5min）
    let updater_token = shutdown.clone();
    let updater_config = config.clone();
    let h_updater = tokio::spawn(async move {
        self_updater::run(updater_config, updater_token).await;
    });

    // 初始化 heartbeat 采集参数（gnb_map_path + claw_port）
    heartbeat::init(
        config.gnb_map_path.to_string_lossy().to_string(),
        config.claw_port,
    );

    // 启动时立即刷新 skills 缓存（确保心跳上报第一帧就有 skills 数据）
    tokio::spawn(async {
        match skills_manager::refresh_cache().await {
            Ok(v)  => tracing::info!("[Skills] 启动时刷新完成，共 {} 个技能", v.len()),
            Err(e) => tracing::warn!("[Skills] 启动时刷新失败: {e}"),
        }
    });

    // 每 5 分钟定时刷新 skills 缓存
    let skills_token = shutdown.clone();
    let h_skills = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
        interval.tick().await; // 跳过第一次
        loop {
            tokio::select! {
                _ = interval.tick() => { let _ = skills_manager::refresh_cache().await; }
                _ = skills_token.cancelled() => { break; }
            }
        }
    });

    // SIGHUP 配置热重载
    let config_path = args.config.clone();
    let h_sighup = tokio::spawn(async move {
        watch_sighup(config_path).await;
    });

    // Console WSS 连接（含自动重连）— 主任务
    let ws_token = shutdown.clone();
    let h_ws = tokio::spawn(async move {
        console_ws::run(config, alert_rx, ws_token).await;
    });

    // 等待关闭信号
    wait_for_shutdown().await;

    // 触发优雅关闭
    tracing::info!("通知所有子任务关闭...");
    shutdown.cancel();

    // 等待子任务退出（5s 超时后强制 abort）
    let grace = tokio::time::Duration::from_secs(5);
    let _ = tokio::time::timeout(grace, async {
        let _ = tokio::join!(h_ws, h_watchdog, h_updater, h_skills);
    }).await;

    h_sighup.abort(); // SIGHUP watcher 无需优雅退出

    tracing::info!("synon-daemon 已关闭");
}
