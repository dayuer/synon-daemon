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
use crate::claw_proxy::{ClawProxy, ClawEvent};
use crate::claw_manager;
use crate::skills_manager;
use crate::exec_handler;
use crate::gnb_controller;
use crate::heartbeat;
use crate::gnb_monitor;
use crate::watchdog::WatchdogAlert;


use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use socket2::{Socket, TcpKeepalive, Domain, Type, Protocol};
use std::time::Duration;
use std::sync::{Arc, atomic::{AtomicBool, AtomicI64, Ordering}};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep};
use tokio_tungstenite::{tungstenite::Message};
use tracing::{debug, info, warn};
use url::Url;

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
const HEARTBEAT_INTERVAL_SECS: u64 = 10;
const RECONNECT_BASE_SECS: u64 = 3;
const RECONNECT_MAX_SECS: u64 = 60;

/// 运行 Console WSS 连接（含自动重连指数退避 + 优雅关闭）
pub async fn run(
    config: DaemonConfig,
    alert_rx: mpsc::Receiver<WatchdogAlert>,
    shutdown: tokio_util::sync::CancellationToken,
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

        // 重连前检查关闭信号
        tokio::select! {
            _ = sleep(Duration::from_secs(retry_secs)) => {}
            _ = shutdown.cancelled() => {
                info!("收到关闭信号，停止 WSS 重连");
                break;
            }
        }
        retry_secs = (retry_secs * 2).min(RECONNECT_MAX_SECS);
    }
}

