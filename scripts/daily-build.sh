#!/usr/bin/env bash
# Daily trio build — runs on the host that can build a given target NATIVELY,
# then publishes the versioned tarballs to the configured release host.
# Cron-driven (see docs/DAILY-BUILDS.md). Supported roles (auto-detected from
# the platform, or set explicitly in ~/.zoder-build.env):
#
#   macos-arm64  (Apple Silicon macOS)
#                -> aarch64-apple-darwin       (native cargo)
#                   aarch64-unknown-linux-gnu   (native arm64 Docker)
#   linux-amd64  (x86_64 Linux)
#                -> x86_64-unknown-linux-gnu  (amd64 Docker)
#   linux-arm64  (arm64 Linux)
#                -> aarch64-unknown-linux-gnu (native arm64 Docker)
#
# Intel macOS (Darwin:x86_64) is deliberately unsupported: the macOS leg only
# produces the arm64 Darwin binary. Auto-detection on an Intel macOS host exits
# with an unsupported-platform error; set ZODER_BUILD_ROLE explicitly only for
# one of the three roles above.
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

# Release-host publish target (set ZODER_PUB_HOST, e.g. root@release.example.invalid).
PUB_HOST="${ZODER_PUB_HOST:?set ZODER_PUB_HOST to the release host}"
PUB_PW="${ZODER_PUB_PW:-$(cat "${ZODER_PUB_PW_FILE:-$HOME/.config/zoder/publish-pw}" 2>/dev/null)}"  # from the environment or a mode-600 file; never commit it
[ -n "$PUB_PW" ] || { echo "daily-build: set ZODER_PUB_PW or ZODER_PUB_PW_FILE" >&2; exit 2; }
PUB_ROOT="/mnt/datapool/zoder-releases"

# GitHub rolling-nightly prerelease (optional; gated on GH_TOKEN in the env).
GH_REPO="ncz-os/zoder"
GH_NIGHTLY_TAG="nightly"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }

# Role selects which target(s) to build. Prefer an explicit ZODER_BUILD_ROLE
# (set in ~/.zoder-build.env) or derive it from the platform; no host names are
# involved.
host_short="$(hostname -s 2>/dev/null || hostname)"
platform="$(uname -s):$(uname -m)"
MODE="${ZODER_BUILD_ROLE:-}"
if [ -z "$MODE" ]; then
  case "$platform" in
    Darwin:arm64)              MODE="macos-arm64" ;;
    Darwin:x86_64)
      log "ERROR: unsupported platform '$platform' on host '$host_short': Intel macOS is not supported (the macOS leg only builds the arm64 Darwin binary). Use a supported host, or set ZODER_BUILD_ROLE explicitly to macos-arm64, linux-amd64, or linux-arm64 only if that role is correct for this machine."
      exit 2
      ;;
    Linux:x86_64|Linux:amd64)  MODE="linux-amd64" ;;
    Linux:aarch64|Linux:arm64) MODE="linux-arm64" ;;
  esac
fi
if [ -z "$MODE" ]; then
  log "ERROR: cannot infer a build role on host '$host_short' for platform '$platform'; accepted ZODER_BUILD_ROLE values: macos-arm64, linux-amd64, linux-arm64. Update ~/.zoder-build.env and set ZODER_BUILD_ROLE to one of these values."
  exit 2
fi
case "$MODE" in
  macos-arm64|linux-amd64|linux-arm64) ;;
  *) log "ERROR: unknown ZODER_BUILD_ROLE '$MODE' on host '$host_short'; accepted values: macos-arm64, linux-amd64, linux-arm64. Update ~/.zoder-build.env and set ZODER_BUILD_ROLE to one of these values."; exit 2 ;;
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

