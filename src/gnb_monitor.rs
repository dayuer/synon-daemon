//! gnb_monitor.rs — 解析 gnb_ctl 输出，获取 P2P 对等体状态
//! 通过调用 gnb_ctl 命令行工具，解析其文本输出。

use anyhow::Result;
use serde::Serialize;
use tokio::process::Command;

/// GNB 对等节点状态
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GnbPeer {
    /// 对端节点 UUID
    pub uuid64: String,
    /// 对端 TUN 地址
    pub tun_addr: String,
    /// 连接状态 ("Direct" | "Relay" | "Offline")
    pub status: String,
    /// 延迟 (微秒，-1=未知)
    pub latency_us: i64,
    /// 接收字节数
    pub in_bytes: u64,
    /// 发送字节数
    pub out_bytes: u64,
}

/// GNB 本地节点摘要
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GnbStatus {
    /// 本机 GNB 节点 ID
    pub node_id: String,
    /// 本机 TUN 地址
    pub tun_addr: String,
    /// TUN 接口是否已就绪
    pub tun_ready: bool,
    /// 对等节点列表
    pub peers: Vec<GnbPeer>,
}

/// 采集 GNB 状态
pub async fn collect(gnb_map_path: &str) -> Result<GnbStatus> {
    // 优先通过 gnb_ctl -a show 获取地址信息
    let tun_addr = read_tun_addr().await;
    let tun_ready = tun_addr.is_some();
    let node_id = read_gnb_node_id(gnb_map_path);

    let peers = collect_peers(gnb_map_path).await.unwrap_or_default();

    Ok(GnbStatus {
        node_id: node_id.unwrap_or_else(|| "unknown".to_string()),
        tun_addr: tun_addr.unwrap_or_else(|| "0.0.0.0".to_string()),
        tun_ready,
        peers,
    })
}

/// 读取 gnb_tun 接口的 inet 地址
async fn read_tun_addr() -> Option<String> {
    // 读 /proc/net/if_inet6 或用 ip addr 解析
    let output = Command::new("ip")
        .args(["addr", "show", "gnb_tun"])
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // 找 "inet X.X.X.X" 行
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with("inet ") && !line.contains("127.") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let Some(addr) = parts.get(1) {
                // 去掉掩码 /24
                return Some(addr.split('/').next()?.to_string());
            }
        }
    }
    None
}

/// 从 address.conf 文件精确读取当前节点 ID
fn read_gnb_node_id(gnb_map_path: &str) -> Option<String> {
    // gnb_map_path 形如 /opt/gnb/conf/<nodeId>/gnb.map
    // 从路径提取 nodeId 段
    std::path::Path::new(gnb_map_path)
        .parent()?
        .file_name()?
        .to_str()
        .map(|s| s.to_string())
}

/// 通过 gnb_ctl 采集对等节点列表
async fn collect_peers(gnb_map_path: &str) -> Result<Vec<GnbPeer>> {
    // gnb_ctl -a show -m <map> 显示节点表
    let output = Command::new("gnb_ctl")
        .args(["-a", "show", "-m", gnb_map_path])
        .output()
        .await;

    let Ok(output) = output else {
        // gnb_ctl 未安装或参数不对，返回空
        return Ok(vec![]);
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let peers = parse_gnb_ctl_output(&stdout);
    Ok(peers)
}

/// 解析 gnb_ctl 文本输出
/// 格式大致为：
///   uuid64  tun_addr  status  latency  in_bytes  out_bytes
fn parse_gnb_ctl_output(text: &str) -> Vec<GnbPeer> {
    let mut peers = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 { continue; }

        // 启发式判断：第1字段是 uuid64（16进制数字长度约16+），第2字段是 IP
        let maybe_uuid = parts[0];
        let maybe_ip = parts[1];
        if maybe_uuid.len() < 8 { continue; }
        if !maybe_ip.contains('.') { continue; }

        let status = match parts.get(2).copied().unwrap_or("?") {
            s if s.contains("Direct") => "Direct",
            s if s.contains("Relay") => "Relay",
            _ => "Offline",
        }.to_string();

        let latency_us = parts.get(3)
            .and_then(|s| s.trim_end_matches("us").parse().ok())
            .unwrap_or(-1i64);
        let in_bytes = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0u64);
        let out_bytes = parts.get(5).and_then(|s| s.parse().ok()).unwrap_or(0u64);

        peers.push(GnbPeer {
            uuid64: maybe_uuid.to_string(),
            tun_addr: maybe_ip.to_string(),
            status,
            latency_us,
            in_bytes,
            out_bytes,
        });
    }
    peers
}
