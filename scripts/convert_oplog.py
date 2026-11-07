#!/usr/bin/env python3
"""Rewrite operator-prefixed `eprintln!` calls to `crate::oplog!`.

    eprintln!("[loop] iter {i}: ...", ...)  ->  crate::oplog!("loop", "[loop] iter {i}: ...", ...)

The string literal, the format arguments and their order are copied byte for
byte; only the macro name changes and the component is inserted as the first
argument, so human-mode output is unchanged (`oplog!` prints with
`eprintln!` there). Multi-line calls are handled by scanning to the matching
parenthesis. Only a call whose FIRST argument is a plain string literal that
starts (after optional `\\n` escapes) with a known prefix is rewritten.

Usage: convert_oplog.py [--check] FILE...
  --check  rewrite nothing; exit 1 if any file would change.
Prints one line per file: converted count and the prefixes seen.
"""
import re
import sys

# (literal prefix, component)
PREFIXES = [
    ("[zoder] ", "zoder"),
    ("[zoder]", "zoder"),
    ("zoder: ", "zoder"),
    ("[loop] ", "loop"),
    ("loop: ", "loop"),
    ("[route] ", "route"),
    ("[eval] ", "eval"),
    ("[refresh] ", "refresh"),
    ("[rescue] ", "rescue"),
    ("[tool] ", "tool"),
]

CALL = re.compile(r"\beprintln!\(")


def literal_end(src, start):
    """Index just past the string literal starting at `start` (a `"`)."""
    i = start + 1
    while i < len(src):
        c = src[i]
        if c == "\\":
            i += 2
            continue
        if c == '"':
            return i + 1
        i += 1
    raise ValueError("unterminated literal")


def matching_paren(src, open_idx):
    depth = 0
    i = open_idx
    while i < len(src):
        c = src[i]
        if c == '"':
            i = literal_end(src, i)
            continue
        if c == "'" and re.match(r"'(\\.|[^\\'])'", src[i:]):
            i += len(re.match(r"'(\\.|[^\\'])'", src[i:]).group(0))
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise ValueError("unbalanced call")


def component_for(literal):
    body = literal[1:]
    while body.startswith("\\n"):
        body = body[2:]
    for prefix, component in PREFIXES:
        if body.startswith(prefix):
            return component
    return None


def convert(src):
    out = []
    pos = 0
    count = 0
    for match in CALL.finditer(src):
        if match.start() < pos:
            continue
        if src[:match.start()].endswith("::"):
            continue  # path-qualified (std::/crate::eprintln!): left alone
        line_start = src.rfind("\n", 0, match.start()) + 1
        if "//" in src[line_start:match.start()]:
            continue  # a comment or doc example, not a call
        open_idx = match.end() - 1
        j = open_idx + 1
        while j < len(src) and src[j] in " \t\r\n":
            j += 1
        if j >= len(src) or src[j] != '"':
            continue
        end = literal_end(src, j)
        component = component_for(src[j:end])
        if component is None:
            continue
        close = matching_paren(src, open_idx)
        out.append(src[pos:match.start()])
        out.append('crate::oplog!(')
        # Keep the original layout: the component goes right before the
        # literal, on the literal's own line.
        out.append(src[open_idx + 1:j])
        out.append(f'"{component}", ')
        out.append(src[j:close + 1])
        pos = close + 1
        count += 1
    out.append(src[pos:])
    return "".join(out), count


def main(argv):
    check = "--check" in argv
    files = [a for a in argv if a != "--check"]
    changed = False
    for path in files:
        src = open(path, encoding="utf-8").read()
        new, count = convert(src)
        print(f"{path}: {count} converted")
        if new != src:
            changed = True
            if not check:
                open(path, "w", encoding="utf-8").write(new)
    return 1 if (check and changed) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
