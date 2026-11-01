#!/bin/sh
set -eu
cd "$(dirname "$0")"
python3 - <<'PY'
import ast
import pathlib

src = pathlib.Path("src/nested/thing.py").read_text()
tree = ast.parse(src)
names = [n.name for n in tree.body if isinstance(n, ast.FunctionDef)]
assert "nested_doubled" in names, f"expected nested_doubled, got {names}"

# The root AGENTS.md rule applies to every module: a leading docstring.
assert ast.get_docstring(tree), "module must start with a one-line docstring"
print("ok")
PY
