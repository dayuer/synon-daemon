//! ssh_proxy.rs — Console SSH Proxy Server
//!
//! 运维人员通过 `ssh root+nodeId@console:2222` 连接到本服务。
//! 本服务作为 SSH Server 接受连接，解析目标 nodeId，
//! 然后作为 SSH Client 连接到 Agent (198.18.x.x:22222)，
//! 双向桥接 PTY 数据流。

use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use russh::{
    server,
    server::Server,
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
        .unwrap_or_else(|_| "/opt/gnb/etc/keys/console_host_key".to_string());

    let host_key = match load_host_key(&host_key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("[SSH-Proxy] 加载 host key 失败 ({}): {}", host_key_path, e);
            tracing::error!("[SSH-Proxy] 请运行 scripts/gen_ssh_keys.sh 生成密钥");
            return;
        }
    };

    // 预加载 Console client key（每个会话复用，避免重复磁盘 I/O）
    let client_key_path = std::env::var("SSH_CLIENT_KEY")
        .unwrap_or_else(|_| "/opt/gnb/etc/keys/console_client_key".to_string());
    let client_key = match load_host_key(&client_key_path) {
        Ok(k) => Arc::new(k),
        Err(e) => {
            tracing::error!("[SSH-Proxy] 加载 client key 失败 ({}): {}", client_key_path, e);
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

    let listener = match tokio::net::TcpListener::bind((bind_addr.as_str(), bind_port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("[SSH-Proxy] 绑定 {}:{} 失败: {}", bind_addr, bind_port, e);
            return;
        }
    };

    // 连接数信号量
    let max_conns: usize = std::env::var("SSH_MAX_CONNECTIONS")
        .unwrap_or_else(|_| "32".to_string())
        .parse()
        .unwrap_or(32);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_conns));

    tracing::info!("[SSH-Proxy] Console SSH Server 启动，监听 {}:{} (max_conn={})", bind_addr, bind_port, max_conns);

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((socket, peer_addr)) => {
                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                tracing::warn!("[SSH-Proxy] 拒绝连接 (已达上限 {}): {}", max_conns, peer_addr);
                                continue;
                            }
                        };
                        let mut server = SshProxyServer {
                            pool: pool.clone(),
                            client_key: client_key.clone(),
                        };
                        let handler = server.new_client(Some(peer_addr));
                        let config_clone = config.clone();

                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) = russh::server::run_stream(config_clone, socket, handler).await {
                                tracing::error!("[SSH-Proxy] 会话执行错误 (来自 {}): {}", peer_addr, e);
                            }
                        });
                    }
                    Err(e) => tracing::error!("[SSH-Proxy] 接收连接失败: {}", e),
                }
            }
            _ = shutdown.cancelled() => {
                tracing::info!("[SSH-Proxy] 收到关闭信号，SSH Server 退出");
                break;
            }
        }
    }
}

/// 加载 SSH 私钥（Ed25519，OpenSSH 格式）
fn load_host_key(path: &str) -> Result<russh::keys::PrivateKey> {
    let key_data = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 key 失败 {}: {}", path, e))?;
    let private_key = russh::keys::decode_secret_key(&key_data, None)
        .map_err(|e| anyhow::anyhow!("解析 key 失败: {}", e))?;
    Ok(private_key)
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

/// 从 DB 查询节点的 SSH 连接地址（GNB TUN IP）
async fn lookup_agent_addr(pool: &Pool, node_id: &str) -> Result<String> {
    let conn = pool.get().await.map_err(|e| anyhow::anyhow!("DB池获取失败: {}", e))?;
    let nid = node_id.to_string();
    let addr = conn
        .interact(move |conn| -> Result<Option<String>, rusqlite::Error> {
            // 从 nodes 表查 gnbIp 或 sysInfo 中的 TUN 地址
            // 兜底: 使用环境变量 SSH_AGENT_ADDR_PREFIX + nodeId
            let addr: Option<String> = conn.query_row(
                "SELECT sysInfo FROM nodes WHERE id = ?1",
                rusqlite::params![nid],
                |row| row.get(0),
            ).ok();
            Ok(addr)
        })
        .await
        .map_err(|e| anyhow::anyhow!("DB交互失败: {}", e))?
        .map_err(|e| anyhow::anyhow!("查询失败: {}", e))?;

    // 兜底: 环境变量覆盖
    let default_prefix = std::env::var("SSH_AGENT_ADDR_PREFIX")
        .unwrap_or_else(|_| "198.18.0.1".to_string());

    // 如果 DB 有 sysInfo，尝试解析 gnbTunIp
    if let Some(sys_info_str) = addr {
        if let Ok(sys_info) = serde_json::from_str::<serde_json::Value>(&sys_info_str) {
            if let Some(tun_ip) = sys_info.get("gnbTunIp").and_then(|v| v.as_str()) {
                return Ok(format!("{}:22222", tun_ip));
            }
        }
    }

    Ok(format!("{}:22222", default_prefix))
}

// ─── Server ────────────────────────────────────────────────────

/// Console SSH Proxy Server state
struct SshProxyServer {
    pool: Pool,
    /// 预加载的 Console client key（连接 Agent 时复用）
    client_key: Arc<russh::keys::PrivateKey>,
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
            client_key: self.client_key.clone(),
            peer_addr: peer,
            authenticated: false,
            operator: None,
            username: None,
            node_id: None,
            session_id: uuid::Uuid::new_v4().to_string(),
            server_handle: None,
            channel_id: None,
            pty_size: None,
            agent_writer: None,
            agent_channel_id: None,
        }
    }
}

