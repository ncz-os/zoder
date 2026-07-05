#!/bin/sh
set -eu

# ── zoder installer ──────────────────────────────────────────────
# Installs the latest nightly "master" build of zoder from GitLab — the
# canonical, authoritative home of the project. GitLab CI rebuilds master every
# night and publishes a per-platform binary to a rolling generic package; this
# script fetches the one for your platform. POSIX sh — no bash required. Works
# on Debian, Alpine-glibc, macOS (Apple Silicon), everywhere with curl.
#
# Interactive (canonical, GitLab):
#   curl -fsSL https://gitlab.com/ncz-os/zoder/-/raw/master/install.sh | sh
#
# The same script is mirrored on GitHub for convenience — it still pulls the
# binaries from GitLab:
#   curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/master/install.sh | sh
#
# Agent / non-interactive (no prompts, machine-readable failures):
#   curl -fsSL https://gitlab.com/ncz-os/zoder/-/raw/master/install.sh \
#     | ZODER_BIN_DIR="$HOME/.local/bin" sh
#
# Knobs (env or flags):
#   ZODER_CHANNEL   / --channel <name>  master (rolling) or a YYYY-MM-DD date to
#                                       pin a specific nightly    (default: master)
#   ZODER_BIN_DIR   / --bin-dir <dir>   install dir              (default: $HOME/.local/bin)
#   ZODER_REPO      / --repo <o/r>      owner/repo (GitLab path) (default: ncz-os/zoder)
#   ZODER_HOST      / --host <host>     GitLab host              (default: gitlab.com)
#   ZODER_NO_VERIFY / --no-verify       skip checksum verify     (not recommended)
#   ZODER_NO_CORPUS / --no-corpus       don't seed corpus + pricing
#                     --dry-run         show actions, change nothing
#                     --help

REPO="${ZODER_REPO:-ncz-os/zoder}"
HOST="${ZODER_HOST:-gitlab.com}"
CHANNEL="${ZODER_CHANNEL:-master}"
BIN_DIR="${ZODER_BIN_DIR:-$HOME/.local/bin}"
NO_VERIFY="${ZODER_NO_VERIFY:-0}"
ZODER_HOME="${ZODER_HOME:-$HOME/.zoder}"
NO_CORPUS="${ZODER_NO_CORPUS:-0}"
DRY_RUN=false

# zoder is the product. zerocode + zeroclaw (the TUI trio) are installed too when
# the nightly publishes them; today the nightly ships `zoder` alone, so those are
# best-effort and skipped silently when absent (forward-compatible).
BIN_REQUIRED="zoder"
BIN_OPTIONAL="zerocode zeroclaw"

# Public, self-serve corpus + pricing, raw-fetched from GitLab master (the single
# source of truth). Seeding at install means a fresh zoder routes immediately
# instead of failing on a missing corpus, independent of any build process.
CORPUS_BASE="https://${HOST}/${REPO}/-/raw/master"

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
$(bold "zoder installer") — latest nightly master build (from GitLab)

Usage: install.sh [options]

Options:
  --channel <name>  Nightly channel: master (rolling) or a YYYY-MM-DD date
                    to pin a specific build (default: master)
  --bin-dir <dir>   Install directory (default: \$HOME/.local/bin)
  --repo <owner/r>  Source repository, GitLab path (default: ncz-os/zoder)
  --host <host>     GitLab host (default: gitlab.com)
  --no-verify       Skip SHA256 checksum verification (not recommended)
  --no-corpus       Don't seed the routing corpus + pricing catalog
  --dry-run         Print what would happen, install nothing
  --help            Show this help

GitLab CI rebuilds master nightly and publishes per-platform binaries for
linux-x86_64, linux-aarch64, and macOS-arm64. The installer detects your
platform, verifies the download against its published SHA256 when available,
and installs zoder into your bin directory.
EOF
}

# ── Arg parsing (flags override env) ─────────────────────────────

while [ $# -gt 0 ]; do
  case "$1" in
  --channel) CHANNEL="${2:?--channel needs a name}"; shift 2 ;;
  --bin-dir) BIN_DIR="${2:?--bin-dir needs a dir}"; shift 2 ;;
  --repo) REPO="${2:?--repo needs owner/repo}"; shift 2 ;;
  --host) HOST="${2:?--host needs a host}"; shift 2 ;;
  --no-verify) NO_VERIFY=1; shift ;;
  --no-corpus) NO_CORPUS=1; shift ;;
  --dry-run) DRY_RUN=true; shift ;;
  -h | --help) usage; exit 0 ;;
  *) die "unknown option: $1 (try --help)" ;;
  esac
