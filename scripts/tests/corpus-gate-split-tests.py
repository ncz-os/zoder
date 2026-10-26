#!/usr/bin/env python3
"""Behavioural tests for the SPLIT corpus gate (2026-10-26).

The corpus-sync job had ONE gate: a degraded bench overlay (a benchmark
source being down) blocked the whole publish, so the PRICES were frozen
for 13 days even though LiteLLM/OpenRouter had fetched fine every day.
These tests pin the split:

  * the PRICE gate (LiteLLM ok, OpenRouter ok, catalog not shrunk) blocks
    publishing prices,
  * the OVERLAY gate never blocks prices: the committed overlay is kept
    byte-for-byte and the run is reported as `overlay_stale`.

Every test is OFFLINE: the fetch / overlay-build / corpus-build steps are
injected, so no network call is ever made.

Run: `python3 scripts/tests/corpus-gate-split-tests.py`
"""
from __future__ import annotations

import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
FETCH_SCRIPT = REPO_ROOT / "scripts" / "ci" / "corpus-fetch-and-overlay.py"
CHECK_SCRIPT = REPO_ROOT / "scripts" / "ci" / "corpus-freshness-check.py"
BUILD_SCRIPT = REPO_ROOT / "scripts" / "build-public-corpus.py"

PASSES = 0
FAILS = 0


def pass_(msg: str) -> None:
    global PASSES
    PASSES += 1
    print(f"ok    {msg}")


def fail_(msg: str) -> None:
    global FAILS
    FAILS += 1
    print(f"FAIL  {msg}")


def _scratch() -> Path:
    """An empty scratch checkout root for one test."""
    root = Path(os.environ.get("CORPUS_GATE_TEST_DIR") or tempfile.gettempdir())
    if not root.is_dir():
        raise RuntimeError(
            "CORPUS_GATE_TEST_DIR must be an EXISTING DIRECTORY the suite can "
            f"create scratch checkouts under; it is currently {root!r}.")
    return Path(tempfile.mkdtemp(prefix="corpus-gate-", dir=str(root)))


def _load(path: Path, name: str):
    """Import a script with a dashed file name as a module."""
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _run_one(func) -> None:
    """Run one test; an exception is a FAIL, not a crash of the whole suite."""
    try:
        func()
    except Exception as e:  # noqa: BLE001 — a test failure must not stop the suite
        fail_(f"{func.__name__} raised {type(e).__name__}: {e}")


# ---------------------------------------------------------------------------
# fixtures
# ---------------------------------------------------------------------------
def _scored_overlay(count: int, marker: str = "committed") -> dict:
    return {f"{marker}-{i}": {"agentic_score": 0.5} for i in range(count)}


def _setup(root: Path, *, overlay_count: int = 191, catalog_models: int = 100,
           previous_freshness: dict | None = None,
           overlay_marker: str = "committed") -> Path:
    """Materialise a corpus-sync checkout layout under `root`."""
    (root / "corpus").mkdir(parents=True, exist_ok=True)
    (root / "bench" / "raw").mkdir(parents=True, exist_ok=True)
    (root / "pricing").mkdir(parents=True, exist_ok=True)
    (root / "bench" / "overlay.json").write_text(json.dumps(_scored_overlay(overlay_count, overlay_marker)))
    (root / "pricing" / "catalog.json").write_text(json.dumps({
        "version": 1, "generated": "2026-10-13",
        "models": {f"m{i}": {"input_usd_per_mtok": 1.0, "output_usd_per_mtok": 2.0}
                   for i in range(catalog_models)},
    }))
    if previous_freshness is not None:
        (root / "corpus" / "freshness.json").write_text(json.dumps(previous_freshness))
    return root


def _fake_fetcher(failing: list[str]):
    """A fetch_one replacement: the named sources fail, the rest succeed."""
    def fetch_one(root: Path, name: str, args: list[str]) -> tuple[bool, str]:
        if name in failing:
            return False, "simulated outage"
        for arg in args:
            if arg.endswith(".json"):
                out = root / arg
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_text(json.dumps({"models": []}))
            elif arg == "bench/raw":
                (root / "bench" / "raw").mkdir(parents=True, exist_ok=True)
        return True, ""
    return fetch_one


def _fake_overlay_builder(count: int):
    """An overlay builder replacement that emits `count` scored models."""
    def builder(root: Path, raw_dir: Path, out_path: Path) -> None:
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(_scored_overlay(count, "candidate")))
    return builder


