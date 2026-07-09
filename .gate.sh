#!/usr/bin/env bash
# Local pre-merge gate for C11 (session store fixes).
#
# Mirrors the project's standard hardening gate (cargo fmt + clippy +
# test) so a single `./.gate.sh` invocation answers "is the workspace
# clean?" without remembering the exact flags. The integration-test
# targets (tests/) are excluded the same way `cargo test --workspace`
# excludes them by default — they're behind the `integration-tests`
# feature so they don't run in the standard pass.
set -e
cd "/home/jasonperlow/zoder-wt-c11-session"
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --locked --all-features