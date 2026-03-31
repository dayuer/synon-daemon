# CLI Specification: `synon-daemon`

`synon-daemon` 是一套双端合一的 Rust 守护进程，用于承载 SynonClaw 系统的边缘代理（Agent）和中控台后台（Console Backend）。

## 概述与调用约定

AI 和外部系统调用 `synon-daemon` 时，应遵循以下参数结构。它采用了经典的 `clap` Subcommand 模式，兼容老旧脚本省略 subcommand 的默认行为。

```bash
synon-daemon [OPTIONS] [COMMAND]
```

## 全局选项 (OPTIONS)

所有模式都支持以下全局选项：

- `--config <CONFIG>`
  - 型号: `String`
  - 默认值: `/opt/gnb/bin/agent.conf`
  - 说明: 配置文件的绝对路径。如果找不到此文件，启动可能会报错或自动生成默认配置（视未来版本而定）。
- `--log-level <LOG_LEVEL>`
  - 型号: `String` (可选 `error`, `warn`, `info`, `debug`, `trace`)
  - 默认值: `info`
  - 说明: 控制日志输出级别，调试时建议传入 `debug` 或 `trace`。
- `-h, --help`
  - 说明: 打印帮助信息。
- `-V, --version`
  - 说明: 打印版本号。

## 子命令 (COMMAND)

通过子命令明确设定 `synon-daemon` 的身份职责。如果不提供子命令，系统**自动回退为 `agent` 模式**。

### 1. `agent` (默认)
**职责**: 作为边缘被控节点，连接到主控台，等待下发命令（脚本运行、升级等）。

**示例调用**:
```bash
# 标准启动
synon-daemon agent

# 使用向后兼容模式启动
synon-daemon --config /etc/synon-daemon.conf
```

### 2. `console`
**职责**: 作为控制面板的纯后端服务。占用本地 3005 端口运行 HTTP/WebSocket Server，并映射和接管操作 `data/registry/nodes.db`。它取代了以前 Node.js 处理的长连接和心跳监控逻辑。

**示例调用**:
```bash
synon-daemon console

# 或带日志调试
synon-daemon --log-level debug console
```

---
*本文档面向 AI 和运维人员，用于指导自动化脚本的撰写与跨进程调用设计。如有接口变动，请同步更新此文件。*
