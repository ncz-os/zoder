#!/usr/bin/env python3
"""Ingest Scale's SEAL SWE-Atlas leaderboards (no API — RSC payload).

Scale's labs leaderboards are Next.js pages whose rows stream in the RSC payload,
so each board is consumable with one HTTP GET. Scale splits coding into several
SWE-Atlas boards (refactoring, test-writing, Q&A); we pull the coding boards and
report one Scale score per model = the mean of its per-board scores (all 0..100,
a locked/standardized harness). The agentic `mcp_atlas` (tool use) board is
excluded by default to keep this a pure-coding solve-rate.

Emits the shared contract:
  {"benchmark": "scale-seal", "source": "scale-seal",
   "models": [{"model", "acc", "boards"}]}

Usage:
  fetch-scale.py [--json OUT.json] [--boards a,b,c]
"""
from __future__ import annotations
import argparse, json, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from rsc_util import fetch, rsc_text, extract_array  # noqa: E402

BASE = "https://labs.scale.com/leaderboard"
DEFAULT_BOARDS = ["sweatlas-refactoring", "sweatlas-tw", "sweatlas-qna"]


def board_scores(slug: str) -> dict[str, float]:
    """Best score per model on one board (a model can repeat across scaffolds)."""
    entries = extract_array(rsc_text(fetch(f"{BASE}/{slug}")), "entries")
    out: dict[str, float] = {}
    for e in entries or []:
        name = e.get("model")
        score = e.get("score")
        if not name or score is None:
            continue
        s = float(score)
        if name not in out or s > out[name]:
            out[name] = s
    return out


def parse(boards: list[str]) -> dict:
    agg: dict[str, list[float]] = {}
    used = []
    for slug in boards:
        try:
            sc = board_scores(slug)
        except Exception as e:
            print(f"  ! board {slug}: {e}", file=sys.stderr)
            continue
        if not sc:
            continue
        used.append(slug)
        for name, s in sc.items():
            agg.setdefault(name, []).append(s)
    if not agg:
        raise SystemExit("scale: no scored rows on any board (format changed?)")
    models = [
        {"model": name, "acc": round(sum(v) / len(v), 2), "boards": len(v)}
        for name, v in agg.items()
    ]
    models.sort(key=lambda r: r["acc"], reverse=True)
    return {"benchmark": "scale-seal", "source": "scale-seal", "boards": used, "models": models}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", default=None)
    ap.add_argument("--boards", default=",".join(DEFAULT_BOARDS))
    args = ap.parse_args()
    data = parse([b.strip() for b in args.boards.split(",") if b.strip()])
    if args.json:
        json.dump(data, open(args.json, "w"), indent=1)
        print(f"wrote {args.json} ({len(data['models'])} models from {data['boards']})")
        print("top-5:", ", ".join(f"{r['model']}={r['acc']:.1f}" for r in data["models"][:5]))
    else:
        print(json.dumps(data, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
