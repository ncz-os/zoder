#!/usr/bin/env bash
# Keep the gitlab.com/ncz-os/zeroclaw fork within the nightly's sync grace
# window of upstream zeroclaw-labs/zeroclaw.
#
# WHY THIS EXISTS
# ---------------
# ncz-os/zeroclaw's `master` is upstream zeroclaw PLUS a small local overlay
# (CI tweaks and a few dependency/security bumps), so it can only advance by a
# real MERGE -- never a fast-forward or force-push. ncz-os/zoder's native
# nightly (GitHub Actions) reads that fork and refuses to build when the oldest
# upstream commit the fork lacks is older than its 36h grace window
# (`.github/workflows/native-builds.yml`, `sources` job).
#
# That merge used to be driven ONLY by the fork's own GitHub Actions schedule
# (`.github/workflows/daily-master-sync.yml`). GitHub silently auto-disables
# scheduled workflows on forks / inactive repositories, so on 2026-10-09 the
# merge stopped firing, the fork crossed the grace window, and the nightly
# aborted at `sources` with no binaries published -- the fleet went stale and
# the only signal was a red run.
#
# This script runs on zoder's RELIABLE GitLab pipeline schedule (09:00 UTC,
# before the 09:15 native build), so the fork is reconciled on infrastructure
# we control. The fork's own GitHub job can stay as a second, best-effort
# driver -- this is the guarantee.
#
# A merge conflict exits non-zero WITHOUT pushing: the pipeline goes red and
# buildwatch_gitlab files a MNEMOS alert. It never force-pushes and never
# advances the fork to a tree that fails to parse.
#
# Env:
#   UPSTREAM_URL     default https://github.com/zeroclaw-labs/zeroclaw.git
#   FORK_URL         default https://gitlab.com/ncz-os/zeroclaw.git   (fetch)
#   FORK_PUSH_URL    default $FORK_URL                                (push)
#   UPSTREAM_BRANCH  default master
#   FORK_BRANCH      default master
#   SYNC_WORKDIR     default $PWD/.zeroclaw-fork-sync
set -euo pipefail

UPSTREAM_URL="${UPSTREAM_URL:-https://github.com/zeroclaw-labs/zeroclaw.git}"
FORK_URL="${FORK_URL:-https://gitlab.com/ncz-os/zeroclaw.git}"
FORK_PUSH_URL="${FORK_PUSH_URL:-$FORK_URL}"
UPSTREAM_BRANCH="${UPSTREAM_BRANCH:-master}"
FORK_BRANCH="${FORK_BRANCH:-master}"
SYNC_WORKDIR="${SYNC_WORKDIR:-$PWD/.zeroclaw-fork-sync}"

export GIT_TERMINAL_PROMPT=0

rm -rf "$SYNC_WORKDIR"
git clone --quiet "$FORK_URL" "$SYNC_WORKDIR"
git -C "$SYNC_WORKDIR" remote add upstream "$UPSTREAM_URL"
git -C "$SYNC_WORKDIR" fetch --quiet upstream "$UPSTREAM_BRANCH"
git -C "$SYNC_WORKDIR" checkout --quiet "$FORK_BRANCH"

UP="$(git -C "$SYNC_WORKDIR" rev-parse "upstream/$UPSTREAM_BRANCH")"
if git -C "$SYNC_WORKDIR" merge-base --is-ancestor "$UP" HEAD; then
  echo "fork $FORK_BRANCH already contains upstream/$UPSTREAM_BRANCH ($UP); nothing to do."
  exit 0
fi

BEHIND="$(git -C "$SYNC_WORKDIR" rev-list --count "HEAD..upstream/$UPSTREAM_BRANCH")"
OLDEST="$(git -C "$SYNC_WORKDIR" log --format=%ct "HEAD..upstream/$UPSTREAM_BRANCH" | sort -n | head -1)"
AGE_H=$(( ( $(date +%s) - OLDEST ) / 3600 ))
echo "fork is $BEHIND commit(s) behind upstream/$UPSTREAM_BRANCH; oldest missing commit is ${AGE_H}h old"

# Real merge, never force. The fork's overlay commits are preserved; upstream
# lands on top. Conflict => loud failure, nothing pushed.
if ! git -C "$SYNC_WORKDIR" \
      -c user.name="ncz-os-sync-bot" \
      -c user.email="sync@ncz-os.local" \
      merge --no-edit "upstream/$UPSTREAM_BRANCH" \
      -m "merge: daily sync master with upstream zeroclaw-labs master ($(date -u +%F))"; then
  echo "::error::fork sync hit a merge conflict that needs human resolution:"
  git -C "$SYNC_WORKDIR" diff --name-only --diff-filter=U || true
  git -C "$SYNC_WORKDIR" merge --abort || true
  exit 1
fi

# A clean text merge can still duplicate dependency keys; verify manifests
# before the fork is advanced.
python3 - "$SYNC_WORKDIR" <<'PY'
import pathlib, sys, tomllib

root = pathlib.Path(sys.argv[1])
bad = []
for path in root.rglob("Cargo.toml"):
    if any(part in (".git", "target") for part in path.parts):
        continue
    try:
        tomllib.loads(path.read_text())
    except Exception as exc:  # noqa: BLE001 - report every bad manifest
        bad.append((path, exc))
if bad:
    for path, exc in bad:
        print(f"::error::invalid manifest {path}: {exc}")
    sys.exit(1)
print("manifests OK")
PY

git -C "$SYNC_WORKDIR" push --quiet "$FORK_PUSH_URL" "HEAD:refs/heads/$FORK_BRANCH"
echo "pushed fork $FORK_BRANCH -> $(git -C "$SYNC_WORKDIR" rev-parse HEAD)"
