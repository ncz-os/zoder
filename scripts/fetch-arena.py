#!/usr/bin/env python3
"""Ingest the arena.ai leaderboards from the official public dataset.

arena.ai is a crowd-preference arena: scores are an Elo `rating` (webdev) or a
0..1 preference `score` (agent), NOT a task solve-rate. So these feed a SEPARATE
`preference` signal in the corpus, never the solve-rate composite (averaging an
Elo into a mean of pass-rates would be meaningless).

The arena.ai leaderboard PAGES are behind a Cloudflare managed challenge
(`HTTP 403` + `cf-mitigated: challenge`) for every non-browser client, and we do
not attempt to get around it. Instead this reads the dataset the arena team
publishes on Hugging Face for exactly this purpose:

  lmarena-ai/leaderboard-dataset  (CC-BY-4.0, not gated, refreshed with each
                                   leaderboard publish)

via the public datasets-server JSON API (no extra Python dependency):

  https://datasets-server.huggingface.co/rows
      ?dataset=lmarena-ai/leaderboard-dataset&config=webdev|agent
      &split=latest&offset=N&length=100

Each response is {"features": [...], "rows": [{"row": {...}}, ...],
"num_rows_total": N}. The split carries every category side by side; only
`category == "overall"` rows are kept. Pages are walked until num_rows_total is
reached, retrying each page up to 3 times with backoff on 5xx / 429 / timeouts
(the datasets-server occasionally answers transient errors).

Two surfaces:
  webdev -> {model, rating, rank, votes}      (Elo)
  agent  -> {model, score, rank}              (0..1 win-rate)

Emits one shared-contract file per surface, with the dataset provenance:
  {"benchmark": "arena-webdev"|"arena-agent",
   "source": "lmarena-ai/leaderboard-dataset (Hugging Face)",
   "published": "<leaderboard_publish_date>",
   "models": [{"model", "rating"|"score", "rank", ...}]}

Exits non-zero if either surface yields zero overall rows.

Usage:
  fetch-arena.py [--webdev-json OUT] [--agent-json OUT] [--out-dir DIR]
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

DATASET = "lmarena-ai/leaderboard-dataset"
ROWS_API = "https://datasets-server.huggingface.co/rows"
SOURCE = "lmarena-ai/leaderboard-dataset (Hugging Face)"
PAGE_SIZE = 100
ATTEMPTS = 3


def page_url(config: str, offset: int, length: int = PAGE_SIZE) -> str:
    q = urllib.parse.urlencode(
        {
            "dataset": DATASET,
            "config": config,
            "split": "latest",
            "offset": offset,
            "length": length,
        }
    )
    # Keep the dataset id readable ("owner/name"), as the API documents it.
    q = q.replace("%2F", "/")
    return f"{ROWS_API}?{q}"


def fetch_json(url: str, timeout: int = 60):
    """GET one page of the dataset, retrying transient failures with backoff."""
    req = urllib.request.Request(url, headers={"User-Agent": "zoder-ingest"})
    last = None
    for attempt in range(ATTEMPTS):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return json.loads(r.read().decode("utf-8", "replace"))
        except urllib.error.HTTPError as e:
            last = f"HTTP {e.code}"
            if e.code < 500 and e.code != 429:
                raise SystemExit(f"arena: {url} -> {last} (not retryable)")
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
            last = str(e)
        if attempt + 1 < ATTEMPTS:
            time.sleep(2**attempt)
    raise SystemExit(f"arena: {url} failed after {ATTEMPTS} attempts ({last})")


def iter_rows(config: str):
    """Yield every recorded row of one config's `latest` split, page by page."""
    offset, total = 0, None
    while total is None or offset < total:
        page = fetch_json(page_url(config, offset))
        rows = page.get("rows") or []
        total = page.get("num_rows_total")
        if not rows:
            break  # upstream says there is more but serves nothing: stop
        for entry in rows:
            row = entry.get("row") if isinstance(entry, dict) else None
            if isinstance(row, dict):
                yield row
        offset += len(rows)


def _overall(config: str):
    """(overall rows, publish date) for one config."""
    rows, published = [], None
    for row in iter_rows(config):
        if str(row.get("category", "")).strip().lower() != "overall":
            continue
        rows.append(row)
        if not published and row.get("leaderboard_publish_date"):
            published = row["leaderboard_publish_date"]
    return rows, published


def parse_webdev(rows) -> dict:
    models = []
    for e in rows:
        name = e.get("model_name")
        rating = e.get("rating")
        if not name or not isinstance(rating, (int, float)):
            continue
        models.append(
            {
                "model": name,
                "rating": round(float(rating), 1),
                "rank": e.get("rank"),
                "votes": e.get("vote_count"),
            }
        )
    models.sort(key=lambda r: r["rating"], reverse=True)
    return models


def parse_agent(rows) -> dict:
    models = []
    for e in rows:
        name = e.get("model_name")
        score = e.get("score")
        if not name or not isinstance(score, (int, float)):
            continue
        models.append(
            {"model": name, "score": round(float(score), 4), "rank": e.get("rank")}
        )
    models.sort(key=lambda r: r["score"], reverse=True)
    return models


def build_webdev(rows, published) -> dict:
    return {
        "benchmark": "arena-webdev",
        "source": SOURCE,
        "published": published,
        "models": parse_webdev(rows),
    }


def build_agent(rows, published) -> dict:
    return {
        "benchmark": "arena-agent",
        "source": SOURCE,
        "published": published,
        "models": parse_agent(rows),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--out-dir",
        default=None,
        help="write arena-webdev.json + arena-agent.json here",
    )
    ap.add_argument("--webdev-json", default=None)
    ap.add_argument("--agent-json", default=None)
    args = ap.parse_args()

    webdev_rows, webdev_pub = _overall("webdev")
    agent_rows, agent_pub = _overall("agent")
    if not webdev_rows:
        raise SystemExit("arena webdev: dataset yielded no 'overall' rows")
    if not agent_rows:
        raise SystemExit("arena agent: dataset yielded no 'overall' rows")

    webdev = build_webdev(webdev_rows, webdev_pub)
    agent = build_agent(agent_rows, agent_pub)

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
        print(
            f"wrote {path} ({len(data['models'])} models, published {data['published']})"
        )
    print(
        "webdev top-3:",
        ", ".join(f"{r['model']}={r['rating']:.0f}" for r in webdev["models"][:3]),
    )
    print(
        "agent  top-3:",
        ", ".join(f"{r['model']}={r['score']:.3f}" for r in agent["models"][:3]),
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
