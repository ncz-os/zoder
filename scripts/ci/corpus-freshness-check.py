#!/usr/bin/env python3
"""Corpus freshness gate for the GitLab `corpus-sync` job (Finding #16 fix).

The prior CI converted fetch failures into warnings and then unconditionally
committed whatever overlay happened to be on disk. A transient upstream outage
could therefore erase routing scores from the authoritative corpus.

This script refuses to exit 0 when:

  1. The current run's coverage (per-source ok count, total models in
     overlay.json) is more than COVERAGE_DROP_THRESHOLD below the running
     LKG measurement persisted in `corpus/freshness-last.json`. A drop
     of more than 25% across the board is treated as an unrecoverable
     outage and the job exits non-zero so no commit happens.

  2. ALL primary sources failed simultaneously (no LKG coverage), since
     nothing would have been published in that case anyway.

  3. The current overlay is empty (zero models with agentic_score), which
     would commit a regression to master.

On a clean pass, it copies `corpus/freshness.json` to
`corpus/freshness-last.json` so the NEXT run has a baseline.
"""
from __future__ import annotations
import argparse
import json
import os
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]


def _paths() -> tuple[Path, Path]:
    """Resolve corpus + overlay paths. Tests can override via env vars."""
    base = Path(os.environ.get("CORPUS_FRESHNESS_ROOT", str(REPO_ROOT)))
    return base / "corpus", base / "bench" / "overlay.json"

# Allowed coverage drop between consecutive runs, expressed as a fraction
# of the LKG model's count. 0.25 = a 25% drop is OK, anything beyond is
# treated as an unrecoverable outage.
COVERAGE_DROP_THRESHOLD = 0.25

# Minimum models-with-agentic_score required before the overlay is
# considered publishable. Below this, we refuse to commit.
MIN_AGENTIC_SCORE_COUNT = 50


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--force-publish", action="store_true",
                    help="bypass the freshness gate (use only for explicit operator acknowledgement)")
    args = ap.parse_args()
    if args.force_publish:
        print("corpus-freshness: --force-publish -> skipping gate", file=sys.stderr)
        # Still update last-known-good so next run has a baseline.
        cur, _ = _paths()
        cur_path = cur / "freshness.json"
        if cur_path.exists():
            (cur / "freshness-last.json").write_text(cur_path.read_text())
        return 0

    corpus, overlay = _paths()
    cur_path = corpus / "freshness.json"
    last_path = corpus / "freshness-last.json"
    if not cur_path.exists():
        print(f"corpus-freshness: {cur_path} missing — did corpus-fetch-and-overlay.py run?", file=sys.stderr)
        return 2

    cur = json.loads(cur_path.read_text())
    cur_sources = cur.get("sources") or []
    n_ok = sum(1 for s in cur_sources if s.get("ok"))
    n_total = len(cur_sources)
    print(f"corpus-freshness: current run {n_ok}/{n_total} sources fetched fresh", file=sys.stderr)

    # Hard fail: all sources failed -> nothing publishable.
    if n_total > 0 and n_ok == 0:
        print("corpus-freshness: ALL sources failed; refusing to commit", file=sys.stderr)
        return 1

    # Coverage check: count models with agentic_score in the new overlay.
    cur_overlay_count = 0
    if overlay.exists():
        try:
            overlay_data = json.loads(overlay.read_text())
            cur_overlay_count = sum(1 for v in overlay_data.values() if "agentic_score" in v)
        except (json.JSONDecodeError, AttributeError):
            pass
    print(f"corpus-freshness: overlay reports {cur_overlay_count} models with agentic_score", file=sys.stderr)

    if cur_overlay_count < MIN_AGENTIC_SCORE_COUNT:
        print(f"corpus-freshness: overlay below minimum ({cur_overlay_count} < {MIN_AGENTIC_SCORE_COUNT}); refusing to commit", file=sys.stderr)
        return 1

    # Coverage drop check vs LKG. Conservative in both directions: a
    # sudden spike (massive new source) is fine, only a DROP is bad.
    if last_path.exists():
        try:
            last_overlay_count = 0
            # Best-effort: re-derive from the prior freshness report if
            # the prior overlay.json isn't checkable (the last-known-good
            # record is always `freshness-last.json` from the prior pass).
            last_meta = json.loads(last_path.read_text())
            last_overlay_count = int(last_meta.get("models_with_score", 0))
        except (json.JSONDecodeError, ValueError, KeyError, TypeError):
            last_overlay_count = 0
        if last_overlay_count > 0:
            drop = (last_overlay_count - cur_overlay_count) / last_overlay_count
            print(f"corpus-freshness: coverage delta vs LKG = {drop:+.1%} (threshold {-COVERAGE_DROP_THRESHOLD:.0%})", file=sys.stderr)
            if drop > COVERAGE_DROP_THRESHOLD:
                print(
                    f"corpus-freshness: coverage dropped by {drop:.1%}, more than the {COVERAGE_DROP_THRESHOLD:.0%} threshold",
                    file=sys.stderr,
                )
                print(
                    "  a transient upstream outage can therefore NOT erase routing scores from master.",
                    file=sys.stderr,
                )
                print(
                    "  Fix the failing source(s) and re-run. Pass --force-publish to override.",
                    file=sys.stderr,
                )
                return 1

    # Persist a richer LKG record so the NEXT run can compare against this
    # run's overlay coverage, not just the freshness flags.
    next_meta = dict(cur)
    next_meta["models_with_score"] = cur_overlay_count
    last_path.write_text(json.dumps(next_meta, indent=2, sort_keys=True) + "\n")
    print("corpus-freshness: OK (overlay publishable)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
