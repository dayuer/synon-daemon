//! console_ws.rs — 与 SynonClaw Console 的 WSS 持久连接
//!
//! 协议：
//!   client → server: { type: "hello", nodeId, token, version, gnbStatus, clawStatus, tunAddr }
//!   server → client: { type: "hello-ack", ok: true }
//!   client → server: { type: "heartbeat", nodeId, ts, sysInfo, gnbPeers, clawRunning }
//!   server → client: { type: "pong" }
//!   server → client: { type: "route_update", reqId, addressConf }
//!   client → server: { type: "cmd_result", reqId, ok, payload }
//!   server → client: { type: "claw_rpc", reqId, method, params }

use crate::config::DaemonConfig;
use crate::claw_proxy::ClawProxy;
use crate::gnb_controller;
use crate::heartbeat;
use crate::gnb_monitor;
use crate::watchdog::WatchdogAlert;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
const HEARTBEAT_INTERVAL_SECS: u64 = 10;
const RECONNECT_BASE_SECS: u64 = 3;
const RECONNECT_MAX_SECS: u64 = 60;

/// 运行 Console WSS 连接（含自动重连指数退避）
pub async fn run(
    config: DaemonConfig,
    alert_rx: mpsc::Receiver<WatchdogAlert>,
) {
    let mut alert_rx = alert_rx;
    let mut retry_secs = RECONNECT_BASE_SECS;

    loop {
        info!("正在连接 Console: {}", config.console_url);
        match connect_and_run(&config, &mut alert_rx).await {
            Ok(()) => {
                info!("Console 连接正常退出，准备重连...");
            }
            Err(e) => {
                warn!("Console 连接断开: {e}，{retry_secs}s 后重连...");
            }
        }
        sleep(Duration::from_secs(retry_secs)).await;
        retry_secs = (retry_secs * 2).min(RECONNECT_MAX_SECS);
    }
}

