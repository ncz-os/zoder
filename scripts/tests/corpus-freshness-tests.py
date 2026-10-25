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


def _run_env(cwd: Path, env_overrides: dict[str, str]) -> tuple[int, str]:
    """Run the gate from `cwd` with an explicit environment, verbatim.

    Unlike `_run`, this does NOT inject CORPUS_FRESHNESS_ROOT -- the caller
    passes exactly what it wants to test (including an empty value).
    """
    env = {**os.environ, **env_overrides}
    r = subprocess.run([sys.executable, str(SCRIPT)], cwd=str(cwd),
                       capture_output=True, text=True, env=env)
    return r.returncode, r.stdout + r.stderr


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

    LKG had 191 scored models and the run produced 241 — a +26.2% INCREASE
    that the gate printed as "coverage delta vs LKG = -26.2%", i.e. reported
    as a drop. Growing coverage is never an outage: it must exit 0, and it
    must say +26.2%.

    What the OLD code got wrong here was the NUMBER, not the verdict. With
    `(last - cur) / last` the growth comes out as -0.262, which is NOT
    `> +0.25`, so the old gate also exited 0 — it passed for the wrong reason
    and printed a drop. `test_the_gate_agrees_with_a_signed_delta_table` pins
    that measured table; do not describe this case as one the old code
    rejected, because it did not.
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
    +26% test could be satisfied by removing the gate entirely. The old gate
    also rejected this case (its inverted delta +0.262 tripped `> +0.25`), so
    the fix here is again the sign, not the verdict.
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


def test_the_gate_agrees_with_a_signed_delta_table() -> None:
    """Pin the verdict table of BOTH gates against the same LKG of 191 models.

    This is the regression that keeps the reported numbers honest, and it is
    written as a table so the sign of every case is visible in one place.

    Measured (2026-10-25) from the pre-fix script as committed, reproduced in
    this test by extracting it from git history:

        cur   real delta   pre-fix prints   pre-fix exit
        241      +26.2%        -26.2%            0
        200       +4.7%         -4.7%            0
        191       +0.0%         +0.0%            0
        150      -21.5%        +21.5%            0
        141      -26.2%        +26.2%            1
        100      -47.6%        +47.6%            1

    Two things follow, and both matter:

      * The pre-fix gate INVERTED THE SIGN in both directions: a +26.2%
        increase was printed as -26.2% and a -47.6% drop as +47.6%. That is
        exactly the "coverage delta vs LKG = +26.2% ... coverage dropped by
        26.2%" pair from the incident report, seen from the other side.
      * The pre-fix gate still FAILED a real drop. Its numerator was the
        mirror image of the delta, so the inverted sign was inverted again by
        `drop > +0.25`, and the verdict (pass on growth, fail on a real drop)
        was accidentally right. Only the SIGN was wrong, and the sign is what
        the job prints. So "the old code fails the +26% case" is FALSE: the
        old code passed it, for the wrong reason and with the wrong number.
        The new code keeps the verdict and fixes the number.
    """
    workdir = _scratch()
    prefixed = workdir / "pre-fix-gate.py"   # scratch only: never the repo root
    try:
        try:
            blob = subprocess.run(
                ["git", "-C", str(REPO_ROOT), "show",
                 f"{BASELINE_REV}:scripts/ci/corpus-freshness-check.py"],
                capture_output=True, text=True, check=True).stdout
        except (subprocess.CalledProcessError, FileNotFoundError) as exc:
            # F6: absence of history is a skip, not a failure -- a shallow
            # clone cannot answer this question and must not go red.
            print(f"skip  pre-fix gate unavailable in this checkout ({exc})")
            return
        prefixed.write_text(blob)

        # (cur, real delta, pre-fix exit, pre-fix printed sign, new exit, new sign)
        table = (
            (241, "+26.2%", 0, "-26.2%", 0, "+26.2%"),
            (200, "+4.7%", 0, "-4.7%", 0, "+4.7%"),
            (191, "+0.0%", 0, "+0.0%", 0, "+0.0%"),
            (150, "-21.5%", 0, "+21.5%", 0, "-21.5%"),
            (141, "-26.2%", 1, "+26.2%", 1, "-26.2%"),
            (100, "-47.6%", 1, "+47.6%", 1, "-47.6%"),
        )
        for count, real, old_exit, old_sign, new_exit, new_sign in table:
            _setup(workdir, overlay_count=count, last=_lkg(191))

            old_code, old_out = _run(workdir, script=prefixed)
            old_delta = next((l for l in old_out.splitlines() if "delta" in l), "")
            if old_code == old_exit and old_sign in old_delta:
                pass_(f"pre-fix gate at {real}: prints {old_sign}, exit {old_exit} (as measured)")
            else:
                fail_(f"pre-fix gate drifted at {real}: exit {old_code} (want {old_exit}), line {old_delta!r}")

            _setup(workdir, overlay_count=count, last=_lkg(191))
            new_code, new_out = _run(workdir)
            new_delta = next((l for l in new_out.splitlines() if "delta" in l), "")
            if new_code == new_exit and new_sign in new_delta:
                pass_(f"new gate at {real}: prints {new_sign}, exit {new_exit} (as measured)")
            else:
                fail_(f"new gate drifted at {real}: exit {new_code} (want {new_exit}), line {new_delta!r}")

        # The verdict must be identical in both gates; only the number changed.
        if all(old_exit == new_exit for _, _, old_exit, _, new_exit, _ in table):
            pass_("both gates agree on every verdict; the fix changes the sign, not the policy")
        else:
            fail_("the fix changed a verdict, not just the sign")
    finally:
        prefixed.unlink(missing_ok=True)
        shutil.rmtree(workdir, ignore_errors=True)


