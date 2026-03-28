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

/// 从本地 skills.json 缓存读取已安装技能列表（异步）
pub async fn list_from_cache() -> Vec<SkillInfo> {
    let path = Path::new(SKILLS_CACHE);
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        debug!("[SkillsManager] skills.json 不存在，返回空列表");
        return vec![];
    }
    match tokio::fs::read_to_string(path).await {
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

    // openclaw skills list --json 将 JSON 输出到 stderr（非 stdout），需读 stderr
    let text = String::from_utf8_lossy(&output.stderr);

    // openclaw 输出是 {workspaceDir, managedSkillsDir, skills: [...]}，非裸数组
    let skills: Vec<SkillInfo> = if let Ok(resp) = serde_json::from_str::<SkillsListResponse>(&text) {
        resp.skills
    } else {
        // 兼容旧版可能的裸数组格式
        serde_json::from_str::<Vec<SkillInfo>>(&text).unwrap_or_default()
    };

    // 写入缓存（下次心跳直接读）
    if let Ok(json) = serde_json::to_string_pretty(&skills) {
        let _ = tokio::fs::create_dir_all("/opt/gnb/cache").await;
        let _ = tokio::fs::write(SKILLS_CACHE, json).await;
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

// ─────────────────────────────────────────────────────────────
// source-aware 策略层（镜像 Console 的 skill-command.ts 逻辑）
// ─────────────────────────────────────────────────────────────

/// 按 source 类型决策安装命令并执行（Daemon 端策略自决）
///
/// # 参数
/// - `skill_id`   : 技能 ID（可带版本 `foo@1.0.0`）
/// - `source`     : 安装源类型（clawhub / openclaw / github / npm / skills.sh / openclaw-bundled）
/// - `slug`       : skills.sh / clawhub 的短名（可选）
/// - `github_repo`: github 源仓库名（可选）
pub async fn install_by_source(
    skill_id: &str,
    source: &str,
    slug: Option<&str>,
    github_repo: Option<&str>,
) -> Result<String> {
    let slug_or_id = slug.unwrap_or(skill_id);
    let repo = github_repo.unwrap_or(skill_id);

    let command = match source {
        // 平台内置：仅 enable，不需要下载
        "openclaw-bundled" => format!("openclaw plugins enable {skill_id}"),

        // clawhub 源：clawhub → openclaw plugins → skills.sh（三级回退）
        "clawhub" => format!(
            "(echo '[install] 尝试方式1: clawhub install...' && clawhub install {skill_id}) \
             || (echo '[install] 尝试方式2: openclaw plugins install...' && openclaw plugins install {skill_id}) \
             || (echo '[install] 尝试方式3: npx skills add...' && npx -y skills add {slug_or_id})"
        ),

        // github 源：openclaw github: 前缀安装
        "github" => format!("openclaw plugins install github:{repo}"),

        // openclaw 源：openclaw plugins → clawhub 回退 + 更新 allowlist
        "openclaw" => format!(
            "(echo '[install] 尝试方式1: openclaw plugins install...' && openclaw plugins install {skill_id}) \
             || (echo '[install] 尝试方式2: clawhub install...' && clawhub install {skill_id}) \
             && ALLOW=$(openclaw config get plugins.allow 2>/dev/null || echo '[]') \
             && UPDATED=$(echo \"$ALLOW\" | jq --arg p \"{skill_id}\" 'if type == \"array\" then . + [$p] | unique else [$p] end') \
             && openclaw config set plugins.allow \"$UPDATED\" --strict-json"
        ),

        // skills.sh 源：npx skills add → clawhub 回退
        "skills.sh" => format!(
            "(echo '[install] 尝试方式1: npx skills add...' && npx -y skills add {slug_or_id}) \
             || (echo '[install] 尝试方式2: clawhub install...' && clawhub install {skill_id})"
        ),

        // npm 全局包
        "npm" => format!(
            "npm install -g {skill_id} --registry=https://registry.npmmirror.com"
        ),

        // 未知 source 降级到 openclaw plugins install
        _ => {
            info!("[SkillsManager] 未知 source={source}，降级为 openclaw plugins install");
            format!("openclaw plugins install {skill_id}")
        }
    };

    info!("[SkillsManager] install_by_source source={source} skill={skill_id}");
    let result = exec_by_sh(&command, 120).await;
    if result.is_ok() {
        let _ = refresh_cache().await;
    }
    result
}

/// 按 source 类型决策卸载命令并执行
///
/// # 参数
/// - `skill_id`: 技能 ID
/// - `source`  : 安装源类型（决定卸载方式）
pub async fn uninstall_by_source(skill_id: &str, source: &str) -> Result<String> {
    let command = match source {
        "clawhub" => format!("clawhub uninstall {skill_id}"),

        "github" => format!("openclaw plugins uninstall {skill_id}"),

        "openclaw" => format!(
            "openclaw plugins uninstall {skill_id} \
             && ALLOW=$(openclaw config get plugins.allow 2>/dev/null || echo '[]') \
             && UPDATED=$(echo \"$ALLOW\" | jq --arg p \"{skill_id}\" \
                'if type == \"array\" then [.[] | select(. != $p)] else [] end') \
             && openclaw config set plugins.allow \"$UPDATED\" --strict-json"
        ),

        // openclaw-bundled / 未知 source：disable
        _ => format!("openclaw plugins disable {skill_id}"),
    };

    info!("[SkillsManager] uninstall_by_source source={source} skill={skill_id}");
    let result = exec_by_sh(&command, 60).await;
    if result.is_ok() {
        let _ = refresh_cache().await;
    }
    result
}

/// 用 `sh -c` 执行 Shell 命令，支持 `||` 复合链
///
/// 仅供 install_by_source / uninstall_by_source 内部调用，不对外暴露。
/// 超时单位：秒。
async fn exec_by_sh(command: &str, timeout_secs: u64) -> Result<String> {
    debug!("[SkillsManager] exec_by_sh: {command}");
    let output = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        Command::new("sh").arg("-c").arg(command).output(),
    )
    .await
    .context(format!("命令超时 ({timeout_secs}s)"))?
    .context("sh -c 执行失败")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        let code = output.status.code().unwrap_or(-1);
        Err(anyhow::anyhow!(
            "命令失败 (exit={code}):\nstdout: {stdout}\nstderr: {stderr}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_from_cache_missing_file() {
        // skills.json 不存在（开发机）应返回空 Vec 不 panic
        let skills = list_from_cache().await;
        // 可能为空（开发机没有 /opt/gnb/cache/skills.json），但不应 panic
        let _ = skills;
    }

    #[test]
    fn test_parse_skills_json_valid() {
        let json = r#"[{"name":"测试技能","description":"一个测试技能","source":"npm"}]"#;
        let skills: Vec<SkillInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "测试技能");
        assert_eq!(skills[0].source, "npm");
    }

    #[test]
    fn test_parse_skills_json_empty_array() {
        let skills: Vec<SkillInfo> = serde_json::from_str("[]").unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_parse_skills_json_minimal_fields() {
        // 只有 name 必选，其他有默认值
        let json = r#"[{"name":"s1"}]"#;
        let skills: Vec<SkillInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(skills[0].name, "s1");
        assert!(!skills[0].disabled); // default false
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
