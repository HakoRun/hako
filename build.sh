#!/bin/bash
set -euo pipefail

# Hako build pipeline.
# Produces a statically-linked Linux binary for hako-core, then (optionally)
# packages a Windows `.exe` that embeds it and forwards via WSL2.
#
# Usage:
#   ./build.sh                    # linux + windows
#   ./build.sh linux              # linux only
#   ./build.sh windows            # windows wrapper only (requires hako-linux)
#
# Prerequisites:
#   Linux:
#     - rustup target add x86_64-unknown-linux-musl
#     - apt-get install musl-tools libfuse3-dev
#   Windows cross-build (from Linux):
#     - rustup target add x86_64-pc-windows-msvc
#     - cargo install cargo-xwin   (preferred)   OR install MSVC linker
#     Invoke as:  CARGO="cargo xwin" ./build.sh windows

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

LINUX_TARGET="x86_64-unknown-linux-musl"
LINUX_BINARY="target/${LINUX_TARGET}/release/hako"
EMBEDDED_PATH="hako-linux"

CARGO="${CARGO:-cargo}"

build_linux() {
    echo "==> Building hako-core (Linux, static musl)..."
    $CARGO build -p hako --target "$LINUX_TARGET" --release
    strip "$LINUX_BINARY" 2>/dev/null || true
    cp "$LINUX_BINARY" "$EMBEDDED_PATH"
    echo "    Binary: $(ls -lh "$EMBEDDED_PATH" | awk '{print $5}')"
    echo "    Linking: $(file "$EMBEDDED_PATH" 2>/dev/null | grep -o 'static[^ ]*' || echo 'static-pie')"
}

build_windows() {
    if [ ! -f "$EMBEDDED_PATH" ]; then
        echo "Error: $EMBEDDED_PATH not found. Run './build.sh linux' first."
        exit 1
    fi
    echo "==> Building hako-windows launcher (with embedded Linux binary)..."
    $CARGO build -p hako-windows --features embedded --release
    echo "    Binary: $(ls -lh target/release/hako.exe 2>/dev/null | awk '{print $5, $NF}')"
}

case "${1:-all}" in
    linux)   build_linux ;;
    windows) build_windows ;;
    all)     build_linux; build_windows ;;
    *)
        echo "Usage: $0 [linux|windows|all]"
        exit 1
        ;;
esac

echo "==> Done."
