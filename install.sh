#!/bin/sh
# zoder installer — fetches the version-matched trio (zoder + zerocode + zeroclaw)
# for your platform from the GitHub release and installs it to a bin dir.
#
# Interactive:
#   curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/install.sh | sh
#
# Agent / non-interactive (no prompts, machine-readable on failure):
#   curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/install.sh \
#     | ZODER_VERSION=v0.2.0 ZODER_BIN_DIR="$HOME/.local/bin" sh
#
# Knobs (env):
#   ZODER_VERSION   release tag to install         (default: latest)
#   ZODER_BIN_DIR   install dir                     (default: $HOME/.local/bin)
#   ZODER_REPO      owner/repo                       (default: ncz-os/zoder)
#   ZODER_NO_VERIFY set to 1 to skip checksum verify (not recommended)
set -eu

REPO="${ZODER_REPO:-ncz-os/zoder}"
VERSION="${ZODER_VERSION:-latest}"
BIN_DIR="${ZODER_BIN_DIR:-$HOME/.local/bin}"

err() { echo "zoder-install: $*" >&2; }
die() { err "$*"; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- platform detection ---------------------------------------------------
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Linux)  os_t="unknown-linux-gnu" ;;
  Darwin) os_t="apple-darwin" ;;
  *) die "unsupported OS: $os (Linux or macOS only; on Windows use WSL)" ;;
esac
case "$arch" in
  x86_64|amd64)  arch_t="x86_64" ;;
  aarch64|arm64) arch_t="aarch64" ;;
  *) die "unsupported architecture: $arch" ;;
esac
if [ "$os_t" = "apple-darwin" ] && [ "$arch_t" = "x86_64" ]; then
  die "no x86_64 macOS build; Apple Silicon (arm64) only"
fi
target="${arch_t}-${os_t}"

# --- downloader -----------------------------------------------------------
dl() { # dl URL OUTFILE
  if have curl; then curl -fsSL "$1" -o "$2"
  elif have wget; then wget -qO "$2" "$1"
  else die "need curl or wget"; fi
}

# --- resolve version ------------------------------------------------------
if [ "$VERSION" = "latest" ]; then
  api="https://api.github.com/repos/${REPO}/releases/latest"
  tmptag="$(mktemp)"; dl "$api" "$tmptag" || die "cannot reach GitHub API"
  VERSION="$(sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' "$tmptag" | head -1)"
  rm -f "$tmptag"
  [ -n "$VERSION" ] || die "could not resolve latest release tag"
fi

ver_no_v="${VERSION#v}"
tarball="zoder-${ver_no_v}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/download/${VERSION}"
err "installing zoder ${VERSION} (${target}) -> ${BIN_DIR}"

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
dl "${base}/${tarball}" "${tmp}/${tarball}" || die "download failed: ${base}/${tarball}"

# --- verify checksum ------------------------------------------------------
if [ "${ZODER_NO_VERIFY:-0}" != "1" ]; then
  if dl "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" 2>/dev/null; then
    want="$(grep " ${tarball}\$" "${tmp}/SHA256SUMS" | awk '{print $1}' | head -1)"
    if [ -n "$want" ]; then
      if have sha256sum; then got="$(sha256sum "${tmp}/${tarball}" | awk '{print $1}')"
      elif have shasum;  then got="$(shasum -a 256 "${tmp}/${tarball}" | awk '{print $1}')"
      else got=""; err "no sha256 tool; skipping verify"; fi
      [ -z "$got" ] || [ "$got" = "$want" ] || die "checksum mismatch for ${tarball}"
    fi
  else
    err "no SHA256SUMS published; skipping verify"
  fi
fi

# --- install --------------------------------------------------------------
tar -xzf "${tmp}/${tarball}" -C "${tmp}"
mkdir -p "$BIN_DIR"
n=0
for b in zoder zerocode zeroclaw; do
  f="$(find "$tmp" -type f -name "$b" -perm -u+x 2>/dev/null | head -1)"
  [ -n "$f" ] || f="$(find "$tmp" -type f -name "$b" 2>/dev/null | head -1)"
  if [ -n "$f" ]; then install -m 0755 "$f" "${BIN_DIR}/${b}"; n=$((n+1)); fi
done
[ "$n" -gt 0 ] || die "no binaries found in ${tarball}"

err "installed ${n} binar$( [ "$n" = 1 ] && echo y || echo ies ) to ${BIN_DIR}"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) err "NOTE: ${BIN_DIR} is not on PATH — add: export PATH=\"${BIN_DIR}:\$PATH\"" ;;
esac
"${BIN_DIR}/zoder" --version 2>/dev/null || true
