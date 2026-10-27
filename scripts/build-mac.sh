#!/usr/bin/env bash
# Build and install the Sessions Wallet Mac app on this Mac, from scratch.
#
#   Double-click "Install Sessions Wallet.command" (next to the source
#   archive and breez_api_key), or: bash build-mac.sh
#
# Uses (and installs only into your user account, no sudo):
#   - Xcode command-line tools (asks macOS to install them if missing)
#   - Rust via rustup (~/.cargo)
#   - Node 22 and protoc, downloaded into ~/sessions-wallet-build/tools
# Source: ~/Downloads/sessions-wallet-src.tar.gz (or a clone of the repo).
# Breez key: ~/Downloads/breez_api_key (kept out of the repo).
# Output: ~/sessions-wallet-build/Sessions Wallet_<version>_aarch64.dmg
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Look next to this script first, then in Downloads.
if ls "$HERE"/sessions-wallet-src*.tar* >/dev/null 2>&1; then DL="${DL:-$HERE}"; else DL="${DL:-$HOME/Downloads}"; fi
WORK="$HOME/sessions-wallet-build"
TOOLS="$WORK/tools"
LOG="$WORK/build.log"
mkdir -p "$WORK" "$TOOLS"
exec > >(tee -a "$LOG") 2>&1
trap 'echo; echo "Build stopped. Send Claude the last lines of $LOG"' ERR
step() { printf '\n==> %s\n' "$*"; }

[[ "$(uname -m)" == "arm64" ]] || { echo "This builds for Apple Silicon Macs."; exit 1; }

step "Xcode command-line tools"
if ! xcode-select -p >/dev/null 2>&1; then
  xcode-select --install || true
  echo "macOS is asking to install the command-line tools: click Install in that window."
  echo "Waiting for it to finish (this can take 10+ minutes)..."
  until xcode-select -p >/dev/null 2>&1 && [[ -x "$(xcode-select -p)/usr/bin/clang" ]]; do sleep 20; done
fi

step "Rust"
if ! command -v rustup >/dev/null 2>&1 && [[ ! -x "$HOME/.cargo/bin/rustup" ]]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup toolchain install stable --profile minimal >/dev/null
rustup target add aarch64-apple-darwin >/dev/null

step "Node"
NODE_VER=22.12.0
if command -v node >/dev/null 2>&1 && [[ "$(node -p 'process.versions.node.split(".")[0]')" -ge 20 ]]; then
  echo "Using node $(node -v)"
else
  if [[ ! -x "$TOOLS/node/bin/node" ]]; then
    curl -sSfL "https://nodejs.org/dist/v$NODE_VER/node-v$NODE_VER-darwin-arm64.tar.gz" -o "$TOOLS/node.tgz"
    mkdir -p "$TOOLS/node" && tar -xzf "$TOOLS/node.tgz" -C "$TOOLS/node" --strip-components 1 && rm "$TOOLS/node.tgz"
  fi
  export PATH="$TOOLS/node/bin:$PATH"
  echo "Using node $(node -v) (local)"
fi

step "protoc"
if ! command -v protoc >/dev/null 2>&1; then
  if [[ ! -x "$TOOLS/protoc/bin/protoc" ]]; then
    curl -sSfL "https://github.com/protocolbuffers/protobuf/releases/download/v29.3/protoc-29.3-osx-aarch_64.zip" -o "$TOOLS/protoc.zip"
    mkdir -p "$TOOLS/protoc" && unzip -qo "$TOOLS/protoc.zip" -d "$TOOLS/protoc" && rm "$TOOLS/protoc.zip"
  fi
  export PATH="$TOOLS/protoc/bin:$PATH"
fi
protoc --version

step "Source"
SRC="$WORK/sessions-wallet"
# Safari may unpack the .gz on download, leaving a plain .tar.
ARCHIVE="$(ls -t "$DL"/sessions-wallet-src*.tar.gz "$DL"/sessions-wallet-src*.tar 2>/dev/null | head -1 || true)"
if [[ -n "$ARCHIVE" ]]; then
  echo "Using $ARCHIVE"
  rm -rf "$SRC.new" && mkdir -p "$SRC.new"
  tar -xf "$ARCHIVE" -C "$SRC.new" --strip-components 1
  # Keep build caches (target/) from an earlier run; replace the code.
  if [[ -d "$SRC/target" ]]; then mv "$SRC/target" "$SRC.new/target"; fi
  rm -rf "$SRC" && mv "$SRC.new" "$SRC"
elif [[ ! -d "$SRC/.git" ]]; then
  git clone https://github.com/btcsessions/Sessions-Wallet.git "$SRC"
fi
cd "$SRC"

step "Breez key"
if [[ -z "${BREEZ_API_KEY:-}" ]]; then
  [[ -f "$DL/breez_api_key" ]] || { echo "Put the Breez key in $DL/breez_api_key (or set BREEZ_API_KEY)."; exit 1; }
  BREEZ_API_KEY="$(tr -d '\n\r ' < "$DL/breez_api_key")"
fi
export BREEZ_API_KEY

step "Group messaging library (MarmotKit) — the long step, 15-30 min the first time"
export CARGO_TARGET_DIR="$SRC/target/marmot-bridge"
scripts/build-marmot-bridge.sh --target aarch64-apple-darwin
unset CARGO_TARGET_DIR

step "The app"
cd apps/wallet
npm ci
npx tauri build --target aarch64-apple-darwin --bundles app,dmg

BUNDLE="$SRC/target/aarch64-apple-darwin/release/bundle"
DMG="$(ls -t "$BUNDLE"/dmg/*.dmg | head -1)"
cp "$DMG" "$WORK/"
APP="$(ls -d "$BUNDLE"/macos/*.app | head -1)"
step "Installing"
DEST=/Applications
[[ -w "$DEST" ]] || { DEST="$HOME/Applications"; mkdir -p "$DEST"; }
rm -rf "$DEST/$(basename "$APP")"
ditto "$APP" "$DEST/$(basename "$APP")"
step "Done"
echo "Installed: $DEST/$(basename "$APP")"
echo "(A .dmg copy to share is in $WORK.) You can close this window."
open "$DEST/$(basename "$APP")"
