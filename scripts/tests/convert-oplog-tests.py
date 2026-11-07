#!/usr/bin/env python3
"""Offline regression tests for scripts/convert_oplog.py.

Run: `python3 scripts/tests/convert-oplog-tests.py`
"""

import importlib.util
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("convert_oplog", HERE.parent / "convert_oplog.py")
convert_oplog = importlib.util.module_from_spec(spec)
spec.loader.exec_module(convert_oplog)
convert = convert_oplog.convert

FAILURES = []


def check(name, src, want, count):
    got, n = convert(src)
    if got != want or n != count:
        FAILURES.append(f"{name}:\n  got   {got!r} ({n})\n  want  {want!r} ({count})")


check(
    "single line",
    'eprintln!("[loop] iter {i}: author…");',
    'crate::oplog!("loop", "[loop] iter {i}: author…");',
    1,
)
check(
    "leading newline and arguments kept in order",
    'eprintln!("\\n[zoder] {a} {}", b, c);',
    'crate::oplog!("zoder", "\\n[zoder] {a} {}", b, c);',
    1,
)
check(
    "multi-line call",
    'eprintln!(\n    "zoder: error: {}",\n    f(")", \'(\'),\n);',
    'crate::oplog!(\n    "zoder", "zoder: error: {}",\n    f(")", \'(\'),\n);',
    1,
)
check("unprefixed literal left alone", 'eprintln!("warning: x");', 'eprintln!("warning: x");', 0)
check("non-literal first argument left alone", 'eprintln!("{line}");', 'eprintln!("{line}");', 0)
check(
    "comment left alone",
    '// eprintln!("[loop] x");',
    '// eprintln!("[loop] x");',
    0,
)
check(
    "path-qualified call left alone",
    'std::eprintln!("[loop] x"); crate::eprintln!("[zoder] y");',
    'std::eprintln!("[loop] x"); crate::eprintln!("[zoder] y");',
    0,
)
check(
    "a prefix that is only a word start is not a component",
    'eprintln!("[loops] x");',
    'eprintln!("[loops] x");',
    0,
)

check(
    "a paren inside a comment argument does not end the call",
    'eprintln!("[loop] {}", /* ) */ x);',
    'crate::oplog!("loop", "[loop] {}", /* ) */ x);',
    1,
)
check(
    "line comment inside a multi-line call",
    'eprintln!(\n    "[loop] {}", // ) not the end\n    x\n);',
    'crate::oplog!(\n    "loop", "[loop] {}", // ) not the end\n    x\n);',
    1,
)
check(
    "block comment and doc comment left alone",
    '/* eprintln!("[loop] a"); */\n/// eprintln!("[loop] b");\n//! eprintln!("[loop] c");',
    '/* eprintln!("[loop] a"); */\n/// eprintln!("[loop] b");\n//! eprintln!("[loop] c");',
    0,
)
check(
    "text inside a string literal left alone",
    'let s = "eprintln!(\\"[loop] x\\")"; let r = r#"eprintln!("[loop] y")"#;',
    'let s = "eprintln!(\\"[loop] x\\")"; let r = r#"eprintln!("[loop] y")"#;',
    0,
)

check(
    "a raw string argument with parens and quotes",
    'eprintln!("[loop] {}", r#"f(") (x"#);',
    'crate::oplog!("loop", "[loop] {}", r#"f(") (x"#);',
    1,
)
check(
    "a // inside an earlier string on the same line is not a comment",
    'let u = "http://x"; eprintln!("[loop] {u}");',
    'let u = "http://x"; crate::oplog!("loop", "[loop] {u}");',
    1,
)

if FAILURES:
    print("\n".join(FAILURES))
    sys.exit(1)
print("convert_oplog: all cases pass")