done

# ── Downloader ────────────────────────────────────────────────────

# dl URL OUTFILE — non-atomic plain download. Used for artifact payloads (which
# curl/wget write straight from the network). Corpus/pricing JSON uses
# `dl_atomic` below so a partial body never poisons the canonical catalog file.
dl() { # dl URL OUTFILE
  if have curl; then curl -fsSL "$1" -o "$2"
  elif have wget; then wget -qO "$2" "$1"
  else die "need curl or wget"; fi
}

# Print the SHA256 hex of FILE (64 chars) using whatever tool is available.
# Emits an empty string if no tool is installed; callers in the verify path
# MUST treat that empty result as fatal when verification is required.
sha256_file() {
  if have sha256sum; then sha256sum "$1" 2>/dev/null | awk '{print $1}'
  elif have shasum; then shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'
  else printf ''
  fi
}

# Read the checksum file in the exact format produced by CI: one 64-character
# hexadecimal digest, optionally followed by a final newline. Reject HTML,
# filenames, signatures, and other bodies instead of stripping characters out
# until something digest-shaped appears.
read_checksum() {
  # Command substitution removes trailing newlines but preserves any embedded
  # newline, which the non-hex check below rejects.
  value=$(cat "$1" 2>/dev/null || true)
  [ "${#value}" -eq 64 ] || return 1
  case "$value" in
  *[!0-9a-fA-F]*) return 1 ;;
  esac
  printf '%s' "$value"
}

