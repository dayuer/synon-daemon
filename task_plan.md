# Synon-Daemon 全量文件审计与优化计划

> 根据用户指令，对 `synon-daemon` 实施逐文件检查（完善注释、检查风险、优化能力），并严格遵守 `/tdd` 工作流。

## Task 规划

- [x] Task 1: `main.rs` & `config.rs`
  - 范围: 启动入口、配置加载宏观逻辑
  - 验证: `cargo clippy -- -D warnings 2>&1`
  - 复杂度: standard
  - 风险: 🟢 LOW
- [x] Task 2: `watchdog.rs` & `self_updater.rs`
  - 范围: 进程保活机制、OTA 自动更新
  - 验证: `cargo clippy -- -D warnings 2>&1`
  - 复杂度: standard
  - 风险: 🟡 MED (涉及进程重启和文件替换安全)
- [x] Task 3: `gnb_controller.rs` & `gnb_monitor.rs`
  - 范围: GNB 核心数据面交互与监控解析
  - 验证: `cargo test 2>&1`
  - 复杂度: standard
  - 风险: 🟢 LOW
- [x] Task 4: `claw_manager.rs` & `claw_proxy.rs`
  - 范围: OpenClaw 的本地管理和 WS 连接代理
  - 验证: `cargo clippy -- -D warnings 2>&1`
  - 复杂度: complex
  - 风险: 🟡 MED (涉及多重并发请求和泄漏预防)
- [x] Task 5: `exec_handler.rs` & `skills_manager.rs`
  - 范围: 系统命令执行、AI 技能全生命周期管理
  - 验证: `cargo clippy -- -D warnings 2>&1`
  - 复杂度: complex
  - 风险: 🟡 MED (技能安装涉及命令注入及防风暴)
- [x] Task 6: `task_executor.rs`
  - 范围: 全局单并发的串行任务执行器
  - 验证: `cargo clippy -- -D warnings 2>&1`
  - 复杂度: standard
  - 风险: 🟢 LOW (仅内存状态机)
- [x] Task 7: `console_ws.rs` & `heartbeat.rs`
  - 范围: Console 的 WSS 控制面核心大管家
  - 验证: `cargo clippy -- -D warnings 2>&1`
  - 复杂度: complex
  - 风险: 🟡 MED (协议解析、连接泄漏)
