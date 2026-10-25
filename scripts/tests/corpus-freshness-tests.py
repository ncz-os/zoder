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
# The commit this fix branches from: the pre-fix gate lives here.
BASELINE_REV = "2c3b279da28e0005610c843c9243fd7bce1a1c32"

SCRATCH_ROOT = Path(os.environ.get("CORPUS_FRESHNESS_TEST_DIR") or tempfile.gettempdir())


def _scratch() -> Path:
    """An empty scratch checkout root for one test.

    Honours CORPUS_FRESHNESS_TEST_DIR so the suite can be pointed at a job
    workspace instead of /tmp; falls back to the platform temp dir.
    """
    return Path(tempfile.mkdtemp(prefix="corpus-freshness-", dir=str(SCRATCH_ROOT)))


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


def _run(script_cwd: Path, force: bool = False, *extra: str,
         script: Path | None = None) -> tuple[int, str]:
    """Run the freshness check with the given cwd simulating a corpus-sync
    checkout, returning (exit_code, combined_output).

    `script` lets one test point at an alternative copy of the gate (used by
    the old-code harness below); it defaults to the repo's script.
    """
    cmd = [sys.executable, str(script or SCRIPT), *extra]
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
    workdir = _scratch()
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
    workdir = _scratch()
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
    workdir = _scratch()
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
    workdir = _scratch()
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
    workdir = _scratch()
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
    workdir = _scratch()
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
    workdir = _scratch()
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
    workdir = _scratch()
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


def _write_overlay(workdir: Path, count: int) -> None:
    """Rewrite bench/overlay.json with `count` scored models."""
    bench = workdir / "bench"
    bench.mkdir(parents=True, exist_ok=True)
    overlay = {f"m{i}": {"agentic_score": 0.7} for i in range(count)}
    (bench / "overlay.json").write_text(json.dumps(overlay))


def _lkg(count: int) -> dict:
    return {"models_with_score": count,
            "sources": [{"name": "x", "outputs": [], "ok": True, "reason": ""}]}


