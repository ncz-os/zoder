#!/usr/bin/env python3
"""Corpus freshness gate for the GitLab `corpus-sync` job (Finding #16 fix,
gate split 2026-10-26).

The gate used to be ONE gate: a degraded bench overlay blocked the whole
publish, so a benchmark source being down also froze the PRICES (the
LiteLLM/OpenRouter catalog had been stale for 13 days for exactly that
reason). It is now TWO independent gates:

  PRICE gate -- blocks publishing prices:
    1. LiteLLM must have fetched successfully,
    2. the OpenRouter overlay must have succeeded,
    3. the new catalog must not be more than CATALOG_DROP_THRESHOLD
       smaller than the previously committed catalog.
    A failure prints "PRICE SOURCE DEGRADED (...) - not publishing
    prices. Pass --force-publish to override." and exits non-zero.

  OVERLAY gate -- NEVER blocks prices:
    A bench source being down, or a candidate overlay that lost too much
    coverage, means the previously committed `bench/overlay.json` is kept
    byte-for-byte (it is the last-known-good) and the run is reported as
    `overlay_stale`. That is a WARNING and the run still exits 0, so the
    fresh prices are published. When the overlay IS fresh, the historic
    coverage-delta and minimum-count checks run against
    `corpus/freshness-last.json` as a second line of defence, and that
    baseline is updated (only then).

`--force-publish` overrides everything and says so.

A schema-1 `corpus/freshness.json` (no `schema` key) is still accepted and
keeps the historic, single-gate behaviour, so the older report files and
their tests keep working.
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
    # `os.environ.get(key, default)` evaluates `default` eagerly but only
    # RETURNS it when the key is ABSENT -- a set override has always been
    # honoured, so this is not the bug it first looks like. What the plain
    # form cannot survive is a PRESENT BUT EMPTY value (an unset CI variable
    # exported as ""): `Path("")` is the current directory, so the gate would
    # look for `./corpus/freshness.json` wherever the job happens to be. An
    # empty or whitespace-only override now falls back to the repo root.
    override = (os.environ.get("CORPUS_FRESHNESS_ROOT") or "").strip()
    base = Path(override) if override else REPO_ROOT
    return base / "corpus", base / "bench" / "overlay.json"

# Allowed coverage drop between consecutive runs, expressed as a fraction
# of the LKG model's count. 0.25 = a 25% drop is OK, anything beyond is
# treated as an unrecoverable outage.
COVERAGE_DROP_THRESHOLD = 0.25

# Minimum models-with-agentic_score required before the overlay is
# considered publishable. Below this, we refuse to commit.
MIN_AGENTIC_SCORE_COUNT = 50

# Allowed shrink of the pricing catalog between consecutive runs, as a
# fraction of the previously committed catalog's model count. A price
# source that silently lost more than this is degraded, not refreshed.
CATALOG_DROP_THRESHOLD = 0.25


def _count_scored(overlay: Path) -> int:
    """Number of models with an `agentic_score` in an overlay file."""
    try:
        overlay_data = json.loads(overlay.read_text())
    except (OSError, json.JSONDecodeError):
        return 0
    if not isinstance(overlay_data, dict):
        return 0
    return sum(1 for v in overlay_data.values()
               if isinstance(v, dict) and "agentic_score" in v)


def _price_gate(freshness: dict) -> str:
    """Run the PRICE gate. Returns "" when prices may be published, else
    the human-readable reason the prices must not be published."""
    prices = freshness.get("prices")
    if not isinstance(prices, dict):
        # No price information at all (e.g. a hand-written report): do not
        # invent a verdict, but do not silently publish either.
        return "no price status recorded"

    litellm = prices.get("litellm") if isinstance(prices.get("litellm"), dict) else {}
    if not litellm.get("ok"):
        reason = str(litellm.get("reason") or "fetch failed")
        return f"litellm: {reason}"

    openrouter = prices.get("openrouter") if isinstance(prices.get("openrouter"), dict) else {}
    if not openrouter.get("ok"):
        reason = str(openrouter.get("reason") or "overlay failed")
        return f"openrouter: {reason}"

    previous = prices.get("previous_catalog_models") or 0
    current = prices.get("catalog_models") or 0
    if previous and current < previous * (1.0 - CATALOG_DROP_THRESHOLD):
        shrink = (previous - current) / previous
        return (f"catalog shrank by {shrink:.1%} "
                f"({current} models vs {previous} previously committed)")
    return ""


def _failing_sources(freshness: dict) -> list[str]:
    """Bench sources that neither fetched fresh nor had an LKG file."""
    sources = freshness.get("sources") or []
    names = []
    for s in sources:
        if not isinstance(s, dict):
            continue
        if not s.get("ok") and not s.get("lkg"):
            names.append(str(s.get("name") or "?"))
    return names


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--force-publish", action="store_true",
                    help="bypass the freshness gate (use only for explicit operator acknowledgement)")
    args = ap.parse_args()

    corpus, overlay = _paths()
    cur_path = corpus / "freshness.json"
    last_path = corpus / "freshness-last.json"
    if not cur_path.exists():
        print(f"corpus-freshness: {cur_path} missing — did corpus-fetch-and-overlay.py run?", file=sys.stderr)
        return 2

    cur = json.loads(cur_path.read_text())
    if not isinstance(cur, dict):
        print("corpus-freshness: freshness.json is not an object", file=sys.stderr)
        return 2

    if args.force_publish:
        print("corpus-freshness: --force-publish -> skipping ALL gates "
              "(price + overlay); this overrides every check below", file=sys.stderr)
        # The baseline is only advanced when the overlay is fresh: a stale
        # overlay must not become tomorrow's "last known good". A report
        # without overlay information (schema 1) counts as fresh.
        overlay_meta = cur.get("overlay") if isinstance(cur.get("overlay"), dict) else {}
        stale = bool(cur.get("overlay_stale", overlay_meta.get("stale", False)))
        if not stale:
            next_meta = dict(cur)
            next_meta["models_with_score"] = _count_scored(overlay)
            last_path.write_text(json.dumps(next_meta, indent=2, sort_keys=True) + "\n")
        return 0

    cur_sources = cur.get("sources") or []
    n_ok = sum(1 for s in cur_sources if isinstance(s, dict) and s.get("ok"))
    n_total = len(cur_sources)
    print(f"corpus-freshness: current run {n_ok}/{n_total} sources fetched fresh", file=sys.stderr)

    schema = cur.get("schema") or 1
    try:
        schema = int(schema)
    except (TypeError, ValueError):
        schema = 1

    if schema >= 2:
        return _check_schema2(cur, corpus, overlay, last_path, n_ok, n_total)
    return _check_schema1(cur, overlay, last_path, n_ok, n_total)


def _check_schema2(cur: dict, corpus: Path, overlay: Path, last_path: Path,
                   n_ok: int, n_total: int) -> int:
    """The split gate: PRICE blocks, OVERLAY only warns."""
    # ---- PRICE gate (blocks publishing prices) --------------------------
    reason = _price_gate(cur)
    if reason:
        print(f"corpus-freshness: PRICE SOURCE DEGRADED ({reason}) - "
              "not publishing prices. Pass --force-publish to override.", file=sys.stderr)
        return 1

    # ---- OVERLAY gate (never blocks prices) ----------------------------
    overlay_meta = cur.get("overlay") if isinstance(cur.get("overlay"), dict) else {}
    stale = bool(cur.get("overlay_stale", overlay_meta.get("stale", False)))
    age = cur.get("overlay_age_days")
    if age is None:
        age = overlay_meta.get("age_days", 0)
    try:
        age = int(age or 0)
    except (TypeError, ValueError):
        age = 0

    if stale:
        failing = _failing_sources(cur)
        print(f"corpus-freshness: WARNING - bench overlay kept STALE "
              f"(age {age} day(s)); the previously committed overlay is unchanged", file=sys.stderr)
        if failing:
            print(f"corpus-freshness: WARNING - bench source(s) unavailable: "
                  f"{', '.join(failing)}", file=sys.stderr)
        why = str(overlay_meta.get("reason") or "")
        if why:
            print(f"corpus-freshness: WARNING - overlay reason: {why}", file=sys.stderr)
        print("corpus-freshness: prices are published; routing scores were NOT erased",
              file=sys.stderr)
        # The baseline is only advanced when the overlay is fresh: a stale
        # overlay must not become tomorrow's "last known good".
        return 0

    # The overlay is fresh: keep the historic checks as a second line of
    # defence before the run is accepted.
    cur_overlay_count = _count_scored(overlay)
    print(f"corpus-freshness: overlay reports {cur_overlay_count} models with agentic_score", file=sys.stderr)
    if cur_overlay_count < MIN_AGENTIC_SCORE_COUNT:
        print(f"corpus-freshness: overlay below minimum ({cur_overlay_count} < {MIN_AGENTIC_SCORE_COUNT}); refusing to commit", file=sys.stderr)
        return 1

    if last_path.exists():
        last_overlay_count = 0
        try:
            last_meta = json.loads(last_path.read_text())
            last_overlay_count = int(last_meta.get("models_with_score", 0))
        except (json.JSONDecodeError, ValueError, KeyError, TypeError):
            last_overlay_count = 0
        if last_overlay_count > 0:
            # Signed: positive = growth, negative = drop.
            drop = (cur_overlay_count - last_overlay_count) / last_overlay_count
            drop_magnitude = abs(drop)
            print(
                f"corpus-freshness: coverage delta vs LKG = {drop:+.1%} (threshold -{COVERAGE_DROP_THRESHOLD:.0%})",
                file=sys.stderr,
            )
            # Only a real DROP beyond the threshold fails. An increase of any
            # size passes: growing coverage is the corpus getting better, not
            # an outage.
            if drop < 0 and drop_magnitude > COVERAGE_DROP_THRESHOLD:
                print(
                    f"corpus-freshness: coverage dropped by {drop_magnitude:.1%}, more than the {COVERAGE_DROP_THRESHOLD:.0%} threshold",
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

    next_meta = dict(cur)
    next_meta["models_with_score"] = cur_overlay_count
    last_path.write_text(json.dumps(next_meta, indent=2, sort_keys=True) + "\n")
    print("corpus-freshness: OK (overlay publishable)", file=sys.stderr)
    return 0


def _check_schema1(cur: dict, overlay: Path, last_path: Path,
                   n_ok: int, n_total: int) -> int:
    """The historic single-gate behaviour, kept for schema-1 reports."""
    # Hard fail: all sources failed -> nothing publishable.
    if n_total > 0 and n_ok == 0:
        print("corpus-freshness: ALL sources failed; refusing to commit", file=sys.stderr)
        return 1

    cur_overlay_count = _count_scored(overlay)
    print(f"corpus-freshness: overlay reports {cur_overlay_count} models with agentic_score", file=sys.stderr)

    if cur_overlay_count < MIN_AGENTIC_SCORE_COUNT:
        print(f"corpus-freshness: overlay below minimum ({cur_overlay_count} < {MIN_AGENTIC_SCORE_COUNT}); refusing to commit", file=sys.stderr)
        return 1

    # Coverage delta check vs LKG. Conservative in both directions: a
    # sudden spike (massive new source) is fine, only a DROP beyond the
    # threshold is bad.
    #
    # The signed delta is (cur - last) / last, so a POSITIVE number is growth
    # and a NEGATIVE number is a drop. The number is what the job printed,
    # and printing it with the wrong sign is what made the 2026-10-25
    # incident unreadable: the run really DID lose coverage (191 -> 141
    # scored models, a real -26.2% drop) because terminal-bench and arena
    # stopped fetching, and the old formula `(last - cur) / last` printed
    # that drop as "+26.2%", which read like growth. The verdict was the
    # same before and after the sign fix -- a drop beyond the threshold
    # fails either way -- only the printed number changed.
    if last_path.exists():
        last_overlay_count = 0
        try:
            last_meta = json.loads(last_path.read_text())
            last_overlay_count = int(last_meta.get("models_with_score", 0))
        except (json.JSONDecodeError, ValueError, KeyError, TypeError):
            last_overlay_count = 0
        if last_overlay_count > 0:
            drop = (cur_overlay_count - last_overlay_count) / last_overlay_count
            drop_magnitude = abs(drop)
            print(
                f"corpus-freshness: coverage delta vs LKG = {drop:+.1%} (threshold -{COVERAGE_DROP_THRESHOLD:.0%})",
                file=sys.stderr,
            )
            if drop < 0 and drop_magnitude > COVERAGE_DROP_THRESHOLD:
                print(
                    f"corpus-freshness: coverage dropped by {drop_magnitude:.1%}, more than the {COVERAGE_DROP_THRESHOLD:.0%} threshold",
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

    next_meta = dict(cur)
    next_meta["models_with_score"] = cur_overlay_count
    last_path.write_text(json.dumps(next_meta, indent=2, sort_keys=True) + "\n")
    print("corpus-freshness: OK (overlay publishable)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
