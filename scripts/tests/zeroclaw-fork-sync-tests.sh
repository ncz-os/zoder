#!/usr/bin/env bash
# Regression test for scripts/ci/zeroclaw-fork-sync.sh.
#
# The 2026-10-09 fleet outage was a *silent* stall: the fork stopped advancing,
# so nothing failed until ncz-os/zoder's nightly aborted days later. This test
# pins the sync's contract against disposable local repositories:
#   * already-current fork  -> no-op, exit 0, nothing pushed
#   * behind, clean merge    -> upstream landed, overlay preserved, exit 0
#   * conflict               -> exit 1, fork NOT advanced, conflict listed
#   * clean merge, bad import-> exit 1, fork NOT advanced
#
# Run: bash scripts/tests/zeroclaw-fork-sync-tests.sh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SYNC="${ROOT_DIR}/scripts/ci/zeroclaw-fork-sync.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
pass() { echo "ok   - $1"; }
bad()  { echo "FAIL - $1"; fail=1; }

GIT=(git -c user.name=Fixture -c user.email=fixture@example.invalid \
          -c commit.gpgsign=false)

seed_repos() {
  # $1 = case dir. Builds seed/, upstream.git, fork.git all at one commit.
  local d="$1"; mkdir -p "$d"
  "${GIT[@]}" init -q -b master "$d/seed"
  printf '[package]\nname = "fixture"\nversion = "0.1.0"\n' > "$d/seed/Cargo.toml"
  printf 'line = 1\n' > "$d/seed/shared.txt"
  "${GIT[@]}" -C "$d/seed" add -A
  "${GIT[@]}" -C "$d/seed" commit -q -m "initial"
  "${GIT[@]}" clone -q --bare "$d/seed" "$d/upstream.git"
  "${GIT[@]}" clone -q --bare "$d/seed" "$d/fork.git"
}

run_sync() {
  # $1 = case dir. Returns the script's exit status.
  UPSTREAM_URL="file://$1/upstream.git" \
  FORK_URL="file://$1/fork.git" \
  FORK_PUSH_URL="file://$1/fork.git" \
  SYNC_WORKDIR="$1/syncwork" \
    bash "$SYNC"
}

head_of() { git -C "$1" rev-parse master; }

# ---- case 1: already current -> no-op -------------------------------------
c1="$TMP/current"; seed_repos "$c1"
before="$(head_of "$c1/fork.git")"
if out="$(run_sync "$c1" 2>&1)"; then
  grep -q "already contains upstream" <<<"$out" && pass "current fork is a no-op" \
    || bad "current fork printed unexpected output: $out"
else
  bad "current fork exited non-zero: $out"
fi
[ "$(head_of "$c1/fork.git")" = "$before" ] && pass "current fork not modified" \
  || bad "current fork head changed"

# ---- case 2: behind, clean merge -> overlay preserved ---------------------
c2="$TMP/merge"; seed_repos "$c2"
"${GIT[@]}" clone -q "$c2/fork.git" "$c2/forkdev"
printf 'overlay = true\n' > "$c2/forkdev/overlay.txt"
"${GIT[@]}" -C "$c2/forkdev" add -A
"${GIT[@]}" -C "$c2/forkdev" commit -q -m "our overlay"
"${GIT[@]}" -C "$c2/forkdev" push -q origin master
OVERLAY="$(head_of "$c2/forkdev")"
"${GIT[@]}" clone -q "$c2/upstream.git" "$c2/updev"
printf 'upstream = 2\n' > "$c2/updev/new.txt"
"${GIT[@]}" -C "$c2/updev" add -A
"${GIT[@]}" -C "$c2/updev" commit -q -m "upstream advance"
"${GIT[@]}" -C "$c2/updev" push -q origin master
UP="$(head_of "$c2/updev")"
if run_sync "$c2" >/dev/null 2>&1; then
  git -C "$c2/fork.git" merge-base --is-ancestor "$UP" master \
    && pass "clean merge advanced fork to upstream" || bad "fork did not reach upstream"
  git -C "$c2/fork.git" merge-base --is-ancestor "$OVERLAY" master \
    && pass "clean merge preserved the overlay commit" || bad "overlay commit lost"
else
  bad "clean merge should have succeeded"
fi

# ---- case 3: conflicting edit -> fail, no push ----------------------------
c3="$TMP/conflict"; seed_repos "$c3"
"${GIT[@]}" clone -q "$c3/fork.git" "$c3/forkdev"
printf 'line = fork\n' > "$c3/forkdev/shared.txt"
"${GIT[@]}" -C "$c3/forkdev" commit -q -am "fork edit"
"${GIT[@]}" -C "$c3/forkdev" push -q origin master
before="$(head_of "$c3/fork.git")"
"${GIT[@]}" clone -q "$c3/upstream.git" "$c3/updev"
printf 'line = upstream\n' > "$c3/updev/shared.txt"
"${GIT[@]}" -C "$c3/updev" commit -q -am "upstream edit"
"${GIT[@]}" -C "$c3/updev" push -q origin master
if out="$(run_sync "$c3" 2>&1)"; then
  bad "conflict should have failed, got exit 0: $out"
else
  grep -q "merge conflict" <<<"$out" && pass "conflict reported loudly" \
    || bad "conflict not reported: $out"
  [ "$(head_of "$c3/fork.git")" = "$before" ] && pass "conflict left fork untouched" \
    || bad "conflict advanced the fork"
fi

# ---- case 4: clean merge, bad manifest -> fail, no push -------------------
c4="$TMP/manifest"; seed_repos "$c4"
"${GIT[@]}" clone -q "$c4/upstream.git" "$c4/updev"
printf '[package]\nname = "one"\nname = "duplicate"\n' > "$c4/updev/Cargo.toml"
"${GIT[@]}" -C "$c4/updev" commit -q -am "break manifest"
"${GIT[@]}" -C "$c4/updev" push -q origin master
before="$(head_of "$c4/fork.git")"
if run_sync "$c4" >/dev/null 2>&1; then
  bad "invalid manifest should have failed"
else
  [ "$(head_of "$c4/fork.git")" = "$before" ] && pass "bad manifest left fork untouched" \
    || bad "bad manifest advanced the fork"
fi

if [ "$fail" -ne 0 ]; then
  echo "zeroclaw-fork-sync tests FAILED"
  exit 1
fi
echo "zeroclaw-fork-sync tests passed"