/// 单次连接的完整生命周期（带 TCP keepalive 防 NAT 超时）
async fn connect_and_run(
    config: &DaemonConfig,
    alert_rx: &mut mpsc::Receiver<WatchdogAlert>,
) -> Result<()> {
    // ── 1. 解析 URL → 建立带 SO_KEEPALIVE 的 TCP 连接 ──────────────────────
    let url = Url::parse(&config.console_url)
        .map_err(|e| anyhow::anyhow!("URL 解析失败: {e}"))?;
    let host = url.host_str().ok_or_else(|| anyhow::anyhow!("URL 缺少 host"))?;
    let port = url.port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("URL 缺少端口"))?;
    // DNS 解析
    let addr = tokio::net::lookup_host(format!("{host}:{port}"))
        .await
        .map_err(|e| anyhow::anyhow!("DNS 解析失败: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("DNS 解析返回空"))?;

    // 用 socket2 创建 TCP socket，设置 keepalive 参数后再连接
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(|e| anyhow::anyhow!("socket 创建失败: {e}"))?;
    // TCP keepalive: 30s 空闲后开始探测，每 10s 一次，最多 3 次未回应则断开
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    socket.set_tcp_keepalive(&keepalive)
        .map_err(|e| anyhow::anyhow!("设置 TCP keepalive 失败: {e}"))?;

    // 阻塞 connect（socket2 同步 API），然后转 nonblocking 交给 tokio
    socket.connect(&addr.into())
        .map_err(|e| anyhow::anyhow!("TCP 连接失败: {e}"))?;
    socket.set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("set_nonblocking 失败: {e}"))?;
    let std_stream: std::net::TcpStream = socket.into();
    let tcp = tokio::net::TcpStream::from_std(std_stream)
        .map_err(|e| anyhow::anyhow!("TcpStream::from_std 失败: {e}"))?;

    // ── 2. TLS + WebSocket 握手（tokio-tungstenite 接管已连接的 stream）──────
    let (ws_stream, _) = tokio_tungstenite::client_async_tls_with_config(
        &config.console_url[..],
        tcp,
        None,
        None,
    ).await
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
    write.send(Message::Text(hello.to_string())).await?;

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
    let (claw_evt_tx, mut claw_evt_rx) = mpsc::channel::<ClawEvent>(32);

    // 订阅 OpenClaw 实时事件（health/tick）
    let claw_for_events = claw_proxy.clone_for_events();
    tokio::spawn(async move {
        claw_for_events.subscribe_events(claw_evt_tx).await;
    });

    // 心跳发送 Task
    let beat_config = config.clone();
    // pong 超时检测：服务端 10s 发一次 Ping，若 30s 未收到任何 Pong/数据则认为连接已死
    let last_pong = Arc::new(AtomicI64::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    ));
    let pong_dead = Arc::new(AtomicBool::new(false));
    let last_pong_c = Arc::clone(&last_pong);
    let pong_dead_c = Arc::clone(&pong_dead);
    // Watchdog：每 15s 检查一次，超过 45s 没有 Pong 则触发重连
    tokio::spawn(async move {
        let mut t = interval(Duration::from_secs(15));
        loop {
            t.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if now - last_pong_c.load(Ordering::Relaxed) > 45 {
                warn!("[KeepaliveWatchdog] 超过 45s 未收到 Pong/应用帧，标记连接为死亡");
                pong_dead_c.store(true, Ordering::Relaxed);
                break;
            }
        }
    });

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
        if pong_dead.load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!("[KeepaliveWatchdog] 连接心跳超时，触发重连"));
        }
        tokio::select! {
            // 发送心跳
            Some(msg) = beat_rx.recv() => {
                write.send(Message::Text(msg)).await?;
                debug!("心跳已发送");
            }

            // 转发 claw_event 给 Console
            Some(evt) = claw_evt_rx.recv() => {
                let msg = json!({
                    "type": "claw_event",
                    "nodeId": config.node_id,
                    "event": evt.event_type,
                    "data": evt.data,
                    "ts": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                });
                write.send(Message::Text(msg.to_string())).await?;
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
                write.send(Message::Text(msg.to_string())).await?;
            }

            // 接收 Console 下行消息
            msg = read.next() => {
                match msg {
                    None => return Err(anyhow::anyhow!("连接关闭")),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Message::Text(text))) => {
                        // 收到任何应用帧 → 重置 pong watchdog
                        last_pong.store(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64,
                            Ordering::Relaxed,
                        );
                        if let Ok(val) = serde_json::from_str::<Value>(&text) {
                            if let Err(e) = handle_server_message(&val, &config, &claw_proxy, &mut write).await {
                                warn!("处理 Console 消息失败: {e}");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => return Ok(()),
                    Some(Ok(Message::Ping(d))) => {
                        // 收到 Ping → 重置 pong watchdog + 回 Pong
                        last_pong.store(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64,
                            Ordering::Relaxed,
                        );
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
            write.send(Message::Text(resp.to_string())).await?;
        }

        // 路由拓扑更新（Phase 2）
        "route_update" => {
            if let Some(conf) = msg["addressConf"].as_str() {
                let ok = gnb_controller::apply_route_update(&config.gnb_conf_dir, conf)
                    .map_err(|e| warn!("应用 route_update 失败: {e}"))
                    .is_ok();
                let resp = json!({ "type": "cmd_result", "reqId": req_id, "ok": ok });
                write.send(Message::Text(resp.to_string())).await?;
            }
        }

        // 密钥滚动更新
        "key_rotate" => {
            let ok = handle_key_rotate(msg).is_ok();
            let resp = json!({ "type": "cmd_result", "reqId": req_id, "ok": ok });
            write.send(Message::Text(resp.to_string())).await?;
        }

        // 受限命令执行（Phase 3）
        "exec_cmd" => {
            let command = msg["command"].as_str().unwrap_or("");
            let allowed_extra: Vec<&str> = msg["allowedCmds"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let result = exec_handler::exec_allowed(command, &allowed_extra).await;
            let resp = json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.code == 0,
                "code": result.code,
                "stdout": result.stdout,
                "stderr": result.stderr,
            });
            write.send(Message::Text(resp.to_string())).await?;
        }

        // 文件分发（Phase 3）
        "deploy_file" => {
            let path_str = msg["path"].as_str().unwrap_or("");
            let content_b64 = msg["content_b64"].as_str().unwrap_or("");

            let result = handle_deploy_file(path_str, content_b64);
            let resp = json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "stderr": result.err().map(|e| e.to_string()).unwrap_or_default(),
            });
            write.send(Message::Text(resp.to_string())).await?;
        }


        // ═══════════════════════════════════════════
        // OpenClaw 生命周期管理（claw_manager）
        // ═══════════════════════════════════════════

        // 查询 OpenClaw 完整状态
        "claw_status" => {
            let status = claw_manager::get_full_status(config.claw_port).await;
            let resp = json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": true,
                "payload": serde_json::to_value(&status).unwrap_or(Value::Null),
            });
            write.send(Message::Text(resp.to_string())).await?;
        }

        // 升级 OpenClaw（可选指定版本）
        "claw_upgrade" => {
            let version = msg["version"].as_str();
            let result = claw_manager::upgrade(version).await;
            let resp = json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.unwrap_or_else(|e| e.to_string()),
            });
            write.send(Message::Text(resp.to_string())).await?;
        }

        // 重启 openclaw-gateway
        "claw_restart" => {
            let result = claw_manager::restart().await;
            let resp = json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": result.is_ok(),
                "payload": result.err().map(|e| e.to_string()),
            });
            write.send(Message::Text(resp.to_string())).await?;
        }

        // ═══════════════════════════════════════════
        // Skills 生命周期管理（skills_manager）
        // ═══════════════════════════════════════════

        // 列出已安装技能
        "skill_list" => {
            // 先尝试 CLI 实时查询并刷新缓存；如果 OpenClaw 未运行则降级到缓存
            let skills = if claw.ping().await {
                skills_manager::refresh_cache().await.unwrap_or_else(|_| skills_manager::list_from_cache())
            } else {
                skills_manager::list_from_cache()
            };
            let resp = json!({
                "type": "cmd_result",
                "reqId": req_id,
                "ok": true,
                "payload": skills,
            });
            write.send(Message::Text(resp.to_string())).await?;
        }

        // 安装技能
        "skill_install" => {
            let skill_id = msg["skillId"].as_str().unwrap_or("").to_string();
            if skill_id.is_empty() {
                let resp = json!({ "type": "cmd_result", "reqId": req_id, "ok": false, "payload": "缺少 skillId" });
                write.send(Message::Text(resp.to_string())).await?;
            } else {
                let result = skills_manager::install(&skill_id).await;
                let resp = json!({
                    "type": "cmd_result",
                    "reqId": req_id,
                    "ok": result.is_ok(),
                    "payload": result.unwrap_or_else(|e| e.to_string()),
                });
                write.send(Message::Text(resp.to_string())).await?;
            }
        }

        // 卸载技能
        "skill_uninstall" => {
            let skill_id = msg["skillId"].as_str().unwrap_or("").to_string();
            if skill_id.is_empty() {
                let resp = json!({ "type": "cmd_result", "reqId": req_id, "ok": false, "payload": "缺少 skillId" });
                write.send(Message::Text(resp.to_string())).await?;
            } else {
                let result = skills_manager::uninstall(&skill_id).await;
                let resp = json!({
                    "type": "cmd_result",
                    "reqId": req_id,
                    "ok": result.is_ok(),
                    "payload": result.unwrap_or_else(|e| e.to_string()),
                });
                write.send(Message::Text(resp.to_string())).await?;
            }
        }

        // 更新技能
        "skill_update" => {
            let skill_id = msg["skillId"].as_str().unwrap_or("").to_string();
            if skill_id.is_empty() {
                let resp = json!({ "type": "cmd_result", "reqId": req_id, "ok": false, "payload": "缺少 skillId" });
                write.send(Message::Text(resp.to_string())).await?;
            } else {
                let result = skills_manager::update(&skill_id).await;
                let resp = json!({
                    "type": "cmd_result",
                    "reqId": req_id,
                    "ok": result.is_ok(),
                    "payload": result.unwrap_or_else(|e| e.to_string()),
                });
                write.send(Message::Text(resp.to_string())).await?;
            }
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

/// 文件分发处理 — base64 解码后原子写入目标路径
fn handle_deploy_file(path_str: &str, content_b64: &str) -> Result<()> {
    use std::path::Path;

    if path_str.is_empty() {
        return Err(anyhow::anyhow!("path 为空"));
    }

    let path = Path::new(path_str);

    // 安全校验：必须绝对路径 + 不含路径穿越
    if !path.is_absolute() {
        return Err(anyhow::anyhow!("path 必须是绝对路径: {path_str}"));
    }
    if path_str.contains("..") {
        return Err(anyhow::anyhow!("path 不允许包含 '..': {path_str}"));
    }

    // base64 解码
    let content = base64_decode(content_b64)?;

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("创建目录失败 {}: {e}", parent.display()))?;
    }

    // 原子写入 tmp → rename
    let tmp = path.with_extension("deploy_tmp");
    std::fs::write(&tmp, &content)
        .map_err(|e| anyhow::anyhow!("写临时文件失败: {e}"))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| anyhow::anyhow!("rename 失败: {e}"))?;

    info!("[DeployFile] 写入 {} ({} bytes)", path_str, content.len());
    Ok(())
}

