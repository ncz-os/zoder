#!/usr/bin/env bash
# Local zoder build helper.
#   ./scripts/build.sh mac      -> macOS arm64
#   ./scripts/build.sh mac-x86  -> macOS x86_64
#   ./scripts/build.sh linux    -> linux x86_64 + arm64 via `cross` (needs Docker)
#
# The zoder TUI (`zoder tui`) launches the upstream `zerocode` terminal UI, which
# in turn auto-starts a matching `zeroclaw` engine found beside it. Both are built
# from zeroclaw master (set ZEROCLAW_REF to pin) and dropped into dist/ next to
# the zoder binary so the trio stays version-matched. Set ZODER_SKIP_TUI=1 to
# build only the zoder CLI.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p dist
ZEROCLAW_REPO="${ZEROCLAW_REPO:-https://github.com/zeroclaw-labs/zeroclaw.git}"
ZEROCLAW_REF="${ZEROCLAW_REF:-master}"

build_tui() { # $1 = rust target triple, $2 = dist suffix (may be empty)
  [ "${ZODER_SKIP_TUI:-0}" = 1 ] && return 0
  local tgt="$1" sfx="$2" zc=".zeroclaw-src"
  if [ ! -d "$zc/.git" ]; then git clone --depth 1 -b "$ZEROCLAW_REF" "$ZEROCLAW_REPO" "$zc"; fi
  ( cd "$zc" && git fetch -q origin "$ZEROCLAW_REF" && git checkout -q FETCH_HEAD )
  ( cd "$zc" && cargo build --release --bin zerocode --bin zeroclaw --target "$tgt" )
  cp "$zc/target/$tgt/release/zerocode" "dist/zerocode${sfx}"
  cp "$zc/target/$tgt/release/zeroclaw" "dist/zeroclaw${sfx}"
}

case "${1:-mac}" in
  mac)     rustup target add aarch64-apple-darwin >/dev/null
           cargo build --release --bin zoder --target aarch64-apple-darwin
           cp target/aarch64-apple-darwin/release/zoder dist/zoder
           build_tui aarch64-apple-darwin "" ;;
  mac-x86) rustup target add x86_64-apple-darwin >/dev/null
           cargo build --release --bin zoder --target x86_64-apple-darwin
           cp target/x86_64-apple-darwin/release/zoder dist/zoder-macos-x86_64
           build_tui x86_64-apple-darwin -macos-x86_64 ;;
  linux)   command -v cross >/dev/null || { echo "install: cargo install cross"; exit 1; }
           cross build --release --bin zoder --target x86_64-unknown-linux-gnu
           cross build --release --bin zoder --target aarch64-unknown-linux-gnu
           cp target/x86_64-unknown-linux-gnu/release/zoder dist/zoder-linux-x86_64
           cp target/aarch64-unknown-linux-gnu/release/zoder dist/zoder-linux-arm64 ;;
  *) echo "usage: $0 {mac|mac-x86|linux}"; exit 64 ;;
esac
echo "OK -> dist/"
