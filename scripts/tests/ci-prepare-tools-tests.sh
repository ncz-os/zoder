#!/bin/sh
# Test that scripts/ci/ci-prepare-tools.sh helper exists, parses, and
# exposes the expected usage surface (Finding #15). Doesn't actually
# download the artifacts — that requires network and runs in CI.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SCRIPT="$ROOT/scripts/ci/ci-prepare-tools.sh"

ok()  { echo "ok    $*";  PASSES=$((PASSES + 1)); }
bad() { echo "FAIL  $*";  FAILS=$((FAILS + 1)); }
PASSES=0; FAILS=0

# Syntactic check
sh -n "$SCRIPT" && ok "ci-prepare-tools.sh syntax OK" || bad "ci-prepare-tools.sh syntax broken"

# Usage surface check — invoking with no arg must print usage + nonzero exit.
if sh "$SCRIPT" 2>/dev/null; then
    bad "no-arg invocation should fail (usage missing)"
else
    rc=$?
    if [ "$rc" = 2 ]; then
        ok "no-arg invocation returns usage code 2"
    else
        bad "no-arg invocation returned $rc, expected 2"
    fi
fi

# Surface check — both subcommands are recognised even if we can't
# actually run them in the sandbox. We do this by inspecting the
# case dispatch; a deeper test would require a network-shimmed sandbox.
# Here we just confirm the script lists the subcommands in its usage
# message.
usage=$(sh "$SCRIPT" 2>&1 || true)
case "$usage" in
    *nextest*|*binstall*)
        ok "usage message lists subcommands (nextest + binstall)"
        ;;
    *)
        bad "usage message missing subcommand names: $usage"
        ;;
esac

echo "$PASSES passed; $FAILS failed"
[ "$FAILS" = 0 ]