/// 简单 base64 解码（标准字母表，padding 可选）
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    // 使用 Rust 标准方式：通过 u8 查表
    let input = input.trim();
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        table[c as usize] = i as u8;
    }

    let input: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    let mut i = 0;
    while i < input.len() {
        let rem = input.len() - i;
        let a = table[input[i] as usize];
        if a == 255 { return Err(anyhow::anyhow!("base64 字符无效")); }

        if rem >= 2 {
            let b = table[input[i+1] as usize];
            if b == 255 { return Err(anyhow::anyhow!("base64 字符无效")); }
            out.push((a << 2) | (b >> 4));
            if rem >= 3 {
                let c = table[input[i+2] as usize];
                if c == 255 { return Err(anyhow::anyhow!("base64 字符无效")); }
                out.push((b << 4) | (c >> 2));
                if rem >= 4 {
                    let d = table[input[i+3] as usize];
                    if d == 255 { return Err(anyhow::anyhow!("base64 字符无效")); }
                    out.push((c << 6) | d);
                    i += 4;
                } else { i += 3; }
            } else { i += 2; }
        } else { i += 1; }
    }
    Ok(out)
}

/// 构建心跳 JSON（对齐 node-agent.sh 全量字段，Console ingestFromDaemon 直接消费）
async fn build_heartbeat(config: &DaemonConfig) -> Result<String> {
    let sys = heartbeat::collect().await?;
    let gnb = gnb_monitor::collect(&config.gnb_map_path.to_string_lossy()).ok();

    let msg = json!({
        "type": "heartbeat",
        "nodeId": config.node_id,
        "ts": sys.ts,
        "sysInfo": sys,                            // 完整系统状态（CPU/内存/磁盘等）
        "gnbStatus": sys.gnb_status,               // gnb_ctl -s 原始输出（与 agent.sh 兼容）
        "gnbAddresses": sys.gnb_addresses,         // gnb_ctl -a 原始输出
        "gnbPeers": gnb.as_ref().map(|s| &s.peers),
        "gnbTunAddr": gnb.as_ref().map(|s| &s.tun_addr),
        "clawRunning": sys.claw_running,
        "clawRpcOk": sys.claw_rpc_ok,
        "installedSkills": sys.installed_skills,   // /opt/gnb/cache/skills.json
    });
    Ok(msg.to_string())
}
