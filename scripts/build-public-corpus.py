#!/usr/bin/env python3
"""Build zoder's PUBLIC corpus + pricing artifacts from public data.

Single source of truth for the daily-synced, public, self-serve artifacts that
zoder and the tokenomics plugins pull (raw.githubusercontent.com/ncz-os/zoder/main/...):

  corpus/model_corpus.json   the classified routing corpus (zoder-core ModelEntry shape)
  pricing/catalog.json       per-token rates ({"models": {id: {input_usd_per_mtok, output_usd_per_mtok}}})

Data sources are ALL PUBLIC — no internal/EIH dependency:
  * LiteLLM model_prices_and_context_window.json  (cost + mode)  [authoritative pricing]
  * OpenRouter /api/v1/models                      (cost, gap-fill) [best-effort]
  * Public coding benchmarks (LMArena / SWE-bench / terminal-bench) [best-effort overlay]

Routability rule (operator decision): a model is route_candidate when it is a
zero-cost ("public-free") chat model; bench scores, when available, are an
OPTIONAL overlay used for tier ranking and never gate routability. Per-user
`zoder refresh` then reconciles this baseline against the operator's OWN served
endpoint, and local config can mark provider-specific free-tier models.

Usage:
  build-public-corpus.py [--out-dir DIR] [--no-bench]
Writes corpus/ and pricing/ under --out-dir (default: repo root).
"""
import argparse
import datetime
import json
import sys
import urllib.request

LITELLM_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
OPENROUTER_URL = "https://openrouter.ai/api/v1/models"
PER_TOK_TO_MTOK = 1_000_000.0


