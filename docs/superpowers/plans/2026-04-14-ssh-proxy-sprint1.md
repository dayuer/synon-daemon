# SSH Proxy 跳板机 — Sprint 1 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现最小可用跳板机 — 运维人员可通过 `ssh root+nodeId@console:2222` 经 Console SSH Proxy 连接到 Agent 的完整交互式 Shell。

**Architecture:** Console 端运行 russh SSH Server（:2222），运维人员通过公钥认证接入；Console 解析用户名中的 nodeId，建立到 Agent（198.18.x.x:22222）的 russh SSH Client 连接，双向桥接 PTY 数据流。认证体系与现有 MQTT HMAC 完全独立，使用 Ed25519 密钥对。

**Tech Stack:** Rust, russh 0.60 (aws-lc-rs backend), tokio, SQLite (deadpool), Ed25519 公钥认证

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `Cargo.toml` | Modify | 新增 `russh` + `async-trait` 依赖，新增 `ssh-proxy` feature |
| `src/console/ssh_proxy.rs` | Create | Console SSH Server Handler — 公钥认证 + PTY 桥接 |
| `src/ssh_server.rs` | Create | Agent 端 SSH Server — 接受 Console client key + 本地 PTY |
| `src/console/ssh_db.rs` | Create | SSH 相关 DB schema 和操作（ssh_sessions + authorized_operators） |
| `src/console/mod.rs` | Modify | 集成 SSH Server 启动（feature-gated） |
| `src/main.rs` | Modify | 注册 `ssh_server` 模块（feature-gated） |
| `scripts/gen_ssh_keys.sh` | Create | 一键生成 Console/Agent/运维人员密钥对 |

---

## Task 1: Cargo.toml 新增依赖和 feature

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: 添加 russh 和 async-trait 依赖**

在 `Cargo.toml` 的 `[dependencies]` 段（`# --- Console Backend Dependencies ---` 区域之后）添加：

```toml
# --- SSH Proxy Dependencies ---
russh = { version = "0.60", default-features = false, features = ["aws-lc-rs"], optional = true }
async-trait = { version = "0.1", optional = true }
```

在 `[features]` 段添加 `ssh-proxy` feature：

```toml
[features]
console = ["dep:axum", "dep:tower", "dep:tower-http", "dep:rusqlite", "dep:deadpool-sqlite", "dep:rumqttd"]
ssh-proxy = ["dep:russh", "dep:async-trait", "console"]  # SSH Proxy 依赖 console feature
default = []
```

- [ ] **Step 2: 验证编译通过（不启用 ssh-proxy feature）**

Run: `cargo check 2>&1 | tail -5`
Expected: 编译成功（现有功能不受影响）

- [ ] **Step 3: 验证 ssh-proxy feature 编译**

Run: `cargo check --features ssh-proxy 2>&1 | tail -20`
Expected: 成功下载 russh 0.60 及其依赖并编译通过。可能需要较长时间（首次下载 aws-lc-rs）。

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "feat(ssh-proxy): add russh 0.60 dependency and ssh-proxy feature flag"
```

---

## Task 2: SSH DB Schema — ssh_sessions + authorized_operators

**Files:**
- Create: `src/console/ssh_db.rs`

- [ ] **Step 1: 创建 ssh_db.rs 模块**

创建 `src/console/ssh_db.rs`：

```rust
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