/// 单次连接的完整生命周期
async fn connect_and_run(
    config: &DaemonConfig,
    alert_rx: &mut mpsc::Receiver<WatchdogAlert>,
) -> Result<()> {
    let (ws_stream, _) = connect_async(&config.console_url).await
        .map_err(|e| anyhow::anyhow!("WSS 连接失败: {e}"))?;
    let (mut write, mut read) = ws_stream.split();

    // 1. 发送 hello 握手帧
    let gnb_status = gnb_monitor::collect(&config.gnb_map_path.to_string_lossy())
        .ok()
        .map(|s| s.tun_ready)
        .unwrap_or(false);

    let claw_proxy = ClawProxy::new(
        config.claw_port,
        config.claw_token.as_deref().unwrap_or(""),
    );
    let claw_running = claw_proxy.ping().await;

    let hello = json!({
        "type": "hello",
        "nodeId": config.node_id,
        "token": config.token,
        "version": DAEMON_VERSION,
        "gnbStatus": if gnb_status { "running" } else { "stopped" },
        "clawStatus": if claw_running { "running" } else { "stopped" },
    });
    write.send(Message::Text(hello.to_string().into())).await?;

    // 2. 等待 hello-ack
    let ack = tokio::time::timeout(Duration::from_secs(10), read.next())
        .await
        .map_err(|_| anyhow::anyhow!("hello-ack 超时"))?
        .ok_or_else(|| anyhow::anyhow!("连接关闭"))??;

    let ack_val: Value = serde_json::from_str(ack.to_text()?)?;
    if ack_val["type"] != "hello-ack" || ack_val["ok"] != true {
        return Err(anyhow::anyhow!("hello-ack 失败: {ack_val}"));
    }
    info!("Console 握手成功 (nodeId={})", config.node_id);

    // 3. 重连成功，重置退避计数（通过返回正常退出触发）
    let config = config.clone();
    let (beat_tx, mut beat_rx) = mpsc::channel::<String>(8);

    // 心跳发送 Task
    let beat_config = config.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        loop {
            ticker.tick().await;
            match build_heartbeat(&beat_config).await {
                Ok(msg) => {
                    if beat_tx.send(msg).await.is_err() { break; }
                }
                Err(e) => warn!("心跳采集失败: {e}"),
            }
        }
    });

    // 主事件循环
    loop {
        tokio::select! {
            // 发送心跳
            Some(msg) = beat_rx.recv() => {
                write.send(Message::Text(msg.into())).await?;
                debug!("心跳已发送");
            }

            // 发送 watchdog 告警
            Some(alert) = alert_rx.recv() => {
                let msg = json!({
                    "type": "watchdog_alert",
                    "nodeId": alert.node_id,
                    "service": alert.service,
                    "reason": alert.reason,
                    "restarted": alert.restarted,
                    "ts": alert.ts,
                });
                write.send(Message::Text(msg.to_string().into())).await?;
            }

            // 接收 Console 下行消息
            msg = read.next() => {
                match msg {
                    None => return Err(anyhow::anyhow!("连接关闭")),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(val) = serde_json::from_str::<Value>(&text) {
                            if let Err(e) = handle_server_message(&val, &config, &claw_proxy, &mut write).await {
                                warn!("处理 Console 消息失败: {e}");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => return Ok(()),
                    Some(Ok(Message::Ping(d))) => {
                        write.send(Message::Pong(d)).await?;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 处理来自 Console 的下行消息
async fn handle_server_message(
    msg: &Value,
    config: &DaemonConfig,
    claw: &ClawProxy,
    write: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
) -> Result<()> {
    let msg_type = msg["type"].as_str().unwrap_or("");
    let req_id = msg.get("reqId").cloned().unwrap_or(Value::Null);

    match msg_type {
        "pong" => {} // 保活响应，忽略

        // OpenClaw RPC 代理（Phase 2）
        "claw_rpc" => {
            let method = msg["method"].as_str().unwrap_or("status");
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let result = claw.rpc(method, params).await;
            let resp = json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.unwrap_or_else(|e| json!({ "error": e.to_string() })),
            });
            write.send(Message::Text(resp.to_string().into())).await?;
        }

        // 路由拓扑更新（Phase 2）
        "route_update" => {
            if let Some(conf) = msg["addressConf"].as_str() {
                let ok = gnb_controller::apply_route_update(&config.gnb_conf_dir, conf)
                    .map_err(|e| warn!("应用 route_update 失败: {e}"))
                    .is_ok();
                let resp = json!({ "type": "cmd_result", "reqId": req_id, "ok": ok });
                write.send(Message::Text(resp.to_string().into())).await?;
            }
        }

        // 密钥滚动更新
        "key_rotate" => {
            let ok = handle_key_rotate(&msg).is_ok();
            let resp = json!({ "type": "cmd_result", "reqId": req_id, "ok": ok });
            write.send(Message::Text(resp.to_string().into())).await?;
        }

        unknown => debug!("收到未知消息类型: {unknown}"),
    }
    Ok(())
}

/// 原子更新 authorized_keys — 追加新公钥并可选删除旧公钥
///
/// - Phase 1 (newPubkey only):    追加新公钥到 authorized_keys
/// - Phase 2 (+ removeOldPubkey): 再删除旧公钥
fn handle_key_rotate(msg: &serde_json::Value) -> Result<()> {
    let auth_keys_path = std::path::PathBuf::from("/root/.ssh/authorized_keys");
    let tmp_path = auth_keys_path.with_extension("tmp");

    // 参数提取与格式校验（防注入）
    let new_key = msg["newPubkey"].as_str()
        .ok_or_else(|| anyhow::anyhow!("缺少 newPubkey"))?;
    if !new_key.trim_start().starts_with("ssh-") {
        return Err(anyhow::anyhow!("newPubkey 格式非法（必须以 ssh- 开头）"));
    }

    let existing = std::fs::read_to_string(&auth_keys_path).unwrap_or_default();

    // 构建新的 authorized_keys 内容
    let mut lines: Vec<&str> = existing.lines().collect();

    // 追加新公钥（幂等）
    let new_key_trimmed = new_key.trim();
    if !lines.iter().any(|l| l.trim() == new_key_trimmed) {
        lines.push(new_key_trimmed);
        info!("密钥轮换 Phase1: 已追加新公钥");
    }

    // Phase 2: 删除旧公钥
    if let Some(old_key) = msg["removeOldPubkey"].as_str() {
        let old_trimmed = old_key.trim();
        let before = lines.len();
        lines.retain(|l| l.trim() != old_trimmed);
        if lines.len() < before {
            info!("密钥轮换 Phase2: 已删除旧公钥");
        }
    }

    let content = format!("{}\n", lines.join("\n"));

    // 原子写入 tmp → rename
    std::fs::write(&tmp_path, &content)
        .map_err(|e| anyhow::anyhow!("写 authorized_keys.tmp 失败: {e}"))?;
    std::fs::rename(&tmp_path, &auth_keys_path)
        .map_err(|e| anyhow::anyhow!("rename authorized_keys 失败: {e}"))?;

    Ok(())
}

/// 构建心跳 JSON
async fn build_heartbeat(config: &DaemonConfig) -> Result<String> {
    let sys = heartbeat::collect().await?;
    let gnb = gnb_monitor::collect(&config.gnb_map_path.to_string_lossy()).ok();

    let msg = json!({
        "type": "heartbeat",
        "nodeId": config.node_id,
        "ts": sys.ts,
        "sysInfo": sys,
        "gnbPeers": gnb.as_ref().map(|s| &s.peers),
        "gnbTunAddr": gnb.as_ref().map(|s| &s.tun_addr),
        "clawRunning": sys.claw_running,
    });
    Ok(msg.to_string())
}
