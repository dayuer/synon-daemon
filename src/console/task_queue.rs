use axum::extract::ws::Message;
use deadpool_sqlite::Pool;
use serde_json::json;
use std::time::Duration;
use crate::console::session::SessionState;

/// 任务结构（对应 agent_tasks 表查询结果）
struct AgentTask {
    task_id: String,
    node_id: String,
    task_type: String,
    command: String,
}

/// 后台定期轮询 agent_tasks 表，并将 pending / queued 的任务分发给连接的 Client 节点。
pub async fn run_scheduler(db_pool: Pool, session: SessionState) {
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    let mut last_dream_at = std::time::Instant::now();
    let mut is_idle = false;
    
    loop {
        interval.tick().await;

        // 1. 获取当前处于 queued 的任务（只查询在线 Node 的任务）
        let pool = db_pool.clone();
        let tasks = match pool.get().await {
            Ok(conn) => {
                let res = conn.interact(move |c| -> Result<Vec<AgentTask>, rusqlite::Error> {
                    let mut stmt = c.prepare(
                        "SELECT taskId, nodeId, type, command 
                         FROM agent_tasks 
                         WHERE status = 'queued'
                         ORDER BY queuedAt ASC
                         LIMIT 50" // 每次取最多50条防拥塞
                    )?;
                    
                    let rows = stmt.query_map([], |row| {
                        Ok(AgentTask {
                            task_id: row.get(0)?,
                            node_id: row.get(1)?,
                            task_type: row.get(2)?,
                            command: row.get(3)?,
                        })
                    })?;
                    
                    let mut tasks = Vec::new();
                    for t in rows {
                        if let Ok(task) = t { tasks.push(task); }
                    }
                    Ok(tasks)
                }).await;
                
                match res {
                    Ok(Ok(tasks)) => tasks,
                    Ok(Err(e)) => {
                        tracing::error!("SQLite查询任务失败: {}", e);
                        continue;
                    }
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };

        if tasks.is_empty() {
            if !is_idle {
                tracing::debug!("[TaskQueue] 队列已清空，进入闲置期...");
                is_idle = true;
            }
            if last_dream_at.elapsed().as_secs() >= 300 {
                last_dream_at = std::time::Instant::now();
                tokio::spawn(async move {
                    trigger_auto_dream().await;
                });
            }
            continue;
        } else {
            if is_idle {
                tracing::debug!("[TaskQueue] 队列有新任务，打断闲置期");
                is_idle = false;
            }
            // 每次有任务时重置计时器，确保只有完全无任务的 5 分钟后才触发
            last_dream_at = std::time::Instant::now();
        }

        // 2. 尝试下发任务
        for task in tasks {
            let agents = session.agents.read().await;
            if let Some(sender) = agents.get(&task.node_id) {
                // 构造发给 Agent 的 WebSocket Frame
                let msg_payload = match task.task_type.as_str() {
                    "exec_cmd" => json!({
                        "type": "exec_cmd",
                        "reqId": task.task_id,
                        "command": task.command,
                        "allowedCmds": ["*"] // TODO: 细粒度白名单安全校验
                    }),
                    "skill_install" => json!({
                        "type": "skill_install",
                        "reqId": task.task_id,
                        "skillId": task.command, // task.command 存放的是 skill name
                    }),
                    "skill_uninstall" => json!({
                        "type": "skill_uninstall",
                        "reqId": task.task_id,
                        "skillId": task.command,
                    }),
                    "skill_update" => json!({
                        "type": "skill_update",
                        "reqId": task.task_id,
                        "skillId": task.command,
                    }),
                    "deploy_file" => {
                        let parsed: serde_json::Value = serde_json::from_str(&task.command).unwrap_or(json!({}));
                        json!({
                            "type": "deploy_file",
                            "reqId": task.task_id,
                            "path": parsed["path"],
                            "content_b64": parsed["content_b64"],
                        })
                    },
                    _ => {
                        tracing::warn!("未知任务类型: {}", task.task_type);
                        continue;
                    }
                };

                // 执行下发
                if sender.send(Message::Text(msg_payload.to_string())).is_ok() {
                    tracing::info!("任务 {} 已下发到 {}", task.task_id, task.node_id);
                    // 标记数据库为 dispatched
                    let p2 = db_pool.clone();
                    let tid = task.task_id.clone();
                    tokio::spawn(async move {
                        if let Ok(c) = p2.get().await {
                            let _ = c.interact(move |c| {
                                c.execute(
                                    "UPDATE agent_tasks 
                                     SET status = 'dispatched', 
                                         dispatchedAt = strftime('%Y-%m-%dT%H:%M:%f%z', 'now') 
                                     WHERE taskId = ?1",
                                    rusqlite::params![tid]
                                )
                            }).await;
                        }
                    });
                }
            }
        }
    }
}

/// 触发 Node.js 端的 Auto-Dream 记忆归纳（Sweeper 等清理逻辑）
async fn trigger_auto_dream() {
    tracing::info!("[Auto-Dream] 节点空闲超过 5 分钟，触发记忆离线归纳引擎 (Sweeper) 探针...");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
        
    let nextjs_port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let url = format!("http://127.0.0.1:{}/api/health/sweep", nextjs_port);
    
    match client.post(&url)
        .header("x-agent-mode", "true")
        .send()
        .await 
    {
        Ok(res) => {
            if res.status().is_success() {
                if let Ok(body) = res.text().await {
                    tracing::info!("[Auto-Dream] 归纳完成: {}", body);
                } else {
                    tracing::info!("[Auto-Dream] 归纳完成.");
                }
            } else {
                tracing::warn!("[Auto-Dream] 归纳引擎返回异常状态: {}", res.status());
            }
        }
        Err(e) => {
            tracing::warn!("[Auto-Dream] 调用 Node.js 探针失败: {}", e);
        }
    }
}
