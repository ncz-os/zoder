#!/usr/bin/env bash
# Build a complete, redistributable zoder bundle for one or more targets.
#
# Each bundle is a tarball carrying the version-matched trio:
#   zoder   - cost-aware, free-first coding/review CLI (this repo)
#   zerocode  - interactive terminal UI                  (upstream zeroclaw)
#   zeroclaw  - agent / turn engine                       (upstream zeroclaw)
# plus README, LICENSE, and an INSTALL note. Keeping the three together and
# version-matched is what `zoder tui` expects at runtime.
#
# Usage:
#   scripts/package.sh                                   # host target only
#   scripts/package.sh aarch64-apple-darwin x86_64-unknown-linux-gnu
#   ZODER_SKIP_TUI=1 scripts/package.sh <target>         # CLI-only bundle
#
# Builders: a target whose OS matches the host builds with `cargo`; a foreign
# OS target builds with `cross` (Docker) when available. CI uses native runners
# per target, so `cargo` is used throughout there.
#
# Knobs:
#   ZEROCLAW_REPO            upstream zeroclaw git URL (default below)
#   ZEROCLAW_REF             branch/tag/sha to build   (default: master)
#   ZEROCLAW_BUILD_FEATURES  optional cargo features for the zeroclaw build
#                            (e.g. a theme overlay feature)
#   ZODER_SKIP_TUI=1         build only the main CLI (no zerocode/zeroclaw)
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="zoder"
VERSION="$(awk -F'"' '/^version[[:space:]]*=/{print $2; exit}' Cargo.toml)"
[ -n "$VERSION" ] || VERSION="0.0.0"
# The engine/UI come from the ncz-os zeroclaw fork's `zoder-integration` branch:
# upstream/master + our curated patch stack (offline pricing catalog + cost
# engine, atomic-append cost ledger, panel-plugin/ReportPanel/theme picker).
# That branch is a clean rebasing patch-stack — `master..zoder-integration` is
# exactly our changes; see docs/VENDORING.md for the re-integration runbook.
# Override ZEROCLAW_REPO for a different mirror, or set ZEROCLAW_SRC_DIR to reuse
# an existing local checkout (skips cloning and reuses its build cache).
ZEROCLAW_REPO="${ZEROCLAW_REPO:-https://gitlab.com/ncz-os/zeroclaw.git}"
ZEROCLAW_REF="${ZEROCLAW_REF:-zoder-integration}"
ZEROCLAW_SRC_DIR="${ZEROCLAW_SRC_DIR:-}"
# In the zeroclaw workspace the two bins live in separate packages; name them so
# cargo can build both from the workspace root. Override if upstream renames them.
ZEROCLAW_BIN_PKG="${ZEROCLAW_BIN_PKG:-zeroclawlabs}"  # owns the `zeroclaw` engine bin
ZEROCODE_BIN_PKG="${ZEROCODE_BIN_PKG:-zerocode}"      # owns the `zerocode` TUI bin
HOST_TRIPLE="$(rustc -vV | awk '/^host:/{print $2}')"
DIST="dist"
mkdir -p "$DIST"

host_os() {
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) echo Windows ;;
    *) uname -s ;;
  esac
}
target_os() {
  case "$1" in
    *apple-darwin) echo Darwin ;;
    *linux*) echo Linux ;;
    *windows*) echo Windows ;;
    *) echo Unknown ;;
  esac
}
builder_for() { # -> "cargo" | "cross"  (or exit if a cross build isn't possible)
  local tgt="$1"
  if [ "$(target_os "$tgt")" = "$(host_os)" ]; then
    echo cargo
  elif command -v cross >/dev/null 2>&1; then
    echo cross
  else
    echo "package.sh: need 'cross' (cargo install cross) to build $tgt from $(host_os)" >&2
    return 1
  fi
}
sha256_of() { # $1 = file -> writes $1.sha256
  if command -v shasum >/dev/null 2>&1; then
    ( cd "$(dirname "$1")" && shasum -a 256 "$(basename "$1")" > "$(basename "$1").sha256" )
  else
    ( cd "$(dirname "$1")" && sha256sum "$(basename "$1")" > "$(basename "$1").sha256" )
  fi
}

