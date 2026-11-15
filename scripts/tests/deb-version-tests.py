#!/usr/bin/env python3
"""Regression tests for the release tag -> deb version mapping.

Drives the real `packaging/deb/deb-version.sh` used by
`.github/workflows/release.yml`:

  * `v0.3.0`     -> `0.3.0`      (final release)
  * `v0.3.0-rc1` -> `0.3.0~rc1`  (tilde sorts BELOW the final release in
    Debian version comparison; the old `-rc1` form sorted ABOVE it)
  * `v0.3.1`     -> `0.3.1`
  * any other shape (nightly, stack-*, calendar tags, rc on non-zero
    patch) is rejected with a non-zero exit *before* anything is published.

Run: `python3 scripts/tests/deb-version-tests.py`
"""

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "packaging" / "deb" / "deb-version.sh"
WORKFLOW = (ROOT / ".github" / "workflows" / "release.yml").read_text()

FAILURES = []


def check(name, cond, detail=""):
    if not cond:
        FAILURES.append(f"{name}: {detail}" if detail else name)


def deb_version(tag):
    return subprocess.run(
        ["bash", str(SCRIPT), tag], capture_output=True, text=True
    )


GIVEN = {
    "v0.3.0": "0.3.0",
    "v0.3.0-rc1": "0.3.0~rc1",
    "v0.3.1": "0.3.1",
}

for tag, want in GIVEN.items():
    proc = deb_version(tag)
    check(
        f"mapping {tag}",
        proc.returncode == 0 and proc.stdout.strip() == want,
        f"got rc={proc.returncode} out={proc.stdout.strip()!r} want {want!r}",
    )

REJECTED = (
    "nightly-20261115",
    "stack-foo",
    "2026.11.15",
    "v0.3.1-rc1",  # RCs only exist on .0
    "v0.3",
    "0.3.0",
    "v0.3.0-rc",  # missing RC number
    "v1.2.3.4",
    "",
)

for tag in REJECTED:
    proc = deb_version(tag)
    check(f"reject {tag!r}", proc.returncode != 0, f"accepted as {proc.stdout.strip()!r}")

# The workflow must actually use the mapping script for the deb build…
check(
    "workflow routes deb version through deb-version.sh",
    "deb-version.sh" in WORKFLOW,
    "release.yml does not reference packaging/deb/deb-version.sh",
)
# …must not publish release candidates to the live APT registry…
check(
    "workflow gates APT publish on non-RC tags",
    "-rc" in WORKFLOW and "publish-apt" in WORKFLOW,
    "release.yml publish-apt job has no RC guard",
)
# …and must keep the raw `${GITHUB_REF_NAME#v}` shortcut out of the build.
check(
    "workflow no longer strips v verbatim",
    "${GITHUB_REF_NAME#v}" not in WORKFLOW,
    "release.yml still derives DEB_VERSION as ${GITHUB_REF_NAME#v}",
)

if FAILURES:
    print("\n".join(FAILURES))
    sys.exit(1)
print("deb_version: all cases pass")
