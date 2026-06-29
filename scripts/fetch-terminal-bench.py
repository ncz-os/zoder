#!/usr/bin/env python3
"""Ingest the Terminal-Bench 2.0 leaderboard (no API — RSC payload).

Terminal-Bench 2.0 publishes its leaderboard as a Next.js page whose rows are
embedded in the RSC stream, so it is consumable with a single HTTP GET. Each row
is an (agent, model) pair: Terminal-Bench is *agentic*, so the same model appears
under several scaffolds ("agents"). We key by model and keep the best-scoring
run, recording the winning agent as the harness for provenance (scaffold choice
swings agentic scores a lot).

Emits the shared ingester contract:
  {"benchmark": "terminal-bench", "source": "terminal-bench",
   "models": [{"model", "acc", "date", "harness"}]}

Usage:
  fetch-terminal-bench.py [--url URL] [--json OUT.json]
"""
from __future__ import annotations
import argparse, json, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from rsc_util import fetch, rsc_text, extract_array  # noqa: E402

DEFAULT_URL = "https://www.tbench.ai/leaderboard/terminal-bench/2.0"


def parse(html: str) -> dict:
    rows = extract_array(rsc_text(html), "rows")
    if not rows:
        raise SystemExit("terminal-bench: could not locate leaderboard rows (page format changed?)")
    best: dict[str, dict] = {}
    for r in rows:
        name = (r.get("modelNames") or r.get("model") or [None])[0]
        acc = r.get("accuracy")
        if not name or acc is None:
            continue
        acc = float(acc) * 100.0  # site reports 0..1
        cur = best.get(name)
        if cur is None or acc > cur["acc"]:
            best[name] = {
                "model": name,
                "acc": round(acc, 2),
                "date": r.get("date"),
                "harness": r.get("agent"),
            }
    models = sorted(best.values(), key=lambda r: r["acc"], reverse=True)
    if not models:
        raise SystemExit("terminal-bench: no scored rows found — format changed?")
    return {"benchmark": "terminal-bench", "source": "terminal-bench", "models": models}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=DEFAULT_URL)
    ap.add_argument("--json", default=None)
    args = ap.parse_args()
    data = parse(fetch(args.url))
    if args.json:
        json.dump(data, open(args.json, "w"), indent=1)
        top = data["models"][:5]
        print(f"wrote {args.json} ({len(data['models'])} models)")
        print("top-5:", ", ".join(f"{r['model']}={r['acc']:.1f}%" for r in top))
    else:
        print(json.dumps(data, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
