#!/usr/bin/env python3
"""Ingest the Terminal-Bench leaderboard (no API — RSC payload).

Terminal-Bench publishes its leaderboard as a Next.js page whose row data is
embedded in the RSC stream, so it is consumable with a single HTTP GET. The
site now serves only its CURRENT default leaderboard at the site root (the
per-version URLs such as /leaderboard/terminal-bench/2.0 answer 308 to the home
page, which ignores the requested version), so this fetcher reads
https://www.tbench.ai/ and records the leaderboard title it finds next to the
rows — that title ("Terminal-Bench 4.0", …) is what tells you WHICH version the
numbers belong to.

Each row is an (agent, model) pair: Terminal-Bench is *agentic*, so the same
model appears under several scaffolds ("agents"). We key by model and keep the
best-scoring run, recording the winning agent as the harness for provenance
(scaffold choice swings agentic scores a lot).

Two row schemas are understood, because the site has published both:

  current: {"rank", "metadata": {"date", "model_display": {"label", ...},
            "model_org": {"label"}, "agent_display": {"label"}, ...},
            "metrics": {"accuracy": 0..100, ...}}
  legacy:  {"modelNames": [...], "accuracy": 0..1, "agent": ..., "date": ...}

`accuracy` is normalized to 0..100: a value <= 1.0 is read as a fraction and
multiplied by 100, so either schema lands on the same scale.

Model labels on the current leaderboard are short ("Opus 5.5", "Fable 5.1")
while the corpus uses "claude-opus-5-5". When the row names Anthropic as the
model vendor and the label does not already say "claude", the name is emitted as
"Claude <label>", so scripts/corpus_match.canon_key yields the corpus key. Other
vendors keep their label verbatim.

Emits the shared ingester contract plus the leaderboard version:
  {"benchmark": "terminal-bench", "source": "terminal-bench",
   "leaderboard": "Terminal-Bench 4.0",
   "models": [{"model", "acc", "date", "harness"}]}

Fails loudly (non-zero) when the page yields no rows or no leaderboard title.

Usage:
  fetch-terminal-bench.py [--url URL] [--json OUT.json]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from rsc_util import extract_array, fetch, rsc_text

DEFAULT_URL = "https://www.tbench.ai/"
# The site sometimes serves a prerendered skeleton with an empty dehydrated
# query (no rows). That is a cache state, not a format change, so retry a few
# times before declaring the page broken.
FETCH_ATTEMPTS = 3

# The dehydrated query serializes the leaderboard object immediately before the
# rows it belongs to: `"leaderboard":{...,"title":"Terminal-Bench 4.0",...}`.
_TITLE_RE = re.compile(r'"title"\s*:\s*"((?:[^"\\]|\\.)*)"')


def normalize_acc(value) -> float | None:
    """Accuracy on a 0..100 scale from either schema (fraction or percent)."""
    if value is None or isinstance(value, bool):
        return None
    try:
        acc = float(value)
    except (TypeError, ValueError):
        return None
    if acc <= 1.0:  # site historically reported 0..1
        acc *= 100.0
    return acc


def model_name(row: dict) -> str | None:
    """Model label for a row, with the Anthropic prefix rule applied."""
    meta = row.get("metadata") or {}
    disp = meta.get("model_display")
    if isinstance(disp, dict):
        label, org = disp.get("label"), (meta.get("model_org") or {}).get("label")
    else:
        names = row.get("modelNames") or []
        label, org = (names[0] if names else None), row.get("model_org")
    if not label:
        label = row.get("model")
    if not label:
        return None
    label = str(label).strip()
    if str(org or "").strip().lower() == "anthropic" and "claude" not in label.lower():
        label = f"Claude {label}"
    return label


def row_acc(row: dict) -> float | None:
    metrics = row.get("metrics")
    if isinstance(metrics, dict) and "accuracy" in metrics:
        return normalize_acc(metrics["accuracy"])
    return normalize_acc(row.get("accuracy"))


def row_meta(row: dict) -> tuple:
    """(date, harness) for a row, from either schema."""
    meta = row.get("metadata")
    if isinstance(meta, dict):
        agent = meta.get("agent_display")
        harness = agent.get("label") if isinstance(agent, dict) else agent
        return meta.get("date"), harness
    return row.get("date"), row.get("agent")


def leaderboard_title(text: str, rows_at: int) -> str | None:
    """The leaderboard title that precedes the rows in the same dehydrated query.

    The dehydrated payload serializes `"leaderboard":{...,"title":"...",...}`
    immediately before the `"rows":[` it belongs to, so only titles between the
    last `"leaderboard":{` and the rows marker are candidates.
    """
    lb_at = text.rfind('"leaderboard":{', 0, rows_at)
    if lb_at < 0:
        return None
    titles = list(_TITLE_RE.finditer(text, lb_at, rows_at))
    if not titles:
        return None
    try:
        return json.loads('"' + titles[0].group(1) + '"')
    except json.JSONDecodeError:
        return titles[0].group(1)


def parse_rsc(text: str) -> dict:
    """Parse a decoded RSC payload into the output contract."""
    marker = text.find('"rows":[')
    if marker < 0:
        raise SystemExit(
            "terminal-bench: could not locate leaderboard rows (page format changed?)"
        )
    rows = extract_array(text, "rows") or []
    title = leaderboard_title(text, marker)
    if not rows:
        raise SystemExit(
            "terminal-bench: leaderboard has no rows (page format changed?)"
        )
    if not title:
        raise SystemExit(
            "terminal-bench: could not locate leaderboard title (page format changed?)"
        )

    best: dict[str, dict] = {}
    for r in rows:
        if not isinstance(r, dict):
            continue
        name = model_name(r)
        acc = row_acc(r)
        if not name or acc is None:
            continue
        date, harness = row_meta(r)
        cur = best.get(name)
        if cur is None or acc > cur["acc"]:
            best[name] = {
                "model": name,
                "acc": round(acc, 2),
                "date": date,
                "harness": harness,
            }
    models = sorted(best.values(), key=lambda r: r["acc"], reverse=True)
    if not models:
        raise SystemExit("terminal-bench: no scored rows found — format changed?")
    return {
        "benchmark": "terminal-bench",
        "source": "terminal-bench",
        "leaderboard": title,
        "models": models,
    }


def parse(html: str) -> dict:
    return parse_rsc(rsc_text(html))


def fetch_with_rows(url: str) -> str:
    """Fetch the page until the payload carries leaderboard rows."""
    html = ""
    for attempt in range(FETCH_ATTEMPTS):
        html = fetch(url)
        if '"rows":[' in rsc_text(html):
            return html
        if attempt + 1 < FETCH_ATTEMPTS:
            time.sleep(2**attempt)
    return html


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=DEFAULT_URL)
    ap.add_argument("--json", default=None)
    args = ap.parse_args()
    data = parse(fetch_with_rows(args.url))
    if args.json:
        json.dump(data, open(args.json, "w"), indent=1)
        top = data["models"][:5]
        print(
            f"wrote {args.json} ({len(data['models'])} models, {data['leaderboard']})"
        )
        print("top-5:", ", ".join(f"{r['model']}={r['acc']:.1f}%" for r in top))
    else:
        print(json.dumps(data, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