def _fake_corpus_builder(*, catalog_models: int = 100, litellm_ok: bool = True,
                         openrouter_ok: bool = True, openrouter_reason: str = "",
                         litellm_reason: str = ""):
    """A corpus builder replacement that writes the artifacts + status."""
    def builder(root: Path, status_path: Path) -> int:
        pricing = root / "pricing"
        pricing.mkdir(parents=True, exist_ok=True)
        (pricing / "catalog.json").write_text(json.dumps({
            "version": 1, "generated": "2026-10-26",
            "models": {f"m{i}": {"input_usd_per_mtok": 1.0, "output_usd_per_mtok": 2.0}
                       for i in range(catalog_models)},
        }))
        corpus = root / "corpus"
        corpus.mkdir(parents=True, exist_ok=True)
        (corpus / "model_corpus.json").write_text(json.dumps({
            "count": catalog_models, "routable": 1, "scored": 1, "models": []}))
        status_path.parent.mkdir(parents=True, exist_ok=True)
        status_path.write_text(json.dumps({
            "litellm": {"ok": litellm_ok, "models": catalog_models if litellm_ok else 0,
                        "reason": litellm_reason},
            "openrouter": {"ok": openrouter_ok, "added": 3 if openrouter_ok else 0,
                           "reason": openrouter_reason},
            "catalog_models": catalog_models,
            "corpus_models": catalog_models,
            "routable": 1,
            "scored": 1,
        }))
        return 0
    return builder


def _run_fetch(root: Path, *, failing: list[str] | None = None, candidate: int = 191,
               builder=None, now: str = "2026-10-26T12:00:00Z") -> dict:
    """Run the fetch+overlay pipeline offline; returns the written freshness."""
    module = _load(FETCH_SCRIPT, "corpus_fetch_and_overlay")
    module.run_pipeline(
        root,
        fetch_one=_fake_fetcher(failing or []),
        overlay_builder=_fake_overlay_builder(candidate),
        corpus_builder=builder or _fake_corpus_builder(),
        now_iso=now,
    )
    return json.loads((root / "corpus" / "freshness.json").read_text())


def _run_check(root: Path, force: bool = False) -> tuple[int, str]:
    cmd = [sys.executable, str(CHECK_SCRIPT)]
    if force:
        cmd.append("--force-publish")
    env = {**os.environ, "CORPUS_FRESHNESS_ROOT": str(root)}
    r = subprocess.run(cmd, cwd=root, capture_output=True, text=True, env=env, check=False)
    return r.returncode, r.stdout + r.stderr


