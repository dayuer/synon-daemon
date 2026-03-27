# synon-daemon 架构审计 — Linux Daemon 标准合规报告

> 审计时间: 2026-03-27 | 审计范围: synon-daemon 全部 12 个 Rust 源文件 + systemd unit

## 审计基准 (Linux Daemon 标准)

| # | 标准 | 当前状态 | 严重度 |
|---|------|---------|--------|
| D1 | **SIGTERM 优雅关闭** | ❌ 缺失 | 🔴 HIGH |
| D2 | **SIGHUP 配置热重载** | ❌ 缺失 | 🟡 MED |
| D3 | **PID 文件管理** | ❌ 缺失（systemd 管理可忽略） | 🟢 LOW |
| D4 | **systemd Watchdog 集成** | ❌ 缺失 | 🟡 MED |
| D5 | **sd_notify(READY=1)** | ❌ 缺失 | 🟡 MED |
| D6 | **自更新后优雅重启** | ⚠️ 发 SIGTERM 给自己但无 handler | 🔴 HIGH |
| D7 | **子任务 CancellationToken** | ❌ tokio::spawn 无 abort handle | 🟡 MED |
| D8 | **结构化日志 (journald)** | ✅ tracing + systemd 自动捕获 | 🟢 PASS |
| D9 | **指数退避重连** | ✅ console_ws 已实现 | 🟢 PASS |
| D10 | **命令安全白名单/黑名单** | ✅ exec_handler.rs 完整 | 🟢 PASS |
| D11 | **进程监控 + 自动重启** | ✅ watchdog.rs 完整 | 🟢 PASS |
| D12 | **TCP Keepalive** | ✅ socket2 keepalive 已配置 | 🟢 PASS |

---

## 问题详解

### D1 + D6: 无 SIGTERM handler — 致命

**现状**: `main.rs` 使用 `#[tokio::main] async fn main()` 直接启动，无任何信号处理。
`self_updater.rs` L149 调用 `libc::kill(getpid(), SIGTERM)` 期望优雅退出，但没有 handler 接收 — **进程被操作系统直接杀死**，导致：
- WS 连接不发 close frame → Console 端 10s 后才检测到断连
- 正在执行的 `exec_handler` 子进程成为孤儿进程
- 看门狗 / 心跳定时器无法清理

**修复**: 在 main.rs 使用 `tokio::signal::unix::signal(SIGTERM)` 注册 handler，通过 `CancellationToken` 通知所有子任务优雅退出。

### D2: 无 SIGHUP 配置热重载

**现状**: daemon 运行后若 `agent.conf` 变更，必须重启进程。
有趣的是 `gnb_controller.rs` 正确使用了 SIGHUP 重载 GNB 配置，但 daemon 自身不支持。

**修复**: 注册 SIGHUP handler → 重新加载 `DaemonConfig` → 更新运行时配置（Console URL、claw_port 等）。

### D4 + D5: 缺少 systemd Watchdog 集成

**现状 (initnode.sh L421-436)**:
```ini
[Service]
Type=simple
Restart=always
RestartSec=5
```

缺少 `WatchdogSec` 和 `Type=notify` — systemd 无法检测到 daemon 内部死锁（进程存活但 WS 断开不重连等场景）。

**修复**:
1. Cargo.toml 新增 `libsystemd` 依赖
2. 启动完成后发 `sd_notify(READY=1)`
3. 心跳循环中周期性发 `sd_notify(WATCHDOG=1)`
4. systemd unit 改为 `Type=notify` + `WatchdogSec=30`

### D7: 子任务无取消机制

**现状**: `main.rs` 用 `tokio::spawn` 启动 watchdog、self_updater、skills 刷新，但无 `JoinHandle` 或 `CancellationToken` — 优雅关闭时无法通知它们停止。

**修复**: 引入 `tokio_util::sync::CancellationToken`，所有子任务通过 `select!` 监听 token 取消。

---

## 复杂度分级: **L**（架构变更 — 重构 main.rs 生命周期管理 + 新依赖）

## 变更清单

### Phase 1: 信号处理 + 优雅关闭 (D1 + D6 + D7)

#### [MODIFY] `src/main.rs`
- 引入 `CancellationToken` 全局关闭令牌
- 注册 SIGTERM/SIGINT handler
- 所有 `tokio::spawn` 传入 token.clone()
- 关闭时等待子任务完成（5s 超时后 force abort）

#### [MODIFY] `src/console_ws.rs`
- `run()` 接收 `CancellationToken` 参数
- 重连循环用 `select!` 监听 token
- 断开时发 WS close frame

#### [MODIFY] `src/watchdog.rs`
- `run()` 接收 `CancellationToken`
- 主循环用 `select!` 监听 token

#### [MODIFY] `src/self_updater.rs`
- `run()` 接收 `CancellationToken`
- 删除 `unsafe { libc::kill }` 代码
- 改为 `token.cancel()` 触发优雅关闭 → systemd `Restart=always` 自动拉起

### Phase 2: SIGHUP 配置热重载 (D2)

#### [MODIFY] `src/main.rs`
- 注册 SIGHUP handler
- 重载 `DaemonConfig::load()` → 更新运行时配置

#### [MODIFY] `src/config.rs`
- `DaemonConfig` 使用 `Arc<RwLock<>>` 包装可热更新字段

### Phase 3: systemd Watchdog 集成 (D4 + D5)

#### [MODIFY] `Cargo.toml`
- 新增 `libsystemd` 或 `sd-notify` 依赖

#### [MODIFY] `src/main.rs`
- 启动完成后发 `sd_notify("READY=1")`
- 心跳循环发 `sd_notify("WATCHDOG=1")`

#### [MODIFY] `scripts/initnode.sh` L421-436
- systemd unit 改为 `Type=notify` + `WatchdogSec=30`

## 验证计划

### 自动测试
```bash
cargo test 2>&1
cargo clippy -- -D warnings 2>&1
```

### 手动验证
1. `systemctl stop synon-daemon` → 观察 daemon 日志确认优雅退出
2. 修改 `agent.conf` → `kill -HUP $(pidof synon-daemon)` → 确认配置重载
3. 触发自更新 → 确认 daemon 优雅退出 + systemd 拉起新版本
4. `journalctl -u synon-daemon` 确认 sd_notify 状态上报
