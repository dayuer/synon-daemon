//! auth.rs — Console MQTT 连接认证器 (HMAC-SHA256 签名验证)
//!
//! 验证流程：
//!   1. MQTT CONNECT 的 username = nodeId, password = "timestamp:hmac_base64"
//!   2. 检查 timestamp 不超过 5 分钟（防止重放攻击）
//!   3. 从 GNB 密钥目录读取节点私钥
//!   4. 用 HMAC-SHA256 验证签名
//!
//! 注意：当前使用 HMAC-SHA256 对称方案作为过渡。
//! 后续迁移到真正的 Ed25519 非对称签名需引入 ed25519-dalek。

use std::path::PathBuf;
use tracing::{info, warn};

/// MQTT 认证结果
#[derive(Debug)]
pub enum AuthResult {
    /// 签名验证通过
    Verified,
    /// 认证失败
    Rejected(String),
}

/// 签名认证参数
const MAX_TIMESTAMP_DRIFT_MS: u64 = 5 * 60 * 1000; // 5 分钟容忍窗口

/// 验证 MQTT CONNECT 凭据
///
/// - username: nodeId
/// - password: "timestamp:hmac_base64"
/// - gnb_keys_base_dir: GNB 密钥根目录 (如 /opt/gnb/conf/)
/// - gnb_node_id: 该节点的 GNB 节点 ID
pub fn verify_mqtt_credentials(
    username: &str,
    password: &str,
    gnb_keys_base_dir: &str,
    gnb_node_id: &str,
) -> AuthResult {
    if username.is_empty() || password.is_empty() {
        return AuthResult::Rejected("空用户名或密码".to_string());
    }

    // 解析签名格式: "timestamp:base64_signature"
    let Some(colon_pos) = password.find(':') else {
        return AuthResult::Rejected("密码格式错误：缺少 timestamp:signature 分隔符".to_string());
    };

    let ts_str = &password[..colon_pos];
    let sig_b64 = &password[colon_pos + 1..];

    // 解析 timestamp
    let timestamp: u64 = match ts_str.parse() {
        Ok(t) => t,
        Err(_) => {
            return AuthResult::Rejected(format!("无法解析 timestamp: {}", ts_str));
        }
    };

    // 时间戳漂移检查
    let now = current_ts_ms();
    if now.saturating_sub(timestamp) > MAX_TIMESTAMP_DRIFT_MS {
        return AuthResult::Rejected(format!("签名已过期 (drift={}ms)", now - timestamp));
    }

    // 构造密钥路径: {gnb_keys_base_dir}/{gnb_node_id}/security/ed25519.private
    if gnb_node_id.is_empty() {
        return AuthResult::Rejected(format!("节点 {} 无对应 GNB ID", username));
    }
    let key_path = PathBuf::from(gnb_keys_base_dir)
        .join(gnb_node_id)
        .join("security")
        .join("ed25519.private");

    // 读取密钥
    let key_bytes = match std::fs::read(&key_path) {
        Ok(b) => b,
        Err(e) => {
            warn!("[Auth] 读取密钥失败 ({}) — 此节点可能尚未部署 GNB: {}", key_path.display(), e);
            return AuthResult::Rejected(format!("无法读取节点密钥: {}", e));
        }
    };

    if key_bytes.len() < 32 {
        return AuthResult::Rejected(format!("密钥文件过短 ({} bytes)", key_bytes.len()));
    }

    // 验证 HMAC-SHA256
    let hmac_key = &key_bytes[..32];
    let message = format!("{}|{}", username, ts_str);

    if verify_hmac(hmac_key, message.as_bytes(), sig_b64) {
        info!("[Auth] 节点 {} 签名验证通过", username);
        AuthResult::Verified
    } else {
        warn!("[Auth] 节点 {} 签名验证失败", username);
        AuthResult::Rejected("HMAC 签名不匹配".to_string())
    }
}

/// HMAC-SHA256 验证
fn verify_hmac(key: &[u8], message: &[u8], expected_b64: &str) -> bool {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<sha2::Sha256>;

    let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(key) else {
        return false;
    };
    Mac::update(&mut mac, message);

    let Ok(expected_bytes) = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        expected_b64,
    ) else {
        return false;
    };

    // 常量时间比较
    mac.verify_slice(&expected_bytes).is_ok()
}

fn current_ts_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_format() {
        // 无 ":" 分隔符 → Rejected
        let result = verify_mqtt_credentials("node-abc", "no_colon_token", "/tmp", "1001");
        assert!(matches!(result, AuthResult::Rejected(_)));
    }

    #[test]
    fn test_expired_timestamp() {
        // timestamp = 0 → 过期
        let result = verify_mqtt_credentials("node-abc", "0:abc123sig==", "/tmp", "1001");
        assert!(matches!(result, AuthResult::Rejected(_)));
    }

    #[test]
    fn test_empty_credentials() {
        let result = verify_mqtt_credentials("", "", "/tmp", "1001");
        assert!(matches!(result, AuthResult::Rejected(_)));
    }
}
