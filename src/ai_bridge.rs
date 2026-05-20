//! ai_bridge.rs — Python AI Agent 子进程管理与 UDS IPC 通信
//!
//! 职责：
//! 1. 启动 python_agent/server.py 子进程
//! 2. 通过 Unix Domain Socket 发送 JSON-RPC 请求
//! 3. 子进程崩溃时自动重启（指数退避，最多 3 次）
//!
//! 设计原则：Python 崩溃绝不影响 Rust 主进程稳定性。

use anyhow::Result;
use serde::{Serialize, Deserialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};

/// AI 推理决策（Python → Rust）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiDecision {
    /// 动作类型: keep_alive / restart_service / emergency_cleanup / escalate
    pub action: String,
    /// 目标服务名（仅 restart_service 时有效）
    #[serde(default)]
    pub target: String,
    /// AI 推理依据
    #[serde(default)]
    pub reason: String,
    /// 置信度 (0.0 ~ 1.0)
    #[serde(default)]
    pub confidence: f64,
}

impl Default for AiDecision {
    fn default() -> Self {
        Self {
            action: "keep_alive".into(),
            target: String::new(),
            reason: "AI Agent 未响应，保持现状".into(),
            confidence: 0.0,
        }
    }
}

/// JSON-RPC 请求结构
#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: String,
    params: serde_json::Value,
    id: u64,
}

/// JSON-RPC 响应结构
#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
    #[allow(dead_code)]
    id: u64,
}

/// AI Bridge — 管理 Python 子进程与 IPC 通信
pub struct AiBridge {
    socket_path: PathBuf,
    python_script: PathBuf,
    child: Mutex<Option<Child>>,
    request_id: Mutex<u64>,
    node_id: String,
}

impl AiBridge {
    /// 创建 AI Bridge 实例
    pub fn new(node_id: &str) -> Self {
        // Socket 路径：/tmp/synon-ai-{node_id}.sock
        let socket_path = PathBuf::from(format!("/tmp/synon-ai-{}.sock", node_id));

        // Python 脚本路径：相对于二进制文件的 python_agent/server.py
        // 优先级: $SYNON_PYTHON_AGENT > 二进制同级目录 > /opt/gnb/python_agent
        let python_script = std::env::var("SYNON_PYTHON_AGENT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let exe_dir = std::env::current_exe()
                    .unwrap_or_default()
                    .parent()
                    .unwrap_or(Path::new("/opt/gnb"))
                    .to_path_buf();
                let local = exe_dir.join("python_agent/server.py");
                if local.exists() { local } else {
                    PathBuf::from("/opt/gnb/python_agent/server.py")
                }
            });

