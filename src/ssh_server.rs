//! ssh_server.rs — Agent 端 SSH Server
//!
//! 在 Agent 进程内运行一个 russh SSH Server，监听 GNB TUN 地址 :22222。
//! 仅接受 Console 的 client key（公钥白名单）。
//! 提供完整交互式 Shell（通过 PTY + /bin/bash）。

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio_util::sync::CancellationToken;
use russh::{
    server,
    server::Server,
    Channel, ChannelId,
};
use russh::keys::ssh_key;
use anyhow::Result;

/// Agent SSH Server 运行入口
pub async fn run_agent_ssh_server(shutdown: CancellationToken) {
    // 加载 Agent host key
    let host_key_path = std::env::var("SSH_AGENT_HOST_KEY")
        .unwrap_or_else(|_| "/opt/gnb/etc/keys/agent_host_key".to_string());

    // 路径安全校验：env 覆盖的路径必须在授权目录下
    if std::env::var("SSH_AGENT_HOST_KEY").is_ok() {
        let p = std::path::Path::new(&host_key_path);
        let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let authorized = canonical.starts_with("/opt/gnb/etc/keys/")
            || canonical.starts_with("/etc/ssh/")
            || canonical.is_relative();
        if !authorized {
            tracing::error!("[Agent-SSH] host key 路径不在授权目录下: {}", canonical.display());
            return;
        }
    }

    let host_key = match load_key(&host_key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("[Agent-SSH] 加载 host key 失败 ({}): {}", host_key_path, e);
            return;
        }
    };

    // 加载 authorized_keys（仅信任 Console client key）
    let authorized_keys_path = std::env::var("SSH_AGENT_AUTHORIZED_KEYS")
        .unwrap_or_else(|_| "/opt/gnb/etc/keys/authorized_keys".to_string());

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

    let listener = match tokio::net::TcpListener::bind((bind_addr.as_str(), bind_port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("[Agent-SSH] 绑定 {}:{} 失败: {}", bind_addr, bind_port, e);
            return;
        }
    };

    tracing::info!("[Agent-SSH] SSH Server 启动，监听 {}:{}", bind_addr, bind_port);

    // 连接数信号量 — 防止资源耗尽
    let max_conns: usize = std::env::var("SSH_MAX_CONNECTIONS")
        .unwrap_or_else(|_| "32".to_string())
        .parse()
        .unwrap_or(32);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_conns));
    tracing::info!("[Agent-SSH] 最大并发连接数: {}", max_conns);

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((socket, peer_addr)) => {
                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                tracing::warn!("[Agent-SSH] 拒绝连接 (已达上限 {}): {}", max_conns, peer_addr);
                                continue;
                            }
                        };
                        let mut server = AgentSshServer {
                            authorized_keys: Arc::new(authorized_keys.clone()),
                        };
                        let handler = server.new_client(Some(peer_addr));
                        let config_clone = config.clone();

                        tokio::spawn(async move {
                            let _permit = permit; // 持有 permit 直到会话结束
                            if let Err(e) = russh::server::run_stream(config_clone, socket, handler).await {
                                tracing::error!("[Agent-SSH] 会话执行错误 (来自 {}): {}", peer_addr, e);
                            }
                        });
                    }
                    Err(e) => tracing::error!("[Agent-SSH] 接收连接失败: {}", e),
                }
            }
            _ = shutdown.cancelled() => {
                tracing::info!("[Agent-SSH] 收到关闭信号，SSH Server 退出");
                break;
            }
        }
    }
}

