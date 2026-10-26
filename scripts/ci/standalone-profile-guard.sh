#!/usr/bin/env bash
# Standalone-profile guard.
#
# The public tree already has a hashed denylist guard. This script points that
# same guard at the single-host starter templates and the runtime-default
# modules, and adds a generic RFC1918 check. Literal internal names are never
# written here: the denylist stores only salted digests.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

PATHS=(
  config.standalone.toml
  config.fleet.toml
  crates/zoder-core/src/config.rs
  crates/zoder-core/src/context_opt/mod.rs
  crates/zoder-cli/src/reaper.rs
  crates/zoder-cli/src/provider_slots.rs
  crates/zoder-cli/src/reviewer_pool.rs
)

ARGS=(--reject-private-addresses)
for path in "${PATHS[@]}"; do
  ARGS+=(--path "$path")
done
python3 scripts/ci/public-repo-guard.py "${ARGS[@]}"
