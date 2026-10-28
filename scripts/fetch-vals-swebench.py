#!/usr/bin/env python3
"""Ingest vals.ai SWE-bench Verified multi-axis results (no API — static scrape).

vals.ai publishes the SWE-bench Verified leaderboard as a server-rendered Astro
page; the full dataset is embedded in the page's astro-island `props` (HTML-
escaped JSON), so it is consumable with a plain HTTP GET — no API key, no
headless browser. This ingester fetches the page, decodes the island that holds
`benchmarkView.tasks`, and emits one normalized record per model with every axis:

  accuracy, stderr, cost_per_test, latency_s, max_output_tokens,
  reasoning_effort, compute_effort, provider,
  and per-difficulty-tier accuracy (<15min / 15m-1h / 1-4h / >4h).

These join to the zoder/zoder pricing catalog (by provider+model) to produce the
value axis the dashboard wants: quality (SWE acc) vs cost ($/test here, $/Mtok in
pricing) vs latency.

Usage:
  fetch-vals-swebench.py [--url URL] [--json OUT.json] [--csv OUT.csv]
"""
from __future__ import annotations
import argparse, csv, html, json, re, sys, urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from rsc_util import fetch as _fetch  # noqa: E402

DEFAULT_URL = "https://www.vals.ai/benchmarks/swebench"

def fetch(url: str, **kwargs) -> str:
    # Shared helper: 3 attempts, backoff, 4xx non-retryable — same semantics as
    # the RSC fetcher (rsc_util.fetch). The page parse stays in parse().
    return _fetch(url, **kwargs)

def unwrap(o):
    # Astro serializes props as [TYPE, value]; unwrap recursively to plain JSON.
    if isinstance(o, list) and len(o) == 2 and isinstance(o[0], int):
        return unwrap(o[1])
    if isinstance(o, list):
        return [unwrap(x) for x in o]
    if isinstance(o, dict):
        return {k: unwrap(v) for k, v in o.items()}
    return o

def parse(page: str) -> dict:
    blobs = [html.unescape(b) for b in re.findall(r'props="(\{.*?\})"', page, re.S)]
    bv = None
    for b in blobs:
        if "benchmarkView" not in b:
            continue
        try:
            d = unwrap(json.loads(b))
        except Exception:
            continue
        if isinstance(d, dict) and "benchmarkView" in d:
            bv = d["benchmarkView"]
            break
    if not bv:
        raise SystemExit("could not locate benchmarkView island (page format changed?)")
    tasks = bv["tasks"]
    overall = tasks["overall"]
    tiers = [t for t in tasks if t != "overall"]
    rows = []
    for model, rec in overall.items():
        row = {
            "model": model,
            "provider": rec.get("provider"),
            "accuracy": rec.get("accuracy"),
            "stderr": rec.get("stderr"),
            "cost_per_test": rec.get("cost_per_test"),
            "latency_s": rec.get("latency"),
            "max_output_tokens": rec.get("max_output_tokens"),
            "reasoning_effort": rec.get("reasoning_effort"),
            "compute_effort": rec.get("compute_effort"),
        }
        for t in tiers:
            tr = tasks[t].get(model)
            row[f"acc::{t}"] = tr.get("accuracy") if tr else None
        rows.append(row)
    rows.sort(key=lambda r: (r["accuracy"] or 0), reverse=True)
    return {"benchmark": "swebench-verified", "source": "vals.ai",
            "tiers": tiers, "models": rows}

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=DEFAULT_URL)
    ap.add_argument("--json", default=None)
    ap.add_argument("--csv", default=None)
    args = ap.parse_args()
    data = parse(fetch(args.url))
    if args.json:
        json.dump(data, open(args.json, "w"), indent=1)
        print(f"wrote {args.json} ({len(data['models'])} models)")
    if args.csv:
        cols = list(data["models"][0].keys())
        with open(args.csv, "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=cols)
            w.writeheader()
            w.writerows(data["models"])
        print(f"wrote {args.csv}")
    if not (args.json or args.csv):
        print(json.dumps(data, indent=1))
    else:
        top = data["models"][:5]
        print("top-5:", ", ".join(f"{r['model']}={r['accuracy']:.1f}%/${r['cost_per_test']:.2f}" for r in top))
    return 0

if __name__ == "__main__":
    sys.exit(main())
