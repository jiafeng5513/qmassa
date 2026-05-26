#!/bin/bash
set -e

NDK_PATH="/mnt/workspace/LIBRARIES/install/Qt/AndroidSDK/ndk/25.1.8937393"
ANDROID_LINKER="${NDK_PATH}/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android33-clang"
ANDROID_TARGET="x86_64-linux-android"

usage() {
    echo "Usage: $0 [linux|android|all] [--release]"
    echo "  linux     Build for Linux (default)"
    echo "  android   Build for x86_64 Android"
    echo "  all       Build both"
    echo "  --release Build in release mode"
    exit 1
}

TARGET="linux"
PROFILE=""
PROFILE_FLAG=""

for arg in "$@"; do
    case "$arg" in
        linux|android|all) TARGET="$arg" ;;
        --release) PROFILE="release"; PROFILE_FLAG="--release" ;;
        -h|--help) usage ;;
        *) echo "Unknown argument: $arg"; usage ;;
    esac
done

build_linux() {
    echo "=== Building for Linux ==="
    cargo build -p qmassa $PROFILE_FLAG
    cargo build -p qmmd $PROFILE_FLAG
    echo "Done. Output: target/${PROFILE:-debug}/qmassa, target/${PROFILE:-debug}/qmmd"
}

build_android() {
    echo "=== Building for Android (x86_64) ==="
    if [ ! -f "$ANDROID_LINKER" ]; then
        echo "ERROR: NDK linker not found at: $ANDROID_LINKER"
        echo "Please update NDK_PATH in this script."
        exit 1
    fi
    CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$ANDROID_LINKER" \
        cargo build --target "$ANDROID_TARGET" -p qmassa --no-default-features --features android $PROFILE_FLAG
    echo "Done. Output: target/${ANDROID_TARGET}/${PROFILE:-debug}/qmassa"
}

case "$TARGET" in
    linux)   build_linux ;;
    android) build_android ;;
    all)     build_linux; build_android ;;
esac
