//! claw_proxy.rs — 本地调用 OpenClaw Gateway HTTP/WS API
//! 同机进程，直连 127.0.0.1:18789，零 SSH 依赖

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
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

    /// 订阅 OpenClaw WS 实时事件（health/tick），持续运行，断线重连
    ///
    /// 事件通过 `event_tx` 发给调用方（通常是 console_ws.rs 转发给 Console）
    pub async fn subscribe_events(&self, event_tx: mpsc::Sender<ClawEvent>) {
        let ws_url = format!("ws://127.0.0.1:{}/ws", self.port);
        let token = self.token.clone();

        loop {
            match connect_async(&ws_url).await {
                Err(e) => {
                    debug!("[ClawEvent] OpenClaw WS 连接失败 ({e})，30s 后重试");
                    sleep(Duration::from_secs(30)).await;
                }
                Ok((ws_stream, _)) => {
                    info!("[ClawEvent] 已连接 OpenClaw WS");
                    let (mut write, mut read) = ws_stream.split();

                    // 发送认证帧
                    let auth = json!({ "type": "auth", "token": token });
                    if write.send(Message::Text(auth.to_string().into())).await.is_err() {
                        warn!("[ClawEvent] 发送 auth 帧失败");
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }

                    // 接收事件循环
                    while let Some(msg) = read.next().await {
                        let text = match msg {
                            Ok(Message::Text(t)) => t.to_string(),
                            Ok(Message::Close(_)) => {
                                warn!("[ClawEvent] OpenClaw WS 关闭，重连...");
                                break;
                            }
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
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

                        // 转发 health/tick/error 事件给 Console
                        if matches!(event_type.as_str(), "health" | "tick" | "error" | "heartbeat") {
                            let evt = ClawEvent { event_type, data: val };
                            if event_tx.send(evt).await.is_err() {
                                // 调用方已关闭，退出
                                return;
                            }
                        }
                    }
                }
            }
            sleep(Duration::from_secs(30)).await;
        }
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
