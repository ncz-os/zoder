#!/bin/sh
set -eu
cd "$(dirname "$0")"
python3 - <<'PY'
from src.math import triple

assert triple(3) == 9
print("ok")
PY
