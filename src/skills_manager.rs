//! skills_manager.rs — Skills 生命周期管理
//!
//! 职责：
//!   - 列出已安装技能（从 /opt/gnb/cache/skills.json）
//!   - 安装技能（openclaw skills install <id>[@version]）
//!   - 卸载技能（openclaw skills uninstall <id>）
//!   - 更新技能（openclaw skills update <id>）
//!   - 刷新 skills.json 缓存

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info};

const SKILLS_CACHE: &str = "/opt/gnb/cache/skills.json";

/// 单条技能信息（对齐 openclaw skills list --json 输出格式）
/// 真实格式：{workspaceDir, managedSkillsDir, skills: [...]}
/// 每条 skill: {name, description, emoji, eligible, disabled, bundled, source, ...}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
    /// 技能名称（openclaw 用 name 作为唯一 ID，无单独 id 字段）
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub emoji: String,
    /// 是否满足依赖（bins/env/config 全部就绪）
    #[serde(default)]
    pub eligible: bool,
    #[serde(default)]
    pub disabled: bool,
    /// 是否内置技能（openclaw-bundled）
    #[serde(default)]
    pub bundled: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub homepage: String,
}

/// 从本地 skills.json 缓存读取已安装技能列表（秒级，无 IO 等待）
pub fn list_from_cache() -> Vec<SkillInfo> {
    let path = Path::new(SKILLS_CACHE);
    if !path.exists() {
        debug!("[SkillsManager] skills.json 不存在，返回空列表");
        return vec![];
    }
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(e) => {
            debug!("[SkillsManager] 读取 skills.json 失败: {e}");
            vec![]
        }
    }
}

/// openclaw skills list --json 的外层响应结构
#[derive(Debug, Deserialize)]
struct SkillsListResponse {
    #[serde(default)]
    skills: Vec<SkillInfo>,
    // workspaceDir / managedSkillsDir 忽略
}

/// 通过 openclaw CLI 查询已安装技能（实时），并刷新缓存
pub async fn refresh_cache() -> Result<Vec<SkillInfo>> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("openclaw")
            .args(["skills", "list", "--json"])
            .output(),
    )
    .await
    .context("技能列表查询超时")?
    .context("openclaw skills list 执行失败")?;

    let text = String::from_utf8_lossy(&output.stdout);

    // openclaw 输出是 {workspaceDir, managedSkillsDir, skills: [...]}，非裸数组
    let skills: Vec<SkillInfo> = if let Ok(resp) = serde_json::from_str::<SkillsListResponse>(&text) {
        resp.skills
    } else {
        // 兼容旧版可能的裸数组格式
        serde_json::from_str::<Vec<SkillInfo>>(&text).unwrap_or_default()
    };

    // 写入缓存（下次心跳直接读）
    if let Ok(json) = serde_json::to_string_pretty(&skills) {
        let _ = std::fs::create_dir_all("/opt/gnb/cache");
        let _ = std::fs::write(SKILLS_CACHE, json);
        debug!("[SkillsManager] skills.json 已刷新，共 {} 个", skills.len());
    }

    Ok(skills)
}

/// 安装技能
///
/// - `skill_id`: 技能 ID，可带版本号如 `my-skill@1.0.0`
pub async fn install(skill_id: &str) -> Result<String> {
    info!("[SkillsManager] 安装技能: {skill_id}");
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        Command::new("openclaw")
            .args(["skills", "install", skill_id])
            .output(),
    )
    .await
    .context("安装超时 (120s)")?
    .context("openclaw skills install 失败")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        // 安装成功后刷新缓存（不阻塞，忽略错误）
        let _ = refresh_cache().await;
        Ok(stdout)
    } else {
        Err(anyhow::anyhow!("安装失败:\nstdout: {stdout}\nstderr: {stderr}"))
    }
}

/// 卸载技能
pub async fn uninstall(skill_id: &str) -> Result<String> {
    info!("[SkillsManager] 卸载技能: {skill_id}");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new("openclaw")
            .args(["skills", "uninstall", skill_id])
            .output(),
    )
    .await
    .context("卸载超时")?
    .context("openclaw skills uninstall 失败")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        let _ = refresh_cache().await;
        Ok(stdout)
    } else {
        Err(anyhow::anyhow!("卸载失败:\nstdout: {stdout}\nstderr: {stderr}"))
    }
}

/// 更新技能到最新版
pub async fn update(skill_id: &str) -> Result<String> {
    info!("[SkillsManager] 更新技能: {skill_id}");
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        Command::new("openclaw")
            .args(["skills", "update", skill_id])
            .output(),
    )
    .await
    .context("更新超时")?
    .context("openclaw skills update 失败")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        let _ = refresh_cache().await;
        Ok(stdout)
    } else {
        Err(anyhow::anyhow!("更新失败:\nstdout: {stdout}\nstderr: {stderr}"))
    }
}

/// 构造安装命令字符串（供 exec_handler 白名单校验，预留 Console 端调用）
#[allow(dead_code)]
pub fn install_command(skill_id: &str) -> String {
    format!("openclaw skills install {skill_id}")
}

/// 构造卸载命令字符串（预留 Console 端调用）
#[allow(dead_code)]
pub fn uninstall_command(skill_id: &str) -> String {
    format!("openclaw skills uninstall {skill_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_from_cache_missing_file() {
        // skills.json 不存在（开发机）应返回空 Vec 不 panic
        let skills = list_from_cache();
        // 可能为空（开发机没有 /opt/gnb/cache/skills.json），但不应 panic
        let _ = skills;
    }

    #[test]
    fn test_parse_skills_json_valid() {
        let json = r#"[{"id":"test-skill","name":"测试技能","version":"1.0.0","enabled":true,"source":"npm"}]"#;
        let skills: Vec<SkillInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "test-skill");
        assert_eq!(skills[0].version, "1.0.0");
        assert!(skills[0].enabled);
    }

    #[test]
    fn test_parse_skills_json_empty_array() {
        let skills: Vec<SkillInfo> = serde_json::from_str("[]").unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_parse_skills_json_minimal_fields() {
        // 只有必选字段，enabled/source 有默认值
        let json = r#"[{"id":"s1","name":"S1","version":"0.1.0"}]"#;
        let skills: Vec<SkillInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(skills[0].id, "s1");
        assert!(!skills[0].enabled); // default false
        assert_eq!(skills[0].source, ""); // default ""
    }

    #[test]
    fn test_install_command_format() {
        let cmd = install_command("my-skill@1.0.0");
        assert_eq!(cmd, "openclaw skills install my-skill@1.0.0");
    }

    #[test]
    fn test_uninstall_command_format() {
        let cmd = uninstall_command("my-skill");
        assert_eq!(cmd, "openclaw skills uninstall my-skill");
    }
}