fn load_key(path: &str) -> Result<russh::keys::PrivateKey> {
    let key_data = std::fs::read_to_string(path)?;
    let private_key = russh::keys::decode_secret_key(&key_data, None)?;
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
        if let Ok(key) = ssh_key::PublicKey::from_openssh(line) {
            keys.push(key);
        } else {
            tracing::warn!("[Agent-SSH] 跳过无效 authorized_keys 行: {:?}", line);
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

    fn new_client(&mut self, peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        let peer = peer_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        tracing::info!("[Agent-SSH] 新连接来自 {}", peer);
        AgentSshSession {
            authorized_keys: self.authorized_keys.clone(),
            authenticated: false,
            pty_size: None,
            server_handle: None,
            channel_id: None,
            pty_writer: None,
            pty_master: None,
        }
    }
}

/// PTY 尺寸
struct PtySize {
    col: u16,
    row: u16,
}

/// 单个 Agent SSH 会话
struct AgentSshSession {
    authorized_keys: Arc<Vec<ssh_key::PublicKey>>,
    authenticated: bool,
    pty_size: Option<PtySize>,
    server_handle: Option<server::Handle>,
    channel_id: Option<ChannelId>,
    pty_writer: Option<Arc<StdMutex<Box<dyn std::io::Write + Send>>>>,
    /// PTY master handle — 保留用于 window resize
    pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl server::Handler for AgentSshSession {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        username: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        let authorized = self.authorized_keys.iter().any(|k| {
            k.fingerprint(ssh_key::HashAlg::Sha256) == public_key.fingerprint(ssh_key::HashAlg::Sha256)
        });

        if authorized {
            tracing::info!("[Agent-SSH] Console 公钥认证通过: user={}", username);
            self.authenticated = true;
            Ok(server::Auth::Accept)
        } else {
            tracing::warn!("[Agent-SSH] 拒绝未知公钥: user={}", username);
            Ok(server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        if !self.authenticated {
            return Ok(false);
        }
        self.server_handle = Some(session.handle());
        self.channel_id = Some(channel.id());
        tracing::info!("[Agent-SSH] Session channel 已打开");
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
        self.pty_size = Some(PtySize {
            col: col as u16,
            row: row as u16,
        });
        tracing::info!("[Agent-SSH] PTY 请求: term={} {}x{}", term, col, row);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if !self.authenticated {
            return Err(anyhow::anyhow!("未认证"));
        }

        let server_handle = match self.server_handle.clone() {
            Some(h) => h,
            None => return Err(anyhow::anyhow!("无 server handle")),
        };
        let channel_id = match self.channel_id {
            Some(id) => id,
            None => return Err(anyhow::anyhow!("无 channel id")),
        };

        let (cols, rows) = match &self.pty_size {
            Some(s) => (s.col, s.row),
            None => (80, 24),
        };

        // 创建 PTY
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("创建 PTY 失败: {}", e))?;

        // 启动 bash
        let cmd = portable_pty::CommandBuilder::new("/bin/bash");
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("启动 bash 失败: {}", e))?;

        // 获取 PTY 读写端
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("clone PTY reader 失败: {}", e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("take PTY writer 失败: {}", e))?;

        // 存储 writer 供 data() 回调使用
        self.pty_writer = Some(Arc::new(StdMutex::new(writer)));

        // 保留 master PTY 供 window resize 使用
        self.pty_master = Some(pair.master);

        // PTY stdout → SSH client 桥接
        // 使用 mpsc 解耦 blocking read 和 async write
        let (pty_tx, mut pty_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

        // Blocking 任务: 从 PTY reader 读数据
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF — bash 已退出
                    Ok(n) => {
                        if pty_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break; // 接收端已关闭
                        }
                    }
                    Err(_) => break,
                }
            }
            tracing::debug!("[Agent-SSH] PTY reader 任务退出，正在回收子进程...");
            let _ = child.wait();
        });

        // Async 任务: PTY 数据 → SSH channel → Console Proxy
        // 含空闲超时保护
        let idle_secs: u64 = std::env::var("SSH_IDLE_TIMEOUT")
            .unwrap_or_else(|_| "3600".to_string())
            .parse()
            .unwrap_or(3600);

        tokio::spawn(async move {
            let idle = std::time::Duration::from_secs(idle_secs);
            loop {
                match tokio::time::timeout(idle, pty_rx.recv()).await {
                    Ok(Some(data)) => {
                        if let Err(e) = server_handle.data(channel_id, data).await {
                            tracing::warn!("[Agent-SSH] 发送数据到 SSH client 失败: {:?}", e);
                            break;
                        }
                    }
                    Ok(None) => break, // channel 关闭
                    Err(_) => {
                        tracing::warn!("[Agent-SSH] PTY 空闲超时 ({}s)，关闭会话 channel {:?}", idle_secs, channel_id);
                        break;
                    }
                }
            }
            // bash 退出后关闭 channel
            let _ = server_handle.eof(channel_id).await;
            tracing::info!("[Agent-SSH] PTY→SSH 桥接任务退出 (channel {:?})", channel_id);
        });

        tracing::info!("[Agent-SSH] Shell 已启动 (PTY {}x{})", cols, rows);
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        // 将 Console Proxy 发来的键盘输入写入 PTY stdin
        if let Some(ref writer) = self.pty_writer {
            if let Ok(mut w) = writer.lock() {
                w.write_all(data)?;
            } else {
                tracing::error!("[Agent-SSH] PTY writer lock poisoned");
            }
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col: u32,
        row: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if let Some(ref master) = self.pty_master {
            if let Err(e) = master.resize(portable_pty::PtySize {
                rows: row as u16,
                cols: col as u16,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                tracing::warn!("[Agent-SSH] PTY resize 失败: {}", e);
            } else {
                tracing::debug!("[Agent-SSH] PTY resize 成功: {}x{}", col, row);
            }
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        tracing::info!("[Agent-SSH] Channel EOF");
        // 关闭 PTY writer，bash 将收到 EOF
        self.pty_writer = None;
        Ok(())
    }
}