/// PTY 尺寸参数
struct PtySizeParams {
    #[allow(dead_code)]
    term: String,
    col: u32,
    row: u32,
}

/// 单个 SSH 会话的 Handler
struct SshProxySession {
    pool: Pool,
    /// 预加载的 Console client key
    client_key: Arc<russh::keys::PrivateKey>,
    peer_addr: String,
    authenticated: bool,
    operator: Option<String>,
    username: Option<String>,
    /// 解析后的目标 nodeId（channel_open_session 时填充）
    node_id: Option<String>,
    session_id: String,
    /// 向运维人员写回数据的 handle
    server_handle: Option<server::Handle>,
    /// 运维人员侧 channel id
    channel_id: Option<ChannelId>,
    /// PTY 尺寸（shell_request 前可能收到 pty_request）
    pty_size: Option<PtySizeParams>,
    /// Agent SSH client handle（用于向 Agent 转发输入）
    agent_writer: Option<client::Handle<AgentClient>>,
    /// Agent 侧 channel id
    agent_channel_id: Option<ChannelId>,
}

/// Agent SSH Client Handler（Console 作为客户端连接 Agent 时）
struct AgentClient;

impl client::Handler for AgentClient {
    type Error = anyhow::Error;
}

impl server::Handler for SshProxySession {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        username: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        let fp = public_key.fingerprint(ssh_key::HashAlg::Sha256);
        let fp_str = format!("{}", fp);

        tracing::info!(
            "[SSH-Proxy] 公钥认证: user={} fingerprint={}",
            username, fp_str
        );

        let pool = self.pool.clone();
        let fp_lookup = fp_str.clone();
        let result = ssh_db::lookup_operator_by_fingerprint(&pool, &fp_lookup).await;