# Build one linux target inside a pinned rust container (native to the host
# arch), reusing this checkout. The container runs
# package.sh for its own host target, so no cross is involved.
build_linux_in_docker() {
  local platform="$1"
  log "linux build via $RUST_IMAGE ($platform)"
  # --user: without it the container writes dist/ as root, and the NEXT run's
  # cleanup dies with "rm: cannot remove ... Permission denied" -- which aborts
  # the leg and silently publishes nothing. Observed on an arm64 Linux host 2026-08-03.
  #
  # ZEROCLAW_REF=master, not zoder-integration. The macOS leg runs package.sh
  # natively and therefore inherits build.sh's default of upstream
  # zeroclaw-labs/zeroclaw master, while the Linux legs pinned a side branch --
  # so the same nightly shipped zeroclaw built from two different sources
  # depending on platform. Operator directive 2026-08-04: the nightly builds
  # master from upstream. Set ZEROCLAW_REF in the environment to pin deliberately.
  docker run --rm --platform "$platform" \
    --user "$(id -u):$(id -g)" \
    -e "ZEROCLAW_REPO=$ZC_URL" \
    -e "ZEROCLAW_REF=${ZEROCLAW_REF:-master}" \
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
  macos-arm64)
    log "darwin build (native cargo)"
    ZEROCLAW_REPO="$ZC_URL" ZEROCLAW_REF=zoder-integration \
      bash scripts/package.sh aarch64-apple-darwin
    # arm64 linux in a native arm64 container (separate .zeroclaw-src/target).
    build_linux_in_docker linux/arm64
    ;;
  linux-amd64)
    build_linux_in_docker linux/amd64
    ;;
  linux-arm64)
    # Native arm64 Linux host: arm64 container on arm64 hardware, so
    # linux/arm64 runs with no qemu emulation.
    build_linux_in_docker linux/arm64
    ;;
esac

# Stamp the resolved commit next to the artifacts so a tarball is traceable.
echo "$SHA" > "dist/GIT_COMMIT"
log "artifacts:"; ls -1 dist/*.tar.gz 2>/dev/null || { log "ERROR: no tarballs produced"; exit 1; }

# Publish to the release host: a dated dir plus a per-host `latest` mirror.
DEST="$PUB_ROOT/$DATE-$SHA"
sshpass -p "$PUB_PW" ssh -o StrictHostKeyChecking=no "$PUB_HOST" "mkdir -p '$DEST' '$PUB_ROOT/latest'"
for f in dist/*.tar.gz dist/*.sha256 dist/GIT_COMMIT; do
  [ -e "$f" ] || continue
  sshpass -p "$PUB_PW" scp -o StrictHostKeyChecking=no "$f" "$PUB_HOST:$DEST/"
  sshpass -p "$PUB_PW" scp -o StrictHostKeyChecking=no "$f" "$PUB_HOST:$PUB_ROOT/latest/"
done
log "published -> $PUB_HOST:$DEST"

# Rolling `nightly` GitHub prerelease (optional). Each host uploads ONLY the
# arch tarballs it built, with --clobber, so the single `nightly` release
# accumulates every arch across the build hosts. The git tag is cosmetic for a
# rolling artifact; the notes + the
# GIT_COMMIT asset record the actual source SHA. Gated on GH_TOKEN so the build
# still succeeds (release-host-only) when no GitHub token is configured.
if [ -n "${GH_TOKEN:-}" ]; then
  export GH_TOKEN
  log "publishing rolling nightly prerelease -> github.com/$GH_REPO"
  if ! gh release view "$GH_NIGHTLY_TAG" -R "$GH_REPO" >/dev/null 2>&1; then
    gh release create "$GH_NIGHTLY_TAG" -R "$GH_REPO" --prerelease --target main \
      --title "zoder nightly (rolling)" \
      --notes "Rolling nightly trio build from main, produced by scripts/daily-build.sh on the build hosts. The git tag is cosmetic; see GIT_COMMIT and the notes for the source SHA." \
      || log "WARN: nightly release create raced/failed (continuing)"
  fi
  # Force published + prerelease every run: ncz-os/zoder is a gitlab->github
  # push-mirror with ref-restricted tags, so a release can end up stuck as a
  # DRAFT (no `nightly` tag). --draft=false publishes it (which creates the tag)
  # and is idempotent once published.
  gh release edit "$GH_NIGHTLY_TAG" -R "$GH_REPO" --draft=false --prerelease \
    --notes "Rolling nightly trio build from main (scripts/daily-build.sh). Last update: $(date -u +%Y-%m-%dT%H:%MZ) by ${host_short} @ ${SHA} (v${VERSION}). Artifacts accumulate across hosts: darwin-arm64 + linux-arm64 from the macOS/arm64 builders, x86_64-linux from the x86_64 builder." \
    >/dev/null 2>&1 || true
  if gh release upload "$GH_NIGHTLY_TAG" -R "$GH_REPO" dist/*.tar.gz dist/*.sha256 --clobber; then
    log "github nightly updated with $(ls dist/*.tar.gz | wc -l | tr -d ' ') asset(s) from $host_short"
  else
    log "WARN: github nightly upload failed (release-host publish still succeeded)"
  fi
else
  log "GH_TOKEN unset; skipping GitHub nightly (release-host-only)"
fi

log "DONE"