/// 计算 SSH 公钥的 SHA256 指纹（与 ssh-keygen -lf 格式一致）
pub fn fingerprint_pubkey(pubkey_openssh: &str) -> Result<String> {
    use std::io::Read;
    // 解析 OpenSSH 格式: "ssh-ed25519 AAAA...."
    let parts: Vec<&str> = pubkey_openssh.split_whitespace().collect();
    if parts.len() < 2 {
        anyhow::bail!("公钥格式无效");
    }
    let key_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        parts[1],
    )?;

    let hash = sha2::Sha256::digest(&key_bytes);
    let fp: String = hash.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(":");
    Ok(format!("SHA256:{}", base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        hash,
    ).trim_end_matches('=')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_format() {
        // 使用一个已知的测试公钥
        let pubkey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl test@example.com";
        let fp = fingerprint_pubkey(pubkey).unwrap();
        assert!(fp.starts_with("SHA256:"));
    }

    #[test]
    fn test_fingerprint_invalid_key() {
        let result = fingerprint_pubkey("not-a-key");
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: 在 console/mod.rs 注册 ssh_db 模块**

在 `src/console/mod.rs` 的模块声明区域（第 15-20 行之间）添加：

```rust
pub mod ssh_db;
```

- [ ] **Step 3: 运行测试**

Run: `cargo test --features ssh-proxy ssh_db 2>&1 | tail -15`
Expected: 2 个 ssh_db 测试通过

- [ ] **Step 4: 验证编译**

Run: `cargo check --features ssh-proxy 2>&1 | tail -5`
Expected: 编译成功

- [ ] **Step 5: Commit**

```bash
git add src/console/ssh_db.rs src/console/mod.rs
git commit -m "feat(ssh-proxy): add ssh_db module — authorized_operators + ssh_sessions schema"
```

---

## Task 3: Console SSH Proxy Server

**Files:**
- Create: `src/console/ssh_proxy.rs`

这是核心模块，实现 Console 端的 SSH Server。负责：
1. 公钥认证（查 DB authorized_operators）
2. 解析用户名 `root+nodeId`
3. 建立到 Agent 的 SSH Client 连接
4. 双向桥接 PTY 数据

- [ ] **Step 1: 创建 ssh_proxy.rs**

创建 `src/console/ssh_proxy.rs`：

```rust
//! ssh_proxy.rs — Console SSH Proxy Server
//!
//! 运维人员通过 `ssh root+nodeId@console:2222` 连接到本服务。
//! 本服务作为 SSH Server 接受连接，解析目标 nodeId，
//! 然后作为 SSH Client 连接到 Agent (198.18.x.x:22222)，
//! 双向桥接 PTY 数据流。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use russh::{
    server::{self, Handle},
    Channel, ChannelId,
};
use russh::keys::ssh_key;
use russh::client;
use deadpool_sqlite::Pool;
use anyhow::Result;

use crate::console::ssh_db;

/// Console SSH Server 运行入口
pub async fn run_ssh_server(pool: Pool, shutdown: CancellationToken) {
    // 加载 Console host key
    let host_key_path = std::env::var("SSH_HOST_KEY")
        .unwrap_or_else(|_| "ssh_keys/console_host_key".to_string());

    let host_key = match load_host_key(&host_key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("[SSH-Proxy] 加载 host key 失败 ({}): {}", host_key_path, e);
            tracing::error!("[SSH-Proxy] 请运行 scripts/gen_ssh_keys.sh 生成密钥");
            return;
        }
    };

    let bind_addr = std::env::var("SSH_PROXY_BIND")
        .unwrap_or_else(|_| "0.0.0.0".to_string());
    let bind_port: u16 = std::env::var("SSH_PROXY_PORT")
        .unwrap_or_else(|_| "2222".to_string())
        .parse()
        .unwrap_or(2222);

    let config = Arc::new(server::Config {
        keys: vec![host_key],
        ..Default::default()
    });

    let server = SshProxyServer {
        pool,
        shutdown: shutdown.clone(),
    };

    tracing::info!("[SSH-Proxy] Console SSH Server 启动，监听 {}:{}", bind_addr, bind_port);

    // 使用 tokio select 同时监听连接和关闭信号
    let run = server::run(config, &bind_addr, bind_port, server);
    tokio::select! {
        result = run => {
            if let Err(e) = result {
                tracing::error!("[SSH-Proxy] SSH Server 异常退出: {}", e);
            }
        }
        _ = shutdown.cancelled() => {
            tracing::info!("[SSH-Proxy] 收到关闭信号，SSH Server 退出");
        }
    }
}

/// 加载 SSH host key（Ed25519 私钥，OpenSSH 格式）
fn load_host_key(path: &str) -> Result<ssh_key::private::KeypairData> {
    let key_data = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 host key 失败: {}", e))?;
    let private_key = russh::keys::decode_secret_key(&key_data)
        .map_err(|e| anyhow::anyhow!("解析 host key 失败: {}", e))?;
    Ok(private_key)
}

/// 加载 Console client key（用于连接 Agent）
fn load_client_key(path: &str) -> Result<ssh_key::private::KeypairData> {
    load_host_key(path) // 同样是 Ed25519 OpenSSH 格式
}

/// 解析 SSH 用户名中的 nodeId
/// 格式: "root+nodeId" → Some("nodeId")
/// 也支持纯 nodeId（无前缀）
fn parse_node_id(username: &str) -> Option<&str> {
    if let Some(idx) = username.find('+') {
        let node_id = &username[idx + 1..];
        if !node_id.is_empty() {
            return Some(node_id);
        }
    }
    // 兜底：整个用户名作为 nodeId
    if !username.is_empty() {
        return Some(username);
    }
    None
}

/// Console SSH Proxy Server state
struct SshProxyServer {
    pool: Pool,
    shutdown: CancellationToken,
}

impl server::Server for SshProxyServer {
    type Handler = SshProxySession;

    fn new_client(&mut self, peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        let peer = peer_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        tracing::info!("[SSH-Proxy] 新连接来自 {}", peer);
        SshProxySession {
            pool: self.pool.clone(),
            peer_addr: peer,
            authenticated: false,
            operator: None,
            username: None,
            agent_channel: None,
            agent_handle: None,
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// 单个 SSH 会话的 Handler
struct SshProxySession {
    pool: Pool,
    peer_addr: String,
    authenticated: bool,
    operator: Option<String>,
    username: Option<String>,
    agent_channel: Option<(ChannelId, Handle)>,
    agent_handle: Option<client::Handle>,
    session_id: String,
}

/// Agent SSH Client Handler（Console 作为客户端连接 Agent 时）
struct AgentClient;

impl client::Handler for AgentClient {
    type Error = anyhow::Error;
}

impl server::Handler for SshProxySession {
    type Error = anyhow::Error;

    fn auth_publickey(&mut self, username: &str, public_key: &ssh_key::PublicKey) -> bool {
        // 计算公钥指纹
        let fp = ssh_key.fingerprint(ssh_key::HashAlg::Sha256);
        let fp_str = format!("{:?}", fp); // SHA256:base64...

        tracing::info!(
            "[SSH-Proxy] 公钥认证: user={} fingerprint={}",
            username, fp_str
        );

        // 查询 DB
        let pool = self.pool.clone();
        let fp_lookup = fp_str.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                ssh_db::lookup_operator_by_fingerprint(&pool, &fp_lookup)
            )
        });

        match result {
            Ok(Some(name)) => {
                tracing::info!("[SSH-Proxy] 运维人员 {} 认证成功", name);
                self.authenticated = true;
                self.operator = Some(name);
                self.username = Some(username.to_string());
                true
            }
            Ok(None) => {
                tracing::warn!("[SSH-Proxy] 公钥不在 authorized_operators 中: {}", fp_str);
                false
            }
            Err(e) => {
                tracing::error!("[SSH-Proxy] 认证查询失败: {}", e);
                false
            }
        }
    }

    fn auth_succeeded(&mut self) -> bool {
        tracing::info!(
            "[SSH-Proxy] 认证成功 — operator={} username={}",
            self.operator.as_deref().unwrap_or("?"),
            self.username.as_deref().unwrap_or("?")
        );
        true
    }

    fn channel_open_session(&mut self, channel: Channel<server::Msg>) -> bool {
        if !self.authenticated {
            tracing::warn!("[SSH-Proxy] 未认证的 session 请求");
            return false;
        }

        let username = match &self.username {
            Some(u) => u.clone(),
            None => return false,
        };

        let node_id = match parse_node_id(&username) {
            Some(id) => id.to_string(),
            None => {
                tracing::warn!("[SSH-Proxy] 无法从用户名 {} 解析 nodeId", username);
                return false;
            }
        };

        tracing::info!("[SSH-Proxy] 打开 session → nodeId={}", node_id);

        // 创建会话记录
        let pool = self.pool.clone();
        let sid = self.session_id.clone();
        let op = self.operator.clone().unwrap_or_default();
        let peer = self.peer_addr.clone();
        let nid = node_id.clone();
        tokio::spawn(async move {
            if let Err(e) = ssh_db::create_session(&pool, &sid, &op, &nid, &peer).await {
                tracing::warn!("[SSH-Proxy] 创建会话记录失败: {}", e);
            }
        });

        // 异步建立到 Agent 的 SSH 连接
        let channel_id = channel.id();
        let handle = channel.into_handle();

        self.agent_channel = Some((channel_id, handle));
        true
    }

    fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col: u32,
        row: u32,
        pix_width: u32,
        pix_height: u32,
        modes: &[(russh::Pty, u32)],
    ) -> bool {
        tracing::info!(
            "[SSH-Proxy] PTY 请求: term={} {}x{}", term, col, row
        );
        true // 接受 PTY，稍后在 shell_request 中转发给 Agent
    }

    fn shell_request(&mut self, channel: ChannelId) -> bool {
        let username = match &self.username {
            Some(u) => u.clone(),
            None => return false,
        };
        let node_id = match parse_node_id(&username) {
            Some(id) => id.to_string(),
            None => return false,
        };

        let handle = match self.agent_channel.take() {
            Some((_, h)) => h,
            None => {
                tracing::warn!("[SSH-Proxy] shell_request 但无 agent_channel");
                return false;
            }
        };

        // 在独立任务中建立到 Agent 的连接
        let pool = self.pool.clone();
        let session_id = self.session_id.clone();
        let client_key_path = std::env::var("SSH_CLIENT_KEY")
            .unwrap_or_else(|_| "ssh_keys/console_client_key".to_string());

        tokio::spawn(async move {
            if let Err(e) = bridge_to_agent(
                handle, &node_id, &client_key_path, pool, &session_id
            ).await {
                tracing::error!("[SSH-Proxy] Agent 桥接失败: {}", e);
            }
        });

        true
    }

    fn data(&mut self, channel: ChannelId, data: &[u8]) -> bool {
        // data 通过 bridge_to_agent 中的双向桥接自动转发
        // russh server handler 的 data 在 shell_request 中处理
        true
    }

    fn window_change_request(
        &mut self,
        channel: ChannelId,
        col: u32,
        row: u32,
        pix_width: u32,
        pix_height: u32,
    ) -> bool {
        // 转发窗口大小变化到 Agent（Sprint 2 完整实现）
        tracing::debug!("[SSH-Proxy] 窗口大小变化: {}x{}", col, row);
        true
    }

    fn channel_close(&mut self, channel: ChannelId) {
        tracing::info!("[SSH-Proxy] Channel 关闭: session={}", self.session_id);
        let pool = self.pool.clone();
        let sid = self.session_id.clone();
        tokio::spawn(async move {
            if let Err(e) = ssh_db::close_session(&pool, &sid, "closed").await {
                tracing::warn!("[SSH-Proxy] 关闭会话记录失败: {}", e);
            }
        });
    }
}

/// 建立到 Agent 的 SSH 连接并双向桥接 PTY
async fn bridge_to_agent(
    mut server_handle: Handle,
    node_id: &str,
    client_key_path: &str,
    pool: Pool,
    session_id: &str,
) -> Result<()> {
    // 确定 Agent 地址
    // 查 DB 获取 Agent 的 GNB TUN IP（从 nodes 表或 GNB 配置）
    let agent_addr = format!("198.18.0.1:22222"); // TODO: 从 DB 查询实际 TUN IP

    tracing::info!("[SSH-Proxy] 连接 Agent {}:{}", agent_addr, 22222);

    // 加载 Console client key
    let client_key = load_client_key(client_key_path)?;
    let config = Arc::new(client::Config {
        ..Default::default()
    });

    // 连接 Agent SSH Server
    let mut agent_session = client::connect(Arc::new(client::Config::default()), &agent_addr, AgentClient).await?;

    // 公钥认证
    let auth_ok = agent_session
        .authenticate_publickey("synon-console", Arc::new(client_key))
        .await?;
    if !auth_ok {
        anyhow::bail!("Agent SSH 认证失败");
    }

    // 打开 channel
    let mut agent_channel = agent_session.channel_open_session().await?;

    // 请求 PTY + Shell
    agent_channel.request_pty(false, "xterm-256color", 80, 24, 0, 0, &[]).await?;
    agent_channel.request_shell(false).await?;

    tracing::info!("[SSH-Proxy] Agent Shell 就绪，开始双向桥接");

    // 双向桥接
    let agent_handle = agent_session.handle();

    // 从 server_handle 获取输入 channel
    // server side: 接收来自运维人员的数据，转发给 Agent
    let server_to_agent = async {
        // 读取 server channel 的 data
        // 通过 agent_channel.data() 发送
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };

    // Agent → 运维人员
    let agent_to_server = async {
        loop {
            match agent_channel.wait().await {
                Some(russh::client::ChannelMsg::Data(data)) => {
                    if server_handle.data(channel_id_from_handle(&server_handle), data).is_err() {
                        break;
                    }
                }
                Some(russh::client::ChannelMsg::Eof) | None => break,
                _ => {}
            }
        }
    };

    tokio::select! {
        _ = server_to_agent => {},
        _ = agent_to_server => {},
    }

    // 清理
    let _ = agent_channel.eof().await;
    let _ = ssh_db::close_session(&pool, session_id, "closed").await;

    tracing::info!("[SSH-Proxy] 会话 {} 桥接结束", session_id);
    Ok(())
}

fn channel_id_from_handle(handle: &Handle) -> ChannelId {
    // 获取 server handle 关联的 channel id
    // russh Handle 没有直接暴露 channel_id，需要从 session 中获取
    // 此处为简化实现，实际需要跟踪 channel_id
    ChannelId::new(0)
}
```

> **注意:** 上述代码中的双向桥接部分 (`bridge_to_agent`) 是一个初始骨架。russh 0.60 的 `server::Handle` 和 `client::Handle` API 需要在实际编译时根据具体版本调整。关键是建立架构骨架，确保编译通过后在 Task 7 的端到端测试中验证数据流。

- [ ] **Step 2: 在 console/mod.rs 注册 ssh_proxy 模块**

在 `src/console/mod.rs` 的模块声明区域添加（紧跟 `pub mod ssh_db;` 之后）：

```rust
#[cfg(feature = "ssh-proxy")]
pub mod ssh_proxy;
```

- [ ] **Step 3: 尝试编译，根据 russh 0.60 实际 API 调整**

Run: `cargo check --features ssh-proxy 2>&1 | head -60`
Expected: 可能遇到编译错误（russh 0.60 API 可能与上述代码不完全一致）。根据错误信息调整：
- `server::Server` trait 的签名
- `server::Handler` 方法签名
- `client::connect` 和 `client::Handler` trait
- `Channel` / `Handle` 的具体方法名

> 修复编译错误是这一步的核心工作。参考 russh 0.60 的文档和 examples。

- [ ] **Step 4: 确认编译通过**

Run: `cargo check --features ssh-proxy 2>&1 | tail -5`
Expected: 编译成功

- [ ] **Step 5: Commit**

```bash
git add src/console/ssh_proxy.rs src/console/mod.rs
git commit -m "feat(ssh-proxy): Console SSH Proxy Server skeleton — auth + PTY bridge"
```

---

## Task 4: Agent SSH Server

**Files:**
- Create: `src/ssh_server.rs`
- Modify: `src/main.rs`

Agent 端 SSH Server，监听 TUN 地址 `:22222`，接受 Console client key 的公钥认证，提供本地 PTY Shell。

- [ ] **Step 1: 创建 ssh_server.rs**

创建 `src/ssh_server.rs`：

```rust
//! ssh_server.rs — Agent 端 SSH Server
//!
//! 在 Agent 进程内运行一个 russh SSH Server，监听 GNB TUN 地址 :22222。
//! 仅接受 Console 的 client key（公钥白名单）。
//! 提供完整交互式 Shell（通过 PTY）。

use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use russh::server::{self, Channel, ChannelId};
use russh::keys::ssh_key;
use russh::ChannelMsg;
use anyhow::Result;

/// Agent SSH Server 运行入口
pub async fn run_agent_ssh_server(shutdown: CancellationToken) {
    // 加载 Agent host key
    let host_key_path = std::env::var("SSH_AGENT_HOST_KEY")
        .unwrap_or_else(|_| "ssh_keys/agent_host_key".to_string());

    let host_key = match load_key(&host_key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("[Agent-SSH] 加载 host key 失败 ({}): {}", host_key_path, e);
            return;
        }
    };

    // 加载 authorized_keys（仅信任 Console client key）
    let authorized_keys_path = std::env::var("SSH_AGENT_AUTHORIZED_KEYS")
        .unwrap_or_else(|_| "ssh_keys/authorized_keys".to_string());

    let authorized_keys = match load_authorized_keys(&authorized_keys_path) {
        Ok(keys) => keys,
        Err(e) => {
            tracing::warn!("[Agent-SSH] 加载 authorized_keys 失败: {} — 将拒绝所有连接", e);
            Vec::new()
        }
    };

    let bind_addr = std::env::var("SSH_AGENT_BIND")
        .unwrap_or_else(|_| "198.18.0.1".to_string());
    let bind_port: u16 = std::env::var("SSH_AGENT_PORT")
        .unwrap_or_else(|_| "22222".to_string())
        .parse()
        .unwrap_or(22222);

    let config = Arc::new(server::Config {
        keys: vec![host_key],
        ..Default::default()
    });

    let server = AgentSshServer {
        authorized_keys: Arc::new(authorized_keys),
    };

    tracing::info!("[Agent-SSH] SSH Server 启动，监听 {}:{}", bind_addr, bind_port);

    let run = server::run(config, &bind_addr, bind_port, server);
    tokio::select! {
        result = run => {
            if let Err(e) = result {
                tracing::error!("[Agent-SSH] SSH Server 异常退出: {}", e);
            }
        }
        _ = shutdown.cancelled() => {
            tracing::info!("[Agent-SSH] 收到关闭信号，SSH Server 退出");
        }
    }
}

fn load_key(path: &str) -> Result<ssh_key::private::KeypairData> {
    let key_data = std::fs::read_to_string(path)?;
    let private_key = russh::keys::decode_secret_key(&key_data)?;
    Ok(private_key)
}

/// 加载 authorized_keys 文件，返回公钥列表
fn load_authorized_keys(path: &str) -> Result<Vec<ssh_key::PublicKey>> {
    let content = std::fs::read_to_string(path)?;
    let mut keys = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(key) = russh::keys::parse_public_key(line) {
            keys.push(key);
        }
    }
    tracing::info!("[Agent-SSH] 加载 {} 个 authorized_keys", keys.len());
    Ok(keys)
}

/// Agent SSH Server state
struct AgentSshServer {
    authorized_keys: Arc<Vec<ssh_key::PublicKey>>,
}

impl server::Server for AgentSshServer {
    type Handler = AgentSshSession;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        AgentSshSession {
            authorized_keys: self.authorized_keys.clone(),
            authenticated: false,
            pty: None,
        }
    }
}

/// PTY 信息
struct PtyInfo {
    term: String,
    col: u32,
    row: u32,
}

/// 单个 Agent SSH 会话
struct AgentSshSession {
    authorized_keys: Arc<Vec<ssh_key::PublicKey>>,
    authenticated: bool,
    pty: Option<PtyInfo>,
}

impl server::Handler for AgentSshSession {
    type Error = anyhow::Error;

    fn auth_publickey(&mut self, username: &str, public_key: &ssh_key::PublicKey) -> bool {
        let authorized = self.authorized_keys.iter().any(|k| {
            k.fingerprint(ssh_key::HashAlg::Sha256) == public_key.fingerprint(ssh_key::HashAlg::Sha256)
        });

        if authorized {
            tracing::info!("[Agent-SSH] Console 公钥认证通过: user={}", username);
            self.authenticated = true;
            true
        } else {
            tracing::warn!("[Agent-SSH] 拒绝未知公钥: user={}", username);
            false
        }
    }

    fn channel_open_session(&mut self, channel: Channel<server::Msg>) -> bool {
        if !self.authenticated {
            return false;
        }
        true
    }

    fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col: u32,
        row: u32,
        pix_width: u32,
        pix_height: u32,
        modes: &[(russh::Pty, u32)],
    ) -> bool {
        self.pty = Some(PtyInfo {
            term: term.to_string(),
            col,
            row,
        });
        true
    }

    fn shell_request(&mut self, channel: ChannelId) -> bool {
        if !self.authenticated {
            return false;
        }

        // 通过 nix::unistd::fork + setsid + openpty 创建本地 PTY
        // 然后双向桥接 SSH channel <-> PTY
        tracing::info!("[Agent-SSH] Shell 请求 — 启动本地 PTY");

        // 注意：实际 PTY 创建和桥接需要在编译后根据 russh Handle API
        // 实现数据循环。此处返回 true 表示接受请求，具体桥接在后续步骤。
        true
    }

    fn data(&mut self, channel: ChannelId, data: &[u8]) -> bool {
        // 将运维人员的输入转发到本地 PTY
        true
    }

    fn window_change_request(
        &mut self,
        channel: ChannelId,
        col: u32,
        row: u32,
        pix_width: u32,
        pix_height: u32,
    ) -> bool {
        tracing::debug!("[Agent-SSH] 窗口大小变化: {}x{}", col, row);
        true
    }
}
```

- [ ] **Step 2: 在 main.rs 注册 ssh_server 模块**

在 `src/main.rs` 的模块声明区域（第 21 行 `#[cfg(feature = "console")]` 之前）添加：

```rust
#[cfg(feature = "ssh-proxy")]
mod ssh_server;
```

> 注意：`ssh_server` 使用 `ssh-proxy` feature 而非独立的 `ssh-agent` feature。Agent SSH Server 与 Console SSH Proxy 是一对，一起通过 `ssh-proxy` feature 控制。实际部署时 Agent 不需要编译 console 模块——可以考虑后续拆分为独立 feature。

- [ ] **Step 3: 尝试编译**

Run: `cargo check --features ssh-proxy 2>&1 | head -60`
Expected: 可能遇到编译错误。根据 russh 0.60 实际 API 调整。

- [ ] **Step 4: 确认编译通过**

Run: `cargo check --features ssh-proxy 2>&1 | tail -5`
Expected: 编译成功

- [ ] **Step 5: Commit**

```bash
git add src/ssh_server.rs src/main.rs
git commit -m "feat(ssh-proxy): Agent SSH Server skeleton — authorized_keys + PTY"
```

---

## Task 5: 密钥生成脚本

**Files:**
- Create: `scripts/gen_ssh_keys.sh`

- [ ] **Step 1: 创建 scripts 目录和密钥生成脚本**

```bash
mkdir -p scripts
```

创建 `scripts/gen_ssh_keys.sh`：

```bash
#!/bin/bash
# gen_ssh_keys.sh — 生成 SSH Proxy 所需的全部密钥对
#
# 生成:
#   1. Console host key (SSH Server 身份)
#   2. Console client key (连接 Agent 时的身份)
#   3. Agent host key (Agent SSH Server 身份)
#   4. Operator key (运维人员使用)
#   5. Agent authorized_keys (信任 Console client key)
#
# 用法:
#   ./scripts/gen_ssh_keys.sh [output_dir]
#   默认输出到 ./ssh_keys/

set -euo pipefail

OUTDIR="${1:-ssh_keys}"
mkdir -p "$OUTDIR"

echo "=== 生成 SSH 密钥到 $OUTDIR ==="

# 1. Console host key
if [ ! -f "$OUTDIR/console_host_key" ]; then
    ssh-keygen -t ed25519 -f "$OUTDIR/console_host_key" -N "" -C "console-host"
    echo "[OK] Console host key"
else
    echo "[SKIP] Console host key 已存在"
fi

# 2. Console client key (连接 Agent 时使用)
if [ ! -f "$OUTDIR/console_client_key" ]; then
    ssh-keygen -t ed25519 -f "$OUTDIR/console_client_key" -N "" -C "console-client"
    echo "[OK] Console client key"
else
    echo "[SKIP] Console client key 已存在"
fi

# 3. Agent host key
if [ ! -f "$OUTDIR/agent_host_key" ]; then
    ssh-keygen -t ed25519 -f "$OUTDIR/agent_host_key" -N "" -C "agent-host"
    echo "[OK] Agent host key"
else
    echo "[SKIP] Agent host key 已存在"
fi

# 4. 运维人员 key
if [ ! -f "$OUTDIR/operator_key" ]; then
    ssh-keygen -t ed25519 -f "$OUTDIR/operator_key" -N "" -C "operator"
    echo "[OK] Operator key"
else
    echo "[SKIP] Operator key 已存在"
fi

# 5. Agent authorized_keys — 信任 Console client key
cat "$OUTDIR/console_client_key.pub" > "$OUTDIR/authorized_keys"
echo "[OK] Agent authorized_keys (信任 console_client_key)"

echo ""
echo "=== 密钥生成完成 ==="
echo ""
echo "使用方法:"
echo "  1. Console 端:"
echo "     export SSH_HOST_KEY=$OUTDIR/console_host_key"
echo "     export SSH_CLIENT_KEY=$OUTDIR/console_client_key"
echo "     synon-daemon --config agent.conf console"
echo ""
echo "  2. Agent 端:"
echo "     export SSH_AGENT_HOST_KEY=$OUTDIR/agent_host_key"
echo "     export SSH_AGENT_AUTHORIZED_KEYS=$OUTDIR/authorized_keys"
echo "     synon-daemon --config agent.conf"
echo ""
echo "  3. 运维人员连接 (需先将公钥注册到 DB):"
echo "     ssh -i $OUTDIR/operator_key -p 2222 root+<nodeId>@<console-ip>"
echo ""

# 列出所有文件
echo "生成的文件:"
ls -la "$OUTDIR"/*
```

- [ ] **Step 2: 添加执行权限并运行**

Run: `chmod +x scripts/gen_ssh_keys.sh && ./scripts/gen_ssh_keys.sh`
Expected: 输出所有密钥生成结果，无错误

- [ ] **Step 3: 验证密钥文件**

Run: `ls -la ssh_keys/`
Expected: 看到 console_host_key, console_client_key, agent_host_key, operator_key, authorized_keys

- [ ] **Step 4: 将密钥文件加入 .gitignore**

在 `.gitignore` 中添加：

```
ssh_keys/
```

- [ ] **Step 5: Commit**

```bash
git add scripts/gen_ssh_keys.sh .gitignore
git commit -m "feat(ssh-proxy): add key generation script for SSH Proxy"
```

---

## Task 6: 集成 SSH Server 到 Console 启动流程

**Files:**
- Modify: `src/console/mod.rs` — 在 `run_server` 中启动 SSH Proxy Server
- Modify: `src/main.rs` — Agent 模式下启动 SSH Server（feature-gated）

- [ ] **Step 1: 修改 console/mod.rs 的 run_server 函数**

在 `src/console/mod.rs` 的 `run_server` 函数中，在 MQTT Broker 启动之后（第 67 行 `});` 之后）、Axum 路由构建之前（第 77 行 `let app = ...` 之前），添加 SSH Proxy 启动代码：

```rust
    // ── SSH Proxy Server (feature-gated) ──
    #[cfg(feature = "ssh-proxy")]
    {
        // 初始化 SSH 数据库表
        if let Err(e) = ssh_db::init_tables(&pool).await {
            tracing::error!("[SSH-Proxy] 初始化 SSH 数据库表失败: {}", e);
        }

        let ssh_pool = pool.clone();
        let ssh_shutdown = shutdown_token.clone();
        tokio::spawn(async move {
            ssh_proxy::run_ssh_server(ssh_pool, ssh_shutdown).await;
        });
    }
```

- [ ] **Step 2: 在 Agent 模式启动流程中集成 SSH Server**

在 `src/main.rs` 中，在 MQTT agent 启动之前（第 187 行 `tracing::info!(">>> 使用 MQTT 通信层 <<<");` 之前），添加 Agent SSH Server 启动：

```rust
    // Agent SSH Server (feature-gated, 仅 ssh-proxy feature 启用时编译)
    #[cfg(feature = "ssh-proxy")]
    {
        let ssh_token = shutdown.clone();
        tokio::spawn(async move {
            ssh_server::run_agent_ssh_server(ssh_token).await;
        });
    }
```

- [ ] **Step 3: 确认编译通过**

Run: `cargo check --features ssh-proxy 2>&1 | tail -5`
Expected: 编译成功

- [ ] **Step 4: 同时确认默认 feature 编译不受影响**

Run: `cargo check 2>&1 | tail -5`
Expected: 编译成功（无 ssh-proxy feature 时不编译相关代码）

- [ ] **Step 5: Commit**

```bash
git add src/console/mod.rs src/main.rs
git commit -m "feat(ssh-proxy): integrate SSH Proxy/Server into Console and Agent startup"
```

---

## Task 7: 端到端测试与调试

**Files:**
- 无新文件（手动测试验证）

- [ ] **Step 1: 生成测试密钥**

Run: `./scripts/gen_ssh_keys.sh /tmp/ssh_test_keys`
Expected: 密钥生成成功

- [ ] **Step 2: 注册运维人员公钥到 DB**

使用 sqlite3 直接插入测试数据：

```bash
# 先启动一次 Console 让 DB 表创建，或手动创建
sqlite3 <db_path> "INSERT INTO authorized_operators (name, pubkey, fingerprint) VALUES ('test-admin', '<operator_key.pub 内容>', '<fingerprint>');"
```

其中 fingerprint 通过 `ssh-keygen -lf /tmp/ssh_test_keys/operator_key.pub` 获取。

- [ ] **Step 3: 启动 Console (SSH Proxy 模式)**

```bash
export SSH_HOST_KEY=/tmp/ssh_test_keys/console_host_key
export SSH_CLIENT_KEY=/tmp/ssh_test_keys/console_client_key
export SSH_PROXY_BIND=127.0.0.1
export SSH_PROXY_PORT=2222
cargo run --features ssh-proxy -- console
```

Expected: 日志输出 `[SSH-Proxy] Console SSH Server 启动，监听 127.0.0.1:2222`

- [ ] **Step 4: 测试 SSH 连接**

在另一个终端：

```bash
ssh -i /tmp/ssh_test_keys/operator_key -p 2222 root+test-node@127.0.0.1
```

Expected:
- 如果 Agent 未运行：连接成功但 Agent 桥接失败（预期行为）
- 认证成功日志：`[SSH-Proxy] 运维人员 test-admin 认证成功`

- [ ] **Step 5: 验证 DB 会话记录**

```bash
sqlite3 <db_path> "SELECT * FROM ssh_sessions;"
```

Expected: 看到一条 session 记录，包含 operator, nodeId, sourceIp

- [ ] **Step 6: 修复发现的问题**

根据端到端测试中发现的编译/运行时错误，逐个修复。可能的问题：
- russh 0.60 API 与代码不匹配
- 公钥指纹格式不一致
- Handle / Channel 数据传输 API
- PTY 请求参数

- [ ] **Step 7: Commit 所有修复**

```bash
git add -u
git commit -m "fix(ssh-proxy): end-to-end integration fixes from testing"
```

---

## Self-Review

### Spec Coverage

| Spec 要求 | 对应 Task |
|-----------|----------|
| Cargo.toml 新增 russh + ssh-proxy feature | Task 1 |
| Console SSH Server 骨架 (auth + bridge) | Task 3 |
| Agent SSH Server 骨架 (authorized_keys + PTY) | Task 4 |
| 密钥生成脚本 | Task 5 |
| 集成到 console/mod.rs 启动流程 | Task 6 |
| DB schema: ssh_sessions + authorized_operators | Task 2 |
| 端到端测试 | Task 7 |

### Placeholder Scan

- `bridge_to_agent` 中的双向桥接需要根据 russh 0.60 实际 API 在 Task 3 Step 3 中调整 — 已在步骤说明中标注
- Agent 地址查询（`TODO: 从 DB 查询实际 TUN IP`）— Sprint 2 完善，Sprint 1 使用环境变量或硬编码测试地址
- Agent SSH Server 的 PTY 创建（fork + openpty）— 在 Task 4 中标注需要后续实现

### Type Consistency

- `ssh_db::lookup_operator_by_fingerprint` 返回 `Result<Option<String>>` — 在 `ssh_proxy.rs` 的 `auth_publickey` 中使用一致
- `ssh_db::create_session` / `close_session` 参数类型一致
- `load_host_key` / `load_client_key` 返回 `Result<KeypairData>` — 两处调用一致