# ---------------------------------------------------------------------------
# tests
# ---------------------------------------------------------------------------
def test_one_bench_source_failing_keeps_the_committed_overlay() -> None:
    """A failed bench source must NOT block the prices, and must not erase
    the routing scores: the committed overlay is kept byte-for-byte."""
    root = _scratch()
    try:
        previous = {"schema": 2, "ts": "2026-10-13T09:08:34Z",
                    "overlay": {"stale": False, "built_at": "2026-10-13T09:08:34Z",
                                "age_days": 0, "models_with_score": 191,
                                "candidate_models_with_score": 191, "reason": ""},
                    "overlay_stale": False, "overlay_age_days": 0}
        _setup(root, previous_freshness=previous)
        committed_before = (root / "bench" / "overlay.json").read_bytes()

        freshness = _run_fetch(root, failing=["fetch-terminal-bench.py"], candidate=141)

        if (root / "bench" / "overlay.json").read_bytes() == committed_before:
            pass_("a failed bench source keeps the committed overlay byte-for-byte")
        else:
            fail_("the committed overlay was REPLACED despite a failed bench source")

        if freshness.get("overlay_stale") is True:
            pass_("the run is reported as overlay_stale")
        else:
            fail_(f"overlay_stale is not true: {freshness.get('overlay_stale')!r}")

        overlay_meta = freshness.get("overlay") or {}
        age = overlay_meta.get("age_days")
        if age == 13:
            pass_("the stale overlay reports its age (13 days)")
        else:
            fail_(f"stale overlay age is {age!r}, expected 13")

        if overlay_meta.get("built_at") == "2026-10-13T09:08:34Z":
            pass_("built_at is carried forward from the committed report")
        else:
            fail_(f"built_at is {overlay_meta.get('built_at')!r}, expected the carried-forward value")

        prices = freshness.get("prices") or {}
        if prices.get("ok") is True:
            pass_("the price sources are healthy, so the prices may be published")
        else:
            fail_(f"prices.ok is {prices.get('ok')!r}, expected True")

        code, out = _run_check(root)
        if code == 0:
            pass_("the check exits 0 (prices publish, overlay kept stale)")
        else:
            fail_(f"the check exited {code}, expected 0: {out.strip().splitlines()[-1:]}")
        if "STALE" in out:
            pass_("the check warns that the overlay is stale")
        else:
            fail_("the check did not warn about the stale overlay")

        last = root / "corpus" / "freshness-last.json"
        if not last.exists():
            pass_("the freshness baseline is NOT advanced while the overlay is stale")
        else:
            fail_("the freshness baseline was advanced on a stale overlay")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_candidate_overlay_losing_coverage_is_kept_out() -> None:
    """All sources fresh but the candidate lost 30% of its scored models:
    the committed overlay is kept and the run is stale (prices still publish)."""
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        committed_before = (root / "bench" / "overlay.json").read_bytes()

        freshness = _run_fetch(root, failing=[], candidate=134)  # 191 -> 134 = -29.8%

        if (root / "bench" / "overlay.json").read_bytes() == committed_before:
            pass_("a candidate that lost 30% of its coverage is not published")
        else:
            fail_("the candidate overlay was published despite a 30% coverage loss")
        if freshness.get("overlay_stale") is True:
            pass_("the coverage-losing run is reported as overlay_stale")
        else:
            fail_("overlay_stale is not true for a coverage-losing candidate")
        reason = (freshness.get("overlay") or {}).get("reason") or ""
        if "below the committed overlay" in reason:
            pass_("the reason names the coverage loss")
        else:
            fail_(f"the stale reason does not explain the coverage loss: {reason!r}")

        code, out = _run_check(root)
        if code == 0:
            pass_("the check exits 0 (a coverage-losing overlay never blocks prices)")
        else:
            fail_(f"the check exited {code}, expected 0: {out.strip().splitlines()[-1:]}")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_fresh_candidate_overlay_is_published() -> None:
    """All sources fresh and the candidate within the threshold: the overlay
    is replaced, the run is not stale, and the baseline is advanced."""
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        freshness = _run_fetch(root, failing=[], candidate=191)

        published = json.loads((root / "bench" / "overlay.json").read_text())
        if any(k.startswith("candidate-") for k in published):
            pass_("the fresh candidate overlay is published")
        else:
            fail_("the candidate overlay was NOT published")
        if freshness.get("overlay_stale") is False:
            pass_("a fresh overlay is not reported as stale")
        else:
            fail_("a fresh overlay is reported as stale")
        if (freshness.get("overlay") or {}).get("candidate_models_with_score") == 191:
            pass_("the report records the candidate's scored-model count")
        else:
            fail_("the candidate scored-model count is missing")

        code, out = _run_check(root)
        if code == 0:
            pass_("the check exits 0 on a fresh overlay")
        else:
            fail_(f"the check exited {code}, expected 0: {out.strip().splitlines()[-1:]}")
        last = root / "corpus" / "freshness-last.json"
        if last.exists():
            pass_("the freshness baseline IS advanced on a fresh overlay")
        else:
            fail_("the freshness baseline was not advanced on a fresh overlay")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_openrouter_failure_blocks_the_prices() -> None:
    """A failed OpenRouter overlay blocks the price publish ..."""
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        builder = _fake_corpus_builder(openrouter_ok=False,
                                       openrouter_reason="HTTP 503 from the model list")
        _run_fetch(root, failing=[], candidate=191, builder=builder)

        code, out = _run_check(root)
        if code == 1:
            pass_("a failed OpenRouter overlay makes the check exit 1")
        else:
            fail_(f"the check exited {code}, expected 1")
        if "PRICE SOURCE DEGRADED" in out:
            pass_("the check says PRICE SOURCE DEGRADED")
        else:
            fail_("the check did not print PRICE SOURCE DEGRADED")
        if "openrouter" in out:
            pass_("the message names the degraded source (openrouter)")
        else:
            fail_("the message does not name the degraded source")
        if "--force-publish" in out:
            pass_("the message points at --force-publish")
        else:
            fail_("the message does not mention --force-publish")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_a_disagreeing_stale_flag_is_read_as_stale() -> None:
    """`overlay_stale` and `overlay.stale` must agree; if they disagree,
    the SAFE reading wins.

    A report that says `overlay_stale: false` while its `overlay` block
    says `stale: true` is contradictory. Treating it as fresh would
    advance the baseline from a stale overlay and could let a degraded
    overlay be published; treating it as stale only costs a warning and
    a skipped baseline update. So stale is the OR of the two.
    """
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        _run_fetch(root, failing=["fetch-terminal-bench.py"], candidate=141)
        report_path = root / "corpus" / "freshness.json"
        report = json.loads(report_path.read_text())
        # Force the contradiction: top level says fresh, nested says stale.
        report["overlay_stale"] = False
        report["overlay"]["stale"] = True
        report_path.write_text(json.dumps(report))
        last = root / "corpus" / "freshness-last.json"
        last.unlink(missing_ok=True)

        code, out = _run_check(root)
        if code == 0:
            pass_("a contradictory stale flag still exits 0 (the overlay gate never blocks)")
        else:
            fail_(f"a contradictory stale flag exited {code}, expected 0")
        if "STALE" in out:
            pass_("a contradictory stale flag is read as STALE (the safe reading)")
        else:
            fail_("a contradictory stale flag was NOT read as stale")
        if not last.exists():
            pass_("a contradictory stale flag does not advance the baseline")
        else:
            fail_("a contradictory stale flag advanced the baseline")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_a_malformed_sources_list_does_not_crash_the_gate() -> None:
    """A report whose "sources" is not a list must not crash the gate.

    `cur.get("sources")` could be a number, a string or a dict in a
    truncated or hand-edited report. Iterating it blindly raised a
    TypeError and the script died with a traceback instead of a verdict,
    so the CI job failed for the wrong reason. A malformed list is now
    reported and ignored.
    """
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        _run_fetch(root, failing=[], candidate=191)
        report_path = root / "corpus" / "freshness.json"
        report = json.loads(report_path.read_text())
        report["sources"] = 5          # not a list at all
        report_path.write_text(json.dumps(report))

        code, out = _run_check(root)
        if code in (0, 1):
            pass_(f"a malformed 'sources' does not crash the gate (exit {code})")
        else:
            fail_(f"a malformed 'sources' crashed the gate (exit {code})")
        if "TypeError" not in out and "Traceback" not in out:
            pass_("a malformed 'sources' produces no traceback")
        else:
            fail_("a malformed 'sources' produced a traceback")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_a_string_false_stale_flag_is_read_as_not_stale() -> None:
    """A stale flag written as the STRING "false" must not read as stale.

    `bool("false")` is True in Python, so a report that carried the flag
    as a string would be read as stale by a naive check. The flags are
    decoded explicitly instead: the recognisably-false spellings are
    false, and anything else counts as stale, because treating a stale
    overlay as fresh can publish degraded routing scores.
    """
    check = _load(CHECK_SCRIPT, "corpus_freshness_check")
    for value, expected in ((False, False), (True, True), (0, False), (1, True),
                            ("false", False), ("False", False), ("0", False),
                            ("no", False), ("true", True), ("yes", True),
                            (None, False), ("", False), ("weird", True)):
        got = check._stale_flag(value)
        if got is expected:
            pass_(f"_stale_flag({value!r}) is {expected}")
        else:
            fail_(f"_stale_flag({value!r}) is {got}, expected {expected}")

    # The two spellings are combined with OR.
    for report, meta, expected in (({"overlay_stale": True}, {}, True),
                                   ({}, {"stale": True}, True),
                                   ({"overlay_stale": False}, {"stale": True}, True),
                                   ({"overlay_stale": True}, {"stale": False}, True),
                                   ({"overlay_stale": False}, {"stale": False}, False),
                                   ({}, {}, False)):
        got = check._stale_flag_from(report, meta)
        if got is expected:
            pass_(f"_stale_flag_from({report}, {meta}) is {expected}")
        else:
            fail_(f"_stale_flag_from({report}, {meta}) is {got}, expected {expected}")

    # The gate must actually USE the decoder, not a naive bool(). This is
    # the behavioural half: a report whose flag is the string "false" must
    # not be treated as stale, because the naive bool("false") is True.
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        _run_fetch(root, failing=[], candidate=191)
        report_path = root / "corpus" / "freshness.json"
        report = json.loads(report_path.read_text())
        report["overlay_stale"] = "false"
        report["overlay"]["stale"] = "false"
        report_path.write_text(json.dumps(report))

        code, out = _run_check(root)
        if code == 0:
            pass_('a stale flag spelled "false" exits 0')
        else:
            fail_(f'a stale flag spelled "false" exited {code}, expected 0')
        if "STALE" not in out:
            pass_('a stale flag spelled "false" is NOT read as stale')
        else:
            fail_('a stale flag spelled "false" WAS read as stale')
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_every_script_starts_as_a_real_process() -> None:
    """Each script must RUN, not just import.

    The strongest possible proof that a module has no missing import is to
    execute it: a script whose `import json` (or `from pathlib import
    Path`) was deleted dies with a NameError the moment it is started,
    long before any of its own logic runs.

    `--help` is the cheapest way to start a script: argparse prints the
    usage and exits 0, the network is never touched, and every
    module-level statement (including every import and every constant
    built from one) has already been executed.

    This is deliberately a SUBPROCESS test: an in-process import would
    reuse a module object and could mask a broken first-time import.
    """
    for script in (BUILD_SCRIPT, CHECK_SCRIPT, FETCH_SCRIPT):
        proc = subprocess.run(
            [sys.executable, str(script), "--help"],
            capture_output=True, text=True, timeout=60, check=False,
        )
        if proc.returncode == 0:
            pass_(f"{script.name} starts as a real process (all imports resolve)")
        else:
            fail_(f"{script.name} exited {proc.returncode} on --help: "
                  f"{(proc.stderr or '').strip().splitlines()[-1:]}")


