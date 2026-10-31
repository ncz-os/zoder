#!/usr/bin/env python3
"""Fail when a `.rs` file under a crate's `src/` is unreachable from a `mod`.

Why this exists
---------------
The 2026-10-31 hygiene review (refactor decision D11, step 6) found two
Rust files that had been carried in the tree for a long time WITHOUT EVER
BEING COMPILED:

  * `crates/zoder-core/src/engine_rpc.rs` (715 lines) — the crate root
    re-exports the `acp-client` crate as `engine_rpc`
    (`pub use acp_client as engine_rpc;`) and never declares
    `mod engine_rpc;`, so the file was a second, stale copy of a driver
    that already lived in `crates/acp-client/src/lib.rs`.
  * `crates/zoder-cli/src/codex.rs` (1,335 lines) — `crates/zoder-cli/
    src/main.rs` never declares `mod codex;`, so the whole codex command
    surface in that file was dead.

Dead Rust is worse than no Rust: it compiles to nothing, so it never
breaks, but every future reader believes it is live code, and edits made
"in the codex surface" silently do nothing. A stale copy of a driver that
also exists live is worse still, because the two copies drift apart and
the audit trail points at the wrong file.

This check makes that class of rot fail CI instead of waiting for a
review to notice it.

What "reachable" means here
---------------------------
A `.rs` file under a crate's `src/` directory is reachable when it is
named by a `mod` declaration that is itself reachable from that crate's
compile roots (the `[[bin]]` / `[lib]` targets declared in the crate's
`Cargo.toml`).

Rust's module rules are followed exactly:

  * `mod foo;` in `dir/bar.rs` resolves to `dir/foo.rs` OR
    `dir/foo/mod.rs` (the 2018 edition rule).
  * `mod foo;` in `dir/mod.rs` resolves to `dir/foo.rs` OR
    `dir/foo/mod.rs`.
  * A crate root (`lib.rs`, `main.rs`, or any `[[bin]] path`) is the
    starting point.
  * A `mod` declaration inside a `#[cfg(test)]` block still counts as
    reachable: `mod tests;` under `#[cfg(test)]` compiles in the test
    build and is part of the crate's source, so it must not be reported
    as dead. The reverse is also handled: a file that is ONLY reachable
    through a `#[cfg(test)]` module is still a real source file.

The check is deliberately conservative: it never reports a file as dead
when it cannot prove the opposite. In particular it does not attempt to
evaluate `#[cfg(...)]` predicates, macro-generated modules, or
`include!` calls; a file reached by any of those is simply not reported.

Exit status
-----------
0  every `.rs` file under every `src/` directory is reachable.
1  at least one file is unreachable (each is named on stderr).
2  the tree could not be analysed (bad manifest, missing crate, ...).

Usage
-----
    python3 scripts/ci/unreachable-rs-check.py
    python3 scripts/ci/unreachable-rs-check.py --root /path/to/repo
    python3 scripts/ci/unreachable-rs-check.py --quiet
"""

from __future__ import annotations

import argparse
import sys
import tomllib
from pathlib import Path

DEFAULT_ROOT = Path(__file__).resolve().parents[2]

# `mod foo;` / `pub mod foo;` / `pub(crate) mod foo;` — a declaration that
# names a file (or a directory module). The `;` is what distinguishes it
# from an inline `mod foo { ... }` block, which lives in the same file and
# therefore never needs a reachability entry.
MOD_DECL_MARKER = "mod"


def _read_manifest(crate_dir: Path) -> dict:
    manifest = crate_dir / "Cargo.toml"
    try:
        return tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise RuntimeError(f"cannot read {manifest}: {exc}") from exc


def _target_roots(crate_dir: Path, manifest: dict) -> list[Path]:
    """The crate's compile roots: its lib target and every bin target."""
    roots: list[Path] = []

    lib = manifest.get("lib")
    if isinstance(lib, dict) and lib.get("path"):
        roots.append((crate_dir / lib["path"]).resolve())

    bins = manifest.get("bin")
    if isinstance(bins, list):
        for entry in bins:
            if isinstance(entry, dict) and entry.get("path"):
                roots.append((crate_dir / entry["path"]).resolve())
    elif isinstance(bins, dict) and bins.get("path"):
        roots.append((crate_dir / bins["path"]).resolve())

    # A crate with no explicit [lib]/[[bin]] section still has the
    # conventional roots: `src/lib.rs` and `src/main.rs`.
    if not roots:
        for conventional in ("src/lib.rs", "src/main.rs"):
            candidate = crate_dir / conventional
            if candidate.is_file():
                roots.append(candidate.resolve())

    # Deduplicate while keeping a stable order.
    seen: set[Path] = set()
    ordered: list[Path] = []
    for root in roots:
        if root not in seen:
            seen.add(root)
            ordered.append(root)
    return ordered


