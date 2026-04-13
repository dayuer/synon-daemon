//! task_dedup.rs — 持久化幂等去重存储
//!
//! 已完成的 taskId 持久化到磁盘，防止 Agent 重启后重复执行。
//! 采用 JSON 文件存储 + 滚动淘汰（FIFO），轻量且无外部依赖。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use tracing::{debug, info, warn};

const DEDUP_FILE: &str = "/var/lib/synon-daemon/completed_tasks.json";
/// 滚动窗口上限（~200KB JSON — 足够 Agent 级别工作负载）
const MAX_ENTRIES: usize = 5000;

/// 持久化的已完成任务 ID 集合
///
/// 内部用 `Vec` 保持插入顺序，确保滚动淘汰时 FIFO（移除最早插入的记录）。
/// `contains` 查找通过 HashSet 加速，两者保持同步。
#[derive(Serialize, Deserialize, Default)]
pub struct TaskDedup {
    /// 保持插入顺序的 ID 列表（用于 FIFO 淘汰）
    #[serde(alias = "completed")]
    ordered: Vec<String>,
    /// 快速查找索引（不持久化，启动时从 ordered 重建）
    #[serde(skip)]
    index: HashSet<String>,
}

impl TaskDedup {
    /// 从磁盘加载已完成任务集合（文件不存在则初始化空集）
    pub fn load() -> Self {
        let path = Path::new(DEDUP_FILE);
        if !path.exists() {
            // 确保父目录存在
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            info!("[TaskDedup] 无历史记录，初始化空幂等集");
            return TaskDedup::default();
        }

        match std::fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str::<TaskDedup>(&content) {
                Ok(mut dedup) => {
                    // 重建 HashSet 索引（skip 序列化的字段需要重建）
                    dedup.index = dedup.ordered.iter().cloned().collect();
                    info!("[TaskDedup] 加载 {} 条已完成任务记录", dedup.ordered.len());
                    dedup
                }
                Err(e) => {
                    warn!("[TaskDedup] JSON 解析失败 ({}), 重置幂等集", e);
                    TaskDedup::default()
                }
            },
            Err(e) => {
                warn!("[TaskDedup] 读取文件失败 ({}), 重置幂等集", e);
                TaskDedup::default()
            }
        }
    }

    /// 检查任务是否已执行过
    pub fn is_completed(&self, task_id: &str) -> bool {
        self.index.contains(task_id)
    }

    /// 标记任务已完成（立即持久化到磁盘）
    pub fn mark_completed(&mut self, task_id: &str) {
        if self.index.contains(task_id) {
            return; // 已存在，跳过
        }

        self.ordered.push(task_id.to_string());
        self.index.insert(task_id.to_string());

        // 滚动淘汰：超过上限时移除最早的一半（FIFO）
        if self.ordered.len() > MAX_ENTRIES {
            let half = self.ordered.len() / 2;
            let removed: Vec<String> = self.ordered.drain(..half).collect();
            for id in &removed {
                self.index.remove(id);
            }
            debug!(
                "[TaskDedup] 滚动淘汰：移除最早 {} 条 → 剩余 {} 条",
                removed.len(),
                self.ordered.len()
            );
        }

        self.flush();
    }

    /// 当前记录数量
    pub fn len(&self) -> usize {
        self.ordered.len()
    }

    /// 持久化到磁盘（原子写入：先写 tmp 再 rename）
    fn flush(&self) {
        match serde_json::to_string(&self) {
            Ok(json) => {
                let tmp_path = format!("{}.tmp", DEDUP_FILE);
                if let Err(e) = std::fs::write(&tmp_path, &json) {
                    warn!("[TaskDedup] 写临时文件失败: {}", e);
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, DEDUP_FILE) {
                    warn!("[TaskDedup] rename 失败: {}", e);
                }
            }
            Err(e) => warn!("[TaskDedup] 序列化失败: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_dedup() {
        let mut dedup = TaskDedup::default();
        assert!(!dedup.is_completed("task-1"));
        dedup.mark_completed("task-1");
        assert!(dedup.is_completed("task-1"));
        assert!(!dedup.is_completed("task-2"));
    }

    #[test]
    fn test_fifo_eviction() {
        let mut dedup = TaskDedup::default();
        // 插入 MAX_ENTRIES + 100 条
        for i in 0..MAX_ENTRIES + 100 {
            dedup.mark_completed(&format!("task-{}", i));
        }
        // 早期任务应被淘汰
        assert!(!dedup.is_completed("task-0"));
        assert!(!dedup.is_completed("task-100"));
        // 后期任务应保留
        assert!(dedup.is_completed(&format!("task-{}", MAX_ENTRIES + 99)));
        // 总量不应超过 MAX_ENTRIES
        assert!(dedup.len() <= MAX_ENTRIES);
    }

    #[test]
    fn test_idempotent_mark() {
        let mut dedup = TaskDedup::default();
        dedup.mark_completed("task-1");
        assert_eq!(dedup.len(), 1);
        dedup.mark_completed("task-1"); // 重复标记不应增加
        assert_eq!(dedup.len(), 1);
    }
}
