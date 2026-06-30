#!/usr/bin/env bash
# Install smoke test: verify an assembled/installed zoder bundle actually runs.
# This is the press/insurance gate — a one-line install must produce working
# binaries. Usage:
#   scripts/install-smoke.sh [DIR]    # DIR holding the binaries (default: PATH)
set -uo pipefail
DIR="${1:-}"
bin() { if [ -n "$DIR" ]; then printf '%s/%s' "$DIR" "$1"; else printf '%s' "$1"; fi; }
fail=0
ok()   { echo "ok    $*"; }
bad()  { echo "FAIL  $*"; fail=1; }

# zoder CLI — must report a version.
if "$(bin zoder)" --version >/dev/null 2>&1; then ok "zoder --version"; else bad "zoder --version"; fi

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

if [ "$fail" -eq 0 ]; then echo "INSTALL SMOKE: PASS"; else echo "INSTALL SMOKE: FAIL"; exit 1; fi
