//! mqtt_broker.rs — Console 嵌入式 MQTT Broker
//!
//! 基于 rumqttd 在 Console 进程内运行一个完整的 MQTT v4 Broker。
//! 通过进程内 local link（零网络开销）订阅 Agent 上报的心跳、状态与任务回执，
//! 驱动 SQLite 写入和 Web UI 广播。
//!
//! 主题路由树:
//!   synon/agent/{node_id}/status     — LWT 在线/离线 (Retained)
//!   synon/agent/{node_id}/heartbeat  — 心跳上报 (QoS 0)
//!   synon/agent/{node_id}/event      — claw_event / watchdog_alert (QoS 1)
//!   synon/result/{node_id}/+         — 任务回执 (QoS 1)
//!   synon/cmd/{node_id}/+            — Console → Agent 指令 (QoS 1)

use rumqttd::{Broker, Config, Notification, ConnectionSettings, ServerSettings, RouterConfig};
use rumqttd::local::{LinkTx, LinkRx};
use deadpool_sqlite::Pool;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

use crate::console::heartbeat as console_heartbeat;
use crate::console::session::SessionState;

/// 编程式构建 Broker 配置（不依赖外部 TOML 文件）
///
/// 默认绑定 GNB TUN 地址 198.18.0.1:1883，可通过环境变量覆盖：
///   MQTT_BIND_ADDR=0.0.0.0  （开发环境使用）
///   MQTT_BIND_PORT=1883
pub fn build_broker_config() -> Config {
    let connection_settings = ConnectionSettings {
        connection_timeout_ms: 60_000,
        max_payload_size: 65_536,       // 64KB — 心跳 JSON 最大约 4KB，留足余量
        max_inflight_count: 100,
        auth: None,
        external_auth: None,
        dynamic_filters: true,
    };

    // 默认绑定 TUN 接口（198.18.0.1），仅通过 GNB P2P 隧道可达
    let bind_addr: std::net::Ipv4Addr = std::env::var("MQTT_BIND_ADDR")
        .unwrap_or_else(|_| "198.18.0.1".to_string())
        .parse()
        .unwrap_or(std::net::Ipv4Addr::new(198, 18, 0, 1));
    let bind_port: u16 = std::env::var("MQTT_BIND_PORT")
        .unwrap_or_else(|_| "1883".to_string())
        .parse()
        .unwrap_or(1883);

    let mut v4_listeners = HashMap::new();
    v4_listeners.insert(
        "mqtt-tcp".to_string(),
        ServerSettings {
            name: "mqtt-tcp".to_string(),
            listen: std::net::SocketAddr::new(std::net::IpAddr::V4(bind_addr), bind_port),
            next_connection_delay_ms: 1,
            connections: connection_settings,
            tls: None,
        },
    );

    Config {
        id: 0,
        router: RouterConfig {
            max_connections: 200,
            max_outgoing_packet_count: 200,
            max_segment_size: 100 * 1024 * 1024,  // 100MB
            max_segment_count: 10,
            custom_segment: None,
            initialized_filters: None,
            shared_subscriptions_strategy: Default::default(),
        },
        v4: Some(v4_listeners),
        v5: None,
        ws: None,
        prometheus: None,
        bridge: None,
        console: None,
        cluster: None,
        metrics: None,
    }
}

/// 创建 Broker 实例和两组 local link（在 mod.rs 中调用）
///
/// 返回:
///   - broker: Broker 实例（需在独立线程中 start()）
///   - consumer link (tx, rx): 用于 mqtt_broker::run_consumer_bridge 订阅消费
///   - dispatcher link_tx: 用于 task_queue 发布下行指令（只需 tx，不需要 rx）
pub fn create_broker() -> (Broker, LinkTx, LinkRx, LinkTx) {
    let config = build_broker_config();
    let broker = Broker::new(config);
    let (consumer_tx, consumer_rx) = broker
        .link("console-core")
        .expect("创建 consumer link 不应失败");
    let (dispatcher_tx, _dispatcher_rx) = broker
        .link("console-dispatcher")
        .expect("创建 dispatcher link 不应失败");
    (broker, consumer_tx, consumer_rx, dispatcher_tx)
}

/// 独立消费者线程：从 rumqttd local link 读取消息，通过 mpsc 转发到 tokio 异步世界
pub async fn run_consumer_bridge(
    db_pool: Pool,
    session: SessionState,
    shutdown: CancellationToken,
    mut link_tx: LinkTx,
    link_rx: LinkRx,
) {
    // mpsc 桥接：阻塞式 link_rx → tokio 异步世界
    let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel::<BridgeMessage>(256);

    // 订阅主题
    for topic in &[
        "synon/agent/+/heartbeat",
        "synon/agent/+/status",
        "synon/agent/+/event",
        "synon/result/+/+",
    ] {
        if let Err(e) = link_tx.subscribe(*topic) {
            tracing::error!("[MqttBroker] 订阅 {} 失败: {}", topic, e);
        }
    }

    tracing::info!("[MqttBroker] Console 消费者桥接已就绪");

    // 阻塞式读取线程
    let bridge_tx_clone = bridge_tx.clone();
    std::thread::spawn(move || {
        consume_link_blocking(link_rx, bridge_tx_clone);
    });

    // 异步消费循环
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("[MqttBroker] 消费者收到关闭信号");
                break;
            }
            msg = bridge_rx.recv() => {
                let Some(msg) = msg else { break };
                handle_mqtt_message(&msg.topic, &msg.payload, &db_pool, &session).await;
            }
        }
    }
}