        match result {
            Ok(Some(name)) => {
                tracing::info!("[SSH-Proxy] 运维人员 {} 认证成功", name);
                self.authenticated = true;
                self.operator = Some(name);
                self.username = Some(username.to_string());
                Ok(server::Auth::Accept)
            }
            Ok(None) => {
                tracing::warn!("[SSH-Proxy] 公钥不在 authorized_operators 中: {}", fp_str);
                Ok(server::Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
            Err(e) => {
                tracing::error!("[SSH-Proxy] 认证查询失败: {}", e);
                Ok(server::Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
        }
    }

    async fn auth_succeeded(
        &mut self,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        tracing::info!(
            "[SSH-Proxy] 认证成功 — operator={} username={}",
            self.operator.as_deref().unwrap_or("?"),
            self.username.as_deref().unwrap_or("?")
        );
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        if !self.authenticated {
            tracing::warn!("[SSH-Proxy] 未认证的 session 请求");
            return Ok(false);
        }

        let username = match &self.username {
            Some(u) => u.clone(),
            None => return Ok(false),
        };

        let node_id = match parse_node_id(&username) {
            Some(id) => id.to_string(),
            None => {
                tracing::warn!("[SSH-Proxy] 无法从用户名 {} 解析 nodeId", username);
                return Ok(false);
            }
        };

        // 保存 server handle 和 channel id
        self.server_handle = Some(session.handle());
        self.channel_id = Some(channel.id());

        tracing::info!("[SSH-Proxy] 打开 session → nodeId={}", node_id);

        // 缓存 node_id 供后续 shell_request 使用
        self.node_id = Some(node_id.clone());

        // 创建会话审计记录
        let pool = self.pool.clone();
        let sid = self.session_id.clone();
        let op = self.operator.clone().unwrap_or_default();
        let peer = self.peer_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = ssh_db::create_session(&pool, &sid, &op, &node_id, &peer).await {
                tracing::warn!("[SSH-Proxy] 创建会话记录失败: {}", e);
            }
        });

        Ok(true)
    }

    async fn pty_request(
        &mut self,
        _channel: ChannelId,
        term: &str,
        col: u32,
        row: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        tracing::info!("[SSH-Proxy] PTY 请求: term={} {}x{}", term, col, row);
        self.pty_size = Some(PtySizeParams {
            term: term.to_string(),
            col,
            row,
        });
        Ok(())
    }

    async fn shell_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let node_id = match &self.node_id {
            Some(id) => id.clone(),
            None => return Err(anyhow::anyhow!("无 nodeId")),
        };

        let server_handle = match self.server_handle.clone() {
            Some(h) => h,
            None => return Err(anyhow::anyhow!("无 server handle")),
        };
        let operator_channel_id = match self.channel_id {
            Some(id) => id,
            None => return Err(anyhow::anyhow!("无 channel id")),
        };

        let (cols, rows) = match &self.pty_size {
            Some(s) => (s.col, s.row),
            None => (80, 24),
        };

        // 查询 Agent 地址
        let agent_addr = lookup_agent_addr(&self.pool, &node_id).await?;
        tracing::info!("[SSH-Proxy] 连接 Agent {}", agent_addr);

        // 使用预加载的 client key
        let key_with_hash = russh::keys::PrivateKeyWithHashAlg::new(self.client_key.clone(), None);

        // 建立 SSH Client 连接到 Agent
        let config = Arc::new(client::Config {
            ..Default::default()
        });

        let mut agent_session = client::connect(config, &agent_addr, AgentClient).await
            .map_err(|e| anyhow::anyhow!("连接 Agent {} 失败: {}", agent_addr, e))?;

        // 公钥认证 — 使用预加载的 client key
        let auth_result = agent_session
            .authenticate_publickey("synon-console", key_with_hash)
            .await
            .map_err(|e| anyhow::anyhow!("Agent 认证失败: {}", e))?;

        if auth_result != russh::client::AuthResult::Success {
            return Err(anyhow::anyhow!("Agent SSH 认证被拒绝: {:?}", auth_result));
        }

        // 打开 Agent channel
        let mut agent_channel = agent_session.channel_open_session().await
            .map_err(|e| anyhow::anyhow!("打开 Agent channel 失败: {}", e))?;
        let agent_ch_id = agent_channel.id();

        // 请求 PTY + Shell
        agent_channel.request_pty(false, "xterm-256color", cols, rows, 0, 0, &[]).await
            .map_err(|e| anyhow::anyhow!("Agent PTY 请求失败: {}", e))?;
        agent_channel.request_shell(false).await
            .map_err(|e| anyhow::anyhow!("Agent Shell 请求失败: {}", e))?;

        tracing::info!("[SSH-Proxy] Agent Shell 就绪，开始双向桥接");

        // 保存 agent handle 和 channel id 供 data() 回调使用
        self.agent_writer = Some(agent_session);
        self.agent_channel_id = Some(agent_ch_id);

        // ── Agent → Operator 桥接 ──
        // agent_channel.wait() 循环读取 Agent 输出，转发给运维人员
        let pool = self.pool.clone();
        let sid = self.session_id.clone();

        tokio::spawn(async move {
            use russh::ChannelMsg;
            loop {
                match agent_channel.wait().await {
                    Some(ChannelMsg::Data { data }) => {
                        if let Err(e) = server_handle.data(operator_channel_id, data.to_vec()).await {
                            tracing::warn!("[SSH-Proxy] 转发 Agent 数据到运维失败: {:?}", e);
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, ext }) => {
                        // 仅转发 stderr (ext=1)，忽略其他扩展数据类型
                        if ext == 1 {
                            let _ = server_handle.extended_data(
                                operator_channel_id,
                                ext,
                                data.to_vec(),
                            ).await;
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                        tracing::info!("[SSH-Proxy] Agent channel 关闭");
                        break;
                    }
                    Some(_) => {}
                    None => {
                        tracing::info!("[SSH-Proxy] Agent channel 结束");
                        break;
                    }
                }
            }

            // 清理
            let _ = server_handle.eof(operator_channel_id).await;
            if let Err(e) = ssh_db::close_session(&pool, &sid, "closed").await {
                tracing::warn!("[SSH-Proxy] 关闭会话记录失败: {}", e);
            }
            tracing::info!("[SSH-Proxy] 会话 {} 桥接结束", sid);
        });

        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        // 转发运维人员输入到 Agent
        if let (Some(ref agent_handle), Some(agent_ch)) = (&self.agent_writer, self.agent_channel_id) {
            if let Err(e) = agent_handle.data(agent_ch, data.to_vec()).await {
                tracing::warn!("[SSH-Proxy] 转发输入到 Agent 失败: {:?}", e);
            }
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        tracing::info!("[SSH-Proxy] Channel EOF: session={}", self.session_id);
        // 关闭 Agent 连接 — bridge 任务会检测到并执行 close_session
        self.agent_writer = None;
        Ok(())
    }
}
