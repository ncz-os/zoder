#!/usr/bin/env python3
"""Behavioural tests for scripts/ci/unreachable-rs-check.py.

The check guards a real rot class found by the 2026-10-31 hygiene review
(refactor decision D11, step 6): a `.rs` file under a crate's `src/`
directory that no `mod` declaration reaches. Such a file never compiles,
so it never breaks a build — but every reader believes it is live code,
and a stale copy of a driver that ALSO exists live (the
`zoder-core/src/engine_rpc.rs` case) drifts away from the real one
without anyone noticing.

These tests are OFFLINE and use disposable fixture trees, so they never
depend on the repo's own layout. The last test runs the check against
the REAL repository, which is the regression that matters: it fails on
the base commit (where `engine_rpc.rs` and `codex.rs` are present) and
passes once the dead files are gone.

Run: python3 scripts/tests/unreachable-rs-tests.py
"""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CHECK_SCRIPT = REPO_ROOT / "scripts" / "ci" / "unreachable-rs-check.py"

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


def _run_one(func) -> None:
    """Run one test; an exception is a FAIL, not a crash of the whole suite."""
    try:
        func()
    except Exception as e:  # noqa: BLE001 — a test failure must not stop the suite
        fail_(f"{func.__name__} raised {type(e).__name__}: {e}")


def _load(path: Path, name: str):
    """Import a script with a dashed file name as a module."""
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _scratch() -> Path:
    """An empty scratch directory for one test.

    The scratch directory lives under the repository (never a tmpfs):
    the task rules forbid tmpfs, and the repo's `.gitignore` keeps the
    scratch tree out of the way.
    """
    base = Path(os.environ.get("UNREACHABLE_RS_TEST_DIR") or (REPO_ROOT / ".tmp"))
    base.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix="unreachable-rs-", dir=str(base)))


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _workspace(crates: dict[str, str]) -> Path:
    """Build a one-crate workspace with the given `src/` files.

    `crates` maps a crate name to a dict of relative source paths -> file
    body. The crate root (`src/lib.rs`) and the manifest are generated
    automatically.
    """
    root = _scratch()
    manifest = [
        "[workspace]",
        'resolver = "2"',
        "members = [",
    ]
    for crate in crates:
        manifest.append(f'    "crates/{crate}",')
    manifest.append("]")
    _write(root / "Cargo.toml", "\n".join(manifest) + "\n")

    for crate, files in crates.items():
        crate_dir = root / "crates" / crate
        _write(
            crate_dir / "Cargo.toml",
            f'[package]\nname = "{crate}"\nversion = "0.1.0"\nedition = "2021"\n',
        )
        for rel, body in files.items():
            _write(crate_dir / "src" / rel, body)
    return root


