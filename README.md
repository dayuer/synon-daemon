# Synon-Daemon

**Synon-Daemon** 是 SynonClaw 中台架构下运行在每个受管边缘节点上的**纯 Rust 守护进程**（2MB，极低资源占用）。它作为一个核心控制平面代理，接管了节点侧所有的运维心跳上报、长连接命令下发与本地任务的执行。

## 核心特性

- **完全异步与非阻塞 (Tokio)**: 所有系统环境命令调用、API 请求及本地 I/O（如 `gnb_ctl`, `openclaw` CLI，配置文件读写）都被设计为 100% 异步执行。这意味着即使系统挂起或响应慢，长连接的心跳保活也不会被阻塞。
- **并发任务串行排队与锁定 (exec_handler + task_executor)**: Console 下发的耗时任务（如包安装、技能安装等）通过 MPSC 通道进入局部的单并发状态机排队执行，带有防重入 `reqId` 锁定机制。有效防御多端管理员并发下达矛盾命令导致的任务风暴雪崩。
- **降权隔离防护模型 (synon 身份)**: 该守护组件和后续分派执行的所有 Shell 操作，都在受限系统用户 `synon` 下进行，实现严格的权限降级与 RBAC 控制面板对接，禁止使用 root。
- **WSS 长连接双通复用 (console_ws)**: 一号端点集成 gnb 隧道网络心跳、节点硬件健康状态上报及 Console/Web 操作大厅的直接双向 RPC 执行链路及实时 Shell 拦截功能。
- **防内存泄漏与主动崩溃自测**: 各个连接、派生任务均有 Drop 保护和看门狗 (Watchdog) 保活保护。

## 环境要求与运行

- 依赖: `gnb_ctl` (可选), `openclaw` (CLI / RPC proxy), `npm` (可选), `ip`。
- 构建与部署: 项目随主管理控制台一同编译并使用 `initnode.sh` 进行自动化部署。

### 构建
```bash
cargo build --release
```

## 目录结构
- `src/main.rs`: 守护进程和 Watchdog 启动口。
- `src/console_ws.rs`: Console WebSockets 连接生命周期与事件扇出。
- `src/heartbeat.rs`: 系统综合信息高频汇聚采集器。
- `src/task_executor.rs`: 单并发安全排队执行器。
- `src/exec_handler.rs`: 系统安全命令白名单过滤与隔离执行沙箱。
- `src/gnb_controller.rs` / `src/gnb_monitor.rs`: 跨进程管控核心 `gnb_ctl` 的数据面监控。
- `src/claw_manager.rs` / `src/claw_proxy.rs`: 开源大模型运维 AI 工具包的对接中介层。
