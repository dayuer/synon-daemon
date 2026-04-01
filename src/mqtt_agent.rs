//! mqtt_agent.rs — Agent 模式 MQTT 客户端
//!
//! 替代 console_ws.rs，使用 rumqttc AsyncClient 连接 Console 嵌入的 MQTT Broker。
//!
//! 连接行为:
//!   - client_id: "agent-{node_id}"
//!   - LWT: synon/agent/{node_id}/status → "offline" (Retain, QoS 1)
//!   - 连接成功后 publish: synon/agent/{node_id}/status → "online" (Retain)
//!   - 订阅: synon/cmd/{node_id}/# (QoS 1)
//!   - 心跳: 每 10s publish synon/agent/{node_id}/heartbeat (QoS 0)

use crate::config::DaemonConfig;
use crate::claw_proxy::{ClawProxy, ClawEvent};
use crate::claw_manager;
use crate::skills_manager;
use crate::gnb_controller;
use crate::heartbeat;
use crate::gnb_monitor;
use crate::task_executor::{self, TaskMessage};
use crate::watchdog::WatchdogAlert;

use anyhow::Result;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, LastWill};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// 运行 Agent MQTT 客户端（含自动重连 — rumqttc eventloop 内置）
pub async fn run(
    config: DaemonConfig,
    mut alert_rx: mpsc::Receiver<WatchdogAlert>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let node_id = &config.node_id;
    let client_id = format!("agent-{}", node_id);

    // ── MQTT Broker 地址解析 ──────────────────────────────────
    // 优先级: MQTT_HOST 环境变量 → agent.conf MQTT_HOST → console_url 提取 host (降级)
    // 正确做法是使用 Console 的 TUN IP (198.18.0.1)，而非公网域名
    let mqtt_host = std::env::var("MQTT_HOST")
        .ok()
        .or_else(|| config.mqtt_host.clone())
        .unwrap_or_else(|| {
            let fallback = extract_host(&config.console_url).unwrap_or("127.0.0.1".to_string());
            warn!("[MqttAgent] 未配置 MQTT_HOST，降级到 console_url 提取: {}", fallback);
            fallback
        });
    let mqtt_port: u16 = std::env::var("MQTT_PORT")
        .unwrap_or_else(|_| "1883".to_string())
        .parse()
        .unwrap_or(1883);

    info!("[MqttAgent] MQTT 目标: {}:{} (client_id={})", mqtt_host, mqtt_port, client_id);

    // ── 配置 MQTT 连接 ──────────────────────────────────────
    let mut mqttoptions = MqttOptions::new(&client_id, &mqtt_host, mqtt_port);
    mqttoptions.set_keep_alive(Duration::from_secs(30));
    mqttoptions.set_clean_session(true);
    // LWT 遗嘱：TCP 异常断开时 Broker 自动发布 "offline"
    let lwt = LastWill::new(
        format!("synon/agent/{}/status", node_id),
        "offline",
        QoS::AtLeastOnce,
        true, // retain
    );
    mqttoptions.set_last_will(lwt);
    // 认证：使用 node_id/token 作为 username/password
    mqttoptions.set_credentials(&config.node_id, &config.token);

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 64);

    // ── OpenClaw 事件订阅 ────────────────────────────────────
    let (claw_evt_tx, mut claw_evt_rx) = mpsc::channel::<ClawEvent>(32);
    let claw_proxy = ClawProxy::new(
        config.claw_port,
        config.claw_token.as_deref().unwrap_or(""),
    );
    let claw_for_events = claw_proxy.clone_for_events();
    let claw_shutdown = shutdown.clone();
    tokio::spawn(async move {
        claw_for_events.subscribe_events(claw_evt_tx, claw_shutdown).await;
    });

    // ── 串行任务执行器 ──────────────────────────────────────
    let (task_tx, task_rx) = mpsc::channel::<TaskMessage>(16);
    let in_flight = task_executor::new_in_flight();

    // 任务执行器：结果通过 MQTT publish 发送（而非旧 WSS 通道）
    let exec_client = client.clone();
    let exec_node_id = node_id.clone();
    let exec_in_flight = in_flight.clone();
    tokio::spawn(async move {
        run_task_executor(task_rx, exec_client, exec_node_id, exec_in_flight).await;
    });

    // ── 心跳发送循环 ────────────────────────────────────────
    let beat_client = client.clone();
    let beat_config = config.clone();
    let beat_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        loop {
            tokio::select! {
                _ = beat_shutdown.cancelled() => {
                    tracing::debug!("[MqttAgent] 心跳循环收到关闭信号");
                    break;
                }
                _ = ticker.tick() => {
                    // systemd watchdog 心跳
                    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
                    match build_heartbeat(&beat_config).await {
                        Ok(payload) => {
                            let topic = format!("synon/agent/{}/heartbeat", beat_config.node_id);
                            if let Err(e) = beat_client.publish(
                                topic, QoS::AtMostOnce, false, payload.into_bytes()
                            ).await {
                                warn!("[MqttAgent] 心跳发布失败: {}", e);
                            }
                        }
                        Err(e) => warn!("[MqttAgent] 心跳采集失败: {}", e),
                    }
                }
            }
        }
    });

    // ── 主事件循环 ──────────────────────────────────────────
    let mut connected = false;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("[MqttAgent] 收到关闭信号");
                // 优雅离线：发布 offline (Retain)，1s 超时防止阻塞
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    let _ = client.publish(
                        format!("synon/agent/{}/status", node_id),
                        QoS::AtLeastOnce, true, "offline"
                    ).await;
                    let _ = client.disconnect().await;
                }).await;
                break;
            }

            // 转发 claw_event
            Some(evt) = claw_evt_rx.recv() => {
                let msg = json!({
                    "type": "claw_event",
                    "nodeId": config.node_id,
                    "event": evt.event_type,
                    "data": evt.data,
                    "ts": ts_ms(),
                });
                let topic = format!("synon/agent/{}/event", config.node_id);
                let _ = client.publish(topic, QoS::AtLeastOnce, false, msg.to_string().into_bytes()).await;
            }

            // 转发 watchdog 告警
            Some(alert) = alert_rx.recv() => {
                let msg = json!({
                    "type": "watchdog_alert",
                    "nodeId": alert.node_id,
                    "service": alert.service,
                    "reason": alert.reason,
                    "restarted": alert.restarted,
                    "ts": alert.ts,
                });
                let topic = format!("synon/agent/{}/event", config.node_id);
                let _ = client.publish(topic, QoS::AtLeastOnce, false, msg.to_string().into_bytes()).await;
            }

            // MQTT eventloop 驱动
            event = eventloop.poll() => {
                match event {
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        info!("[MqttAgent] MQTT 连接成功 ({}:{})", mqtt_host, mqtt_port);
                        connected = true;

                        // 发布 online 状态 (Retained)
                        let _ = client.publish(
                            format!("synon/agent/{}/status", node_id),
                            QoS::AtLeastOnce, true, "online"
                        ).await;

                        // 订阅指令主题
                        let cmd_topic = format!("synon/cmd/{}/#", node_id);
                        if let Err(e) = client.subscribe(&cmd_topic, QoS::AtLeastOnce).await {
                            warn!("[MqttAgent] 订阅 {} 失败: {}", cmd_topic, e);
                        }

                        // 订阅全局配置
                        let _ = client.subscribe("synon/sys/config/#", QoS::AtLeastOnce).await;
                    }

                    Ok(Event::Incoming(Incoming::Publish(publish))) => {
                        let topic = publish.topic.clone();
                        let payload = publish.payload.to_vec();
                        if let Err(e) = handle_incoming_command(
                            &topic, &payload, &config, &claw_proxy,
                            &client, &task_tx, &in_flight,
                        ).await {
                            warn!("[MqttAgent] 处理指令失败 ({}): {}", topic, e);
                        }
                    }

                    Ok(Event::Incoming(Incoming::Disconnect)) => {
                        if connected {
                            warn!("[MqttAgent] 连接断开，rumqttc 将自动重连...");
                            connected = false;
                        }
                    }

                    Err(e) => {
                        if connected {
                            warn!("[MqttAgent] eventloop 错误: {}, 等待自动重连...", e);
                            connected = false;
                        }
                        // rumqttc 在下次 poll() 时自动重连
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }

                    _ => {} // PingResp, SubAck 等忽略
                }
            }
        }
    }
}

