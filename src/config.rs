//! config.rs — 读取节点配置
//! 数据来源: agent.conf (注册时生成) + openclaw.json (OpenClaw 本地配置)

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

/// 完整 Daemon 配置（由多个来源合并）
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Console WSS 地址，例如 wss://api.synonclaw.com/ws/daemon
    pub console_url: String,
    /// 节点 API Token（注册时颁发）
    pub token: String,
    /// 节点 ID（平台分配）
    pub node_id: String,
    /// GNB 节点配置目录
    pub gnb_conf_dir: PathBuf,
    /// GNB mmap map 路径（用于 gnb_ctl）
    pub gnb_map_path: PathBuf,
    /// OpenClaw 配置文件路径
#[allow(dead_code)]
    pub claw_config_path: PathBuf,
    /// OpenClaw 网关端口（默认 18789）
    pub claw_port: u16,
    /// OpenClaw 认证 Token（从 openclaw.json 读取）
    pub claw_token: Option<String>,
}

/// agent.conf 环境文件结构（由 initnode.sh 生成）
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AgentEnv {
    #[serde(rename = "CONSOLE_URL")]
    console_url: Option<String>,
    #[serde(rename = "TOKEN")]
    token: Option<String>,
    #[serde(rename = "NODE_ID")]
    node_id: Option<String>,
    #[serde(rename = "GNB_NODE_ID")]
    gnb_node_id: Option<String>,
    #[serde(rename = "GNB_MAP_PATH")]
    gnb_map_path: Option<String>,
    #[serde(rename = "CLAW_PORT")]
    claw_port: Option<String>,
}

/// OpenClaw 配置文件结构
#[derive(Debug, Deserialize)]
struct OpenClawConfig {
    gateway: Option<OpenClawGateway>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenClawGateway {
    port: Option<u16>,
    auth: Option<OpenClawAuth>,
}

#[derive(Debug, Deserialize)]
struct OpenClawAuth {
    token: Option<String>,
}

/// 解析 KEY=VALUE 格式的 env 文件
fn parse_env_file(content: &str) -> std::collections::HashMap<String, String> {
    content.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { return None; }
            let mut parts = line.splitn(2, '=');
            let key = parts.next()?.trim().to_string();
            let val = parts.next().unwrap_or("").trim()
                .trim_matches('"').trim_matches('\'').to_string();
            Some((key, val))
        })
        .collect()
}

impl DaemonConfig {
    /// 从默认路径加载配置（/opt/gnb/bin/agent.conf）
    pub fn load(env_path: Option<&str>) -> Result<Self> {
        let env_file = env_path.unwrap_or("/opt/gnb/bin/agent.conf");
        let content = fs::read_to_string(env_file)
            .with_context(|| format!("读取 agent.conf 失败: {env_file}"))?;
        let env = parse_env_file(&content);

        let console_url = env.get("CONSOLE_URL")
            .cloned()
            .context("agent.conf 缺少 CONSOLE_URL")?;
        let token = env.get("TOKEN")
            .cloned()
            .context("agent.conf 缺少 TOKEN")?;
        let node_id = env.get("NODE_ID")
            .cloned()
            .context("agent.conf 缺少 NODE_ID")?;
        let gnb_node_id = env.get("GNB_NODE_ID").cloned().unwrap_or_default();
        let gnb_map_path = env.get("GNB_MAP_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/opt/gnb/conf/{gnb_node_id}/gnb.map")));
        let claw_port = env.get("CLAW_PORT")
            .and_then(|p| p.parse().ok())
            .unwrap_or(18789u16);

        // Console URL → WSS URL 转换
        // "https://api.synonclaw.com" → "wss://api.synonclaw.com/ws/daemon"
        let console_wss = if console_url.starts_with("https://") {
            format!("{}/ws/daemon", console_url.replace("https://", "wss://"))
        } else if console_url.starts_with("http://") {
            format!("{}/ws/daemon", console_url.replace("http://", "ws://"))
        } else {
            format!("wss://{}/ws/daemon", console_url)
        };

        // 读取 OpenClaw token（可选，不影响启动）
        let claw_config_path = PathBuf::from("/root/.openclaw/openclaw.json");
        let claw_token = Self::read_claw_token(&claw_config_path);

        let gnb_conf_dir = PathBuf::from(format!("/opt/gnb/conf/{gnb_node_id}"));

        Ok(DaemonConfig {
            console_url: console_wss,
            token,
            node_id,
            gnb_conf_dir,
            gnb_map_path,
            claw_config_path,
            claw_port,
            claw_token,
        })
    }

    /// 从 openclaw.json 提取 gateway.auth.token
    fn read_claw_token(path: &PathBuf) -> Option<String> {
        let content = fs::read_to_string(path).ok()?;
        let cfg: OpenClawConfig = serde_json::from_str(&content).ok()?;
        cfg.gateway?.auth?.token
    }
}
