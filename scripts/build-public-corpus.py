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
import os
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from corpus_match import canon_key  # noqa: E402

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


# Closed-weight commercial vendors with no $0 public API. A zero price for these
# in the public feed is a placeholder/region row, never a real free tier. The
# open-weight gpt-oss family is intentionally NOT here (it can legitimately be $0).
_COMMERCIAL_PAID_ONLY = (
    "anthropic", "claude-", "/claude", ".claude",
    "openai/o", "/o1", "/o3", "/o4", "-o3-", "gpt-4", "gpt-5",
    "gemini", "google/", "/palm",
)


def _is_commercial_paid_only(mid):
    low = mid.lower()
    if "gpt-oss" in low:
        return False
    return any(mark in low for mark in _COMMERCIAL_PAID_ONLY)


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


def load_known_good(out_dir):
    """Read the curated high-SWE allow-list (bench/known-good-swe.json).

    This is the corpus's only QUALITY dimension: the public pricing feeds carry
    no capability score, so without it every model ranks 0 and the router has
    nothing to sort on. Returns the ordered pattern list (first match wins), [].
    """
    import os

    path = os.path.join(out_dir, "bench", "known-good-swe.json")
    try:
        with open(path) as f:
            data = json.load(f)
        pats = data.get("patterns", [])
        print(f"  known-good overlay: {len(pats)} patterns from {path}", file=sys.stderr)
        return pats
    except FileNotFoundError:
        print("  known-good overlay: none (bench/known-good-swe.json absent)", file=sys.stderr)
        return []
    except Exception as e:  # noqa: BLE001
        print(f"  known-good overlay: skipped ({e})", file=sys.stderr)
        return []


def match_known_good(mid, patterns):
    """First pattern (ordered most-specific first) any of whose `match`
    substrings appear in `mid` (case-insensitive). Returns the capability +
    workflow fields to merge into the ModelEntry, or None. Applies to free AND
    paid models alike: a paid known-good model stays non-routed but carries a
    score so a `--strong`/`--allow-paid` escalation can rank it."""
    low = mid.lower()
    for p in patterns:
        if any(sub.lower() in low for sub in p.get("match", [])):
            swe = p["swe"]
            return {
                "agentic_score": round(swe / 100.0, 3),
                "w_swe": round(swe / 100.0, 3),
                "capability": {
                    "swe_verified": {
                        "acc": float(swe),
                        "source": "ncz-known-good",
                        "harness": "curated",
                    }
                },
                "workflows": {
                    "single_pass": p["single_pass"],
                    "grind": p["grind"],
                },
            }
    return None


def build_corpus(econ, generated, bench=None, known_good=None):
    """Project economics (+ optional bench + curated known-good) into the
    ModelEntry corpus shape."""
    bench = bench or {}
    known_good = known_good or []
    models = []
    routable = 0
    scored = 0
    for mid, e in sorted(econ.items()):
        host, leaf, family = split_id(mid)
        explicit_zero = e.get("_explicit_zero", False)
        priced = e.get("_priced", False)
        # Strip internal helper keys before publishing the economics block so it
        # deserializes cleanly into zoder-core Economics.
        econ_pub = {k: v for k, v in e.items() if not k.startswith("_")}
        # A closed-weight commercial vendor (Anthropic/OpenAI/Gemini) has no $0
        # public API; a zero price in the feed is a placeholder, not a free tier,
        # so it must never be auto-routed as "free". Open-weight gpt-oss is exempt.
        commercial_zero = explicit_zero and _is_commercial_paid_only(mid)
        free = explicit_zero and not commercial_zero
        route_candidate = free  # free chat models route out-of-box; refresh narrows to served
        if commercial_zero:
            gated = "listed $0 in public feed but vendor is paid-only — not auto-routed"
        elif route_candidate:
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
        # Curated known-good is applied FIRST as a fallback: its `workflows`
        # tags (which no benchmark provides) survive, while the measured bench
        # overlay below OVERRIDES any curated capability/agentic_score with real
        # leaderboard data when a model is name-matched there.
        kg = match_known_good(mid, known_good)
        if kg:
            entry.update(kg)
        # Bench overlay (capability / preference / elo), matched by canonical key
        # so every provider/region variant inherits it. Never gates routing.
        b = bench.get(canon_key(mid))
        if b:
            entry.update(b)
        if kg or b:
            scored += 1
        if route_candidate:
            routable += 1
        models.append(entry)
    return {
        "source": "public: litellm+openrouter pricing"
        + (" + public bench overlay" if bench else "")
        + (" + ncz known-good swe overlay" if known_good else ""),
        "arena_date": generated,
        "generated": generated,
        "count": len(models),
        "routable": routable,
        "scored": scored,
        "models": models,
    }


def _hhmm_to_min(s):
    h, m = s.split(":")
    return (int(h) * 60 + int(m)) % 1440


def load_peak_pricing(out_dir):
    """Curated time-of-day (off-peak) pricing windows (pricing/peak-pricing.json)."""
    path = os.path.join(out_dir, "pricing", "peak-pricing.json")
    try:
        with open(path) as f:
            wins = json.load(f).get("windows", [])
        print(f"  peak-pricing overlay: {len(wins)} windows from {path}", file=sys.stderr)
        return wins
    except FileNotFoundError:
        return []
    except Exception as e:  # noqa: BLE001
        print(f"  peak-pricing overlay: skipped ({e})", file=sys.stderr)
        return []


def _match_peak(mid, windows):
    low = mid.lower()
    for w in windows:
        if any(sub.lower() in low for sub in w.get("match", [])):
            return w
    return None


def build_pricing(econ, generated, peak=None):
    peak = peak or []
    models = {}
    for mid, e in sorted(econ.items()):
        inp = e["input_usd_per_mtok"]
        out = e["output_usd_per_mtok"]
        entry = {"input_usd_per_mtok": inp, "output_usd_per_mtok": out}
        # Off-peak overlay: apply the curated discount to the base rates and record
        # the UTC window so consumers can charge the cheaper rate inside it.
        w = _match_peak(mid, peak)
        if w and (inp > 0 or out > 0):
            start, end = w["window_utc"]
            entry["off_peak"] = {
                "input_usd_per_mtok": round(inp * w.get("input_mult", 1.0), 6),
                "output_usd_per_mtok": round(out * w.get("output_mult", 1.0), 6),
                "window_start_utc_min": _hhmm_to_min(start),
                "window_end_utc_min": _hhmm_to_min(end),
            }
        models[mid] = entry
    return {
        "version": 1,
        "generated": generated,
        "source": "litellm+openrouter (public)" + (" + ncz peak-pricing overlay" if peak else ""),
        "models": models,
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

    # Curated known-good SWE overlay is the corpus quality dimension; always
    # applied (it is a committed file, not a heavy fetch, so --no-bench keeps it).
    known_good = load_known_good(args.out_dir)

    import os
    corpus = build_corpus(econ, generated, bench, known_good)
    pricing = build_pricing(econ, generated, load_peak_pricing(args.out_dir))

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
