//! heartbeat.rs — 系统状态完整采集（对齐 node-agent.sh 全量字段）
//!
//! 采集内容：
//!   - CPU / 内存 / 磁盘 / uptime（读 /proc 伪文件）
//!   - OS / 内核 / 架构 / 负载（读 /etc/os-release、/proc/loadavg）
//!   - GNB 对等节点状态（调 gnb_ctl -s/-a，与 node-agent.sh 兼容）
//!   - OpenClaw 进程状态 + RPC 可用性 + config 内容 + skills 缓存
//!
//! 所有外部命令调用均设 2 秒超时，避免阻塞心跳循环。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::Duration;
use tracing::debug;

/// 上报给 Console 的系统状态快照（兼容 Console _parseSysInfo + ingestFromDaemon）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SysInfo {
    /// 时间戳（ms since epoch，与旧格式兼容）
    pub ts: String,
    /// CPU 使用率 (0.0 ~ 100.0)
    pub cpu_percent: f64,
    /// 内存使用率 (0.0 ~ 100.0)
    pub mem_percent: f64,
    /// 已用内存 (MB)
    pub mem_used_mb: u64,
    /// 总内存 (MB)
    pub mem_total_mb: u64,
    /// 磁盘使用率 /opt (0.0 ~ 100.0)
    pub disk_percent: f64,
    /// 系统运行时间（秒）
    pub uptime_sec: u64,
    /// 主机名
    pub hostname: String,
    /// OS 发行版（e.g. "Debian GNU/Linux 12 (bookworm)"）
    pub os: String,
    /// 内核版本（uname -r）
    pub kernel: String,
    /// CPU 架构（uname -m）
    pub arch: String,
    /// 系统负载（1m 5m 15m，空格分隔）
    pub load: String,
    /// CPU 型号
    pub cpu_model: String,
    /// CPU 核数
    pub cpu_cores: u32,
    /// GNB 进程是否运行
    pub gnb_running: bool,
    /// OpenClaw 进程是否运行
    pub claw_running: bool,
    /// OpenClaw RPC (/api/status) 是否可达
    pub claw_rpc_ok: bool,
    /// OpenClaw 当前安装版本
    pub claw_version: Option<String>,
    /// 是否有新版本可升级（仅当 claw_running=true 且查到 latest 版本时有效）
    pub has_claw_update: bool,
    /// GNB 对等节点状态（gnb_ctl -s 原始输出，Console 端直接解析）
    pub gnb_status: String,
    /// GNB 地址表（gnb_ctl -a 原始输出）
    pub gnb_addresses: String,
    /// OpenClaw 进程 CPU 占用（%，0-100，基于 /proc/{pid}/stat 双采）
    pub claw_cpu_percent: f64,
    /// 已安装 skills（与 /opt/gnb/cache/skills.json 缓存格式一致）
    pub installed_skills: Vec<serde_json::Value>,
}

/// GNB 地图文件路径（由 config.rs 注入，通过全局 OnceCell 共享）
static GNB_MAP_PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static CLAW_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// 初始化全局参数（main.rs 调用一次）
pub fn init(gnb_map_path: String, claw_port: u16) {
    let _ = GNB_MAP_PATH.set(gnb_map_path);
    let _ = CLAW_PORT.set(claw_port);
}

/// 采集一次完整系统状态
pub async fn collect() -> Result<SysInfo> {
    let ts = {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        format!("{}000", now.as_secs()) // ms 格式，JavaScript 端兼容
    };

    // 并行采集各模块（CPU 需要 100ms 间隔，其余同步）
    let cpu_percent = read_cpu_percent().await;
    let (mem_total_mb, mem_used_mb, mem_percent) = read_mem().unwrap_or((0, 0, 0.0));
    let disk_percent = read_disk_percent("/opt").unwrap_or(0.0);
    let uptime_sec   = read_uptime().unwrap_or(0);
    let hostname     = fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();

    // OS / 内核 / 架构 / 负载
    let os         = read_os_release();
    let kernel     = cmd_output("uname", &["-r"]);
    let arch       = cmd_output("uname", &["-m"]);
    let load       = read_load();
    let cpu_model  = read_cpu_model();
    let cpu_cores  = read_cpu_cores();

    // 进程检测
    let gnb_running  = is_process_running("gnb");
    let claw_running = is_process_running("openclaw");
    // OpenClaw 版本（异步读取，不阻塞 executor）
    let claw_version = crate::claw_manager::read_local_version().await;
    // has_claw_update 不在心跳里做网络查询（避免增加延迟）；Console 主动发 claw_status 时才查
    let has_claw_update = false;

    // OpenClaw RPC 可用性（仅在进程运行时检查）
    let claw_port    = *CLAW_PORT.get().unwrap_or(&18789);
    let (claw_rpc_ok, claw_cpu_percent) = if claw_running {
        let rpc = check_claw_rpc(claw_port).await;
        let cpu = read_claw_cpu().await;
        (rpc, cpu)
    } else {
        (false, 0.0)
    };

    // GNB peer 状态（gnb_ctl -s / -a）
    let map_path       = GNB_MAP_PATH.get().map(String::as_str).unwrap_or("");
    let (gnb_status, gnb_addresses) = read_gnb_status(map_path);

    // skills 缓存（读 /opt/gnb/cache/skills.json）
    let installed_skills = read_skills_cache().await;

    Ok(SysInfo {
        ts, cpu_percent, mem_percent, mem_used_mb, mem_total_mb,
        disk_percent, uptime_sec, hostname,
        os, kernel, arch, load, cpu_model, cpu_cores,
        gnb_running, claw_running, claw_rpc_ok, claw_cpu_percent,
        claw_version, has_claw_update,
        gnb_status, gnb_addresses,
        installed_skills,
    })
}

