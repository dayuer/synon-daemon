# Synon-Daemon

**Synon-Daemon** 是 SynonClaw 中台架构下的**核心纯 Rust 守护进程**（极低资源占用，跨平台静态编译）。经过底层通信架构重构，该服务目前运行模式分为**双端形态**：
1. **Agent 面（边缘工作节点）**：接收并执行远端命令、高频上报状态采集信息、进程看门狗（Watchdog），并管理 AI 技能的分配与调用。
2. **Console 面（中控台直连核心）**：运行在主控服务器上，内置了高吞吐量的 `rumqttd` 代理，串联控制平台与全网成百上千个 Agent 工作负载，并将数据持久化到 SQLite (`nodes.db`) 中，桥接给前端的 Next.js 服务。

## 核心架构特性

- **Ed25519 签名零信任认证**: 完全移除传统的明文 Token 依赖。Agent 节点在发送 MQTT 数据包时自动应用基于主控面板生成的非对称秘钥对进行时间戳 + Signature 签名，避免中间人或重放攻击。
- **十万级并发的 MQTT TCP 协议**: 从原有的 WebSocket (WSS) 演进到了严酷网络长连接更稳定的 MQTT 机制。利用 `rumqttc` 在终端机建立健壮的 QoS 1 保活（心跳 30s）。
- **双工作模式切换**:
  - `synon-daemon console`: Console 模式，需要带 `--features console` 编译。内置本地 broker 与 SQLite 管理。
  - `synon-daemon agent`: 或直接运行无参名二进制，Agent 模式。
- **并发任务串行排队与防重入**: 控制台下发的任务指令具备全局锁，借助通道 MPSC 模型确保耗时任务的局域单点安全性。
- **降权隔离防护模型**: （可选）所有 AI 调用及高危 Shell 代码过滤，被置于系统低权限 `synon` 用户层下运行。

## 环境与构建

随主控台 `scripts/initnode.sh` 或全链路构建体系联合部署打包。

### 本地编译
**1. 编译终端 Agent (纯净版, 裁剪了中控服务体积以分发)**
```bash
cargo build --release
```

**2. 编译跳板中控台版 (包含全量 SQLite Driver 与 rumqttd)**
```bash
cargo build --release --features console
```

## 目录索引指南

- `src/main.rs`: 守护进程双模启动入口及系统看门狗（Watchdog）配置。
- `src/mqtt_agent.rs`: **[核心]** Agent 侧 MQTT 客户端的链路状态机、订阅事件树、Pub/Sub 及网络断供重连层。
- `src/console/`: **[核心]** Console 中控台侧内置的 `mqtt_broker`, `auth` (Ed25519校验) 以及 `nodes.db` 实体状态更新器等。
- `src/config.rs`: 统一读取 `agent.conf` 参数环境。
- `src/heartbeat.rs`: Agent 各项硬件、网络探测的高频采集器。
- `src/exec_handler.rs` / `src/task_executor.rs`: 安全命令白名单与局域单并发串行命令调度锁框架。
- `src/skills_manager.rs`: Agent 侧自动化处理从 GitHub/源站获取并管理 AI 分析脚本的中心。
- `src/self_updater.rs`: 面向节点自身的二进制热派发重载（OTA 更新验证）。
- `src/claw_manager.rs` / `src/gnb_controller.rs`: 进程管控组件代理封装。
