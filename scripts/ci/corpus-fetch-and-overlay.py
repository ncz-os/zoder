#!/usr/bin/env python3
"""Deterministic corpus fetch + overlay build for the GitLab `corpus-sync`
job (Finding #16 fix, gate split 2026-10-26).

The prior inline shell did this:

    run() { echo "== $1"; python3 "scripts/$1" "${@:2}" || echo "WARN: $1 failed - degrading coverage"; }
    run fetch-vals-swebench.py ...
    ...
    python3 scripts/build-bench-overlay.py --out-dir .
    python3 scripts/build-public-corpus.py --out-dir .

Two defects (Finding #16):
  1. Failed fetches became warnings, so an empty upstream response was
     treated the same as a full success and the overlay got thinner.
  2. `bench/raw` started empty on first run, so a transient outage
     could turn the overlay empty, and the next day's job would
     commit that empty overlay to master.

What this script does now:
  * Fetches every bench source into `bench/raw/`, snapshotting the
    previous bytes into the last-known-good directory first so a failed
    source can fall back to its LKG bytes (locally available only --
    `bench/raw/` is git-ignored, so a clean CI checkout has no LKG).
  * Builds the CANDIDATE overlay into a non-published path and only
    promotes it to `bench/overlay.json` when it is safe:
      - every bench source fetched fresh (or had a per-source LKG file),
      - the candidate has at least MIN_AGENTIC_SCORE_COUNT scored models,
      - the candidate is not more than OVERLAY_DROP_THRESHOLD below the
        currently committed `bench/overlay.json`.
    Otherwise the committed overlay is kept BYTE-FOR-BYTE (it is the
    last-known-good: a transient outage must never erase routing
    scores) and the run is marked `overlay_stale`.
  * Rebuilds the corpus + pricing catalog from whichever overlay won and
    records the run in `corpus/freshness.json` (schema 2), including the
    price-source status so the CI gate does not have to parse stderr.

The gates themselves live in `corpus-freshness-check.py`:
  * the PRICE gate (LiteLLM + OpenRouter + catalog shrink) blocks the
    publish of prices;
  * the OVERLAY gate never blocks prices -- a stale overlay is a warning.

Exit code: 0 unless the run could not be recorded at all. A degraded
price source is reported in `corpus/freshness.json` and turned into a
non-zero exit by `corpus-freshness-check.py`, so the two scripts together
keep the "publish" decision in exactly one place.
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]   # scripts/ci -> repo root

# Sources the corpus-sync job fetches. Each entry: (script-name, output files it produces, args)
SOURCES = [
    ("fetch-vals-swebench.py",  ["vals-swebench.json"],  ["--json", "bench/raw/vals-swebench.json"]),
    ("fetch-aider-polyglot.py", ["aider-polyglot.json"], ["--json", "bench/raw/aider-polyglot.json"]),
    ("fetch-arena.py",          ["arena-webdev.json", "arena-agent.json"], ["--out-dir", "bench/raw"]),
    ("fetch-scale.py",          ["scale-seal.json"],    ["--json", "bench/raw/scale-seal.json"]),
    ("fetch-terminal-bench.py", ["terminal-bench.json"], ["--json", "bench/raw/terminal-bench.json"]),
]

# Minimum models-with-agentic_score a candidate overlay must carry before
# it may replace the committed one.
MIN_AGENTIC_SCORE_COUNT = 50

# A candidate may not be more than this fraction below the committed
# overlay's scored-model count (0.25 -> a 25% drop is still acceptable).
OVERLAY_DROP_THRESHOLD = 0.25

# A refreshed pricing catalog may not be more than this fraction smaller
# than the previously committed one: a price source that silently lost a
# quarter of its models is degraded, not refreshed.
CATALOG_DROP_THRESHOLD = 0.25

# Name of the candidate overlay inside the (git-ignored) raw directory.
# It is deliberately NOT `bench/overlay.json`: the candidate must never be
# visible at the published path until it has been accepted.
CANDIDATE_NAME = "overlay-candidate.json"

# Name of the price-source status file the corpus builder writes.
STATUS_NAME = "build-status.json"


# --------------------------------------------------------------------------
# time helpers
# --------------------------------------------------------------------------
def utc_now_iso() -> str:
    """Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`."""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_iso(value: str | None) -> datetime | None:
    """Parse a `YYYY-MM-DDTHH:MM:SSZ` timestamp; None when unparsable."""
    if not value or not isinstance(value, str):
        return None
    text = value.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def age_in_days(built_at: str | None, now_iso: str) -> int:
    """Whole days between `built_at` and `now_iso` (0 when unknown)."""
    start = parse_iso(built_at)
    end = parse_iso(now_iso)
    if start is None or end is None:
        return 0
    return max(0, (end.date() - start.date()).days)


# --------------------------------------------------------------------------
# overlay helpers
# --------------------------------------------------------------------------
def count_scored(path: Path) -> int:
    """Number of models carrying an `agentic_score` in an overlay file."""
    try:
        data = json.loads(Path(path).read_text())
    except (OSError, json.JSONDecodeError):
        return 0
    if not isinstance(data, dict):
        return 0
    return sum(1 for v in data.values() if isinstance(v, dict) and "agentic_score" in v)


def snapshot_lkg(raw: Path, lkg: Path) -> None:
    """If RAW already has files, mirror them into LKG before fetching."""
    lkg.mkdir(parents=True, exist_ok=True)
    if not raw.exists():
        return
    for f in raw.iterdir():
        if f.is_file() and f.name not in (CANDIDATE_NAME, STATUS_NAME):
            shutil.copy2(f, lkg / f.name)


def restore_lkg_for(entry: dict, raw: Path, lkg: Path) -> bool:
    """Restore a failed source's LKG bytes into RAW.

    Returns True only when EVERY output of the source had a local LKG file,
    so a multi-output source (arena writes two files) is never reported as
    covered while one of its outputs is missing. Outputs that do have an LKG
    file are restored either way.
    """
    outputs = entry.get("outputs", [])
    restored = 0
    for outfile in outputs:
        lkg_file = lkg / outfile
        if lkg_file.exists():
            raw.mkdir(parents=True, exist_ok=True)
            shutil.copy2(lkg_file, raw / outfile)
            restored += 1
    return bool(outputs) and restored == len(outputs)


def decide_overlay(source_entries: list[dict], candidate_scored: int,
                   committed_scored: int, committed_exists: bool) -> tuple[bool, str]:
    """Decide whether the candidate overlay may replace the committed one.

    Returns (accept, reason). `reason` explains why the candidate was
    rejected; it is empty when the candidate was accepted.
    """
    reasons: list[str] = []
    missing = [e["name"] for e in source_entries
               if not e.get("ok") and not e.get("lkg")]
    if missing:
        reasons.append("bench source(s) unavailable with no LKG: " + ", ".join(missing))
    if candidate_scored < MIN_AGENTIC_SCORE_COUNT:
        reasons.append(
            f"candidate overlay has {candidate_scored} scored models "
            f"(minimum {MIN_AGENTIC_SCORE_COUNT})")
    if committed_exists and committed_scored > 0:
        drop = (committed_scored - candidate_scored) / committed_scored
        if drop > OVERLAY_DROP_THRESHOLD:
            reasons.append(
                f"candidate overlay is {drop:.1%} below the committed overlay "
                f"({candidate_scored} vs {committed_scored})")
    if reasons:
        return False, "; ".join(reasons)
    return True, ""


# --------------------------------------------------------------------------
# subprocess runners (the seams the offline tests replace)
# --------------------------------------------------------------------------
def run_source(root: Path, src_script: str, args: list[str]) -> tuple[bool, str]:
    """Run one fetch script with the same cwd the runner uses."""
    cmd = ["python3", f"scripts/{src_script}", *args]
    try:
        r = subprocess.run(cmd, cwd=root, capture_output=True, text=True, timeout=600, check=False)
    except subprocess.TimeoutExpired:
        return False, "timeout after 600s"
    except Exception as e:  # noqa: BLE001 — a spawn failure is a source failure
        return False, f"spawn failed: {e}"
    if r.returncode != 0:
        # One line, not a traceback: the reason lands in the committed
        # corpus/freshness.json, which is a public file.
        detail = "non-zero exit"
        for line in reversed(r.stderr.strip().splitlines()):
            if line.strip() and not line.startswith((" ", "\t")):
                detail = line.strip()
                break
        return False, detail[:300]
    # The fetcher writes its output via the args (out-dir or --json file
    # path). Verify the expected files exist + are non-empty.
    for outfile in args:
        if outfile.endswith(".json"):
            p = root / outfile
            if not p.exists() or p.stat().st_size == 0:
                return False, f"missing/empty output: {outfile}"
    return True, ""


def run_overlay_builder(root: Path, raw_dir: Path, out_path: Path) -> None:
    """Build the candidate overlay into a non-published path."""
    subprocess.run(
        ["python3", "scripts/build-bench-overlay.py",
         "--raw-dir", str(raw_dir), "--out", str(out_path)],
        cwd=root, check=True,
    )


def run_corpus_builder(root: Path, status_path: Path) -> int:
    """Build the public corpus + pricing catalog, writing a run status."""
    return subprocess.run(
        ["python3", "scripts/build-public-corpus.py",
         "--out-dir", ".", "--status-out", str(status_path)],
        cwd=root,
        check=False,
    ).returncode


# --------------------------------------------------------------------------
# pipeline
# --------------------------------------------------------------------------
def fetch_sources(root: Path, fetch_one=run_source) -> list[dict]:
    """Fetch every bench source; returns one entry per source."""
    entries: list[dict] = []
    for name, outputs, src_args in SOURCES:
        ok, why = fetch_one(root, name, src_args)
        entries.append({
            "name": name,
            "kind": "bench",
            "outputs": outputs,
            "ok": ok,
            "reason": why,
            "lkg": False,
        })
    return entries


def read_catalog_count(path: Path) -> int:
    """Number of models in a pricing catalog (0 when absent/unreadable)."""
    try:
        data = json.loads(Path(path).read_text())
    except (OSError, json.JSONDecodeError):
        return 0
    models = data.get("models") if isinstance(data, dict) else None
    return len(models) if isinstance(models, dict) else 0


def read_catalog_generated(path: Path) -> str:
    try:
        data = json.loads(Path(path).read_text())
    except (OSError, json.JSONDecodeError):
        return ""
    value = data.get("generated") if isinstance(data, dict) else ""
    return value if isinstance(value, str) else ""


def carried_built_at(freshness_path: Path) -> str:
    """`built_at` of the currently published overlay, carried forward.

    Schema 2 carries `overlay.built_at`; a schema-1 report only has the
    run timestamp `ts`, which is the closest thing to a build time it has.
    """
    try:
        data = json.loads(freshness_path.read_text())
    except (OSError, json.JSONDecodeError):
        return ""
    if not isinstance(data, dict):
        return ""
    overlay = data.get("overlay")
    if isinstance(overlay, dict) and isinstance(overlay.get("built_at"), str):
        return overlay["built_at"]
    ts = data.get("ts")
    return ts if isinstance(ts, str) else ""


def run_pipeline(root: Path, *, fetch_one=run_source, overlay_builder=run_overlay_builder,
                 corpus_builder=run_corpus_builder, now_iso: str | None = None,
                 allow_lkg_fallback: bool = True) -> int:
    """Fetch, build the candidate overlay, decide, and rebuild the corpus.

    `root` is the checkout the run operates on. The three runner arguments
    are the seams the offline tests replace (no network in tests).
    """
    root = Path(root)
    raw = root / "bench" / "raw"
    lkg = raw / ".lkg"
    bench = root / "bench"
    corpus_dir = root / "corpus"
    pricing_dir = root / "pricing"
    raw.mkdir(parents=True, exist_ok=True)
    snapshot_lkg(raw, lkg)

    now = now_iso or utc_now_iso()

    # -- fetch every bench source -----------------------------------------
    entries = fetch_sources(root, fetch_one)
    failed = [e for e in entries if not e["ok"]]
    for entry in failed:
        entry["lkg"] = restore_lkg_for(entry, raw, lkg) if allow_lkg_fallback else False
    if failed:
        print(
            f"corpus-fetch: {len(failed)} source(s) failed -> "
            + ", ".join(f"{e['name']} ({'LKG' if e['lkg'] else 'no LKG'})" for e in failed),
            file=sys.stderr,
        )

    # -- build the CANDIDATE overlay (never at the published path) --------
    candidate_path = raw / CANDIDATE_NAME
    candidate_path.unlink(missing_ok=True)
    overlay_builder(root, raw, candidate_path)
    candidate_scored = count_scored(candidate_path)

    committed_path = bench / "overlay.json"
    committed_exists = committed_path.exists()
    committed_scored = count_scored(committed_path)

    accept, reason = decide_overlay(entries, candidate_scored, committed_scored, committed_exists)
    if accept:
        bench.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(candidate_path, committed_path)
        stale = False
        built_at = now
        models_with_score = candidate_scored
        overlay_reason = ""
    else:
        # Keep the committed overlay byte-for-byte: a transient outage can
        # never erase routing scores.
        stale = True
        built_at = carried_built_at(corpus_dir / "freshness.json") or now
        models_with_score = committed_scored
        overlay_reason = reason
        print(f"corpus-fetch: overlay kept stale ({reason})", file=sys.stderr)

    # -- rebuild corpus + pricing, recording the price-source status ------
    previous_catalog_models = read_catalog_count(pricing_dir / "catalog.json")
    status_path = raw / STATUS_NAME
    status_path.unlink(missing_ok=True)
    builder_rc = corpus_builder(root, status_path)
    status: dict = {}
    if status_path.exists():
        try:
            status = json.loads(status_path.read_text())
        except json.JSONDecodeError:
            status = {}
    if not isinstance(status, dict):
        status = {}
    litellm = status.get("litellm") if isinstance(status.get("litellm"), dict) else {}
    openrouter = status.get("openrouter") if isinstance(status.get("openrouter"), dict) else {}
    if builder_rc != 0 and not litellm:
        litellm = {"ok": False, "models": 0, "reason": f"builder exited {builder_rc}"}
    if builder_rc != 0 and not openrouter:
        openrouter = {"ok": False, "added": 0, "reason": f"builder exited {builder_rc}"}
    litellm_status = {
        "ok": bool(litellm.get("ok")),
        "models": int(litellm.get("models") or 0),
        "reason": str(litellm.get("reason") or ""),
    }
    openrouter_status = {
        "ok": bool(openrouter.get("ok")),
        "added": int(openrouter.get("added") or 0),
        "reason": str(openrouter.get("reason") or ""),
    }
    catalog_models = int(status.get("catalog_models") or 0)
    shrink = 0.0
    if previous_catalog_models and catalog_models < previous_catalog_models:
        shrink = (previous_catalog_models - catalog_models) / previous_catalog_models
    prices_ok = litellm_status["ok"] and openrouter_status["ok"] and shrink <= CATALOG_DROP_THRESHOLD

    # -- record the run ----------------------------------------------------
    freshness = {
        "schema": 2,
        "ts": now,
        "sources": entries,
        "prices": {
            "ok": prices_ok,
            "generated": read_catalog_generated(pricing_dir / "catalog.json"),
            "litellm": litellm_status,
            "openrouter": openrouter_status,
            "catalog_models": catalog_models,
            "previous_catalog_models": previous_catalog_models,
        },
        "overlay": {
            "stale": stale,
            "built_at": built_at,
            "age_days": age_in_days(built_at, now),
            "models_with_score": models_with_score,
            "candidate_models_with_score": candidate_scored,
            "reason": overlay_reason,
        },
        "overlay_stale": stale,
        "overlay_age_days": age_in_days(built_at, now),
    }
    corpus_dir.mkdir(parents=True, exist_ok=True)
    with open(corpus_dir / "freshness.json", "w") as f:
        json.dump(freshness, f, indent=2, sort_keys=True)
        f.write("\n")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--allow-lkg-fallback", action="store_true",
                    help="recover from per-source fetch failures by reading last-known-good raw bytes")
    args = ap.parse_args()
    # Without the flag a failed source is never backfilled from LKG bytes:
    # it counts as unavailable, so the committed overlay is kept stale.
    return run_pipeline(REPO_ROOT, allow_lkg_fallback=args.allow_lkg_fallback)


if __name__ == "__main__":
    sys.exit(main())
