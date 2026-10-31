#!/bin/sh
# The task's acceptance check: the flag exists, has the right default and
# rejects a bad value.
set -eu
cd "$(dirname "$0")"
python3 - <<'PY'
from src.cli import build_parser

parser = build_parser()
args = parser.parse_args([])
assert args.color == "auto", f"default must be auto, got {args.color!r}"
args = parser.parse_args(["--color", "never"])
assert args.color == "never"
try:
    parser.parse_args(["--color", "banana"])
except SystemExit:
    pass
else:
    raise AssertionError("--color banana must be rejected")
print("ok")
PY