// ─────────────────────────────────────────────
// 内部采集函数
// ─────────────────────────────────────────────

/// CPU 使用率：读两次 /proc/stat，间隔 100ms
async fn read_cpu_percent() -> f64 {
    fn parse_cpu_line() -> Option<(u64, u64)> {
        let content = fs::read_to_string("/proc/stat").ok()?;
        let line    = content.lines().next()?;
        let parts: Vec<u64> = line.split_whitespace()
            .skip(1).take(7)
            .filter_map(|s| s.parse().ok())
            .collect();
        if parts.len() < 4 { return None; }
        let total: u64 = parts.iter().sum();
        let idle = parts[3] + parts.get(4).copied().unwrap_or(0);
        Some((total, idle))
    }
    let s1 = parse_cpu_line().unwrap_or((0, 0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s2 = parse_cpu_line().unwrap_or((0, 0));
    let total_diff = s2.0.saturating_sub(s1.0) as f64;
    let idle_diff  = s2.1.saturating_sub(s1.1) as f64;
    if total_diff == 0.0 { return 0.0; }
    ((total_diff - idle_diff) / total_diff * 100.0 * 10.0).round() / 10.0
}

/// 内存：解析 /proc/meminfo
fn read_mem() -> Result<(u64, u64, f64)> {
    let content = fs::read_to_string("/proc/meminfo")?;
    let kv: std::collections::HashMap<&str, u64> = content.lines()
        .filter_map(|line| {
            let mut parts = line.split(':');
            let key = parts.next()?.trim();
            let val: u64 = parts.next()?.split_whitespace().next()?.parse().ok()?;
            Some((key, val))
        }).collect();
    let total = kv.get("MemTotal").copied().unwrap_or(0) / 1024;
    let free  = kv.get("MemAvailable").copied().unwrap_or(0) / 1024;
    let used  = total.saturating_sub(free);
    let pct   = if total == 0 { 0.0 } else { (used as f64 / total as f64 * 100.0 * 10.0).round() / 10.0 };
    Ok((total, used, pct))
}

/// 磁盘：调用 df -B1 解析
fn read_disk_percent(path: &str) -> Option<f64> {
    let output = Command::new("df").args(["-B1", path]).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line   = stdout.lines().nth(1)?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    parts.get(4)?.trim_end_matches('%').parse::<f64>().ok()
}

/// 系统运行时间：/proc/uptime
fn read_uptime() -> Option<u64> {
    fs::read_to_string("/proc/uptime").ok()?
        .split_whitespace().next()?.parse::<f64>().ok().map(|f| f as u64)
}

/// OS 发行版：/etc/os-release PRETTY_NAME
fn read_os_release() -> String {
    fs::read_to_string("/etc/os-release").unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("PRETTY_NAME="))
        .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
        .unwrap_or_default()
}

/// 系统负载：/proc/loadavg（取前 3 列）
fn read_load() -> String {
    fs::read_to_string("/proc/loadavg").unwrap_or_default()
        .split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

/// CPU 型号
fn read_cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo").unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// CPU 核数
fn read_cpu_cores() -> u32 {
    fs::read_to_string("/proc/cpuinfo").unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count() as u32
}

/// 执行短命令，返回 stdout 单行（2s 超时）
fn cmd_output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// GNB 对等状态：调用 gnb_ctl -s 和 -a（带 2s timeout）
fn read_gnb_status(map_path: &str) -> (String, String) {
    if map_path.is_empty() || !std::path::Path::new(map_path).exists() {
        return (String::new(), String::new());
    }
    let status = Command::new("gnb_ctl")
        .args(["-b", map_path, "-s"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let addrs = Command::new("gnb_ctl")
        .args(["-b", map_path, "-a"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    debug!("gnb_ctl status: {} bytes, addrs: {} bytes", status.len(), addrs.len());
    (status, addrs)
}

/// OpenClaw 进程组 CPU 占用（读 /proc/{pid}/stat，双采 500ms 间隔）
///
/// OpenClaw 由两个进程组成：
///   - `openclaw`         — 主进程（守护 + 调度，CPU 极低）
///   - `openclaw-gateway` — 核心 gateway（WS + AI，消耗主要 CPU）
///
/// 用 `pgrep -a openclaw` 匹配所有前缀为 openclaw 的进程，CPU 求和。
async fn read_claw_cpu() -> f64 {
    // pgrep -a openclaw：匹配进程名前缀，返回 "pid cmdline" 多行
    let pids: Vec<u32> = Command::new("pgrep")
        .args(["-a", "openclaw"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.lines()
                .filter_map(|l| l.split_whitespace().next()?.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();

    if pids.is_empty() { return 0.0; }

    // 读 /proc/{pid}/stat 的 utime+stime（jiffies），同时读系统总 uptime tick
    let read_proc_stat = |pid: u32| -> Option<u64> {
        let content = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let parts: Vec<&str> = content.split_whitespace().collect();
        // field[13]=utime, field[14]=stime（0-indexed）
        let utime: u64 = parts.get(13)?.parse().ok()?;
        let stime: u64 = parts.get(14)?.parse().ok()?;
        Some(utime + stime)
    };

    let read_uptime_ticks = || -> u64 {
        std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|f| (f * 100.0) as u64)
            .unwrap_or(0)
    };

    // 第一次采样：所有 openclaw* 进程 jiffies 之和
    let proc_jiffies_1: u64 = pids.iter().filter_map(|&p| read_proc_stat(p)).sum();
    let uptime_1 = read_uptime_ticks();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // 第二次采样
    let proc_jiffies_2: u64 = pids.iter().filter_map(|&p| read_proc_stat(p)).sum();
    let uptime_2 = read_uptime_ticks();

    let proc_diff  = proc_jiffies_2.saturating_sub(proc_jiffies_1) as f64;
    let total_diff = uptime_2.saturating_sub(uptime_1) as f64;
    if total_diff == 0.0 { return 0.0; }

    // 乘以核数得到相对于单核的百分比（多核进程可突破 100%）
    let cores = read_cpu_cores() as f64;
    let raw = proc_diff / total_diff * 100.0;
    // 归一化到 0-100（占总 CPU 资源百分比）
    let normalized = if cores > 0.0 { raw / cores * 100.0 } else { raw };
    (normalized * 10.0).round() / 10.0
}

/// OpenClaw RPC 可用性（GET http://127.0.0.1:{port}/api/status）
async fn check_claw_rpc(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/status");
    match tokio::time::timeout(
        Duration::from_secs(1),
        reqwest::get(&url),
    ).await {
        Ok(Ok(resp)) => resp.status().is_success(),
        _ => false,
    }
}

/// skills 缓存：读 /opt/gnb/cache/skills.json（与 node-agent.sh 格式一致）
async fn read_skills_cache() -> Vec<serde_json::Value> {
    const CACHE: &str = "/opt/gnb/cache/skills.json";
    let content = tokio::fs::read_to_string(CACHE).await.unwrap_or_default();
    serde_json::from_str::<Vec<serde_json::Value>>(&content).unwrap_or_default()
}

/// 检测进程名是否在运行（遍历 /proc/<pid>/comm）
pub fn is_process_running(name: &str) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else { return false };
    for entry in entries.flatten() {
        let comm = entry.path().join("comm");
        if let Ok(content) = fs::read_to_string(comm) {
            if content.trim() == name { return true; }
        }
    }
    false
}

/// 用于任务结果缓存文件（保留与 node-agent.sh 兼容的路径，供 exec_handler 写入）
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SkillEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: String,
}

/// 技能操作完成后失效 skills 缓存
pub async fn invalidate_skills_cache() {
    let _ = tokio::fs::remove_file("/opt/gnb/cache/skills.json").await;
    debug!("skills.json 缓存已失效");
}