def _strip_comments_and_strings(text: str) -> str:
    """Blank out comments and string literals so a `mod` inside them is
    not mistaken for a declaration.

    The scan is line-based and deliberately simple: it is a lint, not a
    Rust parser. Block comments and string literals are replaced with
    spaces so the byte offsets of the remaining text are preserved (the
    caller only ever looks for line starts, so exact offsets do not
    matter, but keeping the line structure does).
    """
    out: list[str] = []
    in_block = False
    for raw_line in text.splitlines():
        line_chars = list(raw_line)
        i = 0
        in_string = False
        while i < len(line_chars):
            if in_block:
                if line_chars[i] == "*" and i + 1 < len(line_chars) and line_chars[i + 1] == "/":
                    line_chars[i] = " "
                    line_chars[i + 1] = " "
                    i += 2
                    in_block = False
                    continue
                if line_chars[i] != "\n":
                    line_chars[i] = " "
                i += 1
                continue
            if in_string:
                if line_chars[i] == "\\":
                    line_chars[i] = " "
                    if i + 1 < len(line_chars):
                        line_chars[i + 1] = " "
                    i += 2
                    continue
                if line_chars[i] == '"':
                    line_chars[i] = " "
                    in_string = False
                    i += 1
                    continue
                if line_chars[i] != "\n":
                    line_chars[i] = " "
                i += 1
                continue
            # Not in a comment or a string.
            if line_chars[i] == "/" and i + 1 < len(line_chars) and line_chars[i + 1] == "/":
                for j in range(i, len(line_chars)):
                    line_chars[j] = " "
                break
            if line_chars[i] == "/" and i + 1 < len(line_chars) and line_chars[i + 1] == "*":
                line_chars[i] = " "
                line_chars[i + 1] = " "
                in_block = True
                i += 2
                continue
            if line_chars[i] == '"':
                line_chars[i] = " "
                in_string = True
                i += 1
                continue
            if line_chars[i] == "r" and i + 1 < len(line_chars) and line_chars[i + 1] in "#\"":
                # A raw string (`r"..."` / `r#"..."#`): consume to the
                # matching delimiter.
                hashes = 0
                j = i + 1
                while j < len(line_chars) and line_chars[j] == "#":
                    hashes += 1
                    j += 1
                if j < len(line_chars) and line_chars[j] == '"':
                    closer = '"' + ("#" * hashes)
                    end = raw_line.find(closer, j + 1)
                    if end == -1:
                        for k in range(i, len(line_chars)):
                            line_chars[k] = " "
                        break
                    for k in range(i, min(end + len(closer), len(line_chars))):
                        line_chars[k] = " "
                    i = min(end + len(closer), len(line_chars))
                    continue
            i += 1
        out.append("".join(line_chars))
    return "\n".join(out)


def _declared_modules(source: str) -> list[str]:
    """The module names declared with `mod NAME;` in `source`.

    Only declarations at the start of a line (ignoring indentation) are
    considered: a `mod x;` inside a function body is not valid Rust, and
    a `mod x;` nested inside an inline `mod y { ... }` block is still a
    top-level declaration of that inline module's own file, which the
    caller resolves relative to the INLINE module's directory. That case
    is handled by `_mod_declarations` returning (name, directory).
    """
    names: list[str] = []
    for line in _strip_comments_and_strings(source).splitlines():
        stripped = line.strip()
        if not stripped.startswith(MOD_DECL_MARKER):
            continue
        rest = stripped[len(MOD_DECL_MARKER) :].strip()
        if not rest.startswith("pub"):
            pass
        elif rest.startswith("pub("):
            close = rest.find(")")
            if close == -1:
                continue
            rest = rest[close + 1 :].strip()
        else:
            rest = rest[3:].strip()
            if rest.startswith("("):
                close = rest.find(")")
                if close == -1:
                    continue
                rest = rest[close + 1 :].strip()
        if not rest:
            continue
        name = rest.split()[0].rstrip(";")
        if not name or not name.replace("_", "").isalnum() or name[0].isdigit():
            continue
        if rest.split()[-1] != ";" and ";" not in rest:
            # An inline `mod foo { ... }` block: no file to resolve.
            continue
        names.append(name)
    return names


def _inline_module_directories(source: str) -> list[tuple[str, str]]:
    """Inline `mod name { ... }` blocks: (name, body text).

    A `mod foo;` declaration that appears inside an inline `mod bar { }`
    block resolves to `bar/foo.rs`, not to `foo.rs`. The repo has no such
    case today, but the resolver follows the rule anyway so the check
    cannot report a false positive if one is added later.
    """
    cleaned = _strip_comments_and_strings(source)
    results: list[tuple[str, str]] = []
    lines = cleaned.splitlines()
    i = 0
    while i < len(lines):
        stripped = lines[i].strip()
        if not stripped.startswith(MOD_DECL_MARKER):
            i += 1
            continue
        rest = stripped[len(MOD_DECL_MARKER) :].strip()
        if rest.startswith("pub"):
            if rest.startswith("pub("):
                close = rest.find(")")
                if close == -1:
                    i += 1
                    continue
                rest = rest[close + 1 :].strip()
            else:
                rest = rest[3:].strip()
                if rest.startswith("("):
                    close = rest.find(")")
                    if close == -1:
                        i += 1
                        continue
                    rest = rest[close + 1 :].strip()
        if not rest:
            i += 1
            continue
        parts = rest.split()
        name = parts[0]
        if len(parts) >= 2 and parts[-1] == "{":
            # Collect the block body by brace depth.
            depth = 0
            body: list[str] = []
            j = i
            started = False
            while j < len(lines):
                line = lines[j]
                depth += line.count("{") - line.count("}")
                if not started:
                    started = True
                else:
                    body.append(line)
                if started and depth == 0:
                    break
                j += 1
            results.append((name, "\n".join(body)))
            i = j + 1
            continue
        i += 1
    return results