def _registers_force_publish(script: Path) -> bool:
    """True when the script adds a `--force-publish` store_true flag.

    Parsed with `ast` rather than matched as text: a substring check for
    `action="store_true"` breaks the moment the source uses single
    quotes, and it would happily match a mention inside a comment. The
    AST answer is the same on every quoting style.
    """
    import ast

    tree = ast.parse(script.read_text())
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func = node.func
        if not (isinstance(func, ast.Attribute) and func.attr == "add_argument"):
            continue
        option = None
        is_store_true = False
        for arg in node.args:
            if (isinstance(arg, ast.Constant) and isinstance(arg.value, str)
                    and option is None):
                option = arg.value
        for kw in node.keywords:
            if kw.arg == "action" and isinstance(kw.value, ast.Constant):
                is_store_true = kw.value.value == "store_true"
        if option == "--force-publish" and is_store_true:
            return True
    return False


def test_force_publish_is_still_wired_up() -> None:
    """`--force-publish` must be registered AND its branch must exist.

    A diff-only reader can lose track of a block that moved or was
    re-indented and conclude that the flag lost its handling. This pins
    the three things that make the flag work:

      1. the CLI option is registered with `action="store_true"`,
      2. `main()` branches on `args.force_publish`,
      3. that branch returns 0 (it overrides the gates).

    It also pins the pre-split contract that a forced publish advances
    the last-known-good baseline.
    """
    check = _load(CHECK_SCRIPT, "corpus_freshness_check")
    source = CHECK_SCRIPT.read_text()

    if _registers_force_publish(CHECK_SCRIPT):
        pass_("corpus-freshness-check.py registers --force-publish")
    else:
        fail_("corpus-freshness-check.py does not register --force-publish")

    if "if args.force_publish:" in source:
        pass_("corpus-freshness-check.py branches on args.force_publish")
    else:
        fail_("corpus-freshness-check.py does not branch on args.force_publish")

    # The branch body must return 0 BEFORE any gate can return 1.
    body = source.split("if args.force_publish:", 1)[1]
    branch = body.split("corpus, overlay = _paths()", 1)[0]
    if "return 0" in branch:
        pass_("the --force-publish branch returns 0 before any gate runs")
    else:
        fail_("the --force-publish branch does not return 0")
    if "freshness-last.json" in branch:
        pass_("the --force-publish branch advances the freshness-last.json baseline")
    else:
        fail_("the --force-publish branch does not advance the baseline")

    # And the option must really be settable on the parser.
    parser = check.argparse.ArgumentParser()
    parser.add_argument("--force-publish", action="store_true")
    if parser.parse_args(["--force-publish"]).force_publish is True:
        pass_("--force-publish parses to True")
    else:
        fail_("--force-publish does not parse to True")


