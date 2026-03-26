//! claw_proxy.rs — 本地调用 OpenClaw Gateway HTTP/WS API
//! 同机进程，直连 127.0.0.1:18789，零 SSH 依赖

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct ClawProxy {
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

impl ClawProxy {
    pub fn new(port: u16, token: &str) -> Self {
        ClawProxy {
            base_url: format!("http://127.0.0.1:{port}"),
            token: token.to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("构建 HTTP 客户端失败"),
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
                // "Runtime: running" 或 JSON 中 running: true
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
        // method 格式：
        //   "status"         → GET /status
        //   "models"         → GET /v1/models
        //   "config.get"     → 通过 CLI openclaw gateway status --json
        //   "config.patch"   → 通过 CLI opencclaw config set
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
