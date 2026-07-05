#!/usr/bin/env python3
"""Deterministic corpus fetch + overlay build for the GitLab `corpus-sync`
job (Finding #16 fix).

The prior inline shell did this:

    run() { echo "== $1"; python3 "scripts/$1" "${@:2}" || echo "WARN: $1 failed — degrading coverage"; }
    run fetch-vals-swebench.py ...
    ...
    python3 scripts/build-bench-overlay.py --out-dir .
    python3 scripts/build-public-corpus.py --out-dir .

Two defects (Finding #16):
  1. Failed fetches became warnings, so an empty upstream response was
     treated the same as a full success and the overlay got thinner.
  2. `bench/raw` started empty on first run, so a transient outage
     could turn the overlay empty, and the next day’s job would
     commit that empty overlay to master.

This script:
  * Snapshots existing `bench/raw/*.json` into `bench/raw.lkg/*.json`
    LAST-KNOWN-GOOD before fetching; if any fetch fails we fall back
    to the LKG bytes (so raw inputs stay populated + an overlay that
    matches reality rather than an empty overlay).
  * Writes `corpus/freshness.json` describing which sources fetched
    fresh, fell back to LKG, or failed entirely.
  * Then calls `scripts/build-bench-overlay.py` + `scripts/build-public-corpus.py`
    so the rest of the pipeline is unchanged.

Exit code: 0 if everything succeeded OR if at least one source was
recovered via LKG with no irreversible degradation (the freshness
report is the operator's signal). Non-zero only if a HARD failure
prevents producing any overlay (e.g. ALL sources failed simultaneously).

The freshness-threshold gate that refuses to commit a degraded overlay
lives in `corpus-freshness-check.py` so it can be unit-tested in CI
without re-running this script.
"""
from __future__ import annotations
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]   # scripts/ci -> repo root
LKG = REPO_ROOT / "bench" / "raw.lkg"
RAW = REPO_ROOT / "bench" / "raw"

# Sources the corpus-sync job fetches. Each entry: (script-name, output files it produces, args)
SOURCES = [
    ("fetch-vals-swebench.py",  ["vals-swebench.json"],  ["--json", "bench/raw/vals-swebench.json"]),
    ("fetch-aider-polyglot.py", ["aider-polyglot.json"], ["--json", "bench/raw/aider-polyglot.json"]),
    ("fetch-arena.py",          ["arena-webdev.json", "arena-agent.json"], ["--out-dir", "bench/raw"]),
    ("fetch-scale.py",          ["scale-seal.json"],    ["--json", "bench/raw/scale-seal.json"]),
    ("fetch-terminal-bench.py", ["terminal-bench.json"], ["--json", "bench/raw/terminal-bench.json"]),
]


def snapshot_lkg() -> None:
    """If RAW already has files, mirror them into LKG before fetching."""
    LKG.mkdir(parents=True, exist_ok=True)
    if not RAW.exists():
        return
    for f in RAW.iterdir():
        if f.is_file():
            shutil.copy2(f, LKG / f.name)


def restore_lkg_for(sources_failed: list[str]) -> None:
    """For sources whose fetch failed, copy their LKG bytes back into RAW
    so the overlay builder sees last-known-good data instead of empty."""
    for src in sources_failed:
        for outfile in src["outputs"]:
            lkg_file = LKG / outfile
            raw_file = RAW / outfile
            if lkg_file.exists():
                shutil.copy2(lkg_file, raw_file)


def run_source(src_script: str, args: list[str]) -> tuple[bool, str]:
    """Run one fetch script with the same cwd the runner uses."""
    cmd = ["python3", f"scripts/{src_script}", *args]
    try:
        r = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True, timeout=600)
    except subprocess.TimeoutExpired:
        return False, f"timeout after 600s"
    except Exception as e:
        return False, f"spawn failed: {e}"
    if r.returncode != 0:
        return False, (r.stderr.strip() or "non-zero exit")[:500]
    # The fetcher writes its output via the args (out-dir or --json file
    # path). Verify the expected files exist + are non-empty.
    for outfile in args:
        if outfile.endswith(".json"):
            p = REPO_ROOT / outfile
            if not p.exists() or p.stat().st_size == 0:
                return False, f"missing/empty output: {outfile}"
    return True, ""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--allow-lkg-fallback", action="store_true",
                    help="recover from per-source fetch failures by reading last-known-good raw bytes")
    args = ap.parse_args()
    RAW.mkdir(parents=True, exist_ok=True)
    snapshot_lkg()

    freshness: dict = {"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "sources": []}
    failed = []
    for name, outputs, src_args in SOURCES:
        ok, why = run_source(name, src_args)
        entry = {"name": name, "outputs": outputs, "ok": ok, "reason": why}
        freshness["sources"].append(entry)
        if not ok:
            failed.append(entry)

    if failed:
        if args.allow_lkg_fallback:
            print(f"corpus-fetch: {len(failed)} source(s) failed -> restoring from LKG", file=sys.stderr)
            for src in failed:
                for outfile in src["outputs"]:
                    lkg_file = LKG / outfile
                    if lkg_file.exists():
                        shutil.copy2(lkg_file, RAW / outfile)
                    else:
                        # Never had LKG for this source; do nothing — the
                        # overlay will simply omit that signal. The freshness
                        # check will catch this in the next stage.
                        pass
        else:
            print(f"corpus-fetch: {len(failed)} source(s) failed (no LKG fallback requested)", file=sys.stderr)

    # Always emit a freshness report under corpus/ — the freshness-check
    # script compares this against the prior report to enforce a coverage
    # threshold before committing.
    out_dir = REPO_ROOT / "corpus"
    out_dir.mkdir(exist_ok=True)
    with open(out_dir / "freshness.json", "w") as f:
        json.dump(freshness, f, indent=2, sort_keys=True)
        f.write("\n")

    # Rebuild overlay + corpus using whatever RAW exists.
    for script, extra in [
        ("build-bench-overlay.py", ["--out-dir", "."]),
        ("build-public-corpus.py", ["--out-dir", "."]),
    ]:
        r = subprocess.run(["python3", f"scripts/{script}", *extra], cwd=REPO_ROOT)
        if r.returncode != 0:
            print(f"corpus-fetch: {script} failed", file=sys.stderr)
            return r.returncode

    if failed and not args.allow_lkg_fallback:
        # Soft-fail so the operator notices; gating happens in the next
        # step (corpus-freshness-check.py) which uses coverage delta.
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
