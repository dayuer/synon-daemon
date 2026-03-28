#!/usr/bin/env bash
# 自动通过云服务器编译 x86_64 musl 静态版本并取回本机
# 使用方法: ./build_remote.sh

set -e

BUILD_SERVER="root@43.156.128.95"
REMOTE_PROJECT_DIR="/root/synon-daemon"
# 自动推导项目根目录
LOCAL_PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_ARCH="x86_64-unknown-linux-musl"
BINARY_NAME="synon-daemon"
DIST_DIR="${LOCAL_PROJECT_DIR}/dist"

echo "🚀 [1/4] 同步代码到编译服务器 ($BUILD_SERVER)..."
# 排除 target 目录和 git 记录，加快同步速度
rsync -avz --exclude 'target/' --exclude '.git/' --exclude 'dist/' "$LOCAL_PROJECT_DIR/" "$BUILD_SERVER:$REMOTE_PROJECT_DIR/"

echo "🛠️  [2/4] 在服务器上执行 musl 静态编译..."
# 确保 cargo 在 PATH 中，并执行编译
ssh "$BUILD_SERVER" "source ~/.cargo/env 2>/dev/null || true; cd $REMOTE_PROJECT_DIR && cargo build --release --target $TARGET_ARCH"

echo "📦 [3/4] 拉取编译产物回本地..."
mkdir -p "$DIST_DIR"
scp "$BUILD_SERVER:$REMOTE_PROJECT_DIR/target/$TARGET_ARCH/release/$BINARY_NAME" "$DIST_DIR/"

echo "✅ [4/4] 编译完成！"
echo "📦 产物路径: $DIST_DIR/$BINARY_NAME"
# 简单验证文件
ls -lh "$DIST_DIR/$BINARY_NAME"
file "$DIST_DIR/$BINARY_NAME"