def test_env_override_is_honoured_when_set() -> None:
    """A SET CORPUS_FRESHNESS_ROOT redirects the gate to that root.

    Note this already worked before the fix -- `os.environ.get(k, default)`
    only applies `default` when the key is absent. It is pinned because the
    suite depends on it to run against scratch layouts at all.
    """
    outer = _scratch()
    workdir = outer / "external-corpus-root"
    try:
        _setup(workdir, overlay_count=200)
        lkg = workdir / "corpus" / "freshness-last.json"
        if lkg.exists():
            fail_("test setup left an LKG behind")
            return
        code, _ = _run(workdir)
        if code == 0 and lkg.exists():
            pass_("a set CORPUS_FRESHNESS_ROOT redirects the gate to that root")
        else:
            fail_(f"set override not honoured: exit={code}, LKG at override={lkg.exists()}")
    finally:
        shutil.rmtree(outer, ignore_errors=True)


def test_empty_env_override_falls_back_to_the_repo_root() -> None:
    """A PRESENT BUT EMPTY override must resolve to REPO_ROOT, not the CWD.

    An unset CI variable exported as "" is a real shape (`env X= cmd`), and
    `Path("")` is "." -- so the gate would look for `./corpus/freshness.json`
    wherever the job happens to be running.

    The assertion does not depend on the checkout's corpus being fresh, on the
    real LKG, or on the real overlay: it asks the script what it resolved to.
    A decoy corpus in the CWD carries model counts (7777/7778) that appear
    nowhere in this repository, so "the decoy was used" is unambiguous.
    """
    cwd = _scratch()          # stands in for "wherever the job happens to be"
    try:
        (cwd / "corpus").mkdir(parents=True)
        (cwd / "bench").mkdir(parents=True)
        (cwd / "corpus" / "freshness.json").write_text(json.dumps(
            {"sources": [{"name": "decoy", "ok": True}]}))
        (cwd / "corpus" / "freshness-last.json").write_text(json.dumps(_lkg(7777)))
        (cwd / "bench" / "overlay.json").write_text(json.dumps(
            {f"m{i}": {"agentic_score": 0.7} for i in range(7778)}))

        # Ask the script itself which paths _paths() produced. This is exact,
        # needs no corpus state, and cannot pass vacuously on a crash.
        probe = (
            "import json, runpy, sys\n"
            f"sys.argv = [{str(SCRIPT)!r}]\n"
            f"g = runpy.run_path({str(SCRIPT)!r}, run_name='probe')\n"
            "corpus, overlay = g['_paths']()\n"
            "print('PROBE' + json.dumps([str(corpus), str(overlay)]))\n"
        )
        env = {**os.environ, "CORPUS_FRESHNESS_ROOT": ""}
        run = subprocess.run([sys.executable, "-c", probe], capture_output=True,
                             text=True, env=env, cwd=str(cwd))
        out = run.stdout + run.stderr
        line = next((l for l in out.splitlines() if l.startswith("PROBE[")), None)
        if line is None:
            fail_(f"probe produced no path tuple: {out.strip()[:200]!r}")
            return
        corpus_path, overlay_path = json.loads(line[len("PROBE"):])
        expected_corpus = str(REPO_ROOT / "corpus")
        if corpus_path == expected_corpus:
            pass_("an empty CORPUS_FRESHNESS_ROOT resolves to REPO_ROOT/corpus, not the CWD")
        else:
            fail_(f"empty override resolved to {corpus_path!r}, expected {expected_corpus!r}")
        if str(cwd) not in corpus_path:
            pass_("the CWD decoy was not selected")
        else:
            fail_("the empty override fell through to the CWD")

        # And end-to-end: the gate must not mention the decoy counts.
        code, gate_out = _run_env(cwd, {"CORPUS_FRESHNESS_ROOT": ""})
        if "7777" not in gate_out and "7778" not in gate_out:
            pass_("a full run with an empty override never reads the CWD decoy")
        else:
            fail_(f"the gate read the CWD decoy: {gate_out.strip()[:200]!r}")
    finally:
        shutil.rmtree(cwd, ignore_errors=True)


def main() -> int:
    test_all_sources_failed_is_hard_fails()
    test_empty_overlay_is_hard_fails()
    test_overlay_below_minimum_threshold_fails()
    test_coverage_drop_below_threshold_passes()
    test_coverage_drop_above_threshold_fails()
    test_coverage_increase_of_26_percent_passes()
    test_coverage_drop_of_26_percent_fails()
    test_coverage_within_threshold_passes_in_both_directions()
    test_coverage_exactly_at_the_threshold_passes()
    test_env_override_is_honoured_when_set()
    test_empty_env_override_falls_back_to_the_repo_root()
    test_the_gate_agrees_with_a_signed_delta_table()
    test_force_publish_overrides_gate()
    test_clean_pass_persists_new_lkg_baseline()
    test_missing_freshness_report_hard_fails()
    print(f"\n{PASSES} passed; {FAILS} failed")
    return 0 if FAILS == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
