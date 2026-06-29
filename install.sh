#!/bin/sh
set -eu

# ── zoder installer ──────────────────────────────────────────────
# Installs the version-matched trio (zoder + zerocode + zeroclaw) from the
# pre-built GitHub release for your platform. POSIX sh — no bash required.
# Works on Debian, Alpine-glibc, macOS (Apple Silicon), everywhere with curl.
#
# Interactive:
#   curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/install.sh | sh
#
# Agent / non-interactive (no prompts, machine-readable failures):
#   curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/install.sh \
#     | ZODER_VERSION=v0.2.0 ZODER_BIN_DIR="$HOME/.local/bin" sh
#
# Knobs (env or flags):
#   ZODER_VERSION   / --version <tag>   release tag to install   (default: latest)
#   ZODER_BIN_DIR   / --bin-dir <dir>   install dir              (default: $HOME/.local/bin)
#   ZODER_REPO      / --repo <o/r>      owner/repo               (default: ncz-os/zoder)
#   ZODER_NO_VERIFY / --no-verify       skip checksum verify     (not recommended)
#                     --dry-run         show actions, change nothing
#                     --help

REPO="${ZODER_REPO:-ncz-os/zoder}"
VERSION="${ZODER_VERSION:-latest}"
BIN_DIR="${ZODER_BIN_DIR:-$HOME/.local/bin}"
NO_VERIFY="${ZODER_NO_VERIFY:-0}"
DRY_RUN=false
BINS="zoder zerocode zeroclaw"

# ── Output helpers (terminal-aware) ──────────────────────────────

if [ -t 1 ]; then
  BOLD='\033[1m' GREEN='\033[32m' YELLOW='\033[33m' RED='\033[31m' RESET='\033[0m'
else
  BOLD='' GREEN='' YELLOW='' RED='' RESET=''
fi

info() { printf "  ${GREEN}✓${RESET} %s\n" "$*"; }
warn() { printf "  ${YELLOW}⚠${RESET} %s\n" "$*" >&2; }
die() {
  printf "  ${RED}✗${RESET} %s\n" "$*" >&2
  exit 1
}
bold() { printf "${BOLD}%s${RESET}" "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

# ── Usage ─────────────────────────────────────────────────────────

usage() {
  cat <<EOF
$(bold "zoder installer") — version-matched trio (zoder + zerocode + zeroclaw)

Usage: install.sh [options]

Options:
  --version <tag>   Release tag to install (default: latest)
  --bin-dir <dir>   Install directory (default: \$HOME/.local/bin)
  --repo <owner/r>  Source repository (default: ncz-os/zoder)
  --no-verify       Skip SHA256 checksum verification (not recommended)
  --dry-run         Print what would happen, install nothing
  --help            Show this help

Each release ships a prebuilt trio for linux-x86_64, linux-aarch64, and
macOS-arm64. The installer detects your platform, verifies the download
against SHA256SUMS, and installs zoder, zerocode, and zeroclaw.
EOF
}

# ── Arg parsing (flags override env) ─────────────────────────────

while [ $# -gt 0 ]; do
  case "$1" in
  --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
  --bin-dir) BIN_DIR="${2:?--bin-dir needs a dir}"; shift 2 ;;
  --repo) REPO="${2:?--repo needs owner/repo}"; shift 2 ;;
  --no-verify) NO_VERIFY=1; shift ;;
  --dry-run) DRY_RUN=true; shift ;;
  -h | --help) usage; exit 0 ;;
  *) die "unknown option: $1 (try --help)" ;;
  esac
done

# ── Downloader ────────────────────────────────────────────────────

dl() { # dl URL OUTFILE
  if have curl; then curl -fsSL "$1" -o "$2"
  elif have wget; then wget -qO "$2" "$1"
  else die "need curl or wget"; fi
}

# ── Platform / target triple detection ───────────────────────────

musl_present() {
  # Glob-safe: a literal `[ -e glob ]` breaks when the glob matches 0 or >1
  # files, so iterate instead.
  for f in /lib/ld-musl-*.so.1; do
    [ -e "$f" ] && return 0
  done
  return 1
}

detect_libc() {
  if musl_present ||
    ldd --version 2>&1 | grep -qi musl ||
    { [ -r /etc/os-release ] && grep -qiE 'alpine|postmarket' /etc/os-release; }; then
    echo "musl"
  else
    echo "gnu"
  fi
}

detect_target_triple() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
  Darwin)
    # A Rosetta-translated shell on Apple Silicon reports x86_64 from
    # `uname -m`; consult sysctl to recover the true CPU so we never hand an
    # arm64 Mac the wrong binary ("bad CPU type in executable").
    if [ "$arch" = "arm64" ] || [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = "1" ]; then
      echo "aarch64-apple-darwin"
    else
      echo "x86_64-apple-darwin"
    fi
    ;;
  Linux)
    libc=$(detect_libc)
    case "$arch" in
    x86_64 | amd64) echo "x86_64-unknown-linux-${libc}" ;;
    aarch64 | arm64) echo "aarch64-unknown-linux-${libc}" ;;
    *) echo "" ;;
    esac
    ;;
  *) echo "" ;;
  esac
}