/// 处理来自 Console 的 MQTT 指令
async fn handle_incoming_command(
    topic: &str,
    payload: &[u8],
    config: &DaemonConfig,
    claw: &ClawProxy,
    client: &AsyncClient,
    task_tx: &mpsc::Sender<TaskMessage>,
    in_flight: &task_executor::InFlight,
) -> Result<()> {
    let parts: Vec<&str> = topic.split('/').collect();
    // synon/cmd/{node_id}/{command_type}
    if parts.len() < 4 || parts[0] != "synon" || parts[1] != "cmd" {
        return Ok(());
    }
    let cmd_type = parts[3];
    let msg: Value = serde_json::from_slice(payload)?;
    let req_id = msg.get("reqId").cloned().unwrap_or(Value::Null);

    match cmd_type {
        // ═══ 快速路径（就地执行）═══
        "claw_rpc" => {
            let method = msg["method"].as_str().unwrap_or("status");
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let result = claw.rpc(method, params).await;
            publish_result(client, config, &req_id, result.is_ok(),
                result.unwrap_or_else(|e| json!({"error": e.to_string()}))).await;
        }

        "route_update" => {
            if let Some(conf) = msg["addressConf"].as_str() {
                let ok = gnb_controller::apply_route_update(&config.gnb_conf_dir, conf).await.is_ok();
                publish_result(client, config, &req_id, ok, Value::Null).await;
            }
        }

        "claw_status" => {
            let status = claw_manager::get_full_status(config.claw_port).await;
            publish_result(client, config, &req_id, true,
                serde_json::to_value(&status).unwrap_or(Value::Null)).await;
        }

        "skill_list" => {
            let skills = match skills_manager::refresh_cache().await {
                Ok(v) => v,
                Err(_) => skills_manager::list_from_cache().await,
            };
            publish_result(client, config, &req_id, true, json!(skills)).await;
        }

        // ═══ 慢速路径（入队串行执行器）═══
        "exec" => {
            let command = msg["command"].as_str().unwrap_or("").to_string();
            let allowed: Vec<String> = msg["allowedCmds"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            dispatch_slow(TaskMessage::ExecCmd { req_id, command, allowed_extra: allowed },
                task_tx, client, config, in_flight).await?;
        }

        "skill_install" => {
            let skill_id = msg["skillId"].as_str().unwrap_or("").to_string();
            let source = msg["source"].as_str().unwrap_or("openclaw").to_string();
            let slug = msg["slug"].as_str().map(String::from);
            let github_repo = msg["githubRepo"].as_str().map(String::from);
            if skill_id.is_empty() {
                publish_result(client, config, &req_id, false, json!("缺少 skillId")).await;
            } else {
                dispatch_slow(TaskMessage::SkillInstall { req_id, skill_id, source, slug, github_repo },
                    task_tx, client, config, in_flight).await?;
            }
        }

        "skill_uninstall" => {
            let skill_id = msg["skillId"].as_str().unwrap_or("").to_string();
            let source = msg["source"].as_str().unwrap_or("").to_string();
            if skill_id.is_empty() {
                publish_result(client, config, &req_id, false, json!("缺少 skillId")).await;
            } else {
                dispatch_slow(TaskMessage::SkillUninstall { req_id, skill_id, source },
                    task_tx, client, config, in_flight).await?;
            }
        }

        "skill_update" => {
            let skill_id = msg["skillId"].as_str().unwrap_or("").to_string();
            if skill_id.is_empty() {
                publish_result(client, config, &req_id, false, json!("缺少 skillId")).await;
            } else {
                dispatch_slow(TaskMessage::SkillUpdate { req_id, skill_id },
                    task_tx, client, config, in_flight).await?;
            }
        }

        "claw_upgrade" => {
            let version = msg["version"].as_str().map(String::from);
            dispatch_slow(TaskMessage::ClawUpgrade { req_id, version },
                task_tx, client, config, in_flight).await?;
        }

        "claw_restart" => {
            dispatch_slow(TaskMessage::ClawRestart { req_id },
                task_tx, client, config, in_flight).await?;
        }

        unknown => debug!("[MqttAgent] 未知指令类型: {}", unknown),
    }
    Ok(())
}

/// 发布任务结果到 MQTT
async fn publish_result(client: &AsyncClient, config: &DaemonConfig, req_id: &Value, ok: bool, payload: Value) {
    let req_id_str = req_id.as_str().unwrap_or("unknown");
    let result = json!({
        "type": "cmd_result",
        "reqId": req_id,
        "ok": ok,
        "payload": payload,
    });
    let topic = format!("synon/result/{}/{}", config.node_id, req_id_str);
    if let Err(e) = client.publish(topic, QoS::AtLeastOnce, false, result.to_string().into_bytes()).await {
        warn!("[MqttAgent] 结果发布失败: {}", e);
    }
}

/// 派发耗时任务到串行执行器
async fn dispatch_slow(
    task: TaskMessage,
    task_tx: &mpsc::Sender<TaskMessage>,
    client: &AsyncClient,
    config: &DaemonConfig,
    in_flight: &task_executor::InFlight,
) -> Result<()> {
    let req_id_s = task.req_id_str();

    if !task_executor::try_claim(in_flight, &req_id_s) {
        publish_result(client, config, &json!(req_id_s), false, json!("任务已在执行中")).await;
        return Ok(());
    }

    match task_tx.try_send(task) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            if let Ok(mut set) = in_flight.lock() { set.remove(&req_id_s); }
            publish_result(client, config, &json!(req_id_s), false, json!("任务队列已满")).await;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            if let Ok(mut set) = in_flight.lock() { set.remove(&req_id_s); }
            return Err(anyhow::anyhow!("任务执行器已关闭"));
        }
    }
    Ok(())
}

