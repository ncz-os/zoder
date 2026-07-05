#!/usr/bin/env python3
"""Behavioral regression tests for the Finding #16 corpus freshness gate
(`scripts/ci/corpus-freshness-check.py`). Pure deterministic subprocess
tests — no network. Each test fails on the OLD code (which silently
committed a degraded/empty overlay) and passes on the NEW one.

Run: `python3 scripts/tests/corpus-freshness-tests.py`
"""
from __future__ import annotations
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "ci" / "corpus-freshness-check.py"

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


def _run(script_cwd: Path, force: bool = False, *extra: str) -> tuple[int, str]:
    """Run the freshness check with the given cwd simulating a corpus-sync
    checkout, returning (exit_code, combined_output)."""
    cmd = [sys.executable, str(SCRIPT), *extra]
    if force:
        cmd.append("--force-publish")
    env = os.environ.copy()
    # Point the script at the temp checkout so it doesn't read the real
    # repo's corpus/ while the test sets up a controlled layout under
    # `script_cwd`.
    env["CORPUS_FRESHNESS_ROOT"] = str(script_cwd)
    r = subprocess.run(cmd, cwd=script_cwd, capture_output=True, text=True, env=env)
    return r.returncode, (r.stdout + r.stderr)


def _setup(workdir: Path, *, freshness: dict | None = None,
           overlay: dict | None = None,
           last: dict | None = None,
           overlay_count: int = 200) -> Path:
    """Materialise a working corpus-sync checkout layout under `workdir`."""
    corpus = workdir / "corpus"
    bench = workdir / "bench"
    corpus.mkdir(parents=True, exist_ok=True)
    bench.mkdir(parents=True, exist_ok=True)
    if freshness is None:
        freshness = {
            "ts": "2026-07-05T00:00:00Z",
            "sources": [
                {"name": "fetch-vals-swebench.py", "outputs": ["vals-swebench.json"], "ok": True, "reason": ""},
                {"name": "fetch-scale.py",        "outputs": ["scale-seal.json"],    "ok": True, "reason": ""},
            ],
        }
    (corpus / "freshness.json").write_text(json.dumps(freshness))
    if last is not None:
        (corpus / "freshness-last.json").write_text(json.dumps(last))
    if overlay is None:
        overlay = {f"m{i}": {"agentic_score": 0.7} for i in range(overlay_count)}
    (bench / "overlay.json").write_text(json.dumps(overlay))
    return workdir


def test_all_sources_failed_is_hard_fails() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        freshness = {
            "ts": "2026-07-05T00:00:00Z",
            "sources": [
                {"name": "fetch-vals-swebench.py", "outputs": ["vals-swebench.json"], "ok": False, "reason": "timeout"},
                {"name": "fetch-scale.py",        "outputs": ["scale-seal.json"],    "ok": False, "reason": "5xx"},
            ],
        }
        _setup(workdir, freshness=freshness, overlay_count=0)
        code, _ = _run(workdir)
        if code != 0:
            pass_("all sources failed -> script refuses to commit (nonzero exit)")
        else:
            fail_("all sources failed -> script BAILED with success (OLD bug)")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_empty_overlay_is_hard_fails() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        # Sources are nominally fresh but produce an empty overlay — this
        # was the "fell through to empty when raw started empty" failure
        # mode from Finding #16.
        _setup(workdir, overlay_count=0)
        code, _ = _run(workdir)
        if code != 0:
            pass_("empty overlay -> script refuses to commit (nonzero exit)")
        else:
            fail_("empty overlay -> script BAILED with success (OLD bug)")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_overlay_below_minimum_threshold_fails() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        _setup(workdir, overlay_count=10)  # below MIN_AGENTIC_SCORE_COUNT
        code, _ = _run(workdir)
        if code != 0:
            pass_("overlay below MIN -> script refuses to commit")
        else:
            fail_("overlay below MIN -> script BAILED with success")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_coverage_drop_below_threshold_passes() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        # LKG had 200 models; current run has 180 — a 10% drop, well
        # under the 25% threshold. Must pass.
        last = {"models_with_score": 200, "sources": [{"name": "x", "outputs": [], "ok": True, "reason": ""}]}
        _setup(workdir, overlay_count=180, last=last)
        code, _ = _run(workdir)
        if code == 0:
            pass_("10% coverage drop (within threshold) -> script commits")
        else:
            fail_("10% coverage drop -> script REJECTED (false alarm)")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_coverage_drop_above_threshold_fails() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        # LKG had 200 models; current run has 100 — a 50% drop, well
        # above the 25% threshold. Must fail so a transient upstream
        # outage can NOT erase routing scores from master.
        last = {"models_with_score": 200, "sources": [{"name": "x", "outputs": [], "ok": True, "reason": ""}]}
        _setup(workdir, overlay_count=100, last=last)
        code, _ = _run(workdir)
        if code != 0:
            pass_("50% coverage drop (beyond threshold) -> script refuses to commit")
        else:
            fail_("50% coverage drop -> script COMMITTED (OLD bug)")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_force_publish_overrides_gate() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        # 50% drop would normally fail; --force-publish lets it through
        # while still updating the LKG baseline.
        last = {"models_with_score": 200, "sources": [{"name": "x", "outputs": [], "ok": True, "reason": ""}]}
        _setup(workdir, overlay_count=100, last=last)
        code, _ = _run(workdir, force=True)
        if code == 0:
            if (workdir / "corpus" / "freshness-last.json").exists():
                pass_("--force-publish bypasses gate + still updates LKG")
            else:
                fail_("--force-publish bypassed but did NOT update LKG baseline")
        else:
            fail_("--force-publish did NOT bypass the gate")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_clean_pass_persists_new_lkg_baseline() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        # No prior LKG; clean run; must exit 0 AND write the baseline.
        _setup(workdir, overlay_count=200)
        # Sanity: no prior LKG file.
        assert not (workdir / "corpus" / "freshness-last.json").exists()
        code, _ = _run(workdir)
        lkg = workdir / "corpus" / "freshness-last.json"
        if code == 0 and lkg.exists():
            try:
                persisted = json.loads(lkg.read_text())
                if persisted.get("models_with_score") == 200:
                    pass_("clean pass persists LKG baseline (models_with_score=200)")
                else:
                    fail_(f"clean pass wrote LKG with wrong models_with_score: {persisted.get('models_with_score')!r}")
            except Exception as e:
                fail_(f"clean pass wrote invalid LKG: {e}")
        else:
            fail_("clean pass did not exit 0 OR write LKG baseline")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_missing_freshness_report_hard_fails() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="corpus-freshness-"))
    try:
        _setup(workdir)
        # Delete the current freshness.json — script must refuse to run
        # without the upstream companion.
        (workdir / "corpus" / "freshness.json").unlink()
        code, _ = _run(workdir)
        if code != 0:
            pass_("missing corpus/freshness.json -> nonzero exit (upstream companion required)")
        else:
            fail_("missing corpus/freshness.json -> script EXIT 0 (no gate)")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def main() -> int:
    test_all_sources_failed_is_hard_fails()
    test_empty_overlay_is_hard_fails()
    test_overlay_below_minimum_threshold_fails()
    test_coverage_drop_below_threshold_passes()
    test_coverage_drop_above_threshold_fails()
    test_force_publish_overrides_gate()
    test_clean_pass_persists_new_lkg_baseline()
    test_missing_freshness_report_hard_fails()
    print(f"\n{PASSES} passed; {FAILS} failed")
    return 0 if FAILS == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
