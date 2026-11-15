#!/usr/bin/env python3
"""Pin the weekly stack-image tag contract (REL-ZD-002).

container-weekly.yml builds from master on a Monday cron, so its output is
a branch snapshot, not a release. Policy: `latest` moves only on a vX.Y.Z
tag push for the newest stable, and `:X` / `:X.Y` are immutable release
aliases. The weekly build may therefore publish ONLY channel tags
(`weekly-<ISO week>` and `edge`) — never `:latest`, `:X` or `:X.Y`.

Run: `python3 scripts/tests/container-weekly-tags-tests.py`
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows" / "container-weekly.yml"

FORBIDDEN = (":latest",)
VERSION_TAG = re.compile(r":\d+(?:\.\d+){0,2}(?:[\"'\s]|$)")


def _publish_scripts() -> list[str]:
    doc = yaml.safe_load(WORKFLOW.read_text())
    scripts = []
    for job in doc.get("jobs", {}).values():
        for step in job.get("steps", []) or []:
            run = step.get("run", "")
            if "imagetools create" in run:
                scripts.append(run)
    return scripts


def _published_tags(script: str) -> list[str]:
    return re.findall(r"-t\s+[\"']?(\$\{IMAGE\}[^\"'\s]*)[\"']?", script)


class WeeklyTagTests(unittest.TestCase):
    def test_weekly_workflow_exists(self) -> None:
        self.assertTrue(WORKFLOW.is_file(), "container-weekly.yml is missing")

    def test_weekly_build_has_a_manifest_step(self) -> None:
        self.assertTrue(
            _publish_scripts(),
            "no 'imagetools create' publish step found in container-weekly.yml",
        )

    def test_weekly_publishes_channel_tags(self) -> None:
        for script in _publish_scripts():
            tags = _published_tags(script)
            self.assertTrue(
                any("weekly-" in tag for tag in tags),
                f"weekly channel tag missing from imagetools create: {tags}",
            )
            self.assertTrue(
                any(tag.endswith(":edge") for tag in tags),
                f"edge channel tag missing from imagetools create: {tags}",
            )

    def test_weekly_never_publishes_release_tags(self) -> None:
        for script in _publish_scripts():
            tags = _published_tags(script)
            for tag in tags:
                for forbidden in FORBIDDEN:
                    self.assertFalse(
                        tag.endswith(forbidden),
                        f"branch build must not publish {forbidden}: {tags}",
                    )
                self.assertIsNone(
                    VERSION_TAG.search(tag),
                    f"branch build must not publish a version tag: {tags}",
                )

    def test_no_latest_reference_in_publish_commands(self) -> None:
        for script in _publish_scripts():
            for line in script.splitlines():
                stripped = line.strip()
                if stripped.startswith("#"):
                    continue
                if "-t" in line:
                    self.assertNotIn(
                        "latest",
                        line,
                        f"publish command references :latest: {line.strip()}",
                    )


if __name__ == "__main__":
    unittest.main()