# ── Shell profile / PATH hint ─────────────────────────────────────

shell_export_syntax() {
  # The literal `$PATH` is meant to appear in the printed export line for the
  # user to copy, so single quotes (no expansion) are intentional here.
  # shellcheck disable=SC2016
  case "$(basename "${SHELL:-/bin/sh}")" in
  fish) printf 'set -gx PATH "%s" $PATH' "$BIN_DIR" ;;
  *) printf 'export PATH="%s:$PATH"' "$BIN_DIR" ;;
  esac
}

shell_profile() {
  case "$(basename "${SHELL:-/bin/sh}")" in
  zsh) echo "$HOME/.zshrc" ;;
  fish) echo "$HOME/.config/fish/config.fish" ;;
  *) echo "$HOME/.bashrc" ;;
  esac
}

# ── Resolve platform + version ───────────────────────────────────

triple=$(detect_target_triple)
[ -n "$triple" ] || die "unsupported platform: $(uname -s)/$(uname -m) (Linux or macOS-arm64 only; on Windows use WSL)"

if [ "$VERSION" = "latest" ]; then
  tmptag=$(mktemp)
  dl "https://api.github.com/repos/${REPO}/releases/latest" "$tmptag" || die "cannot reach GitHub API to resolve latest"
  VERSION=$(sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' "$tmptag" | head -1)
  rm -f "$tmptag"
  [ -n "$VERSION" ] || die "could not resolve latest release tag"
fi

ver_no_v="${VERSION#v}"
asset="zoder-${ver_no_v}-${triple}.tar.gz"
base="https://github.com/${REPO}/releases/download/${VERSION}"

echo
printf "%s\n" "$(bold "Installing zoder ${VERSION} (pre-built trio)")"
info "Platform: $triple"
info "Source:   ${base}/${asset}"
info "Bins:     ${BINS} → ${BIN_DIR}"
echo

if [ "$DRY_RUN" = true ]; then
  info "[dry-run] would download ${base}/${asset}"
  info "[dry-run] would verify against ${base}/SHA256SUMS"
  info "[dry-run] would install ${BINS} to ${BIN_DIR}"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Fetch the checksum manifest first: it lists every published asset, so a
# missing entry means "no prebuilt for this platform" (distinct from a network
# failure), and we never install a tarball we couldn't verify.
verify=1
if [ "$NO_VERIFY" = "1" ]; then
  verify=0
  warn "checksum verification disabled (--no-verify / ZODER_NO_VERIFY=1)"
elif dl "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" 2>/dev/null; then
  want=$(grep " ${asset}\$" "${tmp}/SHA256SUMS" | awk '{print $1}' | head -1)
  [ -n "$want" ] || {
    avail=$(awk '{print "      "$2}' "${tmp}/SHA256SUMS" | grep '\.tar\.gz' || true)
    die "no prebuilt published for ${triple} in ${VERSION}. Available:
${avail}"
  }
else
  verify=0
  warn "no SHA256SUMS published for ${VERSION}; skipping verify"
fi

dl "${base}/${asset}" "${tmp}/${asset}" || die "download failed: ${base}/${asset}"

if [ "$verify" = "1" ]; then
  if have sha256sum; then got=$(sha256sum "${tmp}/${asset}" | awk '{print $1}')
  elif have shasum; then got=$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')
  else die "no checksum tool (sha256sum/shasum); re-run with --no-verify to override"; fi
  [ "$got" = "$want" ] || die "checksum mismatch for ${asset} — download may be corrupt. Expected ${want}, got ${got}"
  info "Checksum verified"
fi

# ── Install ───────────────────────────────────────────────────────

tar -xzf "${tmp}/${asset}" -C "${tmp}"
mkdir -p "$BIN_DIR"
n=0
for b in $BINS; do
  f=$(find "$tmp" -type f -name "$b" -perm -u+x 2>/dev/null | head -1)
  [ -n "$f" ] || f=$(find "$tmp" -type f -name "$b" 2>/dev/null | head -1)
  if [ -n "$f" ]; then
    install -m 0755 "$f" "${BIN_DIR}/${b}"
    n=$((n + 1))
  else
    warn "binary not found in tarball: $b"
  fi
done
[ "$n" -gt 0 ] || die "no binaries found in ${asset}"
info "Installed $n binar$([ "$n" = 1 ] && echo y || echo ies) to ${BIN_DIR}"

# ── PATH hint ─────────────────────────────────────────────────────

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*)
  warn "${BIN_DIR} is not on your PATH"
  printf "    Add to %s:\n    %s\n" "$(shell_profile)" "$(shell_export_syntax)"
  ;;
esac

echo
"${BIN_DIR}/zoder" --version 2>/dev/null || true
printf "%s\n" "$(bold "Done.") Run $(bold zoder) to start, or $(bold "zerocode") for the TUI."