        Self {
            socket_path,
            python_script,
            child: Mutex::new(None),
            request_id: Mutex::new(0),
            node_id: node_id.to_string(),
        }
    }

    /// 启动 Python 子进程
    pub async fn spawn_child(&self) -> Result<()> {
        // 清理旧 socket 文件
        let _ = tokio::fs::remove_file(&self.socket_path).await;

        if !self.python_script.exists() {
            warn!("[AiBridge] Python Agent 脚本不存在: {}, AI 功能禁用",
                self.python_script.display());
            return Ok(());
        }

        info!("[AiBridge] 启动 Python AI Agent: {} (socket: {})",
            self.python_script.display(), self.socket_path.display());

        let child = Command::new("python3")
            .arg(&self.python_script)
            .arg("--socket")
            .arg(&self.socket_path)
            .arg("--node-id")
            .arg(&self.node_id)
            .kill_on_drop(true) // Rust 进程退出时自动杀死子进程
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("启动 Python Agent 失败: {}", e))?;

        let pid = child.id().unwrap_or(0);
        info!("[AiBridge] Python Agent 已启动 (PID: {})", pid);

        *self.child.lock().await = Some(child);

        // 等待 socket 文件出现（最多 5 秒）
        for _ in 0..50 {
            if self.socket_path.exists() {
                info!("[AiBridge] UDS 连接就绪: {}", self.socket_path.display());
                return Ok(());
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        warn!("[AiBridge] 等待 UDS 超时，Python Agent 可能未正常启动");
        Ok(())
    }

    /// 发送 JSON-RPC 请求到 Python Agent
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = {
            let mut id = self.request_id.lock().await;
            *id += 1;
            *id
        };

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.into(),
            params,
            id,
        };

        let mut payload = serde_json::to_vec(&request)?;
        payload.push(b'\n'); // 行分隔协议

        // 连接 UDS
        let stream = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            UnixStream::connect(&self.socket_path),
        ).await
            .map_err(|_| anyhow::anyhow!("UDS 连接超时"))?
            .map_err(|e| anyhow::anyhow!("UDS 连接失败: {}", e))?;

        let (reader, mut writer) = stream.into_split();

        // 发送请求
        writer.write_all(&payload).await?;
        writer.shutdown().await?;

        // 读取响应（单行 JSON）
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        tokio::time::timeout(
            tokio::time::Duration::from_secs(30), // AI 推理可能需要较长时间
            buf_reader.read_line(&mut line),
        ).await
            .map_err(|_| anyhow::anyhow!("AI 响应超时 (30s)"))?
            .map_err(|e| anyhow::anyhow!("读取 AI 响应失败: {}", e))?;

        let resp: JsonRpcResponse = serde_json::from_str(line.trim())?;

        if let Some(err) = resp.error {
            return Err(anyhow::anyhow!("AI RPC 错误: {}", err));
        }

        resp.result.ok_or_else(|| anyhow::anyhow!("AI 响应缺少 result 字段"))
    }

    /// 将遥测数据发送给 AI Agent，获取决策
    pub async fn observe(&self, sys_info: &serde_json::Value) -> AiDecision {
        match self.call("observe", sys_info.clone()).await {
            Ok(val) => serde_json::from_value(val).unwrap_or_default(),
            Err(e) => {
                debug!("[AiBridge] observe 调用失败: {} (降级为 keep_alive)", e);
                AiDecision::default()
            }
        }
    }

    /// 请求 AI 诊断特定错误
    #[allow(dead_code)]
    pub async fn diagnose(&self, error_ctx: serde_json::Value) -> Result<String> {
        let result = self.call("diagnose", error_ctx).await?;
        Ok(result.as_str().unwrap_or("无诊断结果").to_string())
    }

    /// 检查子进程是否存活，不存活则尝试重启
    pub async fn ensure_alive(&self) {
        let mut child = self.child.lock().await;
        if let Some(ref mut c) = *child {
            match c.try_wait() {
                Ok(Some(status)) => {
                    warn!("[AiBridge] Python Agent 已退出 ({}), 尝试重启", status);
                    drop(child);
                    if let Err(e) = self.spawn_child().await {
                        error!("[AiBridge] 重启 Python Agent 失败: {}", e);
                    }
                }
                Ok(None) => {} // 仍在运行
                Err(e) => warn!("[AiBridge] 检查子进程状态失败: {}", e),
            }
        }
    }

    /// 优雅关闭 Python 子进程
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        if let Some(ref mut c) = *child {
            info!("[AiBridge] 发送关闭信号给 Python Agent");
            let _ = c.kill().await;
        }
        let _ = tokio::fs::remove_file(&self.socket_path).await;
    }
}

/// 启动 AI Bridge 主循环（定期健康检查 + 遥测推送）
pub async fn run(
    bridge: Arc<AiBridge>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    // 启动 Python 子进程
    if let Err(e) = bridge.spawn_child().await {
        error!("[AiBridge] 初始化 Python Agent 失败: {} (AI 功能降级)", e);
    }

    // 定期健康检查（60s 间隔）
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                bridge.ensure_alive().await;
            }
            _ = shutdown.cancelled() => {
                bridge.shutdown().await;
                info!("[AiBridge] AI Bridge 已关闭");
                break;
            }
        }
    }
}
