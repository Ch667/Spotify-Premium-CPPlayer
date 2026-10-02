#!/bin/bash
# macOS / Linux 桌面构建并打包。
# 用法：./build_desktop.sh [target] [repo]   target 默认本机平台，macOS 可传两套分别构建。
set -e
cd "$(dirname "$0")/.."   # cpmodule/

TARGET="${1:-}"
REPO="${2:-librespot-org/librespot}"

case "$(uname -s)" in
    Darwin) DEFAULT_TARGET="aarch64-apple-darwin"; PLATFORM="macos"; ENTRY="libcp_spotify.dylib" ;;
    Linux)  DEFAULT_TARGET="x86_64-unknown-linux-gnu"; PLATFORM="linux"; ENTRY="libcp_spotify.so" ;;
    *) echo "Use build_desktop.ps1 / build_android.sh on this platform." >&2; exit 1 ;;
esac
TARGET="${TARGET:-$DEFAULT_TARGET}"

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
echo "Building cp-spotify-module $VERSION for $TARGET..."

cargo build --release --features jni --target "$TARGET"

STAGE=./target/cp_stage
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "./target/$TARGET/release/$ENTRY" "$STAGE/"

python3 scripts/pack_module.py --platform "$PLATFORM" --version "$VERSION" \
    --stage "$STAGE" --repo "$REPO" --out ./target/cp_out
echo "Done: ./target/cp_out/"
