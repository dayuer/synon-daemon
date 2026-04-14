//! ssh_db.rs — SSH Proxy 相关数据库操作
//!
//! 管理两张表:
//!   - authorized_operators: 运维人员公钥白名单
//!   - ssh_sessions: SSH 会话审计记录

use deadpool_sqlite::Pool;
use anyhow::Result;

/// 初始化 SSH 相关表（幂等，CREATE IF NOT EXISTS）
pub async fn init_tables(pool: &Pool) -> Result<()> {
    let conn = pool.get().await.map_err(|e| anyhow::anyhow!("DB池获取失败: {}", e))?;
    conn.interact(|conn| {
        // 运维人员公钥白名单
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS authorized_operators (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                name        TEXT    NOT NULL UNIQUE,         -- 运维人员标识（如 alice, bob）
                pubkey      TEXT    NOT NULL,                 -- OpenSSH 格式公钥（ssh-ed25519 AAAA...）
                fingerprint TEXT    NOT NULL,                 -- SHA256 指纹，用于快速比对
                createdAt   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updatedAt   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_operators_fp ON authorized_operators(fingerprint);

            -- SSH 会话审计记录
            CREATE TABLE IF NOT EXISTS ssh_sessions (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                sessionId   TEXT    NOT NULL UNIQUE,          -- UUID v4
                operator    TEXT    NOT NULL,                 -- 运维人员 name
                nodeId      TEXT    NOT NULL,                 -- 目标 Agent nodeId
                sourceIp    TEXT    NOT NULL DEFAULT '',      -- 运维人员来源 IP
                startedAt   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                endedAt     TEXT,                             -- NULL = 会话进行中
                recordingPath TEXT,                           -- script 录制文件路径（Sprint 2）
                status      TEXT    NOT NULL DEFAULT 'active' -- active | closed | error
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_node ON ssh_sessions(nodeId);
            CREATE INDEX IF NOT EXISTS idx_sessions_active ON ssh_sessions(status);"
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("DB交互失败: {}", e))?
    .map_err(|e| anyhow::anyhow!("SSH表初始化失败: {}", e))?;

    tracing::info!("[SSH-DB] ssh_sessions + authorized_operators 表就绪");
    Ok(())
}

/// 通过公钥指纹查找运维人员
///
/// 返回 Some(operator_name) 如果指纹匹配
pub async fn lookup_operator_by_fingerprint(pool: &Pool, fingerprint: &str) -> Result<Option<String>> {
    let conn = pool.get().await.map_err(|e| anyhow::anyhow!("DB池获取失败: {}", e))?;
    let fp = fingerprint.to_string();
    let result = conn.interact(move |conn| -> Result<Option<String>, rusqlite::Error> {
        let mut stmt = conn.prepare(
            "SELECT name FROM authorized_operators WHERE fingerprint = ?1"
        )?;
        let mut rows = stmt.query(rusqlite::params![fp])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("DB交互失败: {}", e))?
    .map_err(|e| anyhow::anyhow!("查询失败: {}", e))?;

    Ok(result)
}

/// 创建新的 SSH 会话记录
pub async fn create_session(
    pool: &Pool,
    session_id: &str,
    operator: &str,
    node_id: &str,
    source_ip: &str,
) -> Result<()> {
    let conn = pool.get().await.map_err(|e| anyhow::anyhow!("DB池获取失败: {}", e))?;
    let sid = session_id.to_string();
    let op = operator.to_string();
    let nid = node_id.to_string();
    let sip = source_ip.to_string();

    conn.interact(move |conn| {
        conn.execute(
            "INSERT INTO ssh_sessions (sessionId, operator, nodeId, sourceIp)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![sid, op, nid, sip],
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("DB交互失败: {}", e))?
    .map_err(|e| anyhow::anyhow!("创建会话记录失败: {}", e))?;

    Ok(())
}

/// 关闭 SSH 会话记录
pub async fn close_session(pool: &Pool, session_id: &str, status: &str) -> Result<()> {
    let conn = pool.get().await.map_err(|e| anyhow::anyhow!("DB池获取失败: {}", e))?;
    let sid = session_id.to_string();
    let st = status.to_string();

    conn.interact(move |conn| {
        conn.execute(
            "UPDATE ssh_sessions SET endedAt = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), status = ?1
             WHERE sessionId = ?2",
            rusqlite::params![st, sid],
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("DB交互失败: {}", e))?
    .map_err(|e| anyhow::anyhow!("关闭会话记录失败: {}", e))?;

    Ok(())
}

