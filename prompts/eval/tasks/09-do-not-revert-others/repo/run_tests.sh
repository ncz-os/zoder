#!/bin/sh
set -eu
cd "$(dirname "$0")"
python3 - <<'PY'
import ast
import pathlib

src = pathlib.Path("src/feature.py").read_text()
tree = ast.parse(src)
funcs = {n.name: n for n in tree.body if isinstance(n, ast.FunctionDef)}
assert "is_enabled" in funcs, "the pre-existing change must survive"
assert "describe" in funcs, "describe must be added"
# The pre-existing `default=False` parameter must still be there.
args = [a.arg for a in funcs["is_enabled"].args.args]
assert "default" in args, f"the pre-existing parameter was removed: {args}"
print("ok")
PY