/// 阻塞式消费 link_rx，逐条转发到 mpsc bridge
fn consume_link_blocking(
    mut link_rx: LinkRx,
    bridge_tx: tokio::sync::mpsc::Sender<BridgeMessage>,
) {
    loop {
        match link_rx.recv() {
            Ok(Some(notification)) => {
                if let Notification::Forward(forward) = notification {
                    let topic = String::from_utf8_lossy(&forward.publish.topic).to_string();
                    let payload = forward.publish.payload.to_vec();
                    if bridge_tx.blocking_send(BridgeMessage { topic, payload }).is_err() {
                        break; // 接收端已关闭
                    }
                }
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }
    tracing::info!("[MqttBroker] link_rx 消费线程退出");
}

struct BridgeMessage {
    topic: String,
    payload: Vec<u8>,
}

/// 根据 MQTT 主题分发处理
async fn handle_mqtt_message(
    topic: &str,
    payload: &[u8],
    db_pool: &Pool,
    session: &SessionState,
) {
    let parts: Vec<&str> = topic.split('/').collect();

    // synon/agent/{node_id}/heartbeat
    if parts.len() == 4 && parts[0] == "synon" && parts[1] == "agent" && parts[3] == "heartbeat" {
        let node_id = parts[2];
        if let Ok(json_str) = std::str::from_utf8(payload) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
                // 写入 SQLite
                if let Err(e) = console_heartbeat::ingest(node_id, &json, db_pool).await {
                    tracing::warn!("[MqttBroker] 心跳入库失败 ({}): {}", node_id, e);
                }
                // 广播给 Web UI（补上 type 字段让前端能识别）
                let wrapped = serde_json::json!({
                    "type": "heartbeat",
                    "nodeId": node_id,
                    "data": json,
                });
                session.broadcast_to_ui(&wrapped.to_string()).await;
            }
        }
        return;
    }

    // synon/agent/{node_id}/status — LWT 遗嘱 或 Agent 主动发布
    if parts.len() == 4 && parts[0] == "synon" && parts[1] == "agent" && parts[3] == "status" {
        let node_id = parts[2];
        let status = std::str::from_utf8(payload).unwrap_or("unknown");
        tracing::info!("[MqttBroker] 节点 {} 状态变更: {}", node_id, status);

        let nid = node_id.to_string();
        let status_str = status.to_string();
        let db = db_pool.clone();
        tokio::spawn(async move {
            if let Ok(c) = db.get().await {
                let status_owned = status_str.clone();
                let nid_owned = nid.clone();
                let _ = c.interact(move |conn| {
                    // 在线时自动注册
                    if status_owned == "online" {
                        let _ = conn.execute(
                            "INSERT OR IGNORE INTO nodes (id, name, status, submittedAt) 
                             VALUES (?1, ?1, 'online', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                            rusqlite::params![nid_owned],
                        );
                    }
                    conn.execute(
                        "UPDATE nodes SET status = ?1, updatedAt = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
                        rusqlite::params![status_owned, nid_owned],
                    )
                }).await;
            }
        });

        // 广播状态变更给 Web UI
        let ui_msg = serde_json::json!({
            "type": "node_status",
            "nodeId": node_id,
            "status": status,
        });
        session.broadcast_to_ui(&ui_msg.to_string()).await;

        // LWT 离线时额外广播专用事件 — 供 Node.js WsBridge 触发 AutoHeal 快速路径
        if status == "offline" {
            let lwt_msg = serde_json::json!({
                "type": "lwt_offline",
                "nodeId": node_id,
                "source": "mqtt_lwt",
                "ts": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            });
            session.broadcast_to_ui(&lwt_msg.to_string()).await;
            tracing::warn!("[MqttBroker] LWT 离线事件已广播: {}", node_id);
        }
        return;
    }

    // synon/agent/{node_id}/event — claw_event / watchdog_alert
    if parts.len() == 4 && parts[0] == "synon" && parts[1] == "agent" && parts[3] == "event" {
        if let Ok(json_str) = std::str::from_utf8(payload) {
            session.broadcast_to_ui(json_str).await;
        }
        return;
    }

    // synon/result/{node_id}/{req_id} — 任务回执
    if parts.len() == 4 && parts[0] == "synon" && parts[1] == "result" {
        let task_id = parts[3];
        if let Ok(json_str) = std::str::from_utf8(payload) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
                let ok = json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let status_str = if ok { "success" } else { "failed" };
                let payload_str = json.get("payload").and_then(|p| p.as_str()).unwrap_or("");
                let stderr_str = if !ok {
                    json.get("stderr").and_then(|e| e.as_str()).unwrap_or(payload_str)
                } else { "" };
                let stdout_str = if ok { payload_str } else { "" };

                let db = db_pool.clone();
                let tid = task_id.to_string();
                let so = stdout_str.to_string();
                let se = stderr_str.to_string();
                let ss = status_str.to_string();
                tokio::spawn(async move {
                    if let Ok(c) = db.get().await {
                        let _ = c.interact(move |conn| {
                            conn.execute(
                                "UPDATE agent_tasks 
                                 SET status = ?1, resultStdout = ?2, resultStderr = ?3,
                                     completedAt = strftime('%Y-%m-%dT%H:%M:%f%z', 'now') 
                                 WHERE taskId = ?4",
                                rusqlite::params![ss, so, se, tid],
                            )
                        }).await;
                    }
                });
            }
        }
        return;
    }

    tracing::debug!("[MqttBroker] 未匹配主题: {}", topic);
}
