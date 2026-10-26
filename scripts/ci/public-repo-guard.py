#!/usr/bin/env python3
"""Fail when an internal identifier appears in the public tree or on a branch.

The committed denylist stores only salted SHA-256 digests. It supports four
rule kinds:

  token   one normalised word
  bigram  two adjacent normalised words (for dotted host names)
  prefix  an IPv4-like address prefix
  host    a DNS host, or one of its labels/suffixes seen in a URL or an
          ssh/scp destination

Usage
-----
    # Scan the working tree and every commit message unique to this branch.
    python3 scripts/ci/public-repo-guard.py --base origin/main

    # Prove that a known-bad sample is detected (the test term is synthetic and
    # is never present in the tree as a contiguous string).
    python3 scripts/ci/public-repo-guard.py --self-test

Adding a term
-------------
Run one of the helpers below from the repo root and append its output to
`scripts/ci/public-repo-denylist.txt`:

    python3 scripts/ci/public-repo-guard.py --hash 'provider-name'
    python3 scripts/ci/public-repo-guard.py --hash-prefix '198.51.100'
    python3 scripts/ci/public-repo-guard.py --hash-host 'internal.example.invalid'

`--hash` prints a token digest for each word and a bigram digest for each
adjacent pair, so a dotted host name is covered by both forms. Plain common
words (for example a model-family prefix) should not be added as tokens: add
them with `--hash-host` instead, which only matches a URL/ssh host.

The scan prints the rule kind and location, never the matched text.
"""

from __future__ import annotations

import argparse
import hashlib
import re
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

SALT = b"zoder-public-tree-guard-v1-3f6d2c8a"
DENYLIST = Path(__file__).resolve().parent / "public-repo-denylist.txt"
TOKEN_RE = re.compile(r"[a-z0-9]+")
IPV4_RE = re.compile(r"(?:\d{1,3}\.){1,3}\d{1,3}")
URL_HOST_RE = re.compile(r"[A-Za-z][A-Za-z0-9+.-]*://([^\s/?#]+)")
SCP_HOST_RE = re.compile(r"(?<![\w.-])[\w.-]+@([A-Za-z0-9][A-Za-z0-9.-]*):")
SSH_CMD_RE = re.compile(r"(?:^|[\s;&|()])ssh(?=\s|$)")
# ssh short options that consume the following argument.
SSH_VALUE_OPTIONS = set("BbcDEeFIiJLlmOopQRSWw")
RULE_KINDS = ("token", "bigram", "prefix", "host")


def _digest(kind: str, value: str) -> str:
    payload = SALT + b"\x00" + kind.encode("ascii") + b"\x00" + value.encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def token_digest(value: str) -> str:
    return _digest("token", value)


def bigram_digest(first: str, second: str) -> str:
    return _digest("bigram", f"{first} {second}")


def prefix_digest(value: str) -> str:
    return _digest("prefix", value)


def host_digest(value: str) -> str:
    return _digest("host", value)


def normal_tokens(text: str) -> list[str]:
    return TOKEN_RE.findall(text.lower())


def host_candidates(host: str) -> list[str]:
    """A host, each of its labels, and each dotted suffix of its labels."""
    value = host.strip().strip(".").lower()
    if not value:
        return []
    if value.startswith("["):
        return [value]
    labels = [label for label in value.split(".") if label]
    if not labels:
        return []
    out = {".".join(labels)}
    out.update(labels)
    out.update(".".join(labels[i:]) for i in range(1, len(labels)))
    return sorted(out)


def _authority_host(authority: str) -> str:
    value = authority.rsplit("@", 1)[-1]
    if value.startswith("["):
        return value.split("]", 1)[0] + "]"
    return value.split(":", 1)[0]


def _shell_tokens(text: str) -> list[str]:
    try:
        return shlex.split(text, posix=True)
    except ValueError:
        return text.split()


def _ssh_destination(tokens: list[str]) -> str | None:
    """The first non-option token after `ssh`, honouring options' arguments."""
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token == "--":
            index += 1
            break
        if token.startswith("-") and token != "-":
            if len(token) == 2 and token[1] in SSH_VALUE_OPTIONS:
                index += 2
            else:
                index += 1
            continue
        break
    if index >= len(tokens):
        return None
    return tokens[index]


