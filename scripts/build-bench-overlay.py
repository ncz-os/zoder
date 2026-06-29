#!/usr/bin/env python3
"""Combine the per-source benchmark + arena.ai feeds into one bench/overlay.json
keyed by canonical model key, ready for build-public-corpus.py to fold into the
ModelEntry corpus.

This is the "wiring" the public corpus builder punted on: the fetch-*.py scripts
each emit a normalized {benchmark, source, models:[...]} file, but nothing joined
them, name-matched them to corpus ids, and computed the composite the router
sorts on. This does that.

Inputs  (bench/raw/, produced by the fetch-*.py scripts):
  vals-swebench.json   SWE-bench Verified solve-rate + cost/latency (vals.ai)
  aider-polyglot.json  Aider Polyglot pass-rate
  scale-seal.json      Scale SEAL SWE-Atlas mean
  terminal-bench.json  Terminal-Bench agentic shell
  arena-webdev.json    arena.ai webdev/code Elo (+ list pricing)
  arena-agent.json     arena.ai agent win-rate (0..1)

Output  (bench/overlay.json): {canon_key: {capability, preference,
  arena_coding_elo, arena_webdev_elo, w_swe, agentic_score}} — the optional
  capability/preference overlay the corpus builder merges by canon_key(id).

agentic_score (0..1): the single value the Auto router + `zoder models` sort on.
A weighted blend of measured coding solve-rate (authoritative), arena webdev Elo,
and arena agent win-rate, reweighted over whichever signals are present.
"""
from __future__ import annotations
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from corpus_match import canon_key  # noqa: E402

# Solve-rate feeds -> Capability sub-block. (raw filename, capability field, source)
SOLVE = [
    ("vals-swebench.json", "swe_verified", "vals.ai"),
    ("aider-polyglot.json", "aider_polyglot", "aider"),
    ("scale-seal.json", "scale_seal", "scale-seal"),
    ("terminal-bench.json", "terminal_bench", "terminal-bench"),
]


def _load(raw_dir, fname):
    try:
        with open(os.path.join(raw_dir, fname)) as f:
            return json.load(f).get("models", [])
    except FileNotFoundError:
        print(f"  (missing {fname} — skipped)", file=sys.stderr)
        return []


def _acc(m):
    """Solve-rate on a 0..100 scale, from whichever field the source uses."""
    for k in ("accuracy", "acc"):
        if isinstance(m.get(k), (int, float)):
            return float(m[k])
    return None


def _clamp(x, lo=0.0, hi=1.0):
    return max(lo, min(hi, x))


def build_overlay(raw_dir):
    # acc[canon][field] = best BenchScore dict; pref[canon] = {arena_webdev, arena_agent}
    cap: dict[str, dict] = {}
    pref: dict[str, dict] = {}

    for fname, field, source in SOLVE:
        for m in _load(raw_dir, fname):
            k = canon_key(m.get("model", ""))
            acc = _acc(m)
            if not k or acc is None:
                continue
            score = {"acc": round(acc, 2), "source": source}
            if isinstance(m.get("cost_per_test"), (int, float)):
                score["cost_per_test"] = round(float(m["cost_per_test"]), 4)
            if isinstance(m.get("latency_s"), (int, float)):
                score["latency_s"] = round(float(m["latency_s"]), 1)
            if m.get("date"):
                score["date"] = m["date"]
            if m.get("harness"):
                score["harness"] = m["harness"]
            cur = cap.setdefault(k, {})
            # keep the best-scoring run when variants collapse to one key
            if field not in cur or acc > cur[field]["acc"]:
                cur[field] = score

    for m in _load(raw_dir, "arena-webdev.json"):
        k = canon_key(m.get("model", ""))
        r = m.get("rating")
        if not k or not isinstance(r, (int, float)):
            continue
        slot = pref.setdefault(k, {})
        if "arena_webdev" not in slot or r > slot["arena_webdev"]["rating"]:
            slot["arena_webdev"] = {
                "rating": round(float(r), 1),
                "rank": m.get("rank"),
                "votes": m.get("votes"),
                "source": "arena.ai",
            }

    for m in _load(raw_dir, "arena-agent.json"):
        k = canon_key(m.get("model", ""))
        s = m.get("score")
        if not k or not isinstance(s, (int, float)):
            continue
        slot = pref.setdefault(k, {})
        if "arena_agent" not in slot or s > slot["arena_agent"]["score"]:
            slot["arena_agent"] = {
                "score": round(float(s), 4),
                "rank": m.get("rank"),
                "source": "arena.ai",
            }

    overlay = {}
    for k in set(cap) | set(pref):
        cb = cap.get(k, {})
        pb = pref.get(k, {})
        entry: dict = {}
        if cb:
            entry["capability"] = cb
        if pb:
            entry["preference"] = pb

        # Components, each normalized 0..1.
        accs = [v["acc"] for v in cb.values() if "acc" in v]
        cap_norm = (sum(accs) / len(accs) / 100.0) if accs else None
        webdev = pb.get("arena_webdev", {}).get("rating")
        webdev_norm = _clamp((webdev - 1100.0) / 600.0) if webdev else None
        agent = pb.get("arena_agent", {}).get("score")
        agent_norm = _clamp(agent / 0.18) if agent else None

        if webdev:
            entry["arena_webdev_elo"] = round(float(webdev), 1)
            entry["arena_coding_elo"] = round(float(webdev), 1)

        # Weighted blend over present components (measured solve-rate dominates).
        parts = [(0.65, cap_norm), (0.25, webdev_norm), (0.10, agent_norm)]
        present = [(w, v) for w, v in parts if v is not None]
        if present:
            tw = sum(w for w, _ in present)
            entry["agentic_score"] = round(sum(w * v for w, v in present) / tw, 3)
        # Arena-derived SWE weight (0.5..1.0) — the Strong-tier fallback when no
        # measured capability is present for a model.
        anchor = max([v for v in (cap_norm, webdev_norm, agent_norm) if v is not None], default=None)
        if anchor is not None:
            entry["w_swe"] = round(0.5 + 0.5 * anchor, 3)

        overlay[k] = entry
    return overlay


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--raw-dir", default=None, help="dir with the fetch-*.py outputs (default <out>/bench/raw)")
    ap.add_argument("--out", default=None, help="overlay path (default <out>/bench/overlay.json)")
    ap.add_argument("--out-dir", default=".")
    args = ap.parse_args()

    raw_dir = args.raw_dir or os.path.join(args.out_dir, "bench", "raw")
    out = args.out or os.path.join(args.out_dir, "bench", "overlay.json")

    overlay = build_overlay(raw_dir)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as f:
        json.dump(overlay, f, indent=1, sort_keys=True)
        f.write("\n")
    scored = sum(1 for v in overlay.values() if "agentic_score" in v)
    print(f"wrote {out}: {len(overlay)} models ({scored} with agentic_score)", file=sys.stderr)


if __name__ == "__main__":
    main()