def fetch_json(url, timeout=30):
    req = urllib.request.Request(url, headers={"User-Agent": "zoder-corpus-builder"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8"))


def split_id(model_id):
    """host/leaf/family from an id, matching zoder-core ModelEntry::from_served_id."""
    if "/" in model_id:
        host, leaf = model_id.split("/", 1)
        family = host
    else:
        host, leaf, family = "", model_id, model_id
    return host, leaf, family


# Substrings that mark a LiteLLM key as an enterprise SKU / placeholder / region
# variant rather than a routable model id. These pollute a naive zero-cost pool.
_SKU_MARKERS = ("commitment", "*", "/container", "sample_spec", "predibase")


def _is_sku_noise(mid):
    low = mid.lower()
    return any(mark in low for mark in _SKU_MARKERS)


def from_litellm(litellm):
    """Build {id: economics} from the LiteLLM public price list (chat models).

    `free` is determined by the caller, but we only trust an explicit zero: a
    model counts as zero-cost only when BOTH cost keys are present and 0.0. A
    missing key means "LiteLLM doesn't track this price" (unknown), NOT free —
    treating absent-as-free is what floods the pool with unpriced placeholders.
    """
    econ = {}
    for mid, m in litellm.items():
        if mid == "sample_spec" or not isinstance(m, dict):
            continue
        if m.get("mode") != "chat" or _is_sku_noise(mid):
            continue
        has_in = "input_cost_per_token" in m
        has_out = "output_cost_per_token" in m
        inp = (m.get("input_cost_per_token") or 0.0) * PER_TOK_TO_MTOK
        out = (m.get("output_cost_per_token") or 0.0) * PER_TOK_TO_MTOK
        # explicit_zero: both keys present and zero — a real free-tier/local model,
        # not an unpriced placeholder.
        explicit_zero = has_in and has_out and inp == 0.0 and out == 0.0
        econ[mid] = {
            "input_usd_per_mtok": inp,
            "output_usd_per_mtok": out,
            "source": "litellm",
            "_explicit_zero": explicit_zero,
            "_priced": has_in or has_out,
        }
    return econ


def overlay_openrouter(econ):
    """Best-effort: add OpenRouter chat models missing from LiteLLM."""
    try:
        data = fetch_json(OPENROUTER_URL).get("data", [])
    except Exception as e:  # noqa: BLE001 — best-effort enrichment
        print(f"  (openrouter overlay skipped: {e})", file=sys.stderr)
        return econ
    added = 0
    for m in data:
        mid = m.get("id")
        pricing = m.get("pricing") or {}
        if not mid or mid in econ:
            continue
        try:
            inp = float(pricing.get("prompt", 0) or 0) * PER_TOK_TO_MTOK
            out = float(pricing.get("completion", 0) or 0) * PER_TOK_TO_MTOK
        except (TypeError, ValueError):
            continue
        econ[mid] = {"input_usd_per_mtok": inp, "output_usd_per_mtok": out, "source": "openrouter"}
        added += 1
    print(f"  openrouter: +{added} models", file=sys.stderr)
    return econ


def build_corpus(econ, generated, bench=None):
    """Project economics (+ optional bench) into the ModelEntry corpus shape."""
    bench = bench or {}
    models = []
    routable = 0
    for mid, e in sorted(econ.items()):
        host, leaf, family = split_id(mid)
        explicit_zero = e.get("_explicit_zero", False)
        priced = e.get("_priced", False)
        # Strip internal helper keys before publishing the economics block so it
        # deserializes cleanly into zoder-core Economics.
        econ_pub = {k: v for k, v in e.items() if not k.startswith("_")}
        free = explicit_zero
        route_candidate = free  # free chat models route out-of-box; refresh narrows to served
        if route_candidate:
            gated = None
        elif not priced:
            gated = "unpriced in public feed — not auto-routed (run a local classify/refresh)"
        else:
            gated = "paid (non-zero public price)"
        entry = {
            "id": mid,
            "host": host,
            "leaf": leaf,
            "family": family,
            "kind": "chat",
            "route_candidate": route_candidate,
            "free": free,
            "paid": priced and not free,
            "gated_reason": gated,
            "economics": econ_pub,
        }
        # Optional bench overlay (capability / preference / elo) — never gates routing.
        b = bench.get(mid)
        if b:
            entry.update(b)
        if route_candidate:
            routable += 1
        models.append(entry)
    return {
        "source": "public: litellm+openrouter pricing"
        + (" + public bench overlay" if bench else ""),
        "arena_date": generated,
        "generated": generated,
        "count": len(models),
        "routable": routable,
        "models": models,
    }


def build_pricing(econ, generated):
    return {
        "version": 1,
        "generated": generated,
        "source": "litellm+openrouter (public)",
        "models": {
            mid: {
                "input_usd_per_mtok": e["input_usd_per_mtok"],
                "output_usd_per_mtok": e["output_usd_per_mtok"],
            }
            for mid, e in sorted(econ.items())
        },
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", default=".")
    ap.add_argument("--no-bench", action="store_true", help="skip the public bench overlay")
    ap.add_argument("--date", default=None, help="override generated date (ISO); default today UTC")
    args = ap.parse_args()

    generated = args.date or datetime.datetime.now(datetime.timezone.utc).date().isoformat()

    print("fetching LiteLLM public price list…", file=sys.stderr)
    econ = from_litellm(fetch_json(LITELLM_URL))
    print(f"  litellm: {len(econ)} chat models", file=sys.stderr)
    econ = overlay_openrouter(econ)

    bench = None
    if not args.no_bench:
        # Bench overlay is a best-effort enrichment layer. The fetch-*.py
        # scripts in this repo produce id->scores maps; wiring them in is the
        # next layer. Absent bench, the corpus is still valid + routable
        # (capability is optional per zoder-core ModelEntry).
        bench = load_bench_overlay(args.out_dir)

    import os
    corpus = build_corpus(econ, generated, bench)
    pricing = build_pricing(econ, generated)

    cdir = os.path.join(args.out_dir, "corpus")
    pdir = os.path.join(args.out_dir, "pricing")
    os.makedirs(cdir, exist_ok=True)
    os.makedirs(pdir, exist_ok=True)
    with open(os.path.join(cdir, "model_corpus.json"), "w") as f:
        json.dump(corpus, f, indent=2, sort_keys=False)
        f.write("\n")
    with open(os.path.join(pdir, "catalog.json"), "w") as f:
        json.dump(pricing, f, indent=2, sort_keys=False)
        f.write("\n")
    print(
        f"wrote corpus ({corpus['count']} models, {corpus['routable']} routable) "
        f"+ pricing ({len(pricing['models'])} models) [generated {generated}]",
        file=sys.stderr,
    )


def load_bench_overlay(out_dir):
    """Best-effort: read a prebuilt id->bench map if present (bench/overlay.json).

    Kept decoupled so the heavy benchmark fetch/normalize can run on its own
    cadence and drop a file here; this builder folds it in when present.
    """
    import os

    path = os.path.join(out_dir, "bench", "overlay.json")
    try:
        with open(path) as f:
            data = json.load(f)
        print(f"  bench overlay: {len(data)} models from {path}", file=sys.stderr)
        return data
    except FileNotFoundError:
        print("  bench overlay: none (bench/overlay.json absent) — pricing-only corpus", file=sys.stderr)
        return None
    except Exception as e:  # noqa: BLE001
        print(f"  bench overlay: skipped ({e})", file=sys.stderr)
        return None


if __name__ == "__main__":
    main()
