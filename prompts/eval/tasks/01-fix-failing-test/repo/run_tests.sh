#!/bin/sh
# Run the task's test suite. Exits non-zero while the task is unfinished.
set -eu
cd "$(dirname "$0")"
python3 -m pytest tests -q 2>/dev/null || python3 -m unittest discover -s tests -t . -q
