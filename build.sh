#!/usr/bin/env bash
#
# build.sh — deveco-local-model-dashboard 构建脚本（Linux / macOS）
#
# 用法：
#   ./build.sh                # release 编译（默认）
#   ./build.sh --debug        # debug 编译
#   ./build.sh --clean        # 清除编译产物
#   ./build.sh --check        # 编译前先 cargo check
#   ./build.sh --test         # 运行测试
#   ./build.sh --clean --check # 清除后 check + 编译
#
# 可组合使用，例如：./build.sh --debug --check

set -euo pipefail

# ── 解析参数 ──
DEBUG=0
CLEAN=0
CHECK=0
TEST=0

for arg in "$@"; do
    case "$arg" in
        --debug|-d)    DEBUG=1 ;;
        --clean|-c)    CLEAN=1 ;;
        --check)       CHECK=1 ;;
        --test|-t)     TEST=1 ;;
        --help|-h)
            sed -n '2,/^$/p' "$0"
            exit 0
            ;;
        *)
            echo "未知参数: $arg（用 --help 查看用法）" >&2
            exit 1
            ;;
    esac
done

# ── 路径 ──
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$SCRIPT_DIR"

# ── 查找隔离 Rust 工具链（来自同级的 atomcode-deveco 项目） ──
TOOLCHAIN_DIR="$REPO/../atomcode-deveco"
RUSTUP_DIR="$TOOLCHAIN_DIR/.rustup"
CARGO_DIR="$TOOLCHAIN_DIR/.cargo"
if [[ -d "$RUSTUP_DIR" ]]; then
    export RUSTUP_HOME="$RUSTUP_DIR"
    echo "使用隔离 Rust 工具链: $CARGO_DIR"
else
    echo "未找到隔离工具链，使用系统 Rust"
fi
if [[ -d "$CARGO_DIR" ]]; then
    export CARGO_HOME="$CARGO_DIR"
    export PATH="$CARGO_DIR/bin:$PATH"
fi

cd "$REPO"

# ── 模式 ──
if [[ $DEBUG -eq 1 ]]; then
    PROFILE="debug"
else
    PROFILE="release"
fi

TARGET_DIR="$REPO/target"
PROFILE_DIR="$TARGET_DIR/$PROFILE"
BIN_NAME="deveco-local-model-dashboard"
BIN_PATH="$PROFILE_DIR/$BIN_NAME"

# ── 清除 ──
if [[ $CLEAN -eq 1 ]]; then
    echo "==> 清除编译产物 (target/)"
    if [[ -d "$TARGET_DIR" ]]; then
        rm -rf "$TARGET_DIR"
        echo "    已删除 $TARGET_DIR"
    else
        echo "    target/ 不存在，无需清除"
    fi
    OUTPUT_DIR="$REPO/output"
    if [[ -d "$OUTPUT_DIR" ]]; then
        rm -rf "$OUTPUT_DIR"
        echo "    已删除 $OUTPUT_DIR"
    fi
    # 如果只有 --clean，清除后直接退出
    if [[ $CHECK -eq 0 && $TEST -eq 0 ]]; then
        echo "==> 清除完成"
        exit 0
    fi
fi

# ── 测试 ──
if [[ $TEST -eq 1 ]]; then
    echo "==> cargo test"
    cargo test
    if [[ $CHECK -eq 0 ]]; then exit 0; fi
fi

# ── 检查 ──
if [[ $CHECK -eq 1 ]]; then
    echo "==> cargo check"
    cargo check
fi

# ── 编译 ──
if [[ $DEBUG -eq 1 ]]; then
    echo "==> cargo build"
    echo "    [debug 模式] 保留调试符号，编译速度更快，体积更大"
    cargo build
else
    echo "==> cargo build --release"
    echo "    [release 模式] 优化体积与性能（默认）"
    cargo build --release
fi

echo ""
echo "==> 编译产物："
if [[ -f "$BIN_PATH" ]]; then
    SIZE_MB=$(du -m "$BIN_PATH" | cut -f1)
    echo "    $BIN_PATH  (${SIZE_MB} MB)"
fi

# ── 复制到 output/ 目录 ──
OUTPUT_DIR="$REPO/output"
mkdir -p "$OUTPUT_DIR"
cp "$BIN_PATH" "$OUTPUT_DIR/$BIN_NAME"
echo "    已复制到 $OUTPUT_DIR/$BIN_NAME"

echo ""
echo "==> 完成"
echo "    如需打包进 atomcode-deveco，复制到 $TOOLCHAIN_DIR/extend/ 后运行 atomcode-deveco/build.sh --pack"
