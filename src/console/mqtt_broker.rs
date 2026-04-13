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
        max_payload_size: 256 * 1024,       // 256KB — 心跳含 skills 约 40KB，留足余量
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

                        // @v7: 任务补齐 — dispatched 回退 queued（不算重试）
                        let reset_count = conn.execute(
                            "UPDATE agent_tasks 
                             SET status = 'queued'
                             WHERE nodeId = ?1 
                             AND status = 'dispatched'
                             AND (expiresAt IS NULL OR expiresAt > datetime('now'))",
                            rusqlite::params![nid_owned],
                        ).unwrap_or(0);
                        if reset_count > 0 {
                            tracing::info!(
                                "[TaskCatchup] 节点 {} 上线，{} 个 dispatched 任务回退 queued",
                                nid_owned, reset_count
                            );
                        }

                        // @v7: 广播补录 — 检查活跃广播中该节点是否缺少子任务
                        broadcast_catchup(conn, &nid_owned);
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

    // synon/agent/{node_id}/event — claw_event / watchdog_alert / task_sync
    if parts.len() == 4 && parts[0] == "synon" && parts[1] == "agent" && parts[3] == "event" {
        if let Ok(json_str) = std::str::from_utf8(payload) {
            // @v7: 检查是否为 task_sync 请求
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
                if json.get("type").and_then(|v| v.as_str()) == Some("task_sync") {
                    let node_id = parts[2].to_string();
                    let db = db_pool.clone();
                    tokio::spawn(async move {
                        if let Ok(c) = db.get().await {
                            let _ = c.interact(move |conn| {
                                // dispatched → queued 回退
                                let reset = conn.execute(
                                    "UPDATE agent_tasks SET status = 'queued'
                                     WHERE nodeId = ?1 AND status = 'dispatched'
                                     AND (expiresAt IS NULL OR expiresAt > datetime('now'))",
                                    rusqlite::params![node_id],
                                ).unwrap_or(0);
                                if reset > 0 {
                                    tracing::info!(
                                        "[TaskSync] Agent {} 请求补齐，{} 个任务回退 queued",
                                        node_id, reset
                                    );
                                }
                                // 广播补录
                                broadcast_catchup(conn, &node_id);
                            }).await;
                        }
                    });
                    return; // task_sync 不广播给 UI
                }
            }
            session.broadcast_to_ui(json_str).await;
        }
        return;
    }

    // synon/result/{node_id}/{req_id} — 任务回执 / ACK
    if parts.len() == 4 && parts[0] == "synon" && parts[1] == "result" {
        let task_id = parts[3];
        if let Ok(json_str) = std::str::from_utf8(payload) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
                // @v7: 区分 task_ack 和 cmd_result
                let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("cmd_result");

                if msg_type == "task_ack" {
                    // ACK: 确认收到，更新状态为 acked
                    let db = db_pool.clone();
                    let tid = task_id.to_string();
                    tokio::spawn(async move {
                        if let Ok(c) = db.get().await {
                            let _ = c.interact(move |conn| {
                                conn.execute(
                                    "UPDATE agent_tasks
                                     SET status = 'acked',
                                         ackedAt = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                                     WHERE taskId = ?1 AND status = 'dispatched'",
                                    rusqlite::params![tid],
                                )
                            }).await;
                        }
                    });
                    return;
                }

                // cmd_result: 最终执行结果
                let ok = json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let status_str = if ok { "success" } else { "failed" };
                // 兼容两种格式：Agent 发 stdout/stderr，旧格式用 payload
                let stdout_str = json.get("stdout").and_then(|v| v.as_str())
                    .or_else(|| if ok { json.get("payload").and_then(|v| v.as_str()) } else { None })
                    .unwrap_or("");
                let stderr_str = json.get("stderr").and_then(|v| v.as_str())
                    .or_else(|| if !ok { json.get("payload").and_then(|v| v.as_str()) } else { None })
                    .unwrap_or("");

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
                                     completedAt = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                                 WHERE taskId = ?4",
                                rusqlite::params![ss, so, se, tid],
                            ).ok();

                            // @v7: 广播任务进度更新
                            let broadcast_id: Option<String> = conn.query_row(
                                "SELECT broadcastId FROM agent_tasks WHERE taskId = ?1 AND broadcastId != ''",
                                rusqlite::params![tid],
                                |r| r.get(0),
                            ).ok();
                            if let Some(bid) = broadcast_id {
                                update_broadcast_progress(conn, &bid);
                            }
                        }).await;
                    }
                });
            }
        }
        return;
    }

    tracing::debug!("[MqttBroker] 未匹配主题: {}", topic);
}

