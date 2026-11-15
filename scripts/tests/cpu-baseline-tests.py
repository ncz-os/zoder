#!/usr/bin/env python3
"""Regression tests for POOL-ZD-001: the shipped x86_64 zoder binary must run
on x86-64-v2 hardware (SSE4.2/POPCNT, no AVX2/BMI/FMA required).

Background: the released zoder 0.2.1 x86_64 binary died with SIGILL
(exit 132) on pool hosts without AVX2/BMI/FMA (Dell test host: AVX only),
so `zoder config --validate` killed the fleet config deploy. The release
build config pinned no CPU baseline, so the artifact's ISA floated with
whatever the build host's toolchain defaulted to.

The fix pins the x86_64 baseline to x86-64-v2 in the repo build config
plus the release/nightly workflows, and adds an early runtime guard in
`main()` that prints `unsupported CPU: missing <flag>` (exit 69) instead
of dying with SIGILL when a binary built for a higher baseline lands on
an older CPU.

Each test fails on the OLD code (no pin, no guard) and passes on the NEW
one. Run: `python3 scripts/tests/cpu-baseline-tests.py`
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(
    os.environ.get("CPU_BASELINE_REPO_ROOT") or Path(__file__).resolve().parents[2]
)

# The documented x86-64 baseline every release artifact must target.
BASELINE_CPU = "x86-64-v2"
# A higher-baseline flag must never appear in any x86_64 build config.
HIGHER_BASELINE_TOKENS = (
    "target-cpu=native",
    "target-cpu=x86-64-v3",
    "target-cpu=x86-64-v4",
    "target_cpu=native",
    "-march=native",
)

PASSES = 0
FAILS = 0
SKIPS = 0


def pass_(msg: str) -> None:
    global PASSES
    PASSES += 1
    print(f"ok    {msg}")


def fail_(msg: str) -> None:
    global FAILS
    FAILS += 1
    print(f"FAIL  {msg}")


def skip_(msg: str) -> None:
    global SKIPS
    SKIPS += 1
    print(f"skip  {msg}")


def _scratch() -> Path:
    """Scratch dir inside the job workspace (never /tmp or a tmpfs)."""
    root = Path(
        os.environ.get("CPU_BASELINE_TEST_DIR") or (REPO_ROOT / ".tmp-cpu-baseline")
    )
    root.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix="cpu-baseline-", dir=str(root)))


def _read_toml(path: Path) -> dict:
    import tomllib

    with open(path, "rb") as fh:
        return tomllib.load(fh)


def test_cargo_config_pins_x86_64_v2() -> None:
    cfg = REPO_ROOT / ".cargo" / "config.toml"
    if not cfg.is_file():
        fail_(".cargo/config.toml is missing: no in-repo x86_64 baseline pin")
        return
    try:
        data = _read_toml(cfg)
    except Exception as exc:  # noqa: BLE001 -- report, don't crash the suite
        fail_(f".cargo/config.toml does not parse: {exc}")
        return
    target = (data.get("target") or {}).get("x86_64-unknown-linux-gnu") or {}
    flags = target.get("rustflags") or []
    flat = " ".join(flags) if isinstance(flags, list) else str(flags)
    if f"target-cpu={BASELINE_CPU}" in flat:
        pass_(f".cargo/config.toml pins x86_64 to {BASELINE_CPU}")
    else:
        fail_(
            ".cargo/config.toml does not pin x86_64 to "
            f"{BASELINE_CPU} (rustflags={flat!r})"
        )
    # The pin must be x86-only: no other target may gain a target-cpu flag.
    for triple, section in (data.get("target") or {}).items():
        if triple == "x86_64-unknown-linux-gnu":
            continue
        other = " ".join(section.get("rustflags") or [])
        if "target-cpu" in other or "target_cpu" in other:
            fail_(f"non-x86 target {triple} carries a cpu flag: {other!r}")
            return
    pass_("no non-x86 target carries a cpu flag")


def test_release_workflows_pin_baseline() -> None:
    for workflow in ("release.yml", "native-builds.yml"):
        path = REPO_ROOT / ".github" / "workflows" / workflow
        if not path.is_file():
            fail_(f".github/workflows/{workflow} is missing")
            continue
        text = path.read_text()
        if f"target-cpu={BASELINE_CPU}" in text:
            pass_(f"{workflow} pins the x86_64 build to {BASELINE_CPU}")
        else:
            fail_(f"{workflow} does not pin the x86_64 build to {BASELINE_CPU}")
        bad = [tok for tok in HIGHER_BASELINE_TOKENS if tok in text]
        if bad:
            fail_(f"{workflow} still allows a higher baseline: {bad}")
        else:
            pass_(f"{workflow} allows no higher-baseline flag")


def test_no_higher_baseline_in_build_configs() -> None:
    roots = [
        REPO_ROOT / ".cargo",
        REPO_ROOT / ".github",
        REPO_ROOT / "scripts" / "package.sh",
        REPO_ROOT / "scripts" / "build.sh",
        REPO_ROOT / "scripts" / "daily-build.sh",
        REPO_ROOT / "Containerfile",
        REPO_ROOT / ".gitlab-ci.yml",
        REPO_ROOT / "install.sh",
    ]
    hits = []
    for root in roots:
        if not root.exists():
            continue
        files = [root] if root.is_file() else list(root.rglob("*"))
        for path in files:
            if not path.is_file():
                continue
            try:
                text = path.read_text(errors="strict")
            except (OSError, UnicodeDecodeError):
                continue
            for tok in HIGHER_BASELINE_TOKENS:
                if tok in text:
                    hits.append(f"{path.relative_to(REPO_ROOT)}: {tok}")
    if hits:
        fail_(f"higher-baseline flags remain: {hits}")
    else:
        pass_("no higher-baseline flag in any build config")


def test_runtime_guard_present() -> None:
    main_rs = REPO_ROOT / "crates" / "zoder-cli" / "src" / "main.rs"
    if not main_rs.is_file():
        fail_("crates/zoder-cli/src/main.rs is missing")
        return
    text = main_rs.read_text()
    if "unsupported CPU: missing" not in text:
        fail_("main.rs has no 'unsupported CPU: missing <flag>' message")
    else:
        pass_("main.rs prints 'unsupported CPU: missing <flag>'")
    if "is_x86_feature_detected" not in text:
        fail_("main.rs has no runtime CPUID check")
    else:
        pass_("main.rs checks CPU features at startup")
    # The guard must require the full v2 floor plus self-report any v3
    # assumption the binary was compiled with (a pin regression must fail
    # fast on the AVX-only Dell host, not sail through to SIGILL).
    for token in (
        '"sse4.2"',
        '"popcnt"',
        '"avx"',
        'cfg!(target_feature = "avx2")',
        'cfg!(target_feature = "bmi1")',
        'cfg!(target_feature = "bmi2")',
        'cfg!(target_feature = "fma")',
    ):
        if token not in text:
            fail_(f"CPU guard does not cover {token}")
            return
    pass_("CPU guard covers the v2 floor + compiled-in v3 assumptions")
    if 'target_arch = "x86_64"' not in text:
        fail_("main.rs CPU guard is not gated to x86_64")
    else:
        pass_("CPU guard is x86_64-gated (non-x86 targets untouched)")
    # The guard must run at process entry, before Clap parsing (so even
    # `zoder --version` on an old CPU reports cleanly instead of SIGILL).
    # NOTE: search the call site AFTER main's definition — the function's
    # own `fn missing_cpu_features()` definition precedes main().
    # NOTE 2: the guard requires the full v2 floor (sse4.2/popcnt/avx)
    # unconditionally, plus any v3 feature (avx2/bmi1/bmi2/fma) the binary
    # was compiled to assume — so all of these tokens must appear.
    main_fn = text.find("async fn main()")
    call = text.find("missing_cpu_features()", main_fn)
    parse = text.find("Cli::parse()", main_fn)
    if main_fn == -1 or call == -1:
        fail_("CPU guard is not wired into main()")
    elif parse != -1 and not (main_fn < call < parse):
        fail_("CPU guard does not run before CLI parsing in main()")
    else:
        pass_("CPU guard runs at process entry, before CLI parsing")


PROBE_RS = """\
fn main() {
    // Prints the compile-time-enabled v3 codegen features, one per line as
    // `<name>=<true|false>`. Built with the repo's own baseline flags, so
    // the output is what the shipped artifact's Rust codegen assumes about
    // the CPU. NOTE: this inspects the EMITTED TARGET FEATURES, not the
    // final binary's disassembly: a raw objdump mnemonic grep
    // false-positives on ring's runtime-CPUID-dispatched AVX2 crypto
    // assembly (shipped for fast CPUs, never executed on CPUs that lack
    // the feature), and on `tzcnt` (F3 0F BC), which pre-BMI1 CPUs execute
    // as the equivalent `bsf` — the encoding is backward compatible.
    let enabled: &[(&str, bool)] = &[
        ("avx2", cfg!(target_feature = "avx2")),
        ("bmi1", cfg!(target_feature = "bmi1")),
        ("bmi2", cfg!(target_feature = "bmi2")),
        ("fma", cfg!(target_feature = "fma")),
    ];
    for (name, on) in enabled {
        println!("{name}={on}");
    }
}
"""

# v3 codegen features the baseline must NOT enable (present on pegasus,
# absent on the Dell pool host).
FORBIDDEN_FEATURES = ("avx2", "bmi1", "bmi2", "fma")


def _repo_rustflags() -> list[str] | None:
    """The x86_64 rustflags from the repo pin, or None when absent."""
    cfg = REPO_ROOT / ".cargo" / "config.toml"
    if not cfg.is_file():
        return None
    try:
        data = _read_toml(cfg)
    except Exception:  # noqa: BLE001 -- missing/unparseable means no pin
        return None
    target = (data.get("target") or {}).get("x86_64-unknown-linux-gnu") or {}
    flags = target.get("rustflags") or []
    return list(flags) if flags else None


def test_baseline_artifact_has_no_avx2_bmi_fma() -> None:
    rustc = shutil.which("rustc") or Path.home() / ".cargo" / "bin" / "rustc"
    if rustc is None or not Path(str(rustc)).is_file():
        skip_("rustc not available: artifact-feature probe skipped")
        return
    rustc = str(rustc)
    flags = _repo_rustflags()
    if flags is None:
        fail_("no x86_64 rustflags pin to compile the probe with")
        return
    workdir = _scratch()
    try:
        src = workdir / "probe.rs"
        src.write_text(PROBE_RS)
        exe = workdir / "probe"
        # Compile for x86_64 with EXACTLY the repo's baseline flags (plus
        # --target when the host triple matches; on an x86_64 host the
        # default target is already x86_64).
        cmd = [rustc, "--edition", "2021"] + flags + [str(src), "-o", str(exe)]
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if proc.returncode != 0:
            fail_(f"probe did not compile with repo flags: {proc.stderr.strip()}")
            return
        proc = subprocess.run([str(exe)], capture_output=True, text=True, timeout=30)
        if proc.returncode != 0:
            fail_("baseline probe binary did not run")
            return
        enabled = {
            line.split("=")[0]: line.split("=")[1]
            for line in proc.stdout.splitlines()
            if "=" in line
        }
        bad = [f for f in FORBIDDEN_FEATURES if enabled.get(f) == "true"]
        if bad:
            fail_(f"repo baseline enables {bad}: old CPUs would SIGILL")
        else:
            pass_(
                "repo baseline enables no AVX2/BMI/FMA "
                f"({', '.join(f'{k}={v}' for k, v in sorted(enabled.items()))})"
            )
    except subprocess.TimeoutExpired:
        fail_("baseline probe timed out")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def main() -> int:
    test_cargo_config_pins_x86_64_v2()
    test_release_workflows_pin_baseline()
    test_no_higher_baseline_in_build_configs()
    test_runtime_guard_present()
    test_baseline_artifact_has_no_avx2_bmi_fma()
    print(f"\n{PASSES} passed; {FAILS} failed; {SKIPS} skipped")
    return 0 if FAILS == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
