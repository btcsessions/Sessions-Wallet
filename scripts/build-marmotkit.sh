#!/usr/bin/env bash
# Build MarmotKit (the Marmot Development Kit's Android library) from a pinned
# commit and put it into the marmot plugin:
#   plugins/tauri-plugin-marmot/android/src/main/jniLibs/arm64-v8a/libmarmot_uniffi.so
#   plugins/tauri-plugin-marmot/android/src/main/java/generated/...  (Kotlin bindings)
#
# Both are build outputs (git-ignored). Needs the Android NDK (NDK_HOME or
# ANDROID_NDK_HOME) and rustup. Usage: scripts/build-marmotkit.sh [mdk-checkout]
set -euo pipefail

MDK_REPO="https://github.com/marmot-protocol/mdk.git"
MDK_COMMIT="20ded88b166b685a23754a168f7d1514fb96b9ce"

HERE="$(cd "$(dirname "$0")/.." && pwd)"
PLUGIN="$HERE/apps/wallet/src-tauri/plugins/tauri-plugin-marmot/android/src/main"
SRC="${1:-$HERE/target/mdk-src}"

if [[ ! -d "$SRC/.git" ]]; then
  git clone --filter=blob:none "$MDK_REPO" "$SRC"
fi
git -C "$SRC" fetch --quiet origin "$MDK_COMMIT" || true
git -C "$SRC" checkout --quiet -- Cargo.lock 2>/dev/null || true
git -C "$SRC" -c advice.detachedHead=false checkout --quiet "$MDK_COMMIT"
[[ "$(git -C "$SRC" rev-parse HEAD)" == "$MDK_COMMIT" ]] || { echo "MDK checkout isn't at $MDK_COMMIT" >&2; exit 1; }

# Security fixes the pinned lock predates: tokio (RUSTSEC-2026-0386) and lru.
( cd "$SRC" && cargo update --quiet -p tokio --precise 1.54.1 && cargo update --quiet -p lru@0.18.0 )

export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${NDK_HOME:-}}"
rustup target add aarch64-linux-android --toolchain "$(sed -n 's/^channel = "\(.*\)"/\1/p' "$SRC/rust-toolchain.toml")"
( cd "$SRC" && ANDROID_ABIS=arm64-v8a CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HERE/target/mdk}" ./crates/marmot-uniffi/kotlin-bindings.sh all )

OUT="$SRC/crates/marmot-uniffi/output/android"
mkdir -p "$PLUGIN/jniLibs/arm64-v8a" "$PLUGIN/java/generated"
cp "$OUT/jniLibs/arm64-v8a/libmarmot_uniffi.so" "$PLUGIN/jniLibs/arm64-v8a/"
rm -rf "$PLUGIN/java/generated"
mkdir -p "$PLUGIN/java/generated"
cp -r "$OUT/kotlin/." "$PLUGIN/java/generated/"
echo "MarmotKit $MDK_COMMIT installed into the marmot plugin."
