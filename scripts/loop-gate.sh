#!/usr/bin/env bash
# Canonical zoder-loop --check gate (the un-fakeable oracle).
#
# Usage:  zoder loop -i task.md --check "bash scripts/loop-gate.sh" --reviewer z-ai/glm-5.1 ...
#
# The free loop converges ONLY when: it builds, clippy is clean (-D warnings),
# unit tests pass, AND — when the goose binary is present — the REAL `goose acp`
# integration tests pass (real handshake + a real streaming turn). A model-
# authored mock cannot fake these, which is what stops false convergence.
#
# Requires for the real-goose gate: goose >=1.37 on PATH; MINIMAX_API_KEY in env
# (the real-turn test skips itself gracefully if the key is absent).
set -euo pipefail
cargo build -p zoder-core -p zoder-cli
cargo clippy -p zoder-core -p zoder-cli --all-targets -- -D warnings
if command -v goose >/dev/null 2>&1; then
  echo "[loop-gate] goose present -> running unit + REAL goose acp integration tests"
  cargo test -p zoder-core --lib -- --include-ignored
else
  echo "[loop-gate] goose NOT on PATH -> unit tests only (real-goose gate skipped)"
  cargo test -p zoder-core --lib
fi