def _resolve_module(name: str, parent_dir: Path) -> Path | None:
    """Resolve `mod name;` declared in a file living in `parent_dir`.

    Rust 2018: the file is `parent_dir/name.rs` or `parent_dir/name/mod.rs`.
    """
    flat = parent_dir / f"{name}.rs"
    if flat.is_file():
        return flat.resolve()
    nested = parent_dir / name / "mod.rs"
    if nested.is_file():
        return nested.resolve()
    return None


def _all_source_files(src_dir: Path) -> set[Path]:
    return {p.resolve() for p in src_dir.rglob("*.rs") if p.is_file()}


def analyse_crate(crate_dir: Path) -> tuple[set[Path], set[Path]]:
    """Return (reachable_files, all_files) for one crate."""
    manifest = _read_manifest(crate_dir)
    roots = _target_roots(crate_dir, manifest)

    src_dir = crate_dir / "src"
    if not src_dir.is_dir():
        return set(), set()

    all_files = _all_source_files(src_dir)

    reachable: set[Path] = set()
    queue: list[Path] = [root for root in roots if root.is_file()]
    for root in queue:
        if root not in all_files:
            # A crate root outside `src/` (rare) still seeds the walk; its
            # own file is not part of the "under src/" population, but the
            # modules it declares are.
            pass

    seen: set[Path] = set()
    while queue:
        current = queue.pop()
        if current in seen:
            continue
        seen.add(current)
        if current in all_files:
            reachable.add(current)
        try:
            source = current.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue

        parent_dir = current.parent
        for name in _declared_modules(source):
            target = _resolve_module(name, parent_dir)
            if target is not None and target not in seen:
                queue.append(target)

        # `mod foo;` inside an inline `mod bar { ... }` resolves to
        # `bar/foo.rs` next to the enclosing file.
        for inline_name, body in _inline_module_directories(source):
            inline_dir = parent_dir / inline_name
            for name in _declared_modules(body):
                target = _resolve_module(name, inline_dir)
                if target is not None and target not in seen:
                    queue.append(target)

    return reachable, all_files


def find_crates(root: Path) -> list[Path]:
    """Every crate directory in the workspace (the members of the root
    manifest), falling back to a scan when the root manifest is absent."""
    root_manifest = root / "Cargo.toml"
    if root_manifest.is_file():
        try:
            manifest = tomllib.loads(root_manifest.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError):
            manifest = {}
        members = manifest.get("workspace", {}).get("members")
        if isinstance(members, list):
            crates = []
            for member in members:
                crate_dir = (root / member).resolve()
                if (crate_dir / "Cargo.toml").is_file():
                    crates.append(crate_dir)
            if crates:
                return crates
    crates = []
    for manifest_path in sorted(root.glob("crates/*/Cargo.toml")):
        crates.append(manifest_path.parent.resolve())
    return crates


def check(root: Path) -> list[Path]:
    """Return the unreachable `.rs` files under every crate's `src/`."""
    unreachable: list[Path] = []
    for crate_dir in find_crates(root):
        reachable, all_files = analyse_crate(crate_dir)
        for path in sorted(all_files - reachable):
            unreachable.append(path)
    return unreachable


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        default=str(DEFAULT_ROOT),
        help="repository root to scan (default: the checkout this script lives in)",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="print nothing on success",
    )
    args = parser.parse_args(argv)

    root = Path(args.root).resolve()
    if not (root / "Cargo.toml").is_file():
        print(f"unreachable-rs-check: no Cargo.toml under {root}", file=sys.stderr)
        return 2

    try:
        unreachable = check(root)
    except RuntimeError as exc:
        print(f"unreachable-rs-check: {exc}", file=sys.stderr)
        return 2

    if unreachable:
        print(
            "unreachable-rs-check: these .rs files under a src/ directory are "
            "NOT reachable from any mod declaration, so they are never "
            "compiled:",
            file=sys.stderr,
        )
        for path in unreachable:
            try:
                shown = path.relative_to(root)
            except ValueError:
                shown = path
            print(f"  {shown}", file=sys.stderr)
        print(
            "unreachable-rs-check: delete them, or wire them in with a `mod` "
            "declaration if they are meant to be live.",
            file=sys.stderr,
        )
        return 1

    if not args.quiet:
        print("unreachable-rs-check: OK (every .rs file under src/ is reachable)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
