#!/bin/bash
# Android：cargo-ndk 三 ABI 构建 libcp_spotify.so 并打包为模块 zip。
# 依赖：cargo-ndk（cargo install cargo-ndk）、NDK（ANDROID_NDK_HOME 或默认路径）、python3。
set -e
cd "$(dirname "$0")/.."   # cpmodule/

TARGETS=("arm64-v8a" "armeabi-v7a" "x86_64")
VERSION="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
REPO="${2:-librespot-org/librespot}"

echo "Building cp-spotify-module $VERSION for ${TARGETS[*]}..."

if ! command -v cargo-ndk &> /dev/null; then
    echo "Error: cargo-ndk is not installed. Install via: cargo install cargo-ndk" >&2
    exit 1
fi

if [ -z "$ANDROID_NDK_HOME" ] && [ -n "$ANDROID_HOME" ]; then
    LATEST_NDK=$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -n 1 || true)
    [ -n "$LATEST_NDK" ] && export ANDROID_NDK_HOME="$LATEST_NDK" && echo "Using NDK: $ANDROID_NDK_HOME"
fi
if [ -z "$ANDROID_NDK_HOME" ]; then
    echo "Error: ANDROID_NDK_HOME not set and no NDK under ANDROID_HOME/ndk." >&2
    exit 1
fi

# Android 16KB page size 对齐（参考模块同款）
export RUSTFLAGS="-Clink-arg=-Wl,-z,max-page-size=16384"

cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o ./target/jniLibs \
    build --release --features jni-android

STAGE=./target/cp_stage
rm -rf "$STAGE"
for ABI in "${TARGETS[@]}"; do
    mkdir -p "$STAGE/lib/$ABI"
    find "./target/jniLibs/$ABI" -name "libcp_spotify.so" -exec cp {} "$STAGE/lib/$ABI/" \;
done

python3 scripts/pack_module.py --platform android --version "$VERSION" \
    --stage "$STAGE" --repo "$REPO" --out ./target/cp_out
echo "Done: ./target/cp_out/"