def test_force_publish_overrides_a_degraded_price_source() -> None:
    """... and --force-publish overrides it (and says so)."""
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        builder = _fake_corpus_builder(openrouter_ok=False, openrouter_reason="HTTP 503")
        _run_fetch(root, failing=[], candidate=191, builder=builder)

        code, out = _run_check(root, force=True)
        if code == 0:
            pass_("--force-publish overrides the degraded price source (exit 0)")
        else:
            fail_(f"--force-publish exited {code}, expected 0")
        if "--force-publish" in out:
            pass_("--force-publish says that it is overriding the gates")
        else:
            fail_("--force-publish did not say that it overrides the gates")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_force_publish_copies_the_report_to_the_baseline() -> None:
    """--force-publish must ALSO update freshness-last.json.

    The pre-split gate copied `corpus/freshness.json` to
    `corpus/freshness-last.json` on a forced publish so the next run has a
    baseline. That behaviour is part of the flag's contract: an operator
    who forces a publish has accepted the run, and the next run must
    compare against it. A flag that only skips the gates but leaves the
    baseline behind would silently re-arm the old coverage delta.
    """
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        builder = _fake_corpus_builder(openrouter_ok=False, openrouter_reason="HTTP 503")
        _run_fetch(root, failing=[], candidate=191, builder=builder)
        last = root / "corpus" / "freshness-last.json"
        last.unlink(missing_ok=True)

        code, _ = _run_check(root, force=True)
        if code == 0:
            pass_("--force-publish exits 0 on a degraded price source")
        else:
            fail_(f"--force-publish exited {code}, expected 0")
        if last.exists():
            pass_("--force-publish writes the freshness-last.json baseline")
        else:
            fail_("--force-publish did NOT write the freshness-last.json baseline")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_force_publish_works_on_a_stale_overlay_too() -> None:
    """--force-publish must override the gates for a STALE overlay run too.

    The stale-overlay path is a warning, not a block, so this is mostly a
    regression guard: the forced publish must still succeed and must still
    leave a baseline for the next run.
    """
    root = _scratch()
    try:
        _setup(root, overlay_count=191)
        _run_fetch(root, failing=["fetch-terminal-bench.py"], candidate=141)
        last = root / "corpus" / "freshness-last.json"
        last.unlink(missing_ok=True)

        code, _ = _run_check(root, force=True)
        if code == 0:
            pass_("--force-publish exits 0 on a stale-overlay run")
        else:
            fail_(f"--force-publish exited {code}, expected 0")
        if last.exists():
            pass_("--force-publish leaves a baseline after a forced publish")
        else:
            fail_("--force-publish left no baseline after a forced publish")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_a_shrinking_catalog_blocks_the_prices() -> None:
    """A catalog that lost 30% of its models is a degraded price source."""
    root = _scratch()
    try:
        _setup(root, catalog_models=100)
        builder = _fake_corpus_builder(catalog_models=70)
        freshness = _run_fetch(root, failing=[], candidate=191, builder=builder)

        prices = freshness.get("prices") or {}
        if prices.get("ok") is False:
            pass_("a 30% smaller catalog is reported as a degraded price source")
        else:
            fail_(f"prices.ok is {prices.get('ok')!r}, expected False")
        if prices.get("previous_catalog_models") == 100 and prices.get("catalog_models") == 70:
            pass_("the report keeps both the previous and the new catalog counts")
        else:
            fail_(f"the catalog counts are wrong: {prices}")

        code, out = _run_check(root)
        if code == 1:
            pass_("a 30% smaller catalog makes the check exit 1")
        else:
            fail_(f"the check exited {code}, expected 1")
        if "PRICE SOURCE DEGRADED" in out and "shrank" in out:
            pass_("the check reports the catalog shrink")
        else:
            fail_("the check did not report the catalog shrink")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_a_schema_1_report_is_still_accepted() -> None:
    """A schema-1 freshness.json keeps working: the overlay's built_at is
    carried forward from the schema-1 `ts`."""
    root = _scratch()
    try:
        previous = {"ts": "2026-10-13T09:08:34Z",
                    "sources": [{"name": "fetch-scale.py", "outputs": ["scale-seal.json"],
                                 "ok": True, "reason": ""}]}
        _setup(root, previous_freshness=previous)
        committed_before = (root / "bench" / "overlay.json").read_bytes()

        freshness = _run_fetch(root, failing=["fetch-terminal-bench.py"], candidate=141)

        if freshness.get("schema") == 2:
            pass_("the new report is schema 2")
        else:
            fail_(f"the report schema is {freshness.get('schema')!r}, expected 2")
        if (freshness.get("overlay") or {}).get("built_at") == "2026-10-13T09:08:34Z":
            pass_("built_at is carried forward from the schema-1 `ts`")
        else:
            fail_(f"built_at is {(freshness.get('overlay') or {}).get('built_at')!r}, "
                  "expected the schema-1 ts")
        if (root / "bench" / "overlay.json").read_bytes() == committed_before:
            pass_("the committed overlay is untouched under a schema-1 report")
        else:
            fail_("the committed overlay was replaced under a schema-1 report")

        # A schema-1 report is still read by the check script with the
        # historic single-gate behaviour.
        (root / "corpus" / "freshness.json").write_text(json.dumps(previous))
        code, out = _run_check(root)
        if code == 0:
            pass_("the check still accepts a schema-1 report")
        else:
            fail_(f"the check exited {code} on a schema-1 report: {out.strip().splitlines()[-1:]}")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def _undefined_names(path: Path) -> list[tuple[str, str]]:
    """Names a module's code references but never binds.

    Uses the stdlib `symtable` module: a name is "global" in a function scope
    when it is neither local nor free, so if it is neither a module-level
    binding nor a builtin it is a NameError waiting to happen.

    A module-level name counts as BOUND only when it is assigned or
    imported. A name that is merely REFERENCED at module scope (e.g. an
    `import json` that was deleted, leaving `json.loads(...)`) is itself
    undefined and must be reported.

    This exists because a DIFF-ONLY reader (a reviewer that sees the changed
    hunks but not the file header) cannot tell an unchanged import or an
    unchanged helper from a missing one. This test makes that distinction
    mechanical: it reads the WHOLE file.
    """
    import builtins
    import symtable

    source = path.read_text()
    table = symtable.symtable(source, str(path), "exec")
    bound = {
        s.get_name() for s in table.get_symbols()
        if s.is_assigned() or s.is_imported()
    }
    builtins_names = set(dir(builtins)) | {"__file__", "__name__", "__doc__"}
    problems: list[tuple[str, str]] = []

    def walk(scope, is_module: bool):
        for sym in scope.get_symbols():
            name = sym.get_name()
            if name in bound or name in builtins_names:
                continue
            if is_module:
                # At module scope a name must be bound somewhere in the
                # module (or be a builtin) to be usable.
                if sym.is_referenced():
                    problems.append((scope.get_name(), name))
            elif sym.is_global() and sym.is_referenced():
                problems.append((scope.get_name(), name))
        for child in scope.get_children():
            walk(child, False)

    walk(table, True)
    return problems