/// 独立的任务执行器循环（结果通过 MQTT publish 回传，代替 WSS resp_tx）
async fn run_task_executor(
    mut task_rx: mpsc::Receiver<TaskMessage>,
    client: AsyncClient,
    node_id: String,
    in_flight: task_executor::InFlight,
) {
    while let Some(task) = task_rx.recv().await {
        let req_id_s = task.req_id_str();
        let response_json = task_executor::execute(task).await;

        // 解析 execute 返回的 JSON 字符串，提取 reqId 用于 topic
        let topic = format!("synon/result/{}/{}", node_id, req_id_s);
        if let Err(e) = client.publish(topic, QoS::AtLeastOnce, false, response_json.into_bytes()).await {
            warn!("[MqttAgent] 任务结果发布失败: {}", e);
        }

        // 移除 in-flight
        if !req_id_s.is_empty() {
            if let Ok(mut set) = in_flight.lock() { set.remove(&req_id_s); }
        }
    }
}

/// 构建心跳 JSON（复用 heartbeat::collect）
async fn build_heartbeat(config: &DaemonConfig) -> Result<String> {
    let sys = heartbeat::collect().await?;
    let gnb = gnb_monitor::collect(&config.gnb_map_path.to_string_lossy()).await.ok();

    let msg = json!({
        "type": "heartbeat",
        "nodeId": config.node_id,
        "ts": sys.ts,
        "sysInfo": sys,
        "gnbStatus": sys.gnb_status,
        "gnbAddresses": sys.gnb_addresses,
        "gnbPeers": gnb.as_ref().map(|s| &s.peers),
        "gnbTunAddr": gnb.as_ref().map(|s| &s.tun_addr),
        "clawRunning": sys.claw_running,
        "clawRpcOk": sys.claw_rpc_ok,
        "installedSkills": sys.installed_skills,
    });
    Ok(msg.to_string())
}

/// 从 WSS URL 提取 host
fn extract_host(url: &str) -> Option<String> {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from))
}

/// 当前毫秒时间戳
fn ts_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
