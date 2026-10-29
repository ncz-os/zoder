"""Guard for scripts/audit_nonrust_addresses.py.

test_tree_is_within_baseline is the guard proper: it runs --check over the
real tree and fails when any tracked non-Rust file gains a LAN address
beyond scripts/nonrust_address_baseline.json. The remaining tests prove the
guard can fire, using scratch copies; no production file is touched and no
Rust source is ever scanned.

Addresses in this file are assembled at runtime so the test itself adds no
literal LAN address to the audited tree.
"""

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
AUDIT = HERE / "audit_nonrust_addresses.py"
BASELINE = HERE / "nonrust_address_baseline.json"
REPO = HERE.parent

LAN = ".".join(["192", "168", "7", "9"])
LAN_10 = ".".join(["10", "9", "8", "7"])
LAN_172 = ".".join(["172", "20", "1", "1"])


def run(*args):
    return subprocess.run(
        [sys.executable, str(AUDIT), *args], capture_output=True, text=True
    )


def scratch_root(files, baseline):
    root = Path(tempfile.mkdtemp(prefix="addr-audit-"))
    for name, text in files.items():
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text(text)
    base = root / "baseline.json"
    base.write_text(json.dumps({"files": baseline}))
    return root, base


def check(root, base):
    return run("--check", "--root", str(root), "--baseline", str(base))


def test_tree_is_within_baseline():
    result = run("--check")
    assert result.returncode == 0, result.stdout + result.stderr


def test_baseline_covers_a_real_tree():
    files = json.loads(BASELINE.read_text())["files"]
    assert files, "baseline must record the existing occurrences"
    assert all(isinstance(n, int) and n > 0 for n in files.values())
    assert all((REPO / name).is_file() for name in files)


def test_rust_sources_are_never_scanned():
    root, base = scratch_root({"x.rs": 'a="192.168.7.9"\n'}, {})
    try:
        out = run("--root", str(root))
        assert out.returncode == 0
        assert "x.rs" not in out.stdout
        assert check(root, base).returncode == 0
    finally:
        shutil.rmtree(root)


def test_guard_fires_when_a_file_gains_an_address():
    clean = "install.sh"
    baseline = json.loads(BASELINE.read_text())["files"]
    assert clean not in baseline
    original = (REPO / clean).read_text()
    root, base = scratch_root({clean: original}, {})
    try:
        assert check(root, base).returncode == 0
        with (root / clean).open("a") as fh:
            fh.write(f'\nHOST="{LAN}"\n')
        result = check(root, base)
        assert result.returncode == 1
        assert clean in result.stdout
        assert LAN not in result.stdout + result.stderr
    finally:
        shutil.rmtree(root)


def test_guard_fires_when_a_baselined_file_gains_one_more():
    name = "tool.sh"
    root, base = scratch_root({name: f"curl http://{LAN}/x\n"}, {name: 1})
    try:
        assert check(root, base).returncode == 0
        with (root / name).open("a") as fh:
            fh.write(f"ping {LAN_10}\n")
        assert check(root, base).returncode == 1
    finally:
        shutil.rmtree(root)


def test_removing_addresses_never_fails():
    name = "tool.sh"
    root, base = scratch_root({name: "echo ok\n"}, {name: 3})
    try:
        assert check(root, base).returncode == 0
    finally:
        shutil.rmtree(root)


def test_each_private_range_is_detected_and_public_ones_are_not():
    text = f"a={LAN}\nb={LAN_10}\nc={LAN_172}\n"
    public = "d=8.8.8.8\ne=172.32.0.1\nf=172.15.0.1\ng=1.10.1.1.1\nh=192.169.1.1\n"
    root, base = scratch_root({"a.toml": text, "b.json": public}, {})
    try:
        out = run("--root", str(root))
        assert out.returncode == 0
        assert "a.toml" in out.stdout and "b.json" not in out.stdout
        assert out.stdout.splitlines()[0].split()[0] == "3"
    finally:
        shutil.rmtree(root)


def test_markdown_and_binary_files_are_handled():
    root, base = scratch_root({"notes.md": "clean docs\n"}, {})
    (root / "blob.json").write_bytes(b"\0\1\2")
    try:
        assert check(root, base).returncode == 0
    finally:
        shutil.rmtree(root)


def test_shebang_scripts_without_suffix_are_scanned():
    root, base = scratch_root({"tool": f"#!/usr/bin/env python3\nH='{LAN}'\n"}, {})
    try:
        assert check(root, base).returncode == 1
    finally:
        shutil.rmtree(root)


def test_write_baseline_round_trips():
    root, base = scratch_root({"x.py": f"A='{LAN}'\nB='{LAN_10}'\n"}, {})
    try:
        assert (
            run(
                "--write-baseline", "--root", str(root), "--baseline", str(base)
            ).returncode
            == 0
        )
        assert json.loads(base.read_text())["files"] == {"x.py": 2}
        assert check(root, base).returncode == 0
    finally:
        shutil.rmtree(root)


def test_missing_baseline_is_an_error_not_a_pass():
    root, base = scratch_root({"x.py": "pass\n"}, {})
    base.unlink()
    try:
        assert check(root, base).returncode == 2
    finally:
        shutil.rmtree(root)