# dl_atomic URL OUTFILE MIN_BYTES — download to a sibling `.part` file, reject
# anything shorter than MIN_BYTES (so curl can't silently leave a 0-byte file
# behind when the response is truncated mid-flight), then rename into place.
# Caller is responsible for gating on "[ -f OUTFILE ]" beforehand if it wants
# the "leave existing file alone" semantics.
dl_atomic() {
  url="$1"; out="$2"; min="${3:-1}"
  part="${out}.part.$$"
  rm -f "$part"
  if ! dl "$url" "$part" 2>/dev/null; then
    rm -f "$part"
    return 1
  fi
  if [ ! -s "$part" ]; then
    rm -f "$part"
    return 1
  fi
  size=$(wc -c <"$part" 2>/dev/null | tr -d ' ' || echo 0)
  if [ "${size:-0}" -lt "$min" ]; then
    rm -f "$part"
    return 1
  fi
  mkdir -p "$(dirname "$out")"
  mv "$part" "$out"
  return 0
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

# ── Resolve platform + package base ──────────────────────────────

triple=$(detect_target_triple)
[ -n "$triple" ] || die "unsupported platform: $(uname -s)/$(uname -m) (Linux or macOS-arm64 only; on Windows use WSL)"

# URL-encode owner/repo into a GitLab project path (ncz-os/zoder -> ncz-os%2Fzoder).
proj=$(printf '%s' "$REPO" | sed 's#/#%2F#g')
# Rolling nightly generic package: one raw binary per platform, overwritten each
# night by GitLab CI. No version to resolve — the channel IS the latest master.
pkg_base="https://${HOST}/api/v4/projects/${proj}/packages/generic/zoder-nightly/${CHANNEL}"

echo
printf "%s\n" "$(bold "Installing zoder — nightly ${CHANNEL} build (GitLab)")"
info "Platform: $triple"
info "Source:   ${pkg_base}/zoder-${triple}"
info "Install:  ${BIN_DIR}"
echo

if [ "$DRY_RUN" = true ]; then
  info "[dry-run] would download ${pkg_base}/zoder-${triple}"
  info "[dry-run] would verify against ${pkg_base}/zoder-${triple}.sha256 (if published)"
  info "[dry-run] would install zoder to ${BIN_DIR}"
  [ "$NO_CORPUS" = "1" ] || info "[dry-run] would seed corpus + pricing from ${CORPUS_BASE} into ${ZODER_HOME}"
  exit 0
fi

tmp=$(mktemp -d)
mkdir -p "$BIN_DIR"
txn_dir=$(mktemp -d "${BIN_DIR}/.zoder-install.XXXXXX")
transaction_active=0
installed_names=""

rollback_install() {
  for name in $installed_names; do
    if [ -f "${txn_dir}/backup-${name}" ]; then
      mv -f "${txn_dir}/backup-${name}" "${BIN_DIR}/${name}" || true
    else
      rm -f "${BIN_DIR}/${name}"
    fi
  done
}

# Kept as the explicit post-install smoke rollback entrypoint. Signal/error
# cleanup uses `rollback_install` directly while the transaction is active.
rollback_installed() {
  rollback_install
}

cleanup_install() {
  status=$?
  trap - EXIT INT TERM HUP
  if [ "$transaction_active" = 1 ]; then
    rollback_install
  fi
  rm -rf "$tmp" "$txn_dir"
  exit "$status"
}
trap cleanup_install EXIT INT TERM HUP

installed=0

# Two-phase install: stage (download + verify) all binaries into "$tmp", then
# install the whole set in one shot. If anything required fails or the smoke
# test trips, we drop the staged copy and never overwrite anything in BIN_DIR.
# This makes the install effectively transactional for the trio we care about.
required_failed=0
required_staged=0
for b in $BIN_REQUIRED; do
  asset="${b}-${triple}"
  url="${pkg_base}/${asset}"
  rm -f "${tmp}/${b}" "${tmp}/${b}.sha256"
  if ! dl "$url" "${tmp}/${b}" 2>/dev/null; then
    required_failed=$((required_failed + 1))
    warn "could not download ${asset}"
    continue
  fi

  # Mandatory checksum for required binaries. A HTTP-200 body that isn't
  # exactly 64 hex chars, a missing checksum tool, or a missing checksum file
  # is fatal — install an unverified binary is worse than refuse.
  if [ "$NO_VERIFY" = "1" ]; then
    warn "checksum verification disabled for ${b} (--no-verify / ZODER_NO_VERIFY=1)"
  elif dl "${url}.sha256" "${tmp}/${b}.sha256" 2>/dev/null; then
    want=$(read_checksum "${tmp}/${b}.sha256") ||
      die "malformed checksum for ${asset}: expected exactly one 64-character hex digest"
    got=$(sha256_file "${tmp}/${b}")
    [ -n "$got" ] || die "no sha256 tool available on this system; cannot verify ${asset} (re-run with --no-verify to override)"
    [ "$got" = "$want" ] || die "checksum mismatch for ${asset} — download may be corrupt. Expected ${want}, got ${got}"
    info "Checksum verified: ${b}"
  else
    die "no checksum published for ${asset}; refusing to install unverified binary (re-run with --no-verify to override)"
  fi
  required_staged=$((required_staged + 1))
done

if [ "$required_failed" -gt 0 ]; then
  die "no nightly build for ${triple} (channel ${CHANNEL}). The nightly builds x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, and aarch64-apple-darwin; musl and Intel macOS are not published."
fi
[ "$required_staged" -gt 0 ] || die "nothing staged for install"

# Optional binaries: best-effort. A missing nightly artifact or bad/missing
# checksum is a warn-and-skip — never blocks the install of `zoder` itself.
for b in $BIN_OPTIONAL; do
  asset="${b}-${triple}"
  url="${pkg_base}/${asset}"
  if ! dl "$url" "${tmp}/${b}" 2>/dev/null; then
    info "optional binary ${asset} not in this nightly; skipping"
    continue
  fi

  if [ "$NO_VERIFY" = "1" ]; then
    warn "checksum verification disabled for optional ${b} (--no-verify / ZODER_NO_VERIFY=1)"
  elif dl "${url}.sha256" "${tmp}/${b}.sha256" 2>/dev/null; then
    if ! want=$(read_checksum "${tmp}/${b}.sha256"); then
      warn "malformed checksum for optional ${asset}; skipping (not installed)"
      rm -f "${tmp}/${b}"
      continue
    fi
    got=$(sha256_file "${tmp}/${b}")
    if [ -z "$got" ]; then
      warn "no sha256 tool available; skipping optional ${b} (not installed)"
      rm -f "${tmp}/${b}"
      continue
    fi
    if [ "$got" != "$want" ]; then
      warn "checksum mismatch for optional ${asset}; skipping (not installed)"
      rm -f "${tmp}/${b}"
      continue
    fi
    info "Checksum verified (optional): ${b}"
  else
    warn "no checksum published for optional ${asset}; skipping (not installed)"
    rm -f "${tmp}/${b}"
    continue
  fi
done

# Prepare every replacement in a hidden directory on the destination
# filesystem. Each final `mv` is atomic; the active transaction trap restores
# the complete previous set on any error, signal, or interruption.
for b in $BIN_REQUIRED $BIN_OPTIONAL; do
  if [ -f "${tmp}/${b}" ]; then
    install -m 0755 "${tmp}/${b}" "${txn_dir}/new-${b}" ||
      die "failed to stage ${b} for install"
  fi
  if [ -f "${BIN_DIR}/${b}" ]; then
    cp -p "${BIN_DIR}/${b}" "${txn_dir}/backup-${b}" ||
      die "failed to back up existing ${b}"
  fi
  installed_names="${installed_names} ${b}"
done

transaction_active=1
for b in $BIN_REQUIRED $BIN_OPTIONAL; do
  if [ -f "${txn_dir}/new-${b}" ]; then
    mv -f "${txn_dir}/new-${b}" "${BIN_DIR}/${b}" ||
      die "failed to install ${b}; previous binary set will be restored"
    installed=$((installed + 1))
  else
    # The trio describes one release. If an optional member is absent from
    # this release, remove an older copy transactionally rather than leave a
    # stale zerocode/zeroclaw next to the newly installed zoder.
    rm -f "${BIN_DIR}/${b}" ||
      die "failed to remove stale optional ${b}; previous set will be restored"
  fi
done
transaction_active=0
[ "$installed" -gt 0 ] || die "nothing installed"
info "Installed $installed binar$([ "$installed" = 1 ] && echo y || echo ies) to ${BIN_DIR}"

# ── Seed the public corpus + pricing ──────────────────────────────
# Best-effort: a failed fetch (offline/proxy) never fails the install — zoder
# also self-heals these on first run. Existing files are left in place so a
# local `zoder refresh` / `zoder pricing sync` is never clobbered. Downloads
# are atomic (`.part` then `mv`) so a curl that's killed mid-response can
# never leave a truncated JSON file at the canonical path the CLI loads from.
seed_corpus() {
  [ "$NO_CORPUS" = "1" ] && {
    warn "corpus seeding skipped (--no-corpus / ZODER_NO_CORPUS=1)"
    return 0
  }
  mkdir -p "$ZODER_HOME"

  # Corpus goes to the SAME path the CLI resolves via `Config::corpus_path`
  # (=$ZODER_HOME/model_corpus.json). The corpus file is a megabyte-class
  # JSON document; enforce a 1 KiB minimum so an empty HTTP/204 body or a
  # truncated response is rejected as a failed seed, not a quiet one.
  if [ -f "$ZODER_HOME/model_corpus.json" ]; then
    info "Corpus already present at $ZODER_HOME/model_corpus.json (left as-is)"
  elif dl_atomic "${CORPUS_BASE}/corpus/model_corpus.json" \
      "$ZODER_HOME/model_corpus.json" 1024 2>/dev/null; then
    info "Seeded routing corpus → $ZODER_HOME/model_corpus.json"
  else
    warn "could not fetch corpus (zoder will self-heal on first run)"
  fi

  # Pricing goes to the SAME path the CLI loads via `Config::home().join(
  # "pricing.json")` (=$ZODER_HOME/pricing.json). A prior installer revision
  # wrote this catalog under a `data/` subdirectory that the CLI never
  # reads, so fresh installs ended up with an empty catalog. Same 1 KiB
  # minimum as the corpus.
  if [ -f "$ZODER_HOME/pricing.json" ]; then
    info "Pricing catalog already present (left as-is)"
  elif dl_atomic "${CORPUS_BASE}/pricing/catalog.json" \
      "$ZODER_HOME/pricing.json" 1024 2>/dev/null; then
    info "Seeded pricing catalog → $ZODER_HOME/pricing.json"
  else
    warn "could not fetch pricing catalog (run 'zoder pricing sync' later)"
  fi
}
seed_corpus

# ── PATH hint ─────────────────────────────────────────────────────

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*)
  warn "${BIN_DIR} is not on your PATH"
  printf "    Add to %s:\n    %s\n" "$(shell_profile)" "$(shell_export_syntax)"
  ;;
esac

echo
# Smoke test the freshly-installed `zoder` and roll back on failure: the
# smoke MUST be fatal. A non-fatal version check masks a broken binary
# (`bad CPU type`, broken shared lib, corrupt payload that bypassed a
# checksum mismatch) and prints "Done." on top of a non-working install.
smoke_failed=0
if ! "${BIN_DIR}/zoder" --version >/dev/null 2>&1; then
  smoke_failed=1
  warn "zoder --version failed after install; rolling back ${BIN_DIR}/zoder"
fi
if [ "$smoke_failed" -eq 1 ]; then
  rollback_installed
  die "zoder --version check failed after install; install was rolled back"
fi
info "Smoke verified: ${BIN_DIR}/zoder reports a version"
printf "%s\n" "$(bold "Done.") Run $(bold zoder) to start, or $(bold "zoder tui") for the TUI."
