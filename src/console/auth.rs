//! auth.rs — Console MQTT 连接认证器 (Ed25519 签名验证)
//!
//! 注意: 此模块已完整实现但尚未被 mqtt_broker 集成调用，
//! 暂时通过 allow(dead_code) 抑制编译警告。
//!
//! 验证流程：
//!   1. MQTT CONNECT 的 username = nodeId, password = "timestamp:ed25519_sig_base64"
//!   2. 检查 timestamp 不超过 5 分钟（防止重放攻击）
//!   3. 从 GNB 密钥目录读取节点公钥（ed25519.private 文件的后 32 字节）
//!   4. 用 Ed25519 验证签名
//!
//! v3: 已从 HMAC-SHA256 对称方案迁移到 Ed25519 非对称签名。
//! Agent 端使用私钥签名，Console 端使用公钥验签。

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

/// 验证 MQTT CONNECT 凭据（Ed25519 非对称签名）
///
/// - username: nodeId
/// - password: "timestamp:ed25519_signature_base64"
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

    // 读取密钥文件（64 字节：前 32 = seed，后 32 = 公钥）
    let key_bytes = match std::fs::read(&key_path) {
        Ok(b) => b,
        Err(e) => {
            warn!("[Auth] 读取密钥失败 ({}) — 此节点可能尚未部署 GNB: {}", key_path.display(), e);
            return AuthResult::Rejected(format!("无法读取节点密钥: {}", e));
        }
    };

    // 提取公钥：优先使用文件后 32 字节（标准布局），否则从 seed 派生
    let verifying_key = if key_bytes.len() >= 64 {
        // 标准 Ed25519 密钥文件: bytes[32..64] = 公钥
        let pub_bytes: [u8; 32] = match key_bytes[32..64].try_into() {
            Ok(b) => b,
            Err(_) => return AuthResult::Rejected("公钥字节提取失败".to_string()),
        };
        match ed25519_dalek::VerifyingKey::from_bytes(&pub_bytes) {
            Ok(vk) => vk,
            Err(e) => return AuthResult::Rejected(format!("公钥解析失败: {}", e)),
        }
    } else if key_bytes.len() >= 32 {
        // 仅有 seed（32 字节），从 seed 派生公钥
        let seed: [u8; 32] = match key_bytes[..32].try_into() {
            Ok(b) => b,
            Err(_) => return AuthResult::Rejected("seed 字节提取失败".to_string()),
        };
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        signing_key.verifying_key()
    } else {
        return AuthResult::Rejected(format!("密钥文件过短 ({} bytes)", key_bytes.len()));
    };

    // 验证 Ed25519 签名
    let message = format!("{}|{}", username, ts_str);
    if verify_ed25519(&verifying_key, message.as_bytes(), sig_b64) {
        info!("[Auth] 节点 {} Ed25519 签名验证通过", username);
        AuthResult::Verified
    } else {
        warn!("[Auth] 节点 {} Ed25519 签名验证失败", username);
        AuthResult::Rejected("Ed25519 签名不匹配".to_string())
    }
}

/// Ed25519 签名验证
fn verify_ed25519(verifying_key: &ed25519_dalek::VerifyingKey, message: &[u8], sig_b64: &str) -> bool {
    use ed25519_dalek::Verifier;

    // base64 解码签名
    let sig_bytes = match base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        sig_b64,
    ) {
        Ok(b) => b,
        Err(_) => return false,
    };

    // Ed25519 签名固定 64 字节
    let sig_array: [u8; 64] = match sig_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };

    let signature = ed25519_dalek::Signature::from_bytes(&sig_array);
    verifying_key.verify(message, &signature).is_ok()
}

fn current_ts_ms() -> u64 {
    crate::util::ts_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// 生成临时密钥对并写入临时目录，模拟 GNB 密钥布局
    fn setup_test_keys(tmp_dir: &std::path::Path, gnb_node_id: &str) -> SigningKey {
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();

        // 模拟 GNB 密钥文件布局: {tmp}/{gnb_node_id}/security/ed25519.private
        let key_dir = tmp_dir.join(gnb_node_id).join("security");
        std::fs::create_dir_all(&key_dir).unwrap();

        // 标准 64 字节格式: seed(32) + pubkey(32)
        let mut key_file = Vec::with_capacity(64);
        key_file.extend_from_slice(&[42u8; 32]); // seed
        key_file.extend_from_slice(verifying_key.as_bytes());
        std::fs::write(key_dir.join("ed25519.private"), &key_file).unwrap();

        signing_key
    }

    /// 构建合法签名密码
    fn build_valid_password(signing_key: &SigningKey, node_id: &str) -> String {
        use base64::Engine;
        let timestamp = crate::util::ts_ms().to_string();
        let message = format!("{}|{}", node_id, timestamp);
        let signature = signing_key.sign(message.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        format!("{}:{}", timestamp, sig_b64)
    }

    #[test]
    fn test_valid_ed25519_auth() {
        let tmp = std::env::temp_dir().join("synon_auth_test_valid");
        let _ = std::fs::remove_dir_all(&tmp);
        let signing_key = setup_test_keys(&tmp, "1001");
        let password = build_valid_password(&signing_key, "node-abc");

        let result = verify_mqtt_credentials(
            "node-abc",
            &password,
            tmp.to_str().unwrap(),
            "1001",
        );
        assert!(matches!(result, AuthResult::Verified), "expected Verified, got {:?}", result);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_wrong_key_rejected() {
        let tmp = std::env::temp_dir().join("synon_auth_test_wrong_key");
        let _ = std::fs::remove_dir_all(&tmp);
        let _correct_key = setup_test_keys(&tmp, "1001");

        // 用不同的密钥签名
        let wrong_key = SigningKey::from_bytes(&[99u8; 32]);
        let password = build_valid_password(&wrong_key, "node-abc");

        let result = verify_mqtt_credentials(
            "node-abc",
            &password,
            tmp.to_str().unwrap(),
            "1001",
        );
        assert!(matches!(result, AuthResult::Rejected(_)), "expected Rejected, got {:?}", result);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_invalid_format() {
        let result = verify_mqtt_credentials("node-abc", "no_colon_token", "/tmp", "1001");
        assert!(matches!(result, AuthResult::Rejected(_)));
    }

    #[test]
    fn test_expired_timestamp() {
        let result = verify_mqtt_credentials("node-abc", "0:abc123sig==", "/tmp", "1001");
        assert!(matches!(result, AuthResult::Rejected(_)));
    }

    #[test]
    fn test_empty_credentials() {
        let result = verify_mqtt_credentials("", "", "/tmp", "1001");
        assert!(matches!(result, AuthResult::Rejected(_)));
    }
}
