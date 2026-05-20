# Synon-Daemon

**Synon-Daemon** 是 SynonClaw 中台架构下极低资源占用、跨平台的高性能纯 Rust 守护进程。它是驱动网络边缘计算与中心集成的端点。

本守护进程分为两种运行态，依靠启动参数及特征门控器（Feature Flags）切换：

1. **Agent 模式（边缘节点）**：常驻执行环境，高频抓取上报遥测数据，调度 AI 本地核心任务执行，以及建立反向内网穿透加密隧道。
2. **Console 模式（中控模式）**：中心级 API 枢纽，挂载高吞吐并发控制网关、全局心跳监测与资产日志数据库化组件。

---

## 核心架构特性

### 1. AI-Agent Native 智能内核
- **SDK 赋能**: 深度嵌入 `Antigravity` 协议栈，节点不再仅仅是执行器，而是具备语义感知、自主推理能力的智能体。
- **自愈引擎 (Autonomous Healing)**: 基于 SDK 的推理反馈循环，实时监测并修复系统故障（如网络抖动、进程挂死），实现 0 人工干预运维。

### 2. 高级语义化通信栈
- **SDK 规范 RPC**: 抛弃传统 Topic 字符串拼接，转向结构化的 `Service/Method` 通信模型，支持 AI 代理直接下发复杂任务指令。
- **Telemetry 深度观测**: 结合系统指标与 SDK `Observation` 帧，为全局 AI 模型提供高保真度的现场上下文。

### 3. 企业级零信任安全
- **Ed25519 身份标识**: 每个 Agent 拥有唯一的物理密钥，通过 SDK 集成的身份验证机制实现双向鉴权与链路加密。
- **SSH Proxy 与 PTY 映射**: 集成原生 SSH 跳板功能，支持 AI 辅助的会话审计与全量操作溯源。


---

## 编译与工作模式

代码库可按需裁剪编译特性，降低不必要的依赖体积：

1. **基础 Agent 版 (推荐部署态)**
   此版本去除了控制台代码库，拥有极致体积。
   ```bash
   cargo build --release
   ```

2. **控制节点版 (内置嵌入式 Broker + Node SQLite)**
   ```bash
   cargo build --release --features console
   ```

3. **全血跳板机版 (附带 SSH PTY 映射透传网络代理编排)**
   包含原生端对端通信环境。
   ```bash
   cargo build --release --features ssh-proxy
   ```

---

## 核心模块索引表

| 模块 / 路径                     | 职责定义                                                              |
| ------------------------------- | --------------------------------------------------------------------- |
| **`src/main.rs`**               | 系统主入口、并发生命周期统筹与热重载信号触发。                       |
| **`src/mqtt_agent.rs`**         | Agent 的物理长连接传输层（发布 / 订阅与状态机）。                      |
| **`src/console/*`**             | 中控网关子组件集：包括 `mqtt_broker`（总线）、`auth`（非对称校验）等。 |
| **`src/console/ssh_proxy.rs`**  | Console 端 SSH 内网路由与跳板拦截分配调度器。                         |
| **`src/ssh_server.rs`**         | 面向 Agent 端的伪终端 (PTY) 生成器及跨网数据透传执行核心。               |
| **`src/console/ssh_db.rs`**     | 负责公钥与 SSH Session 的双路验证入库与监控审计。                        |
| **`src/exec_handler.rs`**       | Linux 下高危 Shell 进程拦截净化过滤沙箱隔离器。                         |
| **`src/skills_manager.rs`**     | 自动化本地分析脚本同步下载与生命周期拉起引擎。                         |
| **`src/gnb_controller.rs`**     | 专网隔离环境下的物理组网管理套件集成。                                 |
