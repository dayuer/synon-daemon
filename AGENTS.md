# AGENTS.md — Synon-daemon 架构指针

> 四象限防幻觉屏障：Reference

此文件是 `synon-daemon` (Rust) 项目的架构指针与导航路牌。

## 1. 核心模块职责 (Rust Architecture)

- `src/main.rs`：异步运行时入口，负责子任务（MQTT、Watchdog、Console Server）的声明式生命周期管理。
- `src/config.rs`：强类型配置层，支持环境变量优先级与 SIGHUP 热重载。
- `src/mqtt_agent.rs`：**核心通信层**。集成 SDK 规范，负责与 Console 的双向信令交互与 RPC 路由。
- `src/console/`：**中控台模式专属**。
    - `mod.rs`：Axum Web Server 路由分发。
    - `mqtt_broker.rs`：嵌入式 `rumqttd` 实例，处理全网 Agent 接入。
    - `task_queue.rs`：下行指令分发与状态追踪。
- `src/task_executor.rs`：串行任务执行器，负责执行本地 Shell 指令、技能安装等高耗时操作。
- `src/ai_engine.rs` (Planned)：**AI 推理桥接层**。接入 Antigravity 推理逻辑，实现从指标感知到自动决策的闭环。
- `src/watchdog.rs`：服务存活监控与 AI 异常诊断钩子。

## 2. 通信契约 (Antigravity SDK Pattern)

项目抛弃了原始的 Topic 字符串拼接，转向 SDK 规范的语义化路由：

- **Telemetry (遥测)**：`synon/agent/{id}/telemetry` -> 包含系统指标、GNB 状态及 SDK `Observation` 帧。
- **Action (动作)**：`synon/agent/{id}/action/{method}` -> 结构化的 RPC 调用（取代旧版 `synon/cmd/`）。
- **Feedback (反馈)**：`synon/agent/{id}/feedback` -> 任务执行的完整上下文回传。

## 3. 自愈闭环流程

```text
[感知] Watchdog/Heartbeat 采集指标 -> 
[推理] SDK Context/AI Engine 进行语义分析 -> 
[决策] AI 触发修复指令 (Restart/Cleanup/Config Update) -> 
[执行] TaskExecutor 执行物理动作 -> 
[验证] 回调 SDK 验证修复效果
```

## 4. 环境与部署

- **二进制包**：支持 `x86_64-unknown-linux-musl` 静态编译，确保在各发行版无缝运行。
- **数据目录**：默认 `/opt/gnb/`。
- **密钥体系**：基于 GNB Ed25519 密钥对实现 SDK `Identity` 认证。

---
*Last Updated: 2026-05-20 (Post-Audit Refactor)*
