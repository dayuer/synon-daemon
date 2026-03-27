//! claw_proxy.rs — 本地调用 OpenClaw Gateway HTTP/WS API
//! 同机进程，直连 127.0.0.1:18789，零 SSH 依赖

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
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
    pub async fn subscribe_events(&self, event_tx: mpsc::Sender<ClawEvent>) {
        let ws_base = format!("ws://127.0.0.1:{}/ws", self.port);
        let mut retry_secs: u64 = 3;

        loop {
            // 每次连接前动态读取最新 token（OpenClaw 可能在daemon启动后才安装）
            let token = Self::read_claw_token_from_config();
            if token.is_none() {
                debug!("[ClawEvent] openclaw.json 不存在或 token 未配置，30s 后重试");
                sleep(Duration::from_secs(30)).await;
                retry_secs = 3;
                continue;
            }
            let token = token.unwrap();

            // 构建带 Authorization: Bearer <token> 的 WS 握手请求
            let mut ws_req = match ws_base.clone().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    warn!("[ClawEvent] WS URL 解析失败: {e}");
                    sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };
            ws_req.headers_mut().insert(
                "Authorization",
                format!("Bearer {token}").parse().expect("header value parse"),
            );
            match connect_async(ws_req).await {
                Err(e) => {
                    debug!("[ClawEvent] OpenClaw WS 连接失败 ({e})，30s 后重试");
                    sleep(Duration::from_secs(30)).await;
                    retry_secs = 3;
                }
                Ok((ws_stream, _)) => {
                    info!("[ClawEvent] 已连接 OpenClaw WS");
                    let (mut write, mut read) = ws_stream.split();
                    // 每 5s 主动 Ping，防止 OpenClaw 10s idle timeout 踢连
                    let mut ping_ticker = interval(Duration::from_secs(5));
                    ping_ticker.tick().await; // 跳过立即触发的第一次

                    'conn: loop {
                        tokio::select! {
                            // 定时发 Ping 保活
                            _ = ping_ticker.tick() => {
                                if write.send(Message::Ping(vec![])).await.is_err() {
                                    debug!("[ClawEvent] Ping 发送失败，认为连接已断");
                                    break 'conn;
                                }
                            }

                            // 接收服务端消息
                            msg = read.next() => {
                                let msg = match msg {
                                    Some(m) => m,
                                    None => break 'conn,
                                };
                                let text = match msg {
                                    Ok(Message::Text(t)) => t.to_string(),
                                    Ok(Message::Close(_)) => {
                                        debug!("[ClawEvent] OpenClaw WS 正常关闭");
                                        break 'conn;
                                    }
                                    Ok(Message::Ping(data)) => {
                                        let _ = write.send(Message::Pong(data)).await;
                                        continue 'conn;
                                    }
                                    Ok(Message::Pong(_)) => continue 'conn,
                                    Err(e) => {
                                        warn!("[ClawEvent] WS 错误: {e}");
                                        break 'conn;
                                    }
                                    _ => continue 'conn,
                                };

                                let Ok(val): Result<Value, _> = serde_json::from_str(&text) else {
                                    continue 'conn;
                                };
                                let event_type = val["type"].as_str().unwrap_or("").to_string();
                                if matches!(event_type.as_str(), "health" | "tick" | "error" | "heartbeat") {
                                    let evt = ClawEvent { event_type, data: val };
                                    if event_tx.send(evt).await.is_err() {
                                        return; // 调用方已关闭
                                    }
                                }
                            }
                        }
                    }

                    // 连接断开：指数退避（最大 60s）
                    info!("[ClawEvent] OpenClaw WS 断开，{retry_secs}s 后重连");
                    sleep(Duration::from_secs(retry_secs)).await;
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

    /// 检查 OpenClaw 是否可达（GET /v1/models）
    pub async fn ping(&self) -> bool {
        self.client
            .get(format!("{}/v1/models", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// 获取 OpenClaw 状态（通过 CLI 子进程，最可靠）
    pub async fn get_status(&self) -> ClawStatus {
        use std::process::Command;
        let output = Command::new("openclaw")
            .args(["gateway", "status", "--json"])
            .output();

        let running = match output {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                text.contains("running")
            }
            _ => false,
        };

        ClawStatus {
            running,
            version: Self::read_version(),
            uptime_ms: None,
        }
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
                Ok(serde_json::from_str(&text).unwrap_or(Value::String(text.to_string())))
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

    fn read_version() -> Option<String> {
        use std::process::Command;
        let output = Command::new("openclaw").arg("--version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        // "OpenClaw 2026.3.13 ..." → "2026.3.13"
        text.split_whitespace().nth(1).map(|s| s.to_string())
    }
}