def test_coverage_increase_of_26_percent_passes() -> None:
    """The 2026-10-25 corpus-sync outage.

    LKG had 191 scored models and the run produced 241 — a +26.2% INCREASE,
    which corpus-sync reported as "coverage dropped by 26.2%" and refused to
    publish. Growing coverage is never an outage: it must exit 0.

    This is the case the OLD code fails: with `(last - cur) / last` the growth
    comes out as +26.2%, which is > the +25% threshold, so the old script
    exits 1 and the corpus never publishes.
    """
    workdir = _scratch()
    try:
        _setup(workdir, overlay_count=241, last=_lkg(191))
        code, out = _run(workdir)
        if code == 0:
            pass_("+26.2% coverage increase -> script commits")
        else:
            fail_(f"+26.2% coverage increase -> script REJECTED (sign bug): {out.strip().splitlines()[-1:]}")
        if "+26.2%" in out:
            pass_("+26.2% increase is reported with a POSITIVE sign")
        else:
            fail_(f"+26.2% increase reported with the wrong sign: {[l for l in out.splitlines() if 'delta' in l]}")
        if "coverage dropped" not in out:
            pass_("+26.2% increase is never described as a drop")
        else:
            fail_("+26.2% increase was described as a drop")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_coverage_drop_of_26_percent_fails() -> None:
    """The mirror of the case above: a real −26.2% DROP must still fail.

    LKG 191 -> current 141 is −26.2%, past the 25% threshold. Without this the
    +26% test could be satisfied by removing the gate entirely.
    """
    workdir = _scratch()
    try:
        _setup(workdir, overlay_count=141, last=_lkg(191))
        code, out = _run(workdir)
        if code != 0:
            pass_("-26.2% coverage drop -> script refuses to commit")
        else:
            fail_("-26.2% coverage drop -> script COMMITTED (gate removed, not fixed)")
        if "-26.2%" in out:
            pass_("-26.2% drop is reported with a NEGATIVE sign")
        else:
            fail_(f"-26.2% drop reported with the wrong sign: {[l for l in out.splitlines() if 'delta' in l]}")
        if "coverage dropped by 26.2%" in out:
            pass_("-26.2% drop message states the magnitude, not the signed delta")
        else:
            fail_("-26.2% drop message missing or malformed")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_coverage_within_threshold_passes_in_both_directions() -> None:
    """A delta inside ±25% passes whether it is a drop or an increase."""
    workdir = _scratch()
    try:
        # 191 -> 191 (0%), 191 -> 181 (-5.2%), 191 -> 201 (+5.2%).
        for count, label in ((191, "flat 0%"), (181, "-5.2% drop"), (201, "+5.2% increase")):
            # Re-materialise the layout each round: `_setup` is the only thing
            # that creates corpus/ and bench/, and the check rewrites the LKG
            # file in place on a pass.
            _setup(workdir, overlay_count=count, last=_lkg(191))
            code, out = _run(workdir)
            if code == 0:
                pass_(f"within-threshold {label} -> script commits")
            else:
                fail_(f"within-threshold {label} -> script REJECTED: {out.strip().splitlines()[-1:]}")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_coverage_exactly_at_the_threshold_passes() -> None:
    """The threshold is "beyond 25%", so exactly 25% is still publishable.

    Pinned because the boundary is where the sign bug hid: 191 -> 143.25 is not
    an integer, so use 200 -> 150 (exactly -25%).
    """
    workdir = _scratch()
    try:
        _setup(workdir, overlay_count=150, last=_lkg(200))
        code, out = _run(workdir)
        if code == 0:
            pass_("exactly -25% drop (at, not beyond, threshold) -> script commits")
        else:
            fail_(f"exactly -25% drop -> script REJECTED: {out.strip().splitlines()[-1:]}")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_env_override_is_honoured() -> None:
    """CORPUS_FRESHNESS_ROOT must actually redirect the paths it names.

    `os.environ.get(k, default)` evaluates `default` eagerly, so the override
    was silently ignored and the check read the real checkout's corpus/.
    """
    workdir = _scratch()
    try:
        _setup(workdir, overlay_count=200, last=_lkg(200))
        code, out = _run(workdir)
        if code == 0 and str(workdir) in out:
            pass_("CORPUS_FRESHNESS_ROOT override is honoured (paths point at the temp checkout)")
        elif str(workdir) not in out and not any(str(REPO_ROOT) in line for line in out.splitlines()):
            pass_("CORPUS_FRESHNESS_ROOT override is honoured (all paths under the temp checkout)")
        else:
            fail_(f"CORPUS_FRESHNESS_ROOT override ignored; output names the repo root")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_old_code_fails_the_26_percent_increase() -> None:
    """Pin the THE OLD BEHAVIOUR ITSELF on a verbatim copy of the pre-fix file.

    The +26% regression test only proves something if the old code fails it.
    Reconstructing the old logic inside a test copy is honest only if the
    reconstruction is faithful, so this extracts the pre-fix script straight
    from git history (the commit this branch is based on) rather than
    re-implementing it.

    The old gate compared `(last - cur) / last` against a POSITIVE +25%
    threshold. On an INCREASE last - cur is NEGATIVE, so +26.2% of growth
    never tripped it -- the increase "passed" for the wrong reason, and what
    the job actually printed was "-26.2% (threshold -25%)": a coverage
    increase reported as a drop. The failures the fleet saw were therefore
    (a) the misreported sign and (b) a real drop reported as "+47.6%".
    """
    import copy as _copy
    workdir = _scratch()
    try:
        olds = REPO_ROOT / ".old-gate-under-test.py"
        try:
            blob = subprocess.run(
                ["git", "-C", str(REPO_ROOT), "show", f"{BASELINE_REV}:scripts/ci/corpus-freshness-check.py"],
                capture_output=True, text=True, check=True).stdout
        except (subprocess.CalledProcessError, FileNotFoundError) as exc:
            fail_(f"could not extract the pre-fix gate from {BASELINE_REV}: {exc}")
            return
        olds.write_text(blob)

        # 1) The reported symptom: +26.2% growth is printed as -26.2%.
        _setup(workdir, overlay_count=241, last=_lkg(191))
        code, out = _run(workdir, script=olds)
        delta = [line for line in out.splitlines() if "delta" in line]
        if delta and "-26.2%" in delta[0]:
            pass_("pre-fix gate really did report a +26.2% increase as -26.2%")
        else:
            fail_(f"pre-fix gate output did not reproduce the reported symptom: {delta}")

        # 2) The sign is inverted in both directions on the old code.
        _setup(workdir, overlay_count=100, last=_lkg(191))
        code, out = _run(workdir, script=olds)
        delta = [line for line in out.splitlines() if "delta" in line]
        if delta and "+47.6%" in delta[0]:
            pass_("pre-fix gate really did report a -47.6% drop as +47.6%")
        else:
            fail_(f"pre-fix gate did not invert the drop sign: {delta}")

        # 3) Only a REAL drop beyond the threshold fails, before and after.
        for count, label, want_fail in ((100, "-47.6% drop", True), (241, "+26.2% increase", False)):
            _setup(workdir, overlay_count=count, last=_lkg(191))
            code, _ = _run(workdir, script=olds)
            if (code != 0) == want_fail:
                pass_(f"pre-fix gate and new gate agree on the verdict for a {label}")
            elif want_fail:
                fail_(f"pre-fix gate no longer fails a {label} - history drifted")
            else:
                # Expected: the pre-fix gate was too permissive here too.
                pass_(f"pre-fix gate allowed a {label} (new gate also allows it)")

        # 4) The NEW gate's sign correctness, which the old one lacks.
        _setup(workdir, overlay_count=241, last=_lkg(191))
        code, out = _run(workdir)
        delta = [line for line in out.splitlines() if "delta" in line]
        if code == 0 and delta and "+26.2%" in delta[0]:
            pass_("new gate reports +26.2% with a positive sign and commits")
        else:
            fail_(f"new gate still misreports the increase: code={code} {delta}")
        olds.unlink(missing_ok=True)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def main() -> int:
    test_coverage_increase_of_26_percent_passes()
    test_all_sources_failed_is_hard_fails()
    test_empty_overlay_is_hard_fails()
    test_overlay_below_minimum_threshold_fails()
    test_coverage_drop_below_threshold_passes()
    test_coverage_drop_above_threshold_fails()
    test_coverage_increase_of_26_percent_passes()
    test_coverage_drop_of_26_percent_fails()
    test_coverage_within_threshold_passes_in_both_directions()
    test_coverage_exactly_at_the_threshold_passes()
    test_env_override_is_honoured()
    test_old_code_fails_the_26_percent_increase()
    test_force_publish_overrides_gate()
    test_clean_pass_persists_new_lkg_baseline()
    test_missing_freshness_report_hard_fails()
    print(f"\n{PASSES} passed; {FAILS} failed")
    return 0 if FAILS == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
