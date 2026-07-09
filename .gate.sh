#!/usr/bin/env bash
# Canonical pre-submission gate for the exec-safety regression fix.
#
# Mirrors the project's published CI bar (.gitlab-ci.yml, .github/workflows)
# so a local-green run is exactly equivalent to a remote-green one:
#
#   1. cargo fmt  --all -- --check               (rustfmt, strict)
#   2. cargo clippy --workspace --all-targets --all-features -- -D warnings
#                                                    (clippy, warnings-as-errors)
#   3. cargo test --workspace --locked --all-features
#                                                    (unit + integration)
#
# Exit 0  => gate green (safe to submit / merge).
# Exit !0 => gate red    (fix the failures, do not submit).
#
# Run from the repo root:
#     bash .gate.sh
# or:
#     /.gate.sh      (the loop driver invokes this exact path verbatim)
set -euo pipefail

cd "$(dirname "$0")"

# Make sure the pinned Rust toolchain is active if rustup is present.
if command -v rustup >/dev/null 2>&1; then
    # shellcheck disable=SC1091
    rustup show active-toolchain >/dev/null 2>&1 || true
fi

# Ensure cargo is on PATH (Homebrew, rustup, system — wherever the host installs it).
export PATH="$HOME/.cargo/bin:$PATH"

echo "==[ gate ]============================================="
echo "phase 1/3  fmt --check"
cargo fmt --all -- --check

echo
echo "==[ gate ]============================================="
echo "phase 2/3  clippy -D warnings (workspace, all-targets, all-features)"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo
echo "==[ gate ]============================================="
echo "phase 3/3  test --locked (workspace, all-features)"
cargo test --workspace --locked --all-features

echo
echo "==[ gate ]============================================="
echo "GATE: ALL GREEN ✅"
