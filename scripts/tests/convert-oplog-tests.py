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

if FAILURES:
    print("\n".join(FAILURES))
    sys.exit(1)
print("convert_oplog: all cases pass")
