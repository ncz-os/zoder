#!/bin/sh
set -eu
cd "$(dirname "$0")"
python3 - <<'PY'
from src.greet import greet

assert greet("Ann") == "Hello, Ann!"
assert greet("Ann", "Hi") == "Hi, Ann!"
print("ok")
PY
