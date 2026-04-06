//! task_dedup.rs — 持久化幂等去重存储
//!
//! 已完成的 taskId 持久化到磁盘，防止 Agent 重启后重复执行。
//! 采用 JSON 文件存储 + 滚动淘汰，轻量且无外部依赖。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use tracing::{debug, info, warn};

const DEDUP_FILE: &str = "/var/lib/synon-daemon/completed_tasks.json";
/// 滚动窗口上限（~200KB JSON — 足够 Agent 级别工作负载）
const MAX_ENTRIES: usize = 5000;

/// 持久化的已完成任务 ID 集合
#[derive(Serialize, Deserialize, Default)]
pub struct TaskDedup {
    completed: HashSet<String>,
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
                Ok(dedup) => {
                    info!("[TaskDedup] 加载 {} 条已完成任务记录", dedup.completed.len());
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
        self.completed.contains(task_id)
    }

    /// 标记任务已完成（立即持久化到磁盘）
    pub fn mark_completed(&mut self, task_id: &str) {
        self.completed.insert(task_id.to_string());

        // 滚动淘汰：超过上限时保留最近一半
        if self.completed.len() > MAX_ENTRIES {
            let half = self.completed.len() / 2;
            let to_keep: Vec<String> = self
                .completed
                .iter()
                .skip(half)
                .cloned()
                .collect();
            self.completed = to_keep.into_iter().collect();
            debug!(
                "[TaskDedup] 滚动淘汰：{} → {} 条",
                MAX_ENTRIES,
                self.completed.len()
            );
        }

        self.flush();
    }

    /// 当前记录数量
    pub fn len(&self) -> usize {
        self.completed.len()
    }

    /// 持久化到磁盘
    fn flush(&self) {
        match serde_json::to_string(&self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(DEDUP_FILE, json) {
                    warn!("[TaskDedup] 持久化失败: {}", e);
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
        dedup.completed.insert("task-1".to_string());
        assert!(dedup.is_completed("task-1"));
        assert!(!dedup.is_completed("task-2"));
    }

    #[test]
    fn test_rolling_eviction() {
        let mut dedup = TaskDedup::default();
        // 插入超过 MAX_ENTRIES 条
        for i in 0..MAX_ENTRIES + 100 {
            dedup.completed.insert(format!("task-{}", i));
        }
        assert!(dedup.completed.len() > MAX_ENTRIES);
        dedup.mark_completed("trigger-eviction");
        assert!(dedup.completed.len() <= MAX_ENTRIES);
    }
}
