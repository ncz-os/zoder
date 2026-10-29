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


def key_is_covered(key: str, code: str, template: str) -> bool:
    """A documented key must appear as a real TOML key or struct field.

    Glob keys (``*``) require every fixed path segment to appear in the
    parsing code (they match many concrete keys by design). A bare
    ``[section]`` needs its literal header in a template or its name in
    the code. A ``section.field`` key needs a ``field =`` assignment in a
    template or a ``pub field:`` struct field in the code — never a bare
    substring, so ``mode`` cannot match ``deployment_mode``.
    """
    if "*" in key:
        fixed = [seg for seg in re.split(r"[^A-Za-z0-9_]+", key) if seg]
        return bool(fixed) and all(seg in code for seg in fixed)
    section_match = re.fullmatch(r"\[([^\].]+)\]", key)
    if section_match:
        section = section_match.group(1)
        return f"[{section}]" in template or section in code
    if "." in key:
        _section, field = key.rsplit(".", 1)
        field = field.strip("[]")
        in_template = (
            re.search(rf"(?m)^\s*{re.escape(field)}\s*=", template) is not None
        )
        in_code = re.search(rf"pub {re.escape(field)}\s*:", code) is not None
        return in_template or in_code
    return key in template or key in code


def test_every_documented_key_exists_in_template_or_code():
    code = CONFIG_RS.read_text() + BUDGET_RS.read_text()
    template = TEMPLATE.read_text() + FLEET.read_text()
    keys = key_table()
    assert keys, "key table must not be empty"
    missing = [key for key in keys if not key_is_covered(key, code, template)]
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
