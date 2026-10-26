#!/usr/bin/env bash
# Install smoke test: verify an assembled/installed zoder bundle actually runs.
# This is the press/insurance gate — a one-line install must produce working
# binaries AND a usable starter config.
#
# Usage:
#   scripts/install-smoke.sh [DIR] [--profile standalone|fleet] [--zoder-only]
#     DIR             directory holding the binaries (default: PATH)
#     --profile NAME  deployment profile to initialize (default: standalone)
#     --zoder-only    check `zoder` only (for CI jobs that do not build the
#                     full zeroclaw/zerocode bundle)
#
# The profile check runs entirely in a temp $ZODER_HOME: it writes the starter
# config, validates it with each hosted provider key in turn, and resolves an
# exec route with --dry-run (no network, no model call).
set -uo pipefail
DIR=""
PROFILE="standalone"
ZODER_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --profile) PROFILE="${2:?--profile needs standalone or fleet}"; shift 2 ;;
    --zoder-only) ZODER_ONLY=1; shift ;;
    --help | -h)
      sed -n '2,14p' "$0"
      exit 0
      ;;
    *) DIR="$1"; shift ;;
  esac
done
case "$PROFILE" in
  standalone | fleet) ;;
  *) echo "FAIL  unknown profile $PROFILE"; exit 1 ;;
esac

bin() { if [ -n "$DIR" ]; then printf '%s/%s' "$DIR" "$1"; else printf '%s' "$1"; fi; }
fail=0
ok()   { echo "ok    $*"; }
bad()  { echo "FAIL  $*"; fail=1; }

# zoder CLI — must report a version.
if "$(bin zoder)" --version >/dev/null 2>&1; then ok "zoder --version"; else bad "zoder --version"; fi

if [ "$ZODER_ONLY" = "1" ]; then
  echo "skip  zeroclaw/zerocode/goose (--zoder-only)"
else
  # zeroclaw engine — must respond to --help (default engine).
  if "$(bin zeroclaw)" --help >/dev/null 2>&1; then ok "zeroclaw --help"; else bad "zeroclaw --help"; fi

  # zerocode TUI — exists + executable only (don't launch the TUI in a smoke).
  if [ -x "$(bin zerocode)" ]; then ok "zerocode present"; else bad "zerocode present"; fi

  # goose second engine — OPTIONAL (ZODER_SKIP_GOOSE bundles omit it). If present,
  # `goose acp` must be invocable (the dual-engine gate).
  if [ -x "$(bin goose)" ] || command -v goose >/dev/null 2>&1; then
    if "$(bin goose)" acp --help >/dev/null 2>&1; then ok "goose acp"; else bad "goose acp"; fi
  else
    echo "skip  goose (not bundled; --engine goose unavailable)"
  fi
fi

# ── Deployment profile: init + validate + resolve a route ──────────
# Keep every file under the temp home; never read or write the caller's
# $ZODER_HOME. The repo corpus is copied in so `exec --dry-run` can route.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
smoke_home=$(mktemp -d "${TMPDIR:-/tmp}/zoder-install-smoke.XXXXXX")
cleanup_smoke() { rm -rf "$smoke_home"; }
trap cleanup_smoke EXIT

if [ -f "$ROOT/corpus/model_corpus.json" ]; then
  cp "$ROOT/corpus/model_corpus.json" "$smoke_home/model_corpus.json"
else
  bad "corpus/model_corpus.json missing; exec --dry-run cannot resolve a route"
fi
[ -f "$ROOT/pricing/catalog.json" ] && cp "$ROOT/pricing/catalog.json" "$smoke_home/pricing.json"

if ZODER_HOME="$smoke_home" ZEROCLAW_CONFIG_DIR="$smoke_home" "$(bin zoder)" init --profile "$PROFILE" >/dev/null 2>&1; then
  ok "zoder init --profile $PROFILE"
else
  bad "zoder init --profile $PROFILE"
fi
case "$PROFILE" in
  standalone) cfg_file="$smoke_home/config.standalone.toml" ;;
  fleet) cfg_file="$smoke_home/config.fleet.toml" ;;
esac
if [ -f "$cfg_file" ]; then ok "config written: $(basename "$cfg_file")"; else bad "config not written: $cfg_file"; fi

# A fresh install must not overwrite that config on a second init.
before=$(cat "$cfg_file" 2>/dev/null || true)
ZODER_HOME="$smoke_home" ZEROCLAW_CONFIG_DIR="$smoke_home" "$(bin zoder)" init --profile "$PROFILE" >/dev/null 2>&1
after=$(cat "$cfg_file" 2>/dev/null || true)
if [ "$before" = "$after" ]; then ok "zoder init is idempotent"; else bad "zoder init overwrote an existing config"; fi

# Validate with each hosted key on its own. For fleet (no providers here) the
# base config still validates; the fleet config repo supplies routes.
if ZODER_HOME="$smoke_home" ZEROCLAW_CONFIG_DIR="$smoke_home" DEEPSEEK_API_KEY=smoke-key \
    "$(bin zoder)" config --validate >/dev/null 2>&1; then
  ok "config --validate with DEEPSEEK_API_KEY"
else
  bad "config --validate with DEEPSEEK_API_KEY"
fi
if ZODER_HOME="$smoke_home" ZEROCLAW_CONFIG_DIR="$smoke_home" MINIMAX_API_KEY=smoke-key \
    "$(bin zoder)" config --validate >/dev/null 2>&1; then
  ok "config --validate with MINIMAX_API_KEY"
else
  bad "config --validate with MINIMAX_API_KEY"
fi

if [ "$PROFILE" = "standalone" ]; then
  if ZODER_HOME="$smoke_home" ZEROCLAW_CONFIG_DIR="$smoke_home" DEEPSEEK_API_KEY=smoke-key \
      "$(bin zoder)" exec --dry-run "smoke" 2>/dev/null | grep -q '^\[dry-run\].*model='; then
    ok "exec --dry-run resolves a route"
  else
    bad "exec --dry-run did not resolve a route"
  fi
fi

if [ "$fail" -eq 0 ]; then echo "INSTALL SMOKE: PASS"; else echo "INSTALL SMOKE: FAIL"; exit 1; fi
