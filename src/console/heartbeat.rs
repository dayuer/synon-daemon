use deadpool_sqlite::Pool;
use serde_json::Value;

/// 处理来自 Agent 的心跳，并更新数据库状态
///
/// 提取 JSON 中的各项运行指标（CPU、内存、GNB 状态等），
/// 1. 更新 `nodes` 表：存活状态、系统版本等静态字段
/// 2. 写入 `metrics` 表：记录时序监控数据
pub async fn ingest(
    node_id: &str,
    frame: &Value,
    db_pool: &Pool,
) -> Result<(), String> {
    // 解析 sysInfo
    let sys = frame.get("sysInfo").unwrap_or(&Value::Null);
    let cpu = sys.get("cpuPercent").and_then(Value::as_i64).unwrap_or(0);
    let mem = sys.get("memPercent").and_then(Value::as_i64).unwrap_or(0);
    let disk = sys.get("diskPercent").and_then(Value::as_i64).unwrap_or(0);
    let load_avg = sys.get("loadAvg").and_then(Value::as_str).unwrap_or("0").to_string();
    let mem_total = sys.get("memTotalMb").and_then(Value::as_i64).unwrap_or(0);
    let mem_used = sys.get("memUsedMb").and_then(Value::as_i64).unwrap_or(0);
    
    // 解析 GNB P2P
    let gnb_peers = frame.get("gnbPeers").and_then(Value::as_array);
    let p2p_total = gnb_peers.map(|p| p.len() as i64).unwrap_or(0);
    let p2p_direct = gnb_peers.map(|p| {
        p.iter().filter(|node| node.get("status").and_then(Value::as_str).unwrap_or("") == "Direct").count() as i64
    }).unwrap_or(0);

    // 获取数据库连接
    let conn = db_pool.get().await.map_err(|e| format!("DB池获取失败: {}", e))?;
    
    // 预先提取必须的值，避免将 &Value 移动到闭包中
    let skills_str = frame.get("installedSkills").map(|s| s.to_string()).unwrap_or_else(|| "[]".to_string());
    let ts = frame.get("ts").and_then(Value::as_i64).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    });
    let node_id_clone = node_id.to_string();

    // 在 spawn_blocking 中执行同步 sqlite 操作
    conn.interact(move |conn| -> Result<(), rusqlite::Error> {
        // 1. 更新节点基础在线状态与配置
        conn.execute(
            "UPDATE nodes SET 
                status = 'online', 
                updatedAt = strftime('%Y-%m-%dT%H:%M:%f%z', 'now'),
                skills = ?1
             WHERE id = ?2",
            rusqlite::params![skills_str, node_id_clone],
        )?;

        // 2. 插入时序 Metrics 数据
        conn.execute(
            "INSERT INTO metrics 
             (nodeId, ts, cpu, memPct, diskPct, sshLatency, loadAvg, p2pDirect, p2pTotal, memTotalMB, memUsedMB) 
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                node_id_clone, ts, cpu, mem, disk, load_avg,
                p2p_direct, p2p_total, mem_total, mem_used
            ],
        )?;

        Ok(())
    }).await
    .map_err(|e| format!("DB交互线程恐慌: {}", e))?
    .map_err(|e| format!("SQLite执行失败: {}", e))?;

    Ok(())
}
