//! heartbeat.rs — 系统状态采集（CPU / 内存 / 磁盘 / 进程）
//! 通过读取 Linux /proc 伪文件系统，Zero-dependency 采集。

use anyhow::Result;
use serde::Serialize;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

/// 上报给 Console 的系统状态快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SysInfo {
    /// 时间戳（ISO 8601）
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
    /// gnb 进程是否运行
    pub gnb_running: bool,
    /// openclaw 进程是否运行
    pub claw_running: bool,
}

/// 采集一次系统状态
pub async fn collect() -> Result<SysInfo> {
    let ts = {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        format_iso8601(now.as_secs())
    };

    let cpu_percent = read_cpu_percent().await;
    let (mem_total_mb, mem_used_mb, mem_percent) = read_mem()?;
    let disk_percent = read_disk_percent("/opt").unwrap_or(0.0);
    let uptime_sec = read_uptime().unwrap_or(0);
    let hostname = fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();

    let gnb_running = is_process_running("gnb");
    let claw_running = is_process_running("openclaw");

    Ok(SysInfo {
        ts, cpu_percent, mem_percent, mem_used_mb, mem_total_mb,
        disk_percent, uptime_sec, hostname, gnb_running, claw_running,
    })
}

/// CPU 使用率：读两次 /proc/stat，间隔 100ms
async fn read_cpu_percent() -> f64 {
    fn parse_cpu_line() -> Option<(u64, u64)> {
        let content = fs::read_to_string("/proc/stat").ok()?;
        let line = content.lines().next()?;
        let parts: Vec<u64> = line.split_whitespace()
            .skip(1)
            .take(7)
            .filter_map(|s| s.parse().ok())
            .collect();
        if parts.len() < 4 { return None; }
        let total: u64 = parts.iter().sum();
        let idle = parts[3] + parts.get(4).copied().unwrap_or(0);
        Some((total, idle))
    }

    let s1 = parse_cpu_line().unwrap_or((0, 0));
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
        })
        .collect();

    let total = kv.get("MemTotal").copied().unwrap_or(0) / 1024;
    let free  = kv.get("MemAvailable").copied().unwrap_or(0) / 1024;
    let used  = total.saturating_sub(free);
    let pct   = if total == 0 { 0.0 } else { (used as f64 / total as f64 * 100.0 * 10.0).round() / 10.0 };
    Ok((total, used, pct))
}

/// 磁盘：调用 df 命令解析（避免 nix statvfs API 版本问题）
fn read_disk_percent(path: &str) -> Option<f64> {
    use std::process::Command;
    let output = Command::new("df")
        .args(["-B1", path])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // df 输出第二行: Filesystem 1B-blocks Used Available Use% Mounted
    let line = stdout.lines().nth(1)?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    // Use% 字段（第5列），格式为 "XX%"
    let pct_str = parts.get(4)?.trim_end_matches('%');
    pct_str.parse::<f64>().ok()
}

/// 系统运行时间：读 /proc/uptime
fn read_uptime() -> Option<u64> {
    let content = fs::read_to_string("/proc/uptime").ok()?;
    content.split_whitespace().next()?.parse::<f64>().ok().map(|f| f as u64)
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

/// Unix 时间戳转 ISO 8601（不依赖 chrono）
fn format_iso8601(secs: u64) -> String {
    // 简单实现：直接返回 Unix 时间戳字符串，JavaScript 端解析
    format!("{secs}000") // ms
}
