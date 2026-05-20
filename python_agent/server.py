#!/usr/bin/env python3
"""
server.py — Antigravity SDK AI Agent (UDS JSON-RPC 服务端)

由 synon-daemon (Rust) 作为子进程拉起。
通过 Unix Domain Socket 接收遥测数据，使用 Antigravity SDK 进行 AI 推理，
返回决策指令。

用法：
    python3 server.py --socket /tmp/synon-ai-1234.sock --node-id 1234

协议：行分隔 JSON-RPC 2.0
"""

import asyncio
import json
import logging
import os
import signal
import sys
import argparse
from pathlib import Path
from typing import Any

logging.basicConfig(
    level=logging.INFO,
    format="[%(asctime)s] [PythonAgent] %(levelname)s %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("synon-agent")

# ──────────────────────────────────────────────────────────
# 观测窗口（最近 N 帧遥测数据，供 AI 推理使用）
# ──────────────────────────────────────────────────────────
MAX_OBSERVATIONS = 10
observations: list[dict] = []


def analyze_observations() -> dict:
    """基于规则的启发式推理（SDK 未安装时的降级逻辑）"""
    if not observations:
        return {"action": "keep_alive", "reason": "无遥测数据", "confidence": 0.0}

    last = observations[-1]

    # 规则 1: GNB 进程连续 3 帧未运行 → 重启
    if len(observations) >= 3:
        gnb_down_streak = all(
            not obs.get("gnbRunning", True) for obs in observations[-3:]
        )
        if gnb_down_streak:
            return {
                "action": "restart_service",
                "target": "gnb",
                "reason": "GNB 进程连续 3 个周期未运行",
                "confidence": 0.95,
            }

    # 规则 2: 磁盘使用率超过 95% → 紧急清理
    disk_pct = last.get("diskPercent", 0)
    if disk_pct > 95:
        return {
            "action": "emergency_cleanup",
            "reason": f"磁盘使用率 {disk_pct}% 超过阈值",
            "confidence": 0.9,
        }

    # 规则 3: CPU 持续超过 90% → 升级告警
    if len(observations) >= 5:
        high_cpu = all(
            obs.get("cpuPercent", 0) > 90 for obs in observations[-5:]
        )
        if high_cpu:
            return {
                "action": "escalate",
                "reason": f"CPU 持续高负载 ({last.get('cpuPercent', 0)}%)",
                "confidence": 0.85,
            }

    return {"action": "keep_alive", "reason": "系统状态正常", "confidence": 1.0}


async def try_sdk_reasoning(sys_info: dict) -> dict | None:
    """尝试使用 Antigravity SDK 进行深度 AI 推理"""
    try:
        from google.antigravity import Agent, LocalAgentConfig

        config = LocalAgentConfig(
            system_instructions=(
                "你是一个服务器运维 AI 助手。基于以下系统遥测数据，"
                "判断是否需要采取修复动作。\n"
                "可选动作: keep_alive / restart_service / emergency_cleanup / escalate\n"
                "返回 JSON: {action, target, reason, confidence}"
            ),
        )
        async with Agent(config) as agent:
            prompt = f"分析以下系统状态并返回决策 JSON:\n```json\n{json.dumps(sys_info, indent=2)}\n```"
            response = await agent.chat(prompt)
            text = await response.text()
            # 尝试从响应中提取 JSON
            import re
            match = re.search(r'\{[^}]+\}', text)
            if match:
                return json.loads(match.group())
    except ImportError:
        log.debug("google-antigravity SDK 未安装，使用规则引擎降级")
    except Exception as e:
        log.warning(f"SDK 推理失败: {e}，降级到规则引擎")
    return None


# ──────────────────────────────────────────────────────────
# JSON-RPC 方法分发
# ──────────────────────────────────────────────────────────
async def handle_method(method: str, params: Any) -> Any:
    """处理 JSON-RPC 方法调用"""

    if method == "observe":
        # 记录遥测帧
        if isinstance(params, dict):
            observations.append(params)
            if len(observations) > MAX_OBSERVATIONS:
                observations.pop(0)

        # 优先使用 SDK 推理，失败则降级到规则引擎
        sdk_result = await try_sdk_reasoning(params)
        if sdk_result:
            return sdk_result
        return analyze_observations()

    elif method == "diagnose":
        # 深度诊断（未来接入 SDK 的 autonomous_shell）
        return f"诊断报告: 收到 {json.dumps(params)[:200]}"

    elif method == "ping":
        return "pong"

    else:
        raise ValueError(f"未知方法: {method}")


async def handle_connection(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    """处理单个 UDS 连接（每个请求一个连接）"""
    try:
        line = await asyncio.wait_for(reader.readline(), timeout=5.0)
        if not line:
            return

        request = json.loads(line.decode().strip())
        method = request.get("method", "")
        params = request.get("params", {})
        req_id = request.get("id", 0)

        try:
            result = await handle_method(method, params)
            response = {"jsonrpc": "2.0", "result": result, "id": req_id}
        except Exception as e:
            response = {
                "jsonrpc": "2.0",
                "error": {"code": -32000, "message": str(e)},
                "id": req_id,
            }

        writer.write((json.dumps(response) + "\n").encode())
        await writer.drain()

    except asyncio.TimeoutError:
        log.warning("连接读取超时")
    except json.JSONDecodeError as e:
        log.warning(f"JSON 解析失败: {e}")
    except Exception as e:
        log.error(f"处理连接异常: {e}")
    finally:
        writer.close()
        await writer.wait_closed()


async def main():
    parser = argparse.ArgumentParser(description="Synon AI Agent (UDS JSON-RPC)")
    parser.add_argument("--socket", required=True, help="Unix Domain Socket 路径")
    parser.add_argument("--node-id", required=True, help="节点 ID")
    args = parser.parse_args()

    socket_path = Path(args.socket)

    # 清理旧 socket
    if socket_path.exists():
        socket_path.unlink()

    log.info(f"AI Agent 启动 (node={args.node_id}, socket={socket_path})")

    # 检查 SDK 可用性
    try:
        import google.antigravity
        log.info("✓ Antigravity SDK 已加载")
    except ImportError:
        log.warning("✗ Antigravity SDK 未安装，使用规则引擎降级模式")

    server = await asyncio.start_unix_server(handle_connection, path=str(socket_path))

    # 设置 socket 权限（仅 owner 可读写）
    os.chmod(str(socket_path), 0o600)

    log.info(f"UDS 服务端就绪: {socket_path}")

    # 优雅关闭
    loop = asyncio.get_event_loop()
    stop = asyncio.Event()

    def _signal_handler():
        log.info("收到关闭信号")
        stop.set()

    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, _signal_handler)

    async with server:
        await stop.wait()

    # 清理
    if socket_path.exists():
        socket_path.unlink()
    log.info("AI Agent 已关闭")


if __name__ == "__main__":
    asyncio.run(main())
