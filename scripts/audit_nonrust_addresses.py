#!/usr/bin/env python3
"""Audit zoder non-Rust files for hardcoded private (LAN) IPv4 addresses.

Scans git-tracked docs, scripts, config templates and TOML/JSON/YAML/shell
(plus Markdown/text/templates and extensionless shebang scripts). Rust
sources (``*.rs``) are never scanned. Prints counts per file.

    audit_nonrust_addresses.py                   print counts per file
    audit_nonrust_addresses.py --check           exit 1 if any file has more
                                                 occurrences than the baseline
    audit_nonrust_addresses.py --write-baseline  record the current counts

Baseline (``scripts/nonrust_address_baseline.json``) maps path -> count. It
is a ratchet: existing occurrences are tolerated, new ones are not.

Stdlib only. Address values are never printed, only paths, line numbers and
counts.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve()
DEFAULT_ROOT = SCRIPT.parent.parent
DEFAULT_BASELINE = SCRIPT.with_name("nonrust_address_baseline.json")

SCAN_SUFFIXES = {
    ".py",
    ".sh",
    ".bash",
    ".toml",
    ".json",
    ".jsonc",
    ".yaml",
    ".yml",
    ".md",
    ".txt",
    ".example",
    ".template",
}
SHEBANG = re.compile(
    rb"^#!\s*\S*\b(?:python[0-9.]*|bash|sh|zsh)\b|^#!\s*/usr/bin/env\s+(?:python[0-9.]*|bash|sh|zsh)\b"
)
MAX_BYTES = 5_000_000

_OCTET = r"(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])"
LAN_ADDRESS = re.compile(
    r"(?<![0-9.])"
    rf"(?:192\.168\.{_OCTET}\.{_OCTET}"
    rf"|10\.{_OCTET}\.{_OCTET}\.{_OCTET}"
    rf"|172\.(?:1[6-9]|2[0-9]|3[01])\.{_OCTET}\.{_OCTET})"
    r"(?![0-9])(?!\.[0-9])"
)


def _git_toplevel(root: Path) -> Path | None:
    try:
        out = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
            check=True,
            capture_output=True,
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    return Path(out.decode().strip()).resolve()


def list_files(root: Path) -> list[str]:
    if _git_toplevel(root) == root:
        out = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z"],
            check=True,
            capture_output=True,
        ).stdout
        return sorted(
            p for p in out.decode("utf-8", "surrogateescape").split("\0") if p
        )
    return sorted(
        str(p.relative_to(root))
        for p in root.rglob("*")
        if p.is_file() and ".git" not in p.relative_to(root).parts
    )


def read_scannable(path: Path) -> str | None:
    if path.is_symlink() or not path.is_file():
        return None
    if path.suffix == ".rs":
        return None
    try:
        if path.stat().st_size > MAX_BYTES:
            return None
        data = path.read_bytes()
    except OSError:
        return None
    if path.suffix.lower() not in SCAN_SUFFIXES:
        if path.suffix or not SHEBANG.match(data[:200]):
            return None
    if b"\0" in data[:8192]:
        return None
    return data.decode("utf-8", "replace")


def scan(root: Path) -> dict[str, list[int]]:
    found: dict[str, list[int]] = {}
    for rel in list_files(root):
        if rel.endswith(".rs"):
            continue
        text = read_scannable(root / rel)
        if text is None:
            continue
        lines = [
            n
            for n, line in enumerate(text.splitlines(), 1)
            for _ in LAN_ADDRESS.finditer(line)
        ]
        if lines:
            found[rel] = lines
    return found


def load_baseline(path: Path) -> dict[str, int]:
    with path.open(encoding="utf-8") as fh:
        data = json.load(fh)
    return {str(k): int(v) for k, v in data["files"].items()}


def write_baseline(path: Path, found: dict[str, list[int]]) -> None:
    payload = {
        "description": (
            "Existing LAN IPv4 address occurrences per non-Rust file. "
            "audit_nonrust_addresses.py --check fails when a file exceeds its count."
        ),
        "files": {k: len(v) for k, v in sorted(found.items())},
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write-baseline", action="store_true")
    ap.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    ap.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    args = ap.parse_args(argv)

    root = args.root.resolve()
    found = scan(root)

    if args.write_baseline:
        write_baseline(args.baseline, found)
        print(
            f"wrote {args.baseline}: {len(found)} files, {sum(map(len, found.values()))} occurrences"
        )
        return 0

    if args.check:
        try:
            baseline = load_baseline(args.baseline)
        except (OSError, ValueError, KeyError) as exc:
            print(f"cannot read baseline {args.baseline}: {exc}", file=sys.stderr)
            return 2
        over = {f: v for f, v in found.items() if len(v) > baseline.get(f, 0)}
        if over:
            print(
                "FAIL: LAN addresses added beyond the baseline (use environment configuration):"
            )
            for f, lines in sorted(over.items()):
                print(
                    f"  {f}: {len(lines)} > baseline {baseline.get(f, 0)} (lines {', '.join(map(str, lines))})"
                )
            return 1
        print(
            f"OK: {len(found)} files, {sum(map(len, found.values()))} occurrences, none over baseline"
        )
        return 0

    for f, lines in sorted(found.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        print(f"{len(lines):5d}  {f}")
    print(f"{sum(map(len, found.values())):5d}  TOTAL in {len(found)} files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
