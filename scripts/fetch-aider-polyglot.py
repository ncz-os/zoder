#!/usr/bin/env python3
"""Ingest the Aider Polyglot leaderboard (no API — raw data file).

Aider publishes its Polyglot leaderboard as a plain YAML data file in the
project repo, so it is consumable with a single HTTP GET — no scraping of
rendered HTML, no headless browser. This is the most robust of the coding
feeds: the file is the leaderboard's own source of truth.

The headline number on the leaderboard is `pass_rate_2` (percent of the 225
exercises solved within the allowed retries). The YAML holds historical runs,
so several rows can share a `model`; we keep the best-scoring run per model and
record its date and edit format for provenance.

Emits the shared ingester contract:
  {"benchmark", "source", "date", "models": [{"model", "acc", ...}]}

Usage:
  fetch-aider-polyglot.py [--url URL] [--json OUT.json]
"""
from __future__ import annotations
import argparse, json, sys, urllib.request
import yaml

DEFAULT_URL = (
    "https://raw.githubusercontent.com/Aider-AI/aider/main/"
    "aider/website/_data/polyglot_leaderboard.yml"
)


def fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0 (zoder-ingest)"})
    with urllib.request.urlopen(req, timeout=40) as r:
        return r.read().decode("utf-8", "replace")


def _date_from_dirname(dirname: str | None) -> str | None:
    # dirnames look like "2025-02-25-20-23-07--gemini-pro"; take the date prefix.
    if not dirname or len(dirname) < 10:
        return None
    head = dirname[:10]
    return head if head[4] == "-" and head[7] == "-" else None


def parse(text: str) -> dict:
    rows = yaml.safe_load(text)
    if not isinstance(rows, list):
        raise SystemExit("aider: unexpected leaderboard shape (not a list) — format changed?")
    best: dict[str, dict] = {}
    for r in rows:
        if not isinstance(r, dict):
            continue
        model = r.get("model")
        acc = r.get("pass_rate_2")
        if not model or acc is None:
            continue
        cur = best.get(model)
        if cur is None or acc > cur["acc"]:
            best[model] = {
                "model": model,
                "acc": float(acc),
                "edit_format": r.get("edit_format"),
                "test_cases": r.get("test_cases"),
                "percent_cases_well_formed": r.get("percent_cases_well_formed"),
                "date": _date_from_dirname(r.get("dirname")),
            }
    models = sorted(best.values(), key=lambda r: r["acc"], reverse=True)
    if not models:
        raise SystemExit("aider: no scored rows found — format changed?")
    return {"benchmark": "aider-polyglot", "source": "aider", "models": models}


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
