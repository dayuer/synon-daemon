//! claw_proxy.rs — 本地调用 OpenClaw Gateway HTTP/WS API
//! 同机进程，直连 127.0.0.1:18789，零 SSH 依赖

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tracing::{debug, info, warn};

pub struct ClawProxy {
    pub port: u16,
    base_url: String,
    token: String,
    client: reqwest::Client,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClawStatus {
    pub running: bool,
    pub version: Option<String>,
    pub uptime_ms: Option<u64>,
}

/// OpenClaw 实时事件（从其 WS 订阅）
#[derive(Debug, Clone, Serialize)]
pub struct ClawEvent {
    pub event_type: String, // "health" | "tick" | "error"
    pub data: Value,
}

impl ClawProxy {
    pub fn new(port: u16, token: &str) -> Self {
        ClawProxy {
            port,
            base_url: format!("http://127.0.0.1:{port}"),
            token: token.to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("构建 HTTP 客户端失败"),
        }
    }

    /// 创建用于 subscribe_events() 的轻量克隆（仅含 port 和 token）
    pub fn clone_for_events(&self) -> ClawProxy {
        ClawProxy::new(self.port, &self.token)
    }

    /// 订阅 OpenClaw WS 实时事件，持续运行，断线重连。
    ///
    /// **Daemon 自主感知**：每次连接前动态读 `/root/.openclaw/openclaw.json`，
    /// 获取最新 token。daemon 可在 OpenClaw 安装前启动，感知到 token 后自动接入，
    /// 无需 initnode.sh 手动写入或重启。
    ///
    /// - token 未就绪（OpenClaw 未安装）：静默等 30s 后重试
    /// - 连接成功后每 5s 主动 Ping 保活，防止服务端 idle timeout
    /// - 断开后指数退避（3→6→12…最大 60s）
    pub async fn subscribe_events(&self, event_tx: mpsc::Sender<ClawEvent>, cancel: CancellationToken) {
        let ws_base = format!("ws://127.0.0.1:{}/ws", self.port);
        let mut retry_secs: u64 = 3;

        loop {
            // 每次连接前动态读取最新 token（OpenClaw 可能在daemon启动后才安装）
            let token = Self::read_claw_token_from_config();
            if token.is_none() {
                debug!("[ClawEvent] openclaw.json 不存在或 token 未配置，30s 后重试");
                tokio::select! {
                    _ = sleep(Duration::from_secs(30)) => {}
                    _ = cancel.cancelled() => { return; }
                }
                retry_secs = 3;
                continue;
            }
            let token = token.unwrap();

            // 构建带 Authorization: Bearer <token> 的 WS 握手请求
            let mut ws_req = match ws_base.clone().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    warn!("[ClawEvent] WS URL 解析失败: {e}");
                    tokio::select! {
                        _ = sleep(Duration::from_secs(30)) => {}
                        _ = cancel.cancelled() => { return; }
                    }
                    continue;
                }
            };
            ws_req.headers_mut().insert(
                AUTHORIZATION,
                format!("Bearer {token}").parse().expect("header value parse"),
            );
            match connect_async(ws_req).await {
                Err(e) => {
                    debug!("[ClawEvent] OpenClaw WS 连接失败 ({e})，30s 后重试");
                    tokio::select! {
                        _ = sleep(Duration::from_secs(30)) => {}
                        _ = cancel.cancelled() => { return; }
                    }
                    retry_secs = 3;
                }
                Ok((ws_stream, _)) => {
                    info!("[ClawEvent] 已连接 OpenClaw WS");
                    let (mut write, mut read) = ws_stream.split();

                    // OpenClaw WS 应用层握手：连接后必须在 handshakeTimeoutMs（默认10s）内
                    // 发送 method=connect 的 JSON-RPC 请求，否则服务端踢掉。
                    // ConnectParamsSchema（逆向自 method-scopes-DhuXuLfv.js）：
                    //   minProtocol/maxProtocol: integer
                    //   client: { id, version, platform, mode }
                    //   auth: { token }（optional）
                    let connect_msg = serde_json::json!({
                        "type": "req",       // RequestFrameSchema 必填：type = "req"
                        "id": "1",           // id 必须是 NonEmptyString，不能是整数
                        "method": "connect",
                        "params": {
                            "minProtocol": 1,
                            "maxProtocol": 99,
                            "client": {
                                "id": "gateway-client",
                                "version": "0.1.0",
                                "platform": "linux",
                                "mode": "backend"
                            },
                            "auth": {
                                "token": token
                            }
                        }
                    });
                    if write.send(Message::Text(connect_msg.to_string())).await.is_err() {
                        warn!("[ClawEvent] 发送 connect 消息失败");
                        sleep(Duration::from_secs(retry_secs)).await;
                        retry_secs = (retry_secs * 2).min(60);
                        continue;
                    }
                    debug!("[ClawEvent] connect 消息已发送，等待 hello-ok");

                    while let Some(msg) = tokio::select! {
                        msg = read.next() => msg,
                        _ = cancel.cancelled() => None,
                    } {
                        let text = match msg {
                            Ok(Message::Text(t)) => t.to_string(),
                            Ok(Message::Close(_)) => {
                                debug!("[ClawEvent] OpenClaw WS 正常关闭");
                                break;
                            }
                            Ok(Message::Ping(data)) => {
                                let _ = write.send(Message::Pong(data)).await;
                                continue;
                            }
                            Ok(Message::Pong(_)) => continue,
                            Err(e) => {
                                warn!("[ClawEvent] WS 错误: {e}");
                                break;
                            }
                            _ => continue,
                        };

                        let Ok(val): Result<Value, _> = serde_json::from_str(&text) else {
                            continue;
                        };
                        let event_type = val["type"].as_str().unwrap_or("").to_string();
                        if matches!(event_type.as_str(), "health" | "tick" | "error" | "heartbeat") {
                            let evt = ClawEvent { event_type, data: val };
                            if event_tx.send(evt).await.is_err() {
                                return; // 调用方已关闭
                            }
                        }
                    }

                    // 如果是 cancel 触发的退出，直接返回
                    if cancel.is_cancelled() { return; }

                    // 连接断开：指数退避（最大 60s）
                    info!("[ClawEvent] OpenClaw WS 断开，{retry_secs}s 后重连");
                    tokio::select! {
                        _ = sleep(Duration::from_secs(retry_secs)) => {}
                        _ = cancel.cancelled() => { return; }
                    }
                    retry_secs = (retry_secs * 2).min(60);
                }
            }
        }
    }

    /// 从 /root/.openclaw/openclaw.json 动态读取 gateway.auth.token
    /// 供 subscribe_events 每次连接前调用，感知 OpenClaw 安装后自动接入
    fn read_claw_token_from_config() -> Option<String> {
        let content = std::fs::read_to_string("/root/.openclaw/openclaw.json").ok()?;
        let cfg: serde_json::Value = serde_json::from_str(&content).ok()?;
        cfg["gateway"]["auth"]["token"].as_str().map(|s| s.to_string())
    }

    /// 获取 OpenClaw 状态（通过 CLI 子进程，最可靠）
    pub async fn get_status(&self) -> ClawStatus {
        let output = tokio::process::Command::new("openclaw")
            .args(["gateway", "status", "--json"])
            .output()
            .await;

        let running = match output {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                text.contains("running")
            }
            _ => false,
        };

        ClawStatus {
            running,
            version: crate::claw_manager::read_local_version().await,
            uptime_ms: None,
        }
    }

    /// 使用 SHA-256 计算 ETag（统一 trim 消除尾部换行差异）
    fn calc_hash(data: &str) -> String {
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(data.trim());
        format!("{:x}", hasher.finalize())
    }

    /// 通用 HTTP 调用（供 Console 的 claw_rpc 消息使用）
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "status" => {
                let status = self.get_status().await;
                Ok(serde_json::to_value(status)?)
            }
            "models" => {
                let resp = self.client
                    .get(format!("{}/v1/models", self.base_url))
                    .bearer_auth(&self.token)
                    .send()
                    .await
                    .context("GET /v1/models 失败")?;
                Ok(resp.json().await?)
            }
            "config.get" => {
                let output = tokio::process::Command::new("openclaw")
                    .args(["config", "get", "--json"])
                    .output()
                    .await
                    .context("openclaw config get 失败")?;
                let text = String::from_utf8_lossy(&output.stdout);
                let data = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text.to_string()));
                let hash = Self::calc_hash(&text);
                // 包装为 { data, hash } 格式，前端据此实现 ETag 防脑裂
                Ok(serde_json::json!({
                    "data": data,
                    "hash": hash
                }))
            }
            "config.patch" => {
                let base_hash = params["baseHash"].as_str().unwrap_or("");
                // ⚠️ TOCTOU: check 与 write 之间存在竞态窗口，目前无法原子化（需 OpenClaw 原生 CAS）
                if !base_hash.is_empty() {
                    let output = tokio::process::Command::new("openclaw")
                        .args(["config", "get", "--json"])
                        .output()
                        .await
                        .context("openclaw config get 失败")?;
                    let current_text = String::from_utf8_lossy(&output.stdout);
                    let current_hash = Self::calc_hash(&current_text);

                    if current_hash != base_hash {
                        anyhow::bail!("E_CONFLICT")
                    }
                }

                // 继续透传到 HTTP API
                let endpoint = format!("/{}", method.replace('.', "/"));
                let resp = self.client
                    .post(format!("{}{}", self.base_url, endpoint))
                    .bearer_auth(&self.token)
                    .json(&params)
                    .send()
                    .await
                    .with_context(|| format!("调用 {method} 失败"))?;
                Ok(resp.json().await.unwrap_or(Value::Null))
            }
            "gateway.restart" => {
                let _ = tokio::process::Command::new("openclaw")
                    .args(["gateway", "restart"])
                    .output()
                    .await;
                Ok(Value::Bool(true))
            }
            _ => {
                // 透传未知方法到 HTTP API
                let endpoint = format!("/{}", method.replace('.', "/"));
                let resp = self.client
                    .post(format!("{}{}", self.base_url, endpoint))
                    .bearer_auth(&self.token)
                    .json(&params)
                    .send()
                    .await
                    .with_context(|| format!("调用 {method} 失败"))?;
                Ok(resp.json().await.unwrap_or(Value::Null))
            }
        }
    }
}
