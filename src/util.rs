//! util.rs — 项目公共工具函数
//!
//! 消除跨模块重复定义（ts_ms / chrono_ts_ms / current_ts_ms → 统一为 ts_ms）。

/// 当前毫秒时间戳（ms since epoch）
pub fn ts_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
