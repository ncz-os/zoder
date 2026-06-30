#!/usr/bin/env bash
# Daily trio build — runs on the host that can build a given target NATIVELY,
# then publishes the versioned tarballs to ARGONAS. Cron-driven (see
# docs/DAILY-BUILDS.md). Host -> target mapping (each builds where it is native,
# so we never fight cross-from-macOS toolchain breakage):
#
#   ULTRA (.60, Apple Silicon)  -> aarch64-apple-darwin       (native cargo)
#                                  aarch64-unknown-linux-gnu   (native arm64 Docker)
#   HYDRA (.78, x86_64 Linux)   -> x86_64-unknown-linux-gnu    (amd64 Docker)
#
# Linux targets build inside a pinned `rust:1.94` container so the release
# toolchain matches the GitLab quality gate exactly, regardless of the host's
# own rust. macOS binaries must build natively (no Docker for Mach-O).
#
# Secrets: a host-local, NON-committed `~/.zoder-build.env` must export
#   ZODER_PAT=glpat-...        # gitlab read token for zoder + zeroclaw clones
# Optional overrides: ZODER_REF (default main), ZODER_DAILY_WORK, RUST_IMAGE.
set -euo pipefail

# shellcheck disable=SC1090
[ -f "$HOME/.zoder-build.env" ] && . "$HOME/.zoder-build.env"
: "${ZODER_PAT:?set ZODER_PAT in ~/.zoder-build.env}"

ZODER_REF="${ZODER_REF:-main}"
WORK="${ZODER_DAILY_WORK:-$HOME/zoder-daily}"
RUST_IMAGE="${RUST_IMAGE:-rust:1.94}"
ZODER_URL="https://oauth2:${ZODER_PAT}@gitlab.com/ncz-os/zoder.git"
ZC_URL="https://oauth2:${ZODER_PAT}@gitlab.com/ncz-os/zeroclaw.git"

# ARGONAS publish target (root-owned NFS git/release store).
PUB_HOST="root@192.168.207.101"
PUB_PW="Gumbo@Kona1b"
PUB_ROOT="/mnt/datapool/zoder-releases"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }

# Role selects which target(s) to build. Prefer an explicit ZODER_BUILD_ROLE
# (set in ~/.zoder-build.env) since hostnames are not reliable (ULTRA's host
# name is "MacBookPersonal"); fall back to a hostname guess.
host_short="$(hostname -s 2>/dev/null || hostname)"
MODE="${ZODER_BUILD_ROLE:-}"
if [ -z "$MODE" ]; then
  case "$host_short" in
    ULTRA|ultra*|Ultra*|MacBook*|macbook*)  MODE="ultra" ;;
    HYDRA|hydra*|Hydra*)                     MODE="hydra" ;;
  esac
fi
case "$MODE" in
  ultra|hydra) ;;
  *) log "ERROR: no daily-build role for host '$host_short' (set ZODER_BUILD_ROLE=ultra|hydra in ~/.zoder-build.env)"; exit 2 ;;
esac

mkdir -p "$WORK"
cd "$WORK"
if [ -d zoder/.git ]; then
  git -C zoder remote set-url origin "$ZODER_URL"
  git -C zoder fetch -q origin "$ZODER_REF"
  git -C zoder checkout -q -B "$ZODER_REF" "origin/$ZODER_REF"
else
  git clone -q -b "$ZODER_REF" "$ZODER_URL" zoder
fi
cd zoder
SHA="$(git rev-parse --short HEAD)"
VERSION="$(awk -F'"' '/^version[[:space:]]*=/{print $2; exit}' Cargo.toml)"
DATE="$(date -u +%Y%m%d)"
log "host=$host_short mode=$MODE ref=$ZODER_REF sha=$SHA version=$VERSION"

rm -rf dist
mkdir -p dist

# Build one linux target inside a pinned rust container (native to the host arch
# — arm64 on ULTRA, amd64 on HYDRA), reusing this checkout. The container runs
# package.sh for its own host target, so no cross is involved.
build_linux_in_docker() {
  local platform="$1"
  log "linux build via $RUST_IMAGE ($platform)"
  docker run --rm --platform "$platform" \
    -e "ZEROCLAW_REPO=$ZC_URL" \
    -e ZEROCLAW_REF=zoder-integration \
    -e GIT_TERMINAL_PROMPT=0 \
    -v "$PWD":/src -w /src \
    "$RUST_IMAGE" bash -c '
      set -e
      git config --global --add safe.directory /src
      git config --global --add safe.directory /src/.zeroclaw-src 2>/dev/null || true
      bash scripts/package.sh
    '
}

case "$MODE" in
  ultra)
    log "darwin build (native cargo)"
    ZEROCLAW_REPO="$ZC_URL" ZEROCLAW_REF=zoder-integration \
      bash scripts/package.sh aarch64-apple-darwin
    # arm64 linux in a native arm64 container (separate .zeroclaw-src/target).
    build_linux_in_docker linux/arm64
    ;;
  hydra)
    build_linux_in_docker linux/amd64
    ;;
esac

# Stamp the resolved commit next to the artifacts so a tarball is traceable.
echo "$SHA" > "dist/GIT_COMMIT"
log "artifacts:"; ls -1 dist/*.tar.gz 2>/dev/null || { log "ERROR: no tarballs produced"; exit 1; }

# Publish to ARGONAS: a dated dir plus a per-host `latest` mirror.
DEST="$PUB_ROOT/$DATE-$SHA"
sshpass -p "$PUB_PW" ssh -o StrictHostKeyChecking=no "$PUB_HOST" "mkdir -p '$DEST' '$PUB_ROOT/latest'"
for f in dist/*.tar.gz dist/*.sha256 dist/GIT_COMMIT; do
  [ -e "$f" ] || continue
  sshpass -p "$PUB_PW" scp -o StrictHostKeyChecking=no "$f" "$PUB_HOST:$DEST/"
  sshpass -p "$PUB_PW" scp -o StrictHostKeyChecking=no "$f" "$PUB_HOST:$PUB_ROOT/latest/"
done
log "published -> ARGONAS:$DEST"
log "DONE"