def ssh_host_matches(text: str) -> list[tuple[str, int]]:
    """Destination hosts of `ssh` commands, with each destination's offset."""
    found: list[tuple[str, int]] = []
    offset = 0
    for line in text.splitlines(keepends=True):
        for command in SSH_CMD_RE.finditer(line):
            destination = _ssh_destination(_shell_tokens(line[command.end() :]))
            if not destination:
                continue
            position = line.find(destination, command.end())
            if position < 0:
                position = command.end()
            found.append((_authority_host(destination), offset + position))
        offset += len(line)
    return found


def host_matches(text: str) -> list[tuple[str, int]]:
    """Every host mention with the offset of the host itself, in text order."""
    found: list[tuple[str, int]] = []
    for match in URL_HOST_RE.finditer(text):
        found.append((_authority_host(match.group(1)), match.start(1)))
    for match in SCP_HOST_RE.finditer(text):
        found.append((match.group(1), match.start(1)))
    found.extend(ssh_host_matches(text))
    return found


def load_denylist(path: Path = DENYLIST) -> dict[str, set[str]]:
    rules: dict[str, set[str]] = {kind: set() for kind in RULE_KINDS}
    if not path.is_file():
        raise SystemExit(f"public-repo-guard: missing denylist at {path}")
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) != 2 or parts[0] not in rules:
            raise SystemExit(f"public-repo-guard: bad denylist line {lineno}")
        kind, digest = parts
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise SystemExit(f"public-repo-guard: bad digest on denylist line {lineno}")
        rules[kind].add(digest)
    return rules


def _line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def scan_text(
    text: str, rules: dict[str, set[str]], where: str
) -> set[tuple[str, int, str]]:
    """Return (where, line, kind) hits for one blob of text."""
    lowered = text.lower()
    hits: set[tuple[str, int, str]] = set()
    matches = list(TOKEN_RE.finditer(lowered))
    for index, match in enumerate(matches):
        token = match.group(0)
        line = _line_of(text, match.start())
        if token_digest(token) in rules["token"]:
            hits.add((where, line, "token"))
        if index + 1 < len(matches):
            pair = f"{token} {matches[index + 1].group(0)}"
            if bigram_digest(*pair.split()) in rules["bigram"]:
                hits.add((where, line, "bigram"))

    for match in IPV4_RE.finditer(lowered):
        candidate = match.group(0)
        line = _line_of(text, match.start())
        for end in range(1, len(candidate) + 1):
            if prefix_digest(candidate[:end]) in rules["prefix"]:
                hits.add((where, line, "prefix"))
                break

    if rules["host"]:
        for host, offset in host_matches(text):
            line = _line_of(text, offset)
            if any(
                host_digest(candidate) in rules["host"]
                for candidate in host_candidates(host)
            ):
                hits.add((where, line, "host"))
    return hits


def tracked_files(root: Path) -> list[str]:
    output = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    return [name for name in output.decode("utf-8").split("\0") if name]


