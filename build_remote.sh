#!/usr/bin/env bash
# 自动通过云服务器编译 x86_64 musl 静态版本并取回本机
# 使用方法: ./build_remote.sh [--deploy]
#   --deploy  编译完成后自动推送到 Console 镜像目录（供 initnode.sh 下载）

set -e

BUILD_SERVER="root@43.156.128.95"
REMOTE_PROJECT_DIR="/root/synon-daemon"
LOCAL_PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET="x86_64-unknown-linux-musl"
BINARY_NAME="synon-daemon"
DIST_DIR="${LOCAL_PROJECT_DIR}/dist"
MIRROR_DIR="/opt/gnb-console/data/mirror/daemon"

# 解析参数
DO_DEPLOY=false
for arg in "$@"; do
    case "$arg" in
        --deploy) DO_DEPLOY=true ;;
    esac
done

echo "🚀 [1/4] 同步代码到编译服务器 ($BUILD_SERVER)..."
rsync -avz --delete \
    --exclude 'target/' --exclude '.git/' --exclude 'dist/' \
    "$LOCAL_PROJECT_DIR/" "$BUILD_SERVER:$REMOTE_PROJECT_DIR/"

echo "🛠️  [2/4] 安装 musl 工具链 + 静态编译..."
ssh "$BUILD_SERVER" "
  set -e

  # 安装 musl 交叉编译工具（幂等）
  if ! command -v musl-gcc &>/dev/null; then
      echo '      安装 musl-tools...'
      apt-get update -qq && apt-get install -y -qq musl-tools
  fi

  # 安装 Rust（幂等）
  if ! command -v rustup &>/dev/null; then
      echo '      安装 Rust...'
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  fi
  source \$HOME/.cargo/env

  # 添加 musl target（幂等）
  rustup target add $TARGET 2>/dev/null || true

  cd $REMOTE_PROJECT_DIR
  echo '      编译中（musl 静态链接）...'
  cargo build --release --target $TARGET 2>&1 | tail -5
  echo '      ✅ 编译完成'
  file target/$TARGET/release/$BINARY_NAME
  ls -lh target/$TARGET/release/$BINARY_NAME
"

echo "📦 [3/4] 拉取编译产物回本地..."
mkdir -p "$DIST_DIR"
scp "$BUILD_SERVER:$REMOTE_PROJECT_DIR/target/$TARGET/release/$BINARY_NAME" "$DIST_DIR/"

echo "✅ [4/4] 编译完成！"
echo "📦 产物路径: $DIST_DIR/$BINARY_NAME"
ls -lh "$DIST_DIR/$BINARY_NAME"
file "$DIST_DIR/$BINARY_NAME"

# --deploy: 推送到 Console 镜像目录
if [ "$DO_DEPLOY" = "true" ]; then
    echo ""
    echo "📡 部署到 Console 镜像目录 ($MIRROR_DIR)..."
    ssh "$BUILD_SERVER" "
      set -e
      mkdir -p $MIRROR_DIR
      cp $REMOTE_PROJECT_DIR/target/$TARGET/release/$BINARY_NAME $MIRROR_DIR/synon-daemon-x86_64-musl
      # 向下兼容旧节点（已部署的 initnode.sh 可能用 -linux-gnu 名字）
      cp $MIRROR_DIR/synon-daemon-x86_64-musl $MIRROR_DIR/synon-daemon-x86_64-linux-gnu
      # 写入 BUILD_VERSION（编译时间戳版本号，格式: YYYYMMDD.HHmmss）
      # 从编译好的二进制 --version 输出中提取 build 版本号
      BUILD_VER=$($REMOTE_PROJECT_DIR/target/$TARGET/release/$BINARY_NAME --version 2>/dev/null | sed -n 's/.*build \([0-9.]*\).*/\1/p')
      [ -z "\\$BUILD_VER" ] && BUILD_VER=$(date '+%Y%m%d.%H%M%S')
      echo \\$BUILD_VER > $MIRROR_DIR/.version
      echo "      版本号: \\$BUILD_VER"
      # 生成 SHA256（供 self_updater 校验）
      sha256sum $MIRROR_DIR/synon-daemon-x86_64-musl | awk '{print \$1}' > $MIRROR_DIR/synon-daemon-x86_64-musl.sha256
      echo '      ✅ 已部署到镜像目录'
      ls -lh $MIRROR_DIR/
    "
fi
