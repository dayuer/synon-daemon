# 并发任务执行 + 串行控制

> 复杂度: **L** — 重构 console_ws 事件循环架构，新增独立模块

## 问题

`console_ws::handle_server_message()` 是同步 `await` — 一个 `skill_install`(120s) 执行期间：
1. 心跳**发送被阻塞**（WS write half 被占用）
2. 后续消息全部排队等待
3. Console 端可能误判节点离线

## 设计：Actor + Semaphore 模型

```
Console WSS
    │
    ├── WS read loop（轻量派发，不 await 耗时操作）
    │     └── 收到 exec_cmd / skill_install / claw_restart...
    │           ├── 快速消息（pong/claw_rpc/route_update） → 就地 await
    │           └── 耗时消息 → 入 task_tx channel → 串行执行器 task
    │
    ├── WS write loop（独立 task，消费 resp_rx）
    │     └── 收到 heartbeat / claw_event / alert / cmd_result → send to WS
    │
    └── 串行执行器（独立 task，消费 task_rx）
          ├── Semaphore(1)：同一时间只执行一个 Console 下发任务
          ├── 去重：reqId 在 in-flight Set 中 → 拒绝
          └── 结果通过 resp_tx → WS write loop
```

### 消息分类表

| 消息类型 | 耗时 | 处理方式 |
|----------|------|----------|
| `pong` | 0ms | 就地处理 |
| `claw_rpc` | <100ms | 就地 await |
| `route_update` | <10ms | 就地 await |
| `key_rotate` | <10ms | 就地处理 |
| `claw_status` | <100ms | 就地 await |
| `exec_cmd` | **≤60s** | 串行执行器 |
| `skill_install` | **≤120s** | 串行执行器 |
| `skill_uninstall` | **≤60s** | 串行执行器 |
| `skill_update` | **≤120s** | 串行执行器 |
| `claw_upgrade` | **≤120s** | 串行执行器 |
| `claw_restart` | **≤10s** | 串行执行器 |
| `deploy_file` | <1s | 就地处理 |

### 防重入机制

```rust
// 串行执行器内部
struct TaskExecutor {
    in_flight: HashSet<String>,  // reqId 去重
    // Semaphore(1) 保证同时只有一个任务在执行
}
```

Console 下发同一 `reqId` 的重复消息 → 直接返回 `{ ok: false, stderr: "任务已在执行中" }`。

### WS Write 解耦

当前 `write` half 被 select! 的多个分支共享，耗时 await 期间阻塞其他分支写入。

**解法**：将 write half 移入独立 task，所有需要写 WS 的操作通过 `resp_tx: mpsc::Sender<String>` 投递：

```rust
// write loop
while let Some(msg) = resp_rx.recv().await {
    if write.send(Message::Text(msg)).await.is_err() { break; }
}
```

这样心跳、claw_event、alert、cmd_result 全部通过 channel 写入，互不阻塞。

## 变更清单

### [NEW] `src/task_executor.rs`
- `TaskExecutor` struct：串行执行器
- `TaskMessage` enum：待执行任务（exec_cmd / skill / claw 操作）
- `run()` 消费 task_rx，串行 await 执行，结果写入 resp_tx
- `HashSet<reqId>` 防重入

### [MODIFY] `src/console_ws.rs`
- 拆分 `connect_and_run`：WS read loop + WS write loop + task executor
- `handle_server_message` 改为非阻塞派发（耗时操作 → task_tx.send）
- 快速消息保持就地 await

### [MODIFY] `src/main.rs`
- 新增 `mod task_executor;`

## 验证计划

### 自动测试
```bash
cargo test 2>&1
cargo clippy -- -D warnings 2>&1
```

### 手动验证
1. 同时下发 skill_install + exec_cmd → 确认串行执行（非并行）
2. 耗时任务执行期间 → 心跳正常发送不中断
3. 重复 reqId → 返回 "已在执行中"