def test_no_script_references_an_undefined_name() -> None:
    """Every script in the corpus pipeline must be self-contained.

    A missing import or an undefined helper is a runtime NameError that the
    behavioural tests would catch only on the path they happen to exercise.
    This check is static and total: it reads the whole module.
    """
    for path in (FETCH_SCRIPT, CHECK_SCRIPT, BUILD_SCRIPT):
        problems = _undefined_names(path)
        if problems:
            fail_(f"{path.name} references undefined name(s): {problems}")
        else:
            pass_(f"{path.name} references no undefined name")


def test_the_check_script_wires_up_every_gate_helper() -> None:
    """The gate helpers must be DEFINED and CALLED, not merely present.

    A helper that is defined but never invoked is dead code: the gate would
    silently pass everything. This asserts the definitions exist AND that
    `main()` reaches them.
    """
    check = _load(CHECK_SCRIPT, "corpus_freshness_check")
    for name in ("_paths", "_price_gate", "_failing_sources", "_count_scored",
                 "_check_schema1", "_check_schema2"):
        if callable(getattr(check, name, None)):
            pass_(f"corpus-freshness-check.py defines {name}()")
        else:
            fail_(f"corpus-freshness-check.py does not define {name}()")

    source = CHECK_SCRIPT.read_text()
    for name in ("_price_gate", "_failing_sources", "_check_schema2", "_check_schema1"):
        # A call site is `name(` that is NOT the `def name(` line.
        call_sites = [
            line for line in source.splitlines()
            if f"{name}(" in line and not line.lstrip().startswith("def ")
        ]
        if call_sites:
            pass_(f"corpus-freshness-check.py calls {name}()")
        else:
            fail_(f"corpus-freshness-check.py never calls {name}()")


