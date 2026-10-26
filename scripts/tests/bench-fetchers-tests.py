#!/usr/bin/env python3
"""Offline regression tests for the benchmark fetchers.

Covers the two sources that broke when the upstream sites changed shape:

  terminal-bench  www.tbench.ai now serves only its current default leaderboard
                  at the site root, in a new row schema, so the fetcher must
                  read `metadata.*` / `metrics.accuracy`, record the leaderboard
                  title, and still parse the legacy `modelNames`/`accuracy`
                  schema. Anthropic labels swap to "Claude <label>" so
                  corpus_match.canon_key lands on the corpus key.
  arena           arena.ai leaderboards answer Cloudflare 403 to non-browser
                  clients, so the fetcher reads the official Hugging Face
                  dataset. It must page over the datasets-server API, keep only
                  `overall` rows, and emit the {model, rating|score, rank} shape
                  the overlay builder consumes.

Every fixture under scripts/tests/fixtures/ is a trimmed copy of a real
response. No network is used.

Run: `python3 scripts/tests/bench-fetchers-tests.py`
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = REPO_ROOT / "scripts"
FIXTURES = Path(__file__).resolve().parent / "fixtures"

sys.path.insert(0, str(SCRIPTS))
from corpus_match import canon_key


def _load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


tb = _load("fetch_terminal_bench", "fetch-terminal-bench.py")
arena = _load("fetch_arena", "fetch-arena.py")

PASSES = 0
FAILS = 0


def check(cond, label, detail=""):
    global PASSES, FAILS
    if cond:
        PASSES += 1
        print(f"ok   {label}")
    else:
        FAILS += 1
        print(f"FAIL {label}" + (f" — {detail}" if detail else ""))


def eq(got, want, label):
    check(got == want, label, f"got {got!r}, want {want!r}")


def fixture(name: str) -> str:
    return (FIXTURES / name).read_text()


# ---------------------------------------------------------------- terminal-bench


def test_tbench_current_schema():
    out = tb.parse(fixture("tbench-home-4.0.rsc.html"))
    eq(out["benchmark"], "terminal-bench", "tbench: benchmark field")
    eq(out["source"], "terminal-bench", "tbench: source field")
    eq(out["leaderboard"], "Terminal-Bench 4.0", "tbench: leaderboard title is emitted")
    names = [m["model"] for m in out["models"]]
    eq(
        names,
        ["Claude Opus 5.5", "Claude Sonnet 5.5", "GPT-6 Astra", "Claude Fable 5.1"],
        "tbench: Anthropic rows get the Claude prefix, other vendors keep their label",
    )
    top = out["models"][0]
    eq(top["acc"], 64.85, "tbench: accuracy read from metrics.accuracy as a percent")
    eq(top["date"], "2026-09-22", "tbench: date read from metadata.date")
    eq(
        top["harness"],
        "Claude Code",
        "tbench: harness read from metadata.agent_display",
    )
    # The fixture holds a second, lower Opus 5.5 run under another agent.
    eq(
        len([m for m in out["models"] if m["model"] == "Claude Opus 5.5"]),
        1,
        "tbench: best run per model is kept",
    )
    eq(
        [m["acc"] for m in out["models"]],
        sorted((m["acc"] for m in out["models"]), reverse=True),
        "tbench: models sorted by accuracy descending",
    )


def test_tbench_canon_keys():
    out = tb.parse(fixture("tbench-home-4.0.rsc.html"))
    keys = {canon_key(m["model"]) for m in out["models"]}
    for want in (
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5-1",
        "gpt-6-astra",
    ):
        check(
            want in keys,
            f"tbench: canon_key matches corpus key {want}",
            f"got {sorted(keys)}",
        )


def test_tbench_old_schema():
    out = tb.parse(fixture("tbench-old-schema.rsc.html"))
    eq(out["leaderboard"], "Terminal-Bench 2.0", "tbench legacy: title parsed")
    names = [m["model"] for m in out["models"]]
    eq(
        names,
        ["claude-opus-4-8", "gpt-5-5"],
        "tbench legacy: rows parsed, best run kept",
    )
    eq(
        out["models"][0]["acc"],
        58.2,
        "tbench legacy: fraction accuracy scaled to percent",
    )
    eq(
        out["models"][0]["harness"],
        "Claude Code",
        "tbench legacy: agent recorded as harness",
    )


def test_tbench_fraction_and_percent():
    eq(tb.normalize_acc(0.6485), 64.85, "acc: fraction <= 1 is scaled")
    eq(tb.normalize_acc(64.85), 64.85, "acc: percent is left alone")
    eq(tb.normalize_acc(1.0), 100.0, "acc: 1.0 is a perfect fraction")
    eq(tb.normalize_acc(1.01), 1.01, "acc: just above 1 is already a percent")
    eq(tb.normalize_acc(None), None, "acc: missing is None")


def test_tbench_anthropic_prefix_rule():
    def nm(label, org):
        return tb.model_name(
            {
                "metadata": {
                    "model_display": {"label": label},
                    "model_org": {"label": org},
                }
            }
        )

    eq(nm("Opus 5.5", "Anthropic"), "Claude Opus 5.5", "name: Anthropic label prefixed")
    eq(nm("Claude Opus 4.8", "Anthropic"), "Claude Opus 4.8", "name: no double prefix")
    eq(nm("GPT-6 Astra", "OpenAI"), "GPT-6 Astra", "name: other vendor verbatim")
    eq(nm("GLM-5.2", "Z.ai"), "GLM-5.2", "name: other vendor verbatim")


def test_tbench_title_is_the_leaderboard_title():
    """A "title" key elsewhere in the query must not become the leaderboard title."""
    payload = {
        "queries": [
            {
                "state": {
                    "data": {
                        "leaderboard": {"title": "Terminal-Bench 4.0", "name": "4-0-0"},
                        "columns": [{"id": "x", "title": "Not A Leaderboard Title"}],
                        "rows": [
                            {
                                "metadata": {
                                    "model_display": {"label": "Opus 5.5"},
                                    "model_org": {"label": "Anthropic"},
                                    "agent_display": {"label": "Claude Code"},
                                    "date": "2026-09-22",
                                },
                                "metrics": {"accuracy": 64.85},
                            }
                        ],
                    }
                }
            }
        ]
    }
    body = json.dumps(payload, separators=(",", ":"))
    html = f"<script>self.__next_f.push([1,{json.dumps('a:' + body)}])</script>"
    eq(
        tb.parse(html)["leaderboard"],
        "Terminal-Bench 4.0",
        "tbench: title comes from the leaderboard object, not a nearby column",
    )


def test_tbench_skeleton_retry():
    """A prerendered skeleton (no rows in the dehydrated query) is retried."""
    skeleton = '<html><body><script>self.__next_f.push([1,"a:{\\"queries\\":[]}"])</script></body></html>'
    good = fixture("tbench-home-4.0.rsc.html")
    calls = []
    original_fetch, original_sleep = tb.fetch, tb.time.sleep

    def fake_fetch(url, timeout=40):
        calls.append(url)
        return skeleton if len(calls) < 2 else good

    tb.fetch, tb.time.sleep = fake_fetch, lambda _s: None
    try:
        html = tb.fetch_with_rows("https://example.invalid/")
    finally:
        tb.fetch, tb.time.sleep = original_fetch, original_sleep
    eq(len(calls), 2, "tbench: skeleton payload retried before parsing")
    eq(
        tb.parse(html)["leaderboard"],
        "Terminal-Bench 4.0",
        "tbench: retry yields the real payload",
    )


def test_tbench_empty_is_loud():
    for label, payload in (
        (
            "no rows",
            '<script>self.__next_f.push([1,"a:{\\"leaderboard\\":{\\"title\\":\\"X\\"}}"])</script>',
        ),
        (
            "no title",
            '<script>self.__next_f.push([1,"a:{\\"rows\\":[{\\"metrics\\":{\\"accuracy\\":5}}]}"])</script>',
        ),
        ("no payload", "<html><body>nothing here</body></html>"),
    ):
        try:
            tb.parse(payload)
        except SystemExit as e:
            check(bool(str(e)), f"tbench: loud failure on {label}")
        else:
            check(
                False,
                f"tbench: loud failure on {label}",
                "parse() returned instead of exiting",
            )


# ---------------------------------------------------------------------- arena


class FakeServer:
    """A urlopen-level fake serving recorded datasets-server pages by offset.

    `fail_first` returns a retryable 503 for the first N attempts *per offset*,
    so the fetcher's own backoff loop is the thing under test.
    """

    def __init__(self, pages, fail_first=0):
        self.pages = pages
        self.fail_first = fail_first
        self.calls = []
        self.seen = {}

    def __call__(self, req, timeout=60):
        url = getattr(req, "full_url", req)
        self.calls.append(url)
        offset = int(url.split("offset=")[1].split("&")[0])
        self.seen[offset] = self.seen.get(offset, 0) + 1
        if self.seen[offset] <= self.fail_first:
            raise arena.urllib.error.HTTPError(
                url, 503, "Service Unavailable", None, None
            )
        return _Response(self.pages[offset])

    def __enter__(self):
        self._orig_urlopen = arena.urllib.request.urlopen
        self._orig_sleep = arena.time.sleep
        arena.urllib.request.urlopen = self
        arena.time.sleep = lambda _s: None
        return self

    def __exit__(self, *exc):
        arena.urllib.request.urlopen = self._orig_urlopen
        arena.time.sleep = self._orig_sleep
        return False


class _Response:
    def __init__(self, body: str):
        self._body = body

    def read(self):
        return self._body.encode()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def test_arena_webdev_pagination():
    pages = {
        0: fixture("lmarena-webdev-page0.json"),
        5: fixture("lmarena-webdev-page1.json"),
    }
    with FakeServer(pages) as server:
        rows, published = arena._overall("webdev")
    data = arena.build_webdev(rows, published)
    eq(len(server.calls), 2, "arena webdev: two pages fetched")
    eq(data["benchmark"], "arena-webdev", "arena webdev: benchmark field")
    eq(
        data["source"],
        "lmarena-ai/leaderboard-dataset (Hugging Face)",
        "arena webdev: source is the HF dataset",
    )
    eq(data["published"], "2026-10-23", "arena webdev: publish date recorded")
    eq(len(rows), 9, "arena webdev: both pages walked, non-overall row dropped")
    check(
        all(r["category"] == "overall" for r in rows),
        "arena webdev: only overall rows kept",
    )
    eq(len(data["models"]), 9, "arena webdev: one model record per overall row")
    top = data["models"][0]
    eq(
        sorted(top),
        ["model", "rank", "rating", "votes"],
        "arena webdev: output contract keys",
    )
    eq(
        top["model"],
        "Claude Opus 5.5 (Max)",
        "arena webdev: model_name mapped to model",
    )
    eq(top["rating"], 1806.9, "arena webdev: rating rounded to 0.1")
    eq(isinstance(top["votes"], int), True, "arena webdev: vote_count mapped to votes")
    check(
        "input_usd_per_mtok" not in top, "arena webdev: dropped list-price passthrough"
    )
    eq(
        [m["rating"] for m in data["models"]],
        sorted((m["rating"] for m in data["models"]), reverse=True),
        "arena webdev: sorted by rating descending",
    )


def test_arena_agent():
    pages = {
        0: fixture("lmarena-agent-page0.json"),
        5: fixture("lmarena-agent-page1.json"),
    }
    with FakeServer(pages):
        rows, published = arena._overall("agent")
    data = arena.build_agent(rows, published)
    eq(data["benchmark"], "arena-agent", "arena agent: benchmark field")
    eq(len(rows), 10, "arena agent: both pages walked")
    top = data["models"][0]
    eq(sorted(top), ["model", "rank", "score"], "arena agent: output contract keys")
    eq(top["model"], "Claude Opus 5.5 (Max)", "arena agent: model_name mapped to model")
    eq(top["score"], 0.2077, "arena agent: score rounded to 4 dp")


def test_arena_pagination_stops_at_total():
    pages = {
        0: fixture("lmarena-agent-page0.json"),
        5: fixture("lmarena-agent-page1.json"),
    }
    with FakeServer(pages) as server:
        rows = list(arena.iter_rows("agent"))
    eq(len(rows), 10, "arena: paging stops once num_rows_total is reached")
    eq(len(server.calls), 2, "arena: exactly two pages requested")
    check(all("length=100" in c for c in server.calls), "arena: pages of 100 requested")
    check(
        all(
            "split=latest" in c and "dataset=lmarena-ai/leaderboard-dataset" in c
            for c in server.calls
        ),
        "arena: dataset + split pinned in the request",
    )
    check(
        all("config=agent" in c for c in server.calls),
        "arena: config pinned in the request",
    )


def test_arena_retry():
    pages = {
        0: fixture("lmarena-agent-page0.json"),
        5: fixture("lmarena-agent-page1.json"),
    }
    with FakeServer(pages, fail_first=1) as server:
        rows, _ = arena._overall("agent")
    eq(len(rows), 10, "arena: transient 5xx retried with backoff")
    eq(server.seen, {0: 2, 5: 2}, "arena: each page retried exactly once")
    eq(len(server.calls), 4, "arena: four requests for two pages")


def test_arena_empty_is_loud():
    """An empty page yields nothing, and main() guards each surface loudly."""
    with FakeServer({0: json.dumps({"features": [], "rows": [], "num_rows_total": 0})}):
        empty_rows, pub = arena._overall("webdev")
    check(
        not empty_rows and pub is None,
        "arena: empty dataset yields no rows and no date",
    )
    # main() fetches before guarding; assert the guards exist so the failure path
    # can never degrade into a silently empty output file.
    src = (SCRIPTS / "fetch-arena.py").read_text()
    check(
        "raise SystemExit(\"arena webdev: dataset yielded no 'overall' rows\")" in src,
        "arena webdev: loud failure on zero overall rows",
    )
    check(
        "raise SystemExit(\"arena agent: dataset yielded no 'overall' rows\")" in src,
        "arena agent: loud failure on zero overall rows",
    )


def main() -> int:
    tests = [
        v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)
    ]
    for t in tests:
        print(f"--- {t.__name__}")
        try:
            t()
        except (AttributeError, NameError, TypeError, SystemExit) as e:
            # A pre-fix fetcher lacks the helpers these tests exercise; report
            # that as a failure instead of aborting the whole suite.
            check(
                False,
                f"{t.__name__} runs against the fetcher API",
                f"{type(e).__name__}: {e}",
            )
    print(f"\n{PASSES} passed, {FAILS} failed")
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