# Resolve the zeroclaw source tree to build the engine/UI from, and echo its path.
# Honors ZEROCLAW_SRC_DIR (an existing checkout, used as-is); otherwise clones
# ZEROCLAW_REPO@ZEROCLAW_REF into .zeroclaw-src. All progress goes to stderr so
# the echoed path on stdout stays clean for command substitution.
ensure_zeroclaw() {
  if [ -n "$ZEROCLAW_SRC_DIR" ]; then
    [ -d "$ZEROCLAW_SRC_DIR" ] || { echo "package.sh: ZEROCLAW_SRC_DIR=$ZEROCLAW_SRC_DIR not found" >&2; return 1; }
    echo "$ZEROCLAW_SRC_DIR"; return 0
  fi
  local zc=".zeroclaw-src"
  if [ ! -d "$zc/.git" ]; then
    git clone --depth 1 -b "$ZEROCLAW_REF" "$ZEROCLAW_REPO" "$zc" >&2
  fi
  ( cd "$zc" && git fetch -q origin "$ZEROCLAW_REF" && git checkout -q FETCH_HEAD ) >&2
  echo "$zc"
}

package_target() {
  local tgt="$1"
  local ext=""; [ "$(target_os "$tgt")" = Windows ] && ext=".exe"
  local b; b="$(builder_for "$tgt")"
  rustup target add "$tgt" >/dev/null 2>&1 || true

  # Native builds (target == host) omit --target so they reuse the default
  # target/ build cache; cross / foreign-OS builds use an explicit --target.
  local tflag=() reldir="target/release"
  if [ "$b" = cross ] || [ "$tgt" != "$HOST_TRIPLE" ]; then
    tflag=(--target "$tgt"); reldir="target/$tgt/release"
  fi

  echo ">> [$tgt] build $BIN ($b)"
  "$b" build --release --bin "$BIN" ${tflag[@]+"${tflag[@]}"}

  local stage="$DIST/${BIN}-${VERSION}-${tgt}"
  rm -rf "$stage"; mkdir -p "$stage"
  cp "$reldir/${BIN}${ext}" "$stage/${BIN}${ext}"

  if [ "${ZODER_SKIP_TUI:-0}" != 1 ] && [ "$(target_os "$tgt")" != Windows ]; then
    local zcsrc; zcsrc="$(ensure_zeroclaw)"
    local feat=(); [ -n "${ZEROCLAW_BUILD_FEATURES:-}" ] && feat=(--features "$ZEROCLAW_BUILD_FEATURES")
    echo ">> [$tgt] build zerocode + zeroclaw ($b) from $zcsrc"
    ( cd "$zcsrc" && "$b" build --release -p "$ZEROCLAW_BIN_PKG" -p "$ZEROCODE_BIN_PKG" --bin zeroclaw --bin zerocode ${tflag[@]+"${tflag[@]}"} ${feat[@]+"${feat[@]}"} )
    cp "$zcsrc/$reldir/zerocode" "$stage/zerocode"
    cp "$zcsrc/$reldir/zeroclaw" "$stage/zeroclaw"
  fi

  cp README.md "$stage/" 2>/dev/null || true
  cp LICENSE "$stage/" 2>/dev/null || true
  cat > "$stage/INSTALL.txt" <<TXT
${BIN} ${VERSION} (${tgt})

Contents:
  ${BIN}     - cost-aware, free-first coding/review CLI
  zerocode   - interactive terminal UI         (launched by: ${BIN} tui)
  zeroclaw   - agent / turn engine             (auto-started by zerocode)

Install: copy the binaries into a directory on your PATH, keeping them together
and version-matched, e.g.

  install -m 0755 ${BIN} zerocode zeroclaw /usr/local/bin/

Then:

  ${BIN} --help
  ${BIN} tui
TXT

  local tar="${BIN}-${VERSION}-${tgt}.tar.gz"
  ( cd "$DIST" && tar -czf "$tar" "${BIN}-${VERSION}-${tgt}" )
  sha256_of "$DIST/$tar"
  rm -rf "$stage"
  echo ">> [$tgt] -> $DIST/$tar"
}

TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
  TARGETS=("$(rustc -vV | awk '/^host:/{print $2}')")
fi
for t in "${TARGETS[@]}"; do package_target "$t"; done
echo "OK -> $DIST/"