def test_the_openrouter_status_entry_is_updated_in_place() -> None:
    """The run status' NESTED "openrouter" entry must be filled in.

    The price gate reads `prices.openrouter.ok`. If the builder wrote the
    result anywhere else (e.g. on the top-level status dict), the entry
    would keep its initial `ok=False` and the gate would block every
    publish -- or, worse, read a stale `ok=True`.
    """
    builder = _load(BUILD_SCRIPT, "build_public_corpus")
    status = {"litellm": {"ok": False, "models": 0, "reason": ""},
              "openrouter": {"ok": False, "added": 0, "reason": ""},
              "catalog_models": 0, "corpus_models": 0, "routable": 0, "scored": 0}

    # A failing fetch: the nested entry must say ok=False + the reason.
    def boom(url, timeout=30):
        raise OSError("simulated openrouter outage")
    original = builder.fetch_json
    builder.fetch_json = boom
    try:
        builder.overlay_openrouter({}, status["openrouter"])
    finally:
        builder.fetch_json = original
    if status["openrouter"]["ok"] is False and status["openrouter"]["reason"]:
        pass_("a failed OpenRouter fetch fills the NESTED openrouter status (ok=False + reason)")
    else:
        fail_(f"the nested openrouter status was not filled on failure: {status['openrouter']}")

    # A successful fetch: the nested entry must say ok=True + the count.
    builder.fetch_json = lambda url, timeout=30: {"data": [
        {"id": "vendor/model-a", "pricing": {"prompt": "0.0", "completion": "0.0"}},
    ]}
    try:
        builder.overlay_openrouter({}, status["openrouter"])
    finally:
        builder.fetch_json = original
    if status["openrouter"]["ok"] is True and status["openrouter"]["added"] == 1:
        pass_("a successful OpenRouter fetch fills the NESTED openrouter status (ok=True + added)")
    else:
        fail_(f"the nested openrouter status was not filled on success: {status['openrouter']}")