/// 广播补录：检查活跃广播中该节点是否缺少子任务，补建行
fn broadcast_catchup(conn: &rusqlite::Connection, node_id: &str) {
    let active_broadcasts: Vec<(String, String, String)> = conn.prepare(
        "SELECT broadcastId, type, command FROM broadcast_tasks 
         WHERE status NOT IN ('completed', 'expired')
         AND (expiresAt IS NULL OR expiresAt > datetime('now'))"
    ).and_then(|mut stmt| {
        stmt.query_map([], |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        )))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
    }).unwrap_or_default();

    for (bid, task_type, command) in active_broadcasts {
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM agent_tasks 
             WHERE broadcastId = ?1 AND nodeId = ?2",
            rusqlite::params![bid, node_id],
            |r| r.get(0),
        ).unwrap_or(true);

        if !exists {
            let task_id = format!("{}-{}", bid, node_id);
            let _ = conn.execute(
                "INSERT OR IGNORE INTO agent_tasks
                 (taskId, nodeId, type, command, scope, broadcastId,
                  status, queuedAt, maxRetries, retryCount, expiresAt)
                 VALUES (?1, ?2, ?3, ?4, 'broadcast', ?5,
                         'queued', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 3, 0,
                         (SELECT expiresAt FROM broadcast_tasks WHERE broadcastId = ?5))",
                rusqlite::params![task_id, node_id, task_type, command, bid],
            );
            // 更新广播计数
            let _ = conn.execute(
                "UPDATE broadcast_tasks SET totalNodes = totalNodes + 1 
                 WHERE broadcastId = ?1",
                rusqlite::params![bid],
            );
            tracing::info!(
                "[BroadcastCatchup] 节点 {} 补录广播任务 {} (taskId={})",
                node_id, bid, task_id
            );
        }
    }
}

/// 更新广播任务的完成进度
fn update_broadcast_progress(conn: &rusqlite::Connection, broadcast_id: &str) {
    #[derive(Default)]
    struct Stats { total: i64, completed: i64, failed: i64 }

    let stats = conn.query_row(
        "SELECT COUNT(*) AS total,
                SUM(CASE WHEN status IN ('success', 'completed') THEN 1 ELSE 0 END) AS completed,
                SUM(CASE WHEN status IN ('failed', 'exhausted') THEN 1 ELSE 0 END) AS failed
         FROM agent_tasks WHERE broadcastId = ?1",
        rusqlite::params![broadcast_id],
        |r| Ok(Stats {
            total: r.get(0)?,
            completed: r.get(1)?,
            failed: r.get(2)?,
        }),
    ).unwrap_or_default();

    let all_done = (stats.completed + stats.failed) >= stats.total;
    let status = if all_done {
        if stats.failed > 0 { "partial_failure" } else { "completed" }
    } else {
        "dispatching"
    };

    let _ = conn.execute(
        "UPDATE broadcast_tasks 
         SET completedNodes = ?1, failedNodes = ?2, status = ?3,
             completedAt = CASE WHEN ?4 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ELSE NULL END
         WHERE broadcastId = ?5",
        rusqlite::params![stats.completed, stats.failed, status, all_done, broadcast_id],
    );
}
