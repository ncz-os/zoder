#!/usr/bin/env python3
"""Conformance tests for the shared pricing / model-matching contract.

`tests/fixtures/pricing-conformance.json` is the ONE fixture that zoder's public
corpus pipeline and an independent pricing database both test against. This file
loads it, validates its schema, and for every `enforced` vector calls the REAL
zoder function and asserts the recorded expectation. `spec` and `unresolved`
vectors are only checked for shape: they name a requirement zoder does not
implement (or an answer nobody knows yet), so asserting an outcome would be a lie.

Offline and deterministic: every input is in the fixture, no network, no temp
files. The two pipeline scripts have dashes in their names, so they are imported
through importlib; scripts/corpus_match.py imports normally.

Run: `python3 scripts/tests/pricing-conformance-tests.py`
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import re
import sys
import unittest
from collections import Counter
from copy import deepcopy
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = REPO_ROOT / "scripts"
FIXTURE = REPO_ROOT / "tests" / "fixtures" / "pricing-conformance.json"

# The rule ids the contract requires. A fixture that adds a rule must add it here
# too, so a typo'd rule id can never make a group of vectors silently unrun.
KNOWN_RULES = {
    "split_id",
    "litellm_ingest",
    "openrouter_ingest",
    "classify",
    "canon_key",
    "peak_pricing",
    "known_good",
    "overlay_composite",
    "spec",
    "unresolved",
}
STATUSES = {"enforced", "spec", "unresolved"}
# Rules whose vectors may legitimately be non-enforced (they describe a contract
# the public pipeline does not implement, or an open question).
NON_ENFORCED_RULES = {"spec", "unresolved"}
# Kept well under the 60 KB budget so the file stays reviewable in a diff.
MAX_FIXTURE_BYTES = 60 * 1024
UNITS = {
    "price": "USD per 1M tokens",
    "litellm_input": "USD per token",
    "openrouter_input": "USD per token, decimal string",
}
# Provenance split for curated-vs-fetched vector: these paths are hand-curated
# and a refresh must never rewrite them.
HAND_CURATED_PATHS = [
    "workflows.single_pass",
    "workflows.grind",
    "capability.swe_verified.acc",
    "capability.swe_verified.source",
    "capability.swe_verified.harness",
]
FETCHED_SOURCES = {"vals.ai", "aider", "scale-seal", "terminal-bench"}
# Any observed citation must look like an ISO date plus a public source.
OBSERVED_RE = re.compile(
    r"^(\d{4}-\d{2}-\d{2}) (https://|litellm|openrouter|[a-z]+\.)", re.IGNORECASE
)


def _load_dashed(name: str, path: Path):
    """Import a script whose filename contains dashes, by path."""
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None, f"cannot load {path}"
    mod = importlib.util.module_from_spec(spec)
    # Register before exec so tracebacks and dataclass-style lookups resolve.
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


# Imported once at module load: a broken pipeline script should fail collection,
# not 40 individual vectors.
sys.path.insert(0, str(SCRIPTS))
import corpus_match

bpc = _load_dashed("zoder_build_public_corpus", SCRIPTS / "build-public-corpus.py")
bbo = _load_dashed("zoder_build_bench_overlay", SCRIPTS / "build-bench-overlay.py")


def _offline(*_args, **_kwargs):
    raise AssertionError("the conformance test must not touch the network")


# OpenRouter is fetched inside overlay_openrouter; stub the fetch so a vector can
# supply trimmed rows without a socket. The stub raises, so an accidental network
# call from any runner is a loud failure rather than a skipped test.
bpc.fetch_json = _offline

_KNOWN_GOOD = bpc.load_known_good(str(REPO_ROOT))
_PEAK_WINDOWS = bpc.load_peak_pricing(str(REPO_ROOT))


def _defaults(v: dict) -> dict:
    """Fill the optional arms of an input so a vector states only what matters."""
    # Deep copy: runners fill defaults and swap in loaded overlays, and the
    # class-level fixture must stay byte-identical for the serialization check.
    inp = deepcopy(v["input"])
    if v["rule"] in {"litellm_ingest", "classify"}:
        inp.setdefault("known_good", _KNOWN_GOOD if v["rule"] == "classify" else [])
        inp.setdefault("bench", {})
    if v["rule"] in {"peak_pricing"}:
        inp.setdefault("peak_windows", _PEAK_WINDOWS)
    if v["rule"] == "openrouter_ingest":
        inp.setdefault("existing", {})
    return inp


def run_vector(v: dict, fixture: dict):
    """Execute one enforced vector against the real function. Returns the
    observed value; each `check_*` asserts on it and raises on mismatch."""
    inp = _defaults(v)
    rule = v["rule"]
    expect = v["expect"]
    if rule == "split_id":
        return bpc.split_id(inp["id"])
    if rule == "litellm_ingest":
        return bpc.from_litellm(inp["rows"])
    if rule == "openrouter_ingest":
        econ = {k: dict(e) for k, e in inp["existing"].items()}
        bpc.fetch_json = lambda *_a, **_k: {"data": inp["rows"]}
        try:
            with contextlib.redirect_stderr(io.StringIO()):  # its "+N" counter line
                return bpc.overlay_openrouter(econ)
        finally:
            bpc.fetch_json = _offline
    if rule == "classify":
        corpus = bpc.build_corpus(
            {k: dict(e) for k, e in inp["econ"].items()},
            "1970-01-01",
            inp["bench"],
            inp["known_good"],
        )
        return {m["id"]: m for m in corpus["models"]}
    if rule == "canon_key":
        return {n: corpus_match.canon_key(n) for n in inp["names"]}
    if rule == "peak_pricing":
        pricing = bpc.build_pricing(
            {k: dict(e) for k, e in inp["econ"].items()},
            "1970-01-01",
            inp["peak_windows"],
        )
        return pricing["models"].get(expect["model"])
    if rule == "known_good":
        if "patterns" in inp:
            return bpc.match_known_good(inp["id"], _KNOWN_GOOD)
        corpus = bpc.build_corpus(
            {k: dict(e) for k, e in inp["econ"].items()},
            "1970-01-01",
            inp["bench"],
            _KNOWN_GOOD,
        )
        return {m["id"]: m for m in corpus["models"]}
    if rule == "overlay_composite":
        return _build_overlay_in_memory(inp["raw_rows"])
    raise AssertionError(f"no runner for rule {rule!r}")


def _build_overlay_in_memory(raw_rows: dict):
    """Run build_overlay without touching disk by feeding it an in-memory
    raw-dir shim: build_overlay only ever calls open(os.path.join(dir, fname))."""
    return bbo.build_overlay(_RawDir(raw_rows))


class _RawDir:
    """Minimal stand-in for a directory of fetch-*.py outputs."""

    def __init__(self, rows: dict):
        self.rows = rows

    def join(self, *parts):
        return parts[-1]

    def __fspath__(self):
        return "<memory>"


def _json_rows(raw_dir, fname):
    """Hook: _load() opens os.path.join(raw_dir, fname); route it to memory."""
    if isinstance(raw_dir, _RawDir):
        return raw_dir.rows.get(fname, [])
    return None


class ConformanceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.raw = FIXTURE.read_bytes()
        cls.fx = json.loads(cls.raw)
        cls.by_id = {}
        for v in cls.fx["vectors"]:
            cls.by_id.setdefault(v["id"], []).append(v)

    # -- schema ---------------------------------------------------------
    def test_fixture_size_budget(self):
        self.assertLess(
            len(self.raw), MAX_FIXTURE_BYTES, "fixture is too large to review in a diff"
        )

    def test_version_and_units(self):
        self.assertEqual(self.fx["version"], 1)
        self.assertEqual(self.fx["units"], UNITS)

    def test_rules_block_describes_each_rule(self):
        rules = self.fx["rules"]
        self.assertEqual(set(rules), KNOWN_RULES)
        for rid, text in rules.items():
            with self.subTest(rule=rid):
                self.assertIsInstance(text, str)
                self.assertGreater(len(text), 120, "rule description too thin")
                if rid not in NON_ENFORCED_RULES:
                    self.assertIn(
                        "scripts/", text, "rule must name the implementing file"
                    )

    def test_vector_ids_unique(self):
        ids = [v["id"] for v in self.fx["vectors"]]
        dupes = [i for i, n in Counter(ids).items() if n > 1]
        self.assertEqual(dupes, [], f"duplicate vector ids: {dupes}")

    def test_vector_schema(self):
        for v in self.fx["vectors"]:
            with self.subTest(vector=v.get("id")):
                self.assertEqual(
                    set(v) - {"observed"},
                    {"id", "rule", "status", "input", "expect", "note"},
                )
                self.assertIn(v["rule"], KNOWN_RULES)
                self.assertIn(v["status"], STATUSES)
                self.assertIsInstance(v["input"], dict)
                self.assertIsInstance(v["expect"], dict)
                self.assertIsInstance(v["note"], str)
                self.assertGreater(len(v["note"]), 20, "note must say why it matters")

    def test_every_rule_has_vectors(self):
        covered = {v["rule"] for v in self.fx["vectors"]}
        self.assertEqual(
            covered, KNOWN_RULES, f"uncovered rules: {KNOWN_RULES - covered}"
        )

    def test_each_rule_has_at_least_two_vectors(self):
        counts = Counter(v["rule"] for v in self.fx["vectors"])
        thin = {r: n for r, n in counts.items() if n < 2}
        self.assertEqual(thin, {}, f"rules with fewer than 2 vectors: {thin}")

    def test_non_enforced_statuses_are_confined_to_their_rules(self):
        for v in self.fx["vectors"]:
            with self.subTest(vector=v["id"]):
                if v["status"] == "enforced":
                    self.assertNotIn(v["rule"], NON_ENFORCED_RULES)
                else:
                    self.assertIn(v["rule"], NON_ENFORCED_RULES)

    def test_spec_and_unresolved_explain_the_gap(self):
        for v in self.fx["vectors"]:
            if v["status"] == "spec":
                with self.subTest(vector=v["id"]):
                    self.assertRegex(
                        v["note"],
                        r"GAP",
                        "a spec vector must name the gap in its note",
                    )
            if v["status"] == "unresolved":
                with self.subTest(vector=v["id"]):
                    self.assertRegex(
                        v["note"],
                        r"evidence that would settle it|would settle",
                        "an unresolved vector must say what evidence would settle it",
                    )
                    self.assertEqual(v["expect"], {}, "unresolved expects nothing")

    def test_observed_citations_are_dated_and_sourced(self):
        seen = 0
        for v in self.fx["vectors"]:
            if "observed" not in v:
                continue
            seen += 1
            with self.subTest(vector=v["id"]):
                self.assertRegex(v["observed"], OBSERVED_RE)
        # The brief requires a handful of vectors taken from live data.
        self.assertGreaterEqual(seen, 10, "too few vectors cite live data")

    def test_runs_offline(self):
        """The fetch hooks must both be stubbed; live GETs belong to the fixture."""
        self.assertIs(bpc.fetch_json, _offline)
        self.assertIs(bbo._load, _patched_load)

    def test_live_data_vectors_exist_for_both_feeds(self):
        cites = " ".join(v.get("observed", "") for v in self.fx["vectors"]).lower()
        self.assertIn("litellm", cites)
        self.assertIn("openrouter", cites)

    def test_fixture_is_canonically_serialized(self):
        canonical = (
            json.dumps(self.fx, indent=2, sort_keys=True, ensure_ascii=True) + "\n"
        )
        self.assertEqual(
            self.raw.decode("utf-8"),
            canonical,
            "fixture must be sorted-keys, 2-space indent (regenerate, don't hand-edit)",
        )

    # -- enforcement ----------------------------------------------------
    def _check_literals(self, got, lit, where=""):
        if lit is None:
            self.assertIsNone(got, f"{where}: expected no match")
            return
        if isinstance(lit, dict) and "literals" in lit:
            lit = lit["literals"]
        self.assertIsInstance(got, dict, f"{where}: expected a mapping")
        if "keys" in lit:
            self.assertEqual(
                list(got), list(lit["keys"]), f"{where}: retained ids differ"
            )
        for key, want in (lit.get("contains") or {}).items():
            for field, value in want.items():
                self.assertIn(field, got[key], f"{where}: {key} missing {field}")
                if isinstance(value, float):
                    self.assertAlmostEqual(
                        got[key][field], value, places=9, msg=f"{where}: {key}.{field}"
                    )
                else:
                    self.assertEqual(got[key][field], value, f"{where}: {key}.{field}")
        for key, fields in (lit.get("absent_keys") or {}).items():
            for field in fields:
                self.assertNotIn(
                    field, got[key], f"{where}: {key} must not carry {field}"
                )

    def _check_subdict(self, got, want, where=""):
        for key, value in want.items():
            self.assertIn(key, got, f"{where}: missing {key}")
            if isinstance(value, dict):
                self._check_subdict(got[key], value, f"{where}.{key}")
            elif isinstance(value, list):
                self.assertEqual(len(got[key]), len(value), f"{where}.{key} length")
                for i, item in enumerate(value):
                    if isinstance(item, dict):
                        self._check_subdict(got[key][i], item, f"{where}.{key}[{i}]")
                    else:
                        self.assertEqual(got[key][i], item, f"{where}.{key}[{i}]")
            elif isinstance(value, float):
                self.assertAlmostEqual(got[key], value, places=9, msg=f"{where}.{key}")
            else:
                self.assertEqual(got[key], value, f"{where}.{key}")

    def test_enforced_vectors(self):
        ran = 0
        for v in self.fx["vectors"]:
            if v["status"] != "enforced":
                continue
            ran += 1
            with self.subTest(vector=v["id"], rule=v["rule"]):
                got = run_vector(v, self.fx)
                self._assert_vector(v, got)
        self.assertGreater(ran, 30)

    def _assert_vector(self, v, got):
        rule, expect = v["rule"], v["expect"]
        if rule == "split_id":
            self.assertEqual(
                got,
                (expect["host"], expect["leaf"], expect["family"]),
                "split mismatch",
            )
        elif rule == "canon_key":
            if "all_equal" in expect:
                values = set(got.values())
                self.assertEqual(
                    values, {expect["all_equal"]}, f"names split into {values}"
                )
            for name, want in expect.get("individual", {}).items():
                self.assertEqual(got[name], want, f"canon_key({name})")
        elif rule == "classify":
            for mid, want in expect["entries"].items():
                self.assertIn(mid, got, f"{mid} missing from corpus")
                for field, value in want.items():
                    self.assertEqual(got[mid][field], value, f"{mid}.{field}")
        elif rule == "peak_pricing":
            lit = expect["literals"]
            self.assertIsNotNone(got, f"pricing has no {expect['model']}")
            # The headline standard-vs-listed rate must match exactly, so a
            # wrong multiplier cannot slip through on a nested-only compare.
            for field in ("input_usd_per_mtok", "output_usd_per_mtok"):
                if field in lit:
                    self.assertAlmostEqual(
                        got.get(field),
                        lit[field],
                        places=9,
                        msg=f"{expect['model']}: standard {field}",
                    )
            for field in expect.get("literals_absent", []):
                self.assertNotIn(
                    field, got, f"{expect['model']} must not carry {field}"
                )
            self._check_literals(got, lit)
        elif rule == "known_good":
            if "provenance" in expect:
                self._check_provenance(got, expect["provenance"])
            elif "hand_curated_paths" in expect:
                self._check_provenance(
                    got,
                    {
                        "hand_curated": expect["hand_curated_paths"],
                        "fetched": expect["fetched_paths"],
                    },
                )
                self._check_literals(got, expect.get("literals"))
            elif "entries" in expect:
                for mid, want in expect["entries"].items():
                    self.assertIn(mid, got, f"{mid} missing from corpus")
                    for field, value in want.items():
                        self._check_subdict(got[mid], {field: value}, mid)
            else:
                self._check_literals(got, expect.get("literals"))
        elif rule == "overlay_composite":
            key = expect["key"]
            self.assertIn(key, got, f"overlay has no {key}")
            self._check_literals(got[key], expect.get("literals"))
        else:
            self._check_literals(got, expect)

    def _check_provenance(self, got, prov):
        """The curated-vs-fetched split: every hand-curated path is present, and
        no fetched signal (a leaderboard source, arena preference, overlay-derived
        score) is present unless the vector says so."""
        for path in prov["hand_curated"]:
            self.assertIsNotNone(_dig(got, path), f"missing curated path {path}")
        if "fetched" in prov and not prov["fetched"]:
            cap = (got or {}).get("capability") or {}
            for field, score in cap.items():
                with self.subTest(field=field):
                    self.assertNotIn(
                        score.get("source"),
                        FETCHED_SOURCES,
                        f"fetched signal leaked into a curated-only row: {field}",
                    )
            self.assertNotIn("preference", got or {})
            self.assertNotIn("arena_webdev_elo", got or {})
        marker = prov.get("curated_marker")
        if marker:
            for path, value in marker.items():
                self.assertEqual(_dig(got, path), value)


def _dig(obj, dotted):
    cur = obj
    for part in dotted.split("."):
        if not isinstance(cur, dict) or part not in cur:
            return None
        cur = cur[part]
    return cur


# build_overlay's _load opens files via builtin open(); redirect it for the
# overlay_composite vectors so those tests stay on disk-free, in-memory rows.
_ORIG_LOAD = bbo._load


def _patched_load(raw_dir, fname):
    rows = _json_rows(raw_dir, fname)
    if rows is not None:
        return rows
    return _ORIG_LOAD(raw_dir, fname)


bbo._load = _patched_load


def summary(fx: dict) -> str:
    rows = Counter((v["rule"], v["status"]) for v in fx["vectors"])
    rules = sorted({v["rule"] for v in fx["vectors"]})
    width = max(len(r) for r in rules)
    lines = ["", "conformance vectors per rule (enforced/spec/unresolved):"]
    totals = {"enforced": 0, "spec": 0, "unresolved": 0}
    for r in rules:
        cells = []
        for s in ("enforced", "spec", "unresolved"):
            n = rows.get((r, s), 0)
            totals[s] += n
            cells.append(f"{n:>2}")
        lines.append(f"  {r:<{width}}  {' / '.join(cells)}")
    lines.append(
        f"  {'TOTAL':<{width}}  {' / '.join(f'{totals[s]:>2}' for s in ('enforced', 'spec', 'unresolved'))}"
    )
    lines.append("")
    return "\n".join(lines)


if __name__ == "__main__":
    fx = json.loads(FIXTURE.read_text())
    print(summary(fx))
    unittest.main(verbosity=2)
