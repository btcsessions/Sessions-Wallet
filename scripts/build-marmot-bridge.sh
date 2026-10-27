#!/usr/bin/env bash
# Build the desktop group-messaging library (native/marmot-bridge: MarmotKit
# behind a JSON C ABI) and put it where the desktop bundle picks it up:
#   apps/wallet/src-tauri/frameworks/libmarmot_bridge.{dylib,so}
#
# macOS bundles copy it into Contents/Frameworks; Linux bundles ship it as a
# resource. Usage: scripts/build-marmot-bridge.sh [--target <triple>]
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
CRATE="$HERE/native/marmot-bridge"
OUT="$HERE/apps/wallet/src-tauri/frameworks"
TARGET_ARGS=()
TRIPLE=""
if [[ "${1:-}" == "--target" ]]; then
  TRIPLE="$2"
  TARGET_ARGS=(--target "$TRIPLE")
fi

TOOLCHAIN="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$CRATE/rust-toolchain.toml")"
rustup toolchain install "$TOOLCHAIN" --profile minimal >/dev/null
[[ -n "$TRIPLE" ]] && rustup target add "$TRIPLE" --toolchain "$TOOLCHAIN" >/dev/null

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HERE/target/marmot-bridge}"
( cd "$CRATE" && cargo build --release --locked "${TARGET_ARGS[@]}" )

DIR="$CARGO_TARGET_DIR/${TRIPLE:+$TRIPLE/}release"
mkdir -p "$OUT"
case "$(uname -s)" in
  Darwin)
    cp "$DIR/libmarmot_bridge.dylib" "$OUT/"
    # Loaded by path from Contents/Frameworks.
    install_name_tool -id "@rpath/libmarmot_bridge.dylib" "$OUT/libmarmot_bridge.dylib"
    ;;
  Linux) cp "$DIR/libmarmot_bridge.so" "$OUT/" ;;
  *) cp "$DIR/marmot_bridge.dll" "$OUT/" ;;
esac
ls -la "$OUT"
