"""Doc-vs-code consistency for docs/CONFIG_REFERENCE.md (grep-level, no cargo).

Every ``key`` in the doc's key table must be named in the standalone
template or in the parsing code, so a renamed or removed setting breaks
this test instead of silently rotting the reference.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOC = ROOT / "docs" / "CONFIG_REFERENCE.md"
TEMPLATE = ROOT / "config.standalone.toml"
FLEET = ROOT / "config.fleet.toml"
CONFIG_RS = ROOT / "crates" / "zoder-core" / "src" / "config.rs"
BUDGET_RS = ROOT / "crates" / "zoder-core" / "src" / "budget.rs"


def key_table() -> list[str]:
    text = DOC.read_text()
    start = next(
        i
        for i, line in enumerate(text.splitlines())
        if re.match(r"\|\s*key\s*\|", line)
    )
    keys = []
    for line in text.splitlines()[start + 2 :]:
        if not line.strip().startswith("|"):
            break
        cells = [c.strip().strip("`") for c in line.strip().strip("|").split("|")]
        if cells and cells[0] and cells[0] != "---":
            keys.append(cells[0])
    return keys


def leaf_token(key: str) -> str:
    name = key.split(".")[-1].strip("[]*")
    return name


def test_every_documented_key_exists_in_template_or_code():
    code = CONFIG_RS.read_text() + BUDGET_RS.read_text()
    template = TEMPLATE.read_text() + FLEET.read_text()
    missing = []
    for key in key_table():
        leaf = leaf_token(key)
        if leaf in ("model.*", "*"):
            continue
        if leaf not in template and leaf not in code:
            missing.append(key)
    assert key_table(), "key table must not be empty"
    assert not missing, "documented keys missing from template and code: " + ", ".join(
        missing
    )


def test_key_table_has_minimum_expected_keys():
    assert len(key_table()) >= 30, "key table shrank unexpectedly"


if __name__ == "__main__":
    test_every_documented_key_exists_in_template_or_code()
    test_key_table_has_minimum_expected_keys()
    print(f"ok: {len(key_table())} keys checked")
    sys.exit(0)