def _check(root: Path) -> tuple[int, str, str]:
    """Run the check script against `root`; return (status, stdout, stderr)."""
    proc = subprocess.run(
        [sys.executable, str(CHECK_SCRIPT), "--root", str(root)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    return proc.returncode, proc.stdout, proc.stderr


# ---------------------------------------------------------------------------
# tests
# ---------------------------------------------------------------------------
def test_a_reachable_file_is_not_reported() -> None:
    root = _workspace(
        {
            "demo": {
                "lib.rs": "pub mod alpha;\npub mod beta;\n",
                "alpha.rs": "pub fn a() {}\n",
                "beta.rs": "pub fn b() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"reachable tree must pass, got {status}: {err}"


def test_an_unreachable_file_fails_the_check() -> None:
    root = _workspace(
        {
            "demo": {
                "lib.rs": "pub mod alpha;\n",
                "alpha.rs": "pub fn a() {}\n",
                "orphan.rs": "pub fn orphan() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 1, f"an unreachable file must fail the check, got {status}"
    assert "orphan.rs" in err, f"the orphan must be named on stderr: {err}"
    assert "alpha.rs" not in err, f"a reachable file must not be named: {err}"


def test_a_nested_directory_module_is_reachable() -> None:
    """`mod gamma;` next to `gamma/mod.rs` is the 2018-edition rule."""
    root = _workspace(
        {
            "demo": {
                "lib.rs": "pub mod gamma;\n",
                "gamma/mod.rs": "pub mod inner;\n",
                "gamma/inner.rs": "pub fn inner() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"a directory module must be reachable, got {status}: {err}"


def test_a_nested_flat_module_is_reachable() -> None:
    """`mod gamma;` next to `gamma.rs` is the 2018-edition rule."""
    root = _workspace(
        {
            "demo": {
                "lib.rs": "pub mod gamma;\n",
                "gamma.rs": "pub fn gamma() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"a flat module must be reachable, got {status}: {err}"


def test_a_cfg_test_module_is_reachable() -> None:
    """`#[cfg(test)] mod tests;` still compiles in the test build."""
    root = _workspace(
        {
            "demo": {
                "lib.rs": "#[cfg(test)]\nmod tests;\n",
                "tests.rs": "#[test]\nfn t() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"a cfg(test) module must be reachable, got {status}: {err}"


def test_a_private_module_declaration_is_reachable() -> None:
    root = _workspace(
        {
            "demo": {
                "lib.rs": "mod alpha;\n",
                "alpha.rs": "pub fn a() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"a private mod must be reachable, got {status}: {err}"


def test_a_pub_crate_module_declaration_is_reachable() -> None:
    root = _workspace(
        {
            "demo": {
                "lib.rs": "pub(crate) mod alpha;\n",
                "alpha.rs": "pub fn a() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"a pub(crate) mod must be reachable, got {status}: {err}"


def test_a_binary_crate_root_seeds_the_walk() -> None:
    """A crate whose only root is `src/main.rs` still reaches its modules."""
    root = _workspace(
        {
            "demo": {
                "main.rs": "mod alpha;\nfn main() {}\n",
                "alpha.rs": "pub fn a() {}\n",
            }
        }
    )
    # The fixture manifest has no [lib] and no [[bin]]; the conventional
    # roots are used.
    status, _out, err = _check(root)
    assert status == 0, f"a bin crate root must be reachable, got {status}: {err}"


def test_a_mod_named_only_in_a_comment_is_not_reachable() -> None:
    """A `mod x;` inside a comment must not make `x.rs` look reachable."""
    root = _workspace(
        {
            "demo": {
                "lib.rs": "// mod alpha;\npub fn nothing() {}\n",
                "alpha.rs": "pub fn a() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 1, f"a commented-out mod must not count, got {status}"
    assert "alpha.rs" in err, f"the orphan must be named: {err}"


def test_a_mod_named_only_in_a_string_is_not_reachable() -> None:
    root = _workspace(
        {
            "demo": {
                "lib.rs": 'pub const NOTE: &str = "mod alpha;";\n',
                "alpha.rs": "pub fn a() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 1, f"a mod inside a string must not count, got {status}"
    assert "alpha.rs" in err, f"the orphan must be named: {err}"


def test_an_inline_mod_block_does_not_need_a_file() -> None:
    root = _workspace(
        {
            "demo": {
                "lib.rs": "mod inline {\n    pub fn f() {}\n}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"an inline mod must not be reported, got {status}: {err}"


def test_a_mod_inside_an_inline_block_resolves_next_to_the_inline_module() -> None:
    """`mod bar { mod leaf; }` resolves `leaf` to `bar/leaf.rs`."""
    root = _workspace(
        {
            "demo": {
                "lib.rs": "mod bar {\n    mod leaf;\n}\n",
                "bar/leaf.rs": "pub fn leaf() {}\n",
            }
        }
    )
    status, _out, err = _check(root)
    assert status == 0, f"a nested inline module must be reachable, got {status}: {err}"


def test_an_explicit_bin_target_root_is_used() -> None:
    """A `[[bin]] path` is a compile root even when it is not `src/main.rs`."""
    root = _workspace(
        {
            "demo": {
                "cli.rs": "mod alpha;\nfn main() {}\n",
                "alpha.rs": "pub fn a() {}\n",
            }
        }
    )
    manifest = root / "crates" / "demo" / "Cargo.toml"
    manifest.write_text(
        '[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2021"\n'
        '\n[[bin]]\nname = "demo"\npath = "src/cli.rs"\n',
        encoding="utf-8",
    )
    status, _out, err = _check(root)
    assert status == 0, f"an explicit bin root must be reachable, got {status}: {err}"


def test_a_missing_manifest_is_an_analysis_error() -> None:
    root = _scratch()
    status, _out, err = _check(root)
    assert status == 2, f"a tree with no Cargo.toml must exit 2, got {status}"
    assert "Cargo.toml" in err, f"the error must name the manifest: {err}"


def test_the_real_repository_has_no_unreachable_rust_file() -> None:
    """THE regression: the repo itself must have no unreachable `.rs` file.

    On the base commit this FAILS, naming `crates/zoder-core/src/
    engine_rpc.rs` and `crates/zoder-cli/src/codex.rs` — the two never-
    compiled files the 2026-10-31 hygiene review found.
    """
    status, _out, err = _check(REPO_ROOT)
    assert status == 0, (
        "the repository contains .rs files that no mod declaration reaches "
        f"(exit {status}):\n{err}"
    )


def main() -> int:
    _run_one(test_a_reachable_file_is_not_reported)
    _run_one(test_an_unreachable_file_fails_the_check)
    _run_one(test_a_nested_directory_module_is_reachable)
    _run_one(test_a_nested_flat_module_is_reachable)
    _run_one(test_a_cfg_test_module_is_reachable)
    _run_one(test_a_private_module_declaration_is_reachable)
    _run_one(test_a_pub_crate_module_declaration_is_reachable)
    _run_one(test_a_binary_crate_root_seeds_the_walk)
    _run_one(test_a_mod_named_only_in_a_comment_is_not_reachable)
    _run_one(test_a_mod_named_only_in_a_string_is_not_reachable)
    _run_one(test_an_inline_mod_block_does_not_need_a_file)
    _run_one(test_a_mod_inside_an_inline_block_resolves_next_to_the_inline_module)
    _run_one(test_an_explicit_bin_target_root_is_used)
    _run_one(test_a_missing_manifest_is_an_analysis_error)
    _run_one(test_the_real_repository_has_no_unreachable_rust_file)
    print(f"\n{PASSES} passed; {FAILS} failed")
    return 0 if FAILS == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
