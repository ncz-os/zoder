#!/usr/bin/env python3
"""Ingest arena.ai leaderboards (no API — RSC payload).

arena.ai is a crowd-preference arena: scores are an Elo `rating` (webdev) or a
0..1 preference `score` (agent), NOT a task solve-rate. So these feed a SEPARATE
`preference` signal in the corpus, never the solve-rate composite (averaging an
Elo into a mean of pass-rates would be meaningless). The webdev table also
carries list pricing, which we pass through for the economics projection.

Two surfaces:
  webdev (code) -> {model, rating, rank, votes, input/output $\u200b/Mtok}
  agent         -> {model, score, rank}

Emits one shared-contract file per surface:
  {"benchmark": "arena-webdev"|"arena-agent", "source": "arena.ai",
   "models": [{"model", "rating"|"score", "rank", ...}]}

Usage:
  fetch-arena.py [--webdev-json OUT] [--agent-json OUT] [--out-dir DIR]
"""
from __future__ import annotations
import argparse, json, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from rsc_util import fetch, rsc_text, extract_array  # noqa: E402

WEBDEV_URL = "https://arena.ai/leaderboard/code/webdev"
AGENT_URL = "https://arena.ai/leaderboard/agent"


def parse_webdev(html: str) -> dict:
    entries = extract_array(rsc_text(html), "entries")
    if not entries:
        raise SystemExit("arena webdev: no entries (format changed?)")
    models = []
    for e in entries:
        name = e.get("modelDisplayName") or e.get("modelKey")
        rating = e.get("rating")
        if not name or rating is None:
            continue
        row = {"model": name, "rating": round(float(rating), 1), "rank": e.get("rank"),
               "votes": e.get("votes")}
        if e.get("inputPricePerMillion") is not None:
            row["input_usd_per_mtok"] = e["inputPricePerMillion"]
        if e.get("outputPricePerMillion") is not None:
            row["output_usd_per_mtok"] = e["outputPricePerMillion"]
        models.append(row)
    models.sort(key=lambda r: r["rating"], reverse=True)
    return {"benchmark": "arena-webdev", "source": "arena.ai", "models": models}


def parse_agent(html: str) -> dict:
    entries = extract_array(rsc_text(html), "entries")
    if not entries:
        raise SystemExit("arena agent: no entries (format changed?)")
    models = []
    for e in entries:
        name = e.get("model") or e.get("contenderName")
        score = e.get("score")
        if not name or score is None:
            continue
        models.append({"model": name, "score": round(float(score), 4), "rank": e.get("rank")})
    models.sort(key=lambda r: r["score"], reverse=True)
    return {"benchmark": "arena-agent", "source": "arena.ai", "models": models}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", default=None, help="write arena-webdev.json + arena-agent.json here")
    ap.add_argument("--webdev-json", default=None)
    ap.add_argument("--agent-json", default=None)
    args = ap.parse_args()

    webdev = parse_webdev(fetch(WEBDEV_URL))
    agent = parse_agent(fetch(AGENT_URL))

    out = {}
    if args.out_dir:
        d = Path(args.out_dir)
        out[d / "arena-webdev.json"] = webdev
        out[d / "arena-agent.json"] = agent
    if args.webdev_json:
        out[Path(args.webdev_json)] = webdev
    if args.agent_json:
        out[Path(args.agent_json)] = agent

    if not out:
        print(json.dumps({"webdev": webdev, "agent": agent}, indent=1))
        return 0
    for path, data in out.items():
        json.dump(data, open(path, "w"), indent=1)
        print(f"wrote {path} ({len(data['models'])} models)")
    print("webdev top-3:", ", ".join(f"{r['model']}={r['rating']:.0f}" for r in webdev["models"][:3]))
    print("agent  top-3:", ", ".join(f"{r['model']}={r['score']:.3f}" for r in agent["models"][:3]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
