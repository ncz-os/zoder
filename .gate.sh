#!/usr/bin/env bash
# Project CI gate. All steps are required; any failure exits non-zero so
# the gate is enforced in CI and locally.
#
# Steps mirror the project's CI policy:
#   1. `cargo fmt --all -- --check`           — formatting drift
#   2. `cargo clippy ... -- -D warnings`      — lint regressions as errors
#   3. `cargo test ... --locked --all-features`— test regressions,
#                                              with the lockfile frozen so
#                                              a transitive bump can't slip
#                                              in under a green test run.
#
# Notes:
#   * `--all-features` covers the (small) feature-flag matrix in this
#     workspace — the prior flag set was already correct.
#   * No `--exclude`: every workspace crate must pass. The earlier
#     `--exclude zeroclaw-desktop` referenced a crate that is not in
#     this workspace (warns and ignores), but excluding it would let a
#     future regression in any added `zeroclaw-desktop` package escape
#     the gate. Drop the flag — keep the gate honest.
#   * `set -e` plus `set -o pipefail` ensures any step failure
#     (including inside a pipeline) aborts the script.
set -e
set -o pipefail

cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> 1/3 cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> 2/3 cargo clippy --workspace --all-targets --all-features -- -D warnings"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> 3/3 cargo test --workspace --locked --all-features"
cargo test --workspace --locked --all-features

echo "==> gate: PASS"