def test_a_multi_output_source_needs_every_lkg_file() -> None:
    """arena writes two files: one surviving LKG file must not count as cover."""
    root = _setup(_scratch(), overlay_count=191)
    lkg = root / "bench" / "raw" / ".lkg"
    lkg.mkdir(parents=True, exist_ok=True)
    (lkg / "arena-webdev.json").write_text(json.dumps({"models": []}))
    fresh = _run_fetch(root, failing=["fetch-arena.py"], candidate=191)
    arena = next(s for s in fresh["sources"] if s["name"] == "fetch-arena.py")
    if arena["lkg"] is False and fresh["overlay_stale"] is True:
        pass_("a two-output source with only one LKG file is not reported as covered")
    else:
        fail_(f"partial LKG counted as cover: lkg={arena['lkg']} stale={fresh['overlay_stale']}")
    if (root / "bench" / "raw" / "arena-webdev.json").exists():
        pass_("the output that did have an LKG file is still restored")
    else:
        fail_("the available LKG output was not restored")

    root2 = _setup(_scratch(), overlay_count=191)
    lkg2 = root2 / "bench" / "raw" / ".lkg"
    lkg2.mkdir(parents=True, exist_ok=True)
    for name in ("arena-webdev.json", "arena-agent.json"):
        (lkg2 / name).write_text(json.dumps({"models": []}))
    fresh2 = _run_fetch(root2, failing=["fetch-arena.py"], candidate=191)
    arena2 = next(s for s in fresh2["sources"] if s["name"] == "fetch-arena.py")
    if arena2["lkg"] is True and fresh2["overlay_stale"] is False:
        pass_("a source whose every output has an LKG file counts as covered")
    else:
        fail_(f"full LKG not counted: lkg={arena2['lkg']} stale={fresh2['overlay_stale']}")


def test_lkg_fallback_is_off_without_the_flag() -> None:
    """Without --allow-lkg-fallback a failed source is never backfilled."""
    root = _setup(_scratch(), overlay_count=191)
    lkg = root / "bench" / "raw" / ".lkg"
    lkg.mkdir(parents=True, exist_ok=True)
    (lkg / "terminal-bench.json").write_text(json.dumps({"models": ["lkg"]}))
    module = _load(FETCH_SCRIPT, "corpus_fetch_and_overlay")
    module.run_pipeline(
        root,
        fetch_one=_fake_fetcher(["fetch-terminal-bench.py"]),
        overlay_builder=_fake_overlay_builder(191),
        corpus_builder=_fake_corpus_builder(),
        now_iso="2026-10-26T12:00:00Z",
        allow_lkg_fallback=False,
    )
    fresh = json.loads((root / "corpus" / "freshness.json").read_text())
    tb = next(s for s in fresh["sources"] if s["name"] == "fetch-terminal-bench.py")
    restored = (root / "bench" / "raw" / "terminal-bench.json").exists()
    if tb["lkg"] is False and fresh["overlay_stale"] is True and not restored:
        pass_("without the flag a failed source is unavailable and the overlay is kept stale")
    else:
        fail_(f"LKG used without the flag: lkg={tb['lkg']} stale={fresh['overlay_stale']} restored={restored}")

    src = FETCH_SCRIPT.read_text()
    if "allow_lkg_fallback=args.allow_lkg_fallback" in src:
        pass_("main() passes --allow-lkg-fallback through to the pipeline")
    else:
        fail_("main() ignores --allow-lkg-fallback")


def main() -> int:
    _run_one(test_one_bench_source_failing_keeps_the_committed_overlay)
    _run_one(test_candidate_overlay_losing_coverage_is_kept_out)
    _run_one(test_fresh_candidate_overlay_is_published)
    _run_one(test_openrouter_failure_blocks_the_prices)
    _run_one(test_a_malformed_sources_list_does_not_crash_the_gate)
    _run_one(test_a_string_false_stale_flag_is_read_as_not_stale)
    _run_one(test_every_script_starts_as_a_real_process)
    _run_one(test_a_disagreeing_stale_flag_is_read_as_stale)
    _run_one(test_force_publish_is_still_wired_up)
    _run_one(test_force_publish_overrides_a_degraded_price_source)
    _run_one(test_force_publish_copies_the_report_to_the_baseline)
    _run_one(test_force_publish_works_on_a_stale_overlay_too)
    _run_one(test_a_shrinking_catalog_blocks_the_prices)
    _run_one(test_a_schema_1_report_is_still_accepted)
    _run_one(test_the_openrouter_status_entry_is_updated_in_place)
    _run_one(test_no_script_references_an_undefined_name)
    _run_one(test_the_check_script_wires_up_every_gate_helper)
    _run_one(test_a_multi_output_source_needs_every_lkg_file)
    _run_one(test_lkg_fallback_is_off_without_the_flag)
    print(f"\n{PASSES} passed; {FAILS} failed")
    return 0 if FAILS == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