def commit_messages(root: Path, base: str | None) -> list[tuple[str, str]]:
    revision = f"{base}..HEAD" if base else "HEAD"
    result = subprocess.run(
        ["git", "-C", str(root), "log", "--format=%H%x1f%B%x1e", revision],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise SystemExit(f"public-repo-guard: cannot read {revision}: {detail}")
    commits: list[tuple[str, str]] = []
    for record in result.stdout.decode("utf-8", "replace").split("\x1e"):
        if not record.strip():
            continue
        sha, _, body = record.strip().partition("\x1f")
        commits.append((sha, body))
    return commits


def scan_tree(root: Path, rules: dict[str, set[str]]) -> set[tuple[str, int, str]]:
    hits: set[tuple[str, int, str]] = set()
    files = tracked_files(root)
    for relative in files:
        path = root / relative
        try:
            data = path.read_bytes()
        except OSError:
            continue
        if b"\x00" in data[:8192]:
            continue
        hits |= scan_text(data.decode("utf-8", "replace"), rules, relative)
    return hits


def scan_branch(
    root: Path, rules: dict[str, set[str]], base: str | None
) -> set[tuple[str, int, str]]:
    hits: set[tuple[str, int, str]] = set()
    for sha, body in commit_messages(root, base):
        hits |= scan_text(body, rules, f"commit {sha}")
    return hits


def self_test(rules: dict[str, set[str]]) -> int:
    # Assembled at runtime so the synthetic token is never a contiguous string
    # in this file (otherwise the guard would flag itself).
    term = "guard" + "self" + "check" + "9f3c" + "4a"
    if token_digest(term) not in rules["token"]:
        raise SystemExit(
            "public-repo-guard self-test: denylist is missing the self-test "
            "token digest; regenerate it with --hash on the assembled term"
        )
    if host_digest(term) not in rules["host"]:
        raise SystemExit(
            "public-repo-guard self-test: denylist is missing the self-test "
            "host digest; regenerate it with --hash-host on the assembled term"
        )
    if scan_text("ordinary source text", rules, "self-test"):
        raise SystemExit("public-repo-guard self-test: clean text was flagged")
    if not scan_text(f"pre {term} post", rules, "self-test"):
        raise SystemExit("public-repo-guard self-test: tree text was not flagged")

    two_lines = (
        f"first https://{term}.example.invalid/a\n"
        f"second https://{term}.example.invalid/b\n"
    )
    host_lines = {
        line
        for where_value, line, kind in scan_text(two_lines, rules, "self-test")
        if where_value == "self-test" and kind == "host"
    }
    if host_lines != {1, 2}:
        raise SystemExit(
            "public-repo-guard self-test: repeated host was not reported on "
            f"both of its lines (got {sorted(host_lines)})"
        )

    ssh_line = (
        f"ssh -o StrictHostKeyChecking=no -p 2222 {term}.example.invalid uptime\n"
    )
    ssh_host_lines = {
        line
        for where_value, line, kind in scan_text(ssh_line, rules, "self-test")
        if where_value == "self-test" and kind == "host"
    }
    if ssh_host_lines != {1}:
        raise SystemExit(
            "public-repo-guard self-test: ssh destination was not found after "
            f"its options (got {sorted(ssh_host_lines)})"
        )

    with tempfile.TemporaryDirectory(prefix="public-repo-guard-") as scratch:
        root = Path(scratch)
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        identity = ["-c", "user.name=guard", "-c", "user.email=guard@example.invalid"]
        subprocess.run(
            ["git", "-C", str(root), *identity, "commit", "--allow-empty", "-m", "clean"],
            check=True,
        )
        subprocess.run(
            [
                "git",
                "-C",
                str(root),
                *identity,
                "commit",
                "--allow-empty",
                "-m",
                f"message {term}",
            ],
            check=True,
        )
        if not scan_branch(root, rules, "HEAD~1"):
            raise SystemExit(
                "public-repo-guard self-test: commit message was not flagged"
            )

    print("public-repo-guard self-test: ok (tree and commit paths fire)")
    return 0


def print_hashes(args: argparse.Namespace) -> int:
    if args.hash is not None:
        tokens = normal_tokens(args.hash)
        if not tokens:
            raise SystemExit("public-repo-guard: --hash needs at least one word")
        for token in dict.fromkeys(tokens):
            print(f"token {token_digest(token)}")
        for first, second in dict.fromkeys(zip(tokens, tokens[1:])):
            print(f"bigram {bigram_digest(first, second)}")
        return 0
    if args.hash_prefix is not None:
        print(f"prefix {prefix_digest(args.hash_prefix.strip().lower())}")
        return 0
    if args.hash_host is not None:
        print(f"host {host_digest(args.hash_host.strip().strip('.').lower())}")
        return 0
    raise SystemExit("public-repo-guard: choose --hash, --hash-prefix, or --hash-host")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="repository root (default: cwd)")
    parser.add_argument(
        "--base",
        help="scan commit messages in BASE..HEAD (default: HEAD only)",
    )
    parser.add_argument("--hash", metavar="TERM", help="print token/bigram digests")
    parser.add_argument(
        "--hash-prefix", metavar="PREFIX", help="print an address-prefix digest"
    )
    parser.add_argument("--hash-host", metavar="HOST", help="print a host digest")
    parser.add_argument("--self-test", action="store_true", help="prove the guard fires")
    args = parser.parse_args(argv)

    if args.hash is not None or args.hash_prefix is not None or args.hash_host is not None:
        return print_hashes(args)

    root = Path(args.root).resolve()
    rules = load_denylist()
    if args.self_test:
        return self_test(rules)

    hits = scan_tree(root, rules)
    hits |= scan_branch(root, rules, args.base)
    if hits:
        print("public-repo-guard: blocked identifier found", file=sys.stderr)
        for where, line, kind in sorted(hits):
            print(f"  {where}:{line}: {kind}", file=sys.stderr)
        return 1

    files = len(tracked_files(root))
    commits = len(commit_messages(root, args.base))
    print(f"public-repo-guard: clean ({files} files, {commits} commit messages)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
