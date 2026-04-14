#!/usr/bin/env bash
# gen_ssh_keys.sh — 一键生成 Synon Daemon SSH Proxy 所需的所有开发密钥
# 真实生产环境中，密钥将由 GNB provisioning 流程分发

set -e

OUTDIR="ssh_keys"
mkdir -p "$OUTDIR"

echo "=== Synon SSH Proxy 密钥生成工具 ==="

gen_ed25519_key() {
    local key_name=$1
    local comment=$2
    local key_path="$OUTDIR/$key_name"
    
    if [ ! -f "$key_path" ]; then
        echo "正在生成 $key_name..."
        ssh-keygen -t ed25519 -N "" -C "$comment" -f "$key_path" -q
    else
        echo "跳过: $key_name 已存在"
    fi
}

# 1. Console 端 SSH Server Host Key
gen_ed25519_key "console_host_key" "synon-console-host"

# 2. Console 端 SSH Client Key (用于连接 Agent)
gen_ed25519_key "console_client_key" "synon-console-client"

# 3. Agent 端 SSH Server Host Key
gen_ed25519_key "agent_host_key" "synon-agent-host"

# 4. 模拟的运维人员个人密钥
gen_ed25519_key "operator_key" "operator@synonclaw"

# 5. 生成 Agent 信任的 authorized_keys (仅信任 Console Client)
echo "生成 Agent authorized_keys..."
cat "$OUTDIR/console_client_key.pub" > "$OUTDIR/authorized_keys"

echo ""
echo "=== 密钥生成完成 ==="
echo "如需进行本地测试，请导出以下环境变量或启动相应服务："
echo ""
echo "  1. Console 端:"
echo "     export SSH_HOST_KEY=$OUTDIR/console_host_key"
echo "     export SSH_CLIENT_KEY=$OUTDIR/console_client_key"
echo "     synon-daemon --config agent.conf console"
echo ""
echo "  2. Agent 端:"
echo "     export SSH_AGENT_HOST_KEY=$OUTDIR/agent_host_key"
echo "     export SSH_AGENT_AUTHORIZED_KEYS=$OUTDIR/authorized_keys"
echo "     synon-daemon --config agent.conf"
echo ""
echo "  3. 运维人员连接 (需先将公钥注册到 DB):"
echo "     ssh -i $OUTDIR/operator_key -p 2222 root+<nodeId>@<console-ip>"
echo ""

# 列出所有文件
echo "生成的文件:"
ls -la "$OUTDIR"/*
