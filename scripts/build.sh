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

if [ -f "$HOME/.cargo/env" ]; then
  # Some local shells do not source Rust's environment before running scripts.
  # Source it here so local builds work from a fresh terminal.
  . "$HOME/.cargo/env"
fi

ensure_target() { # $1 = rust target triple
  local tgt="$1"
  if command -v rustup >/dev/null 2>&1; then
    rustup target add "$tgt" >/dev/null
    return
  fi

  command -v rustc >/dev/null 2>&1 || {
    echo "build.sh: rustc not found; install Rust or source ~/.cargo/env" >&2
    exit 127
  }

  local sysroot libdir
  sysroot="$(rustc --print sysroot)"
  libdir="$sysroot/lib/rustlib/$tgt/lib"
  if [ ! -d "$libdir" ] || ! ls "$libdir"/libstd-* >/dev/null 2>&1; then
    echo "build.sh: Rust target $tgt is not installed and rustup is not available." >&2
    echo "build.sh: install rustup or add the target, then rerun this build." >&2
    exit 1
  fi
}

build_tui() { # $1 = rust target triple, $2 = dist suffix (may be empty)
  [ "${ZODER_SKIP_TUI:-0}" = 1 ] && return 0
  local tgt="$1" sfx="$2" zc=".zeroclaw-src"
  if [ ! -d "$zc/.git" ]; then git clone --depth 1 -b "$ZEROCLAW_REF" "$ZEROCLAW_REPO" "$zc"; fi
  ( cd "$zc" && git fetch -q origin "$ZEROCLAW_REF" && git checkout -q FETCH_HEAD )
  ( cd "$zc" && cargo build --release -p zerocode --bin zerocode --target "$tgt" )
  ( cd "$zc" && cargo build --release --bin zeroclaw --target "$tgt" )
  cp "$zc/target/$tgt/release/zerocode" "dist/zerocode${sfx}"
  cp "$zc/target/$tgt/release/zeroclaw" "dist/zeroclaw${sfx}"
}

case "${1:-mac}" in
  mac)     ensure_target aarch64-apple-darwin
           cargo build --release --bin zoder --target aarch64-apple-darwin
           cp target/aarch64-apple-darwin/release/zoder dist/zoder
           build_tui aarch64-apple-darwin "" ;;
  mac-x86) ensure_target x86_64-apple-darwin
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
