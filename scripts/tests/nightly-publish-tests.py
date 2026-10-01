#!/usr/bin/env python3
"""Exercise the actual nightly publication script with local artifacts and stub APIs.

Run: uv run --with pyyaml python scripts/tests/nightly-publish-tests.py
"""

import hashlib
import json
import os
import shlex
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = yaml.safe_load((ROOT / ".github/workflows/native-builds.yml").read_text())
SCRIPT = WORKFLOW["jobs"]["publish"]["steps"][-1]["run"]
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
)


class NightlyPublishTests(unittest.TestCase):
    def run_publish(self, defect=None):
        with tempfile.TemporaryDirectory(
            prefix="zoder-nightly-artifacts-"
        ) as temporary:
            root = Path(temporary)
            dist = root / "dist"
            dist.mkdir()
            for target in TARGETS:
                if defect == "missing_target" and target == TARGETS[-1]:
                    continue
                stage = dist / ("zoder-" + target)
                stage.mkdir()
                for binary in ("zoder", "zerocode", "zeroclaw"):
                    (stage / binary).write_bytes(b"fixture binary")
                manifest = {
                    "zoder": {"sha": "a" * 40},
                    "engine": {"sha": "b" * 40, "upstream_sha": "e" * 40},
                    "target": target,
                    "locked": True,
                }
                if defect == "wrong_source" and target == TARGETS[-1]:
                    manifest["engine"]["sha"] = "c" * 40
                (stage / "manifest.json").write_text(json.dumps(manifest))
                archive = dist / (stage.name + ".tar.gz")
                with tarfile.open(archive, "w:gz") as stream:
                    stream.add(stage, arcname=stage.name)
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                archive.with_suffix(".gz.sha256").write_text(
                    digest + "  " + archive.name + "\n"
                )
                if defect == "corrupt_archive" and target == TARGETS[-1]:
                    archive.write_bytes(b"corrupt")
            env = os.environ | {
                "ZODER_SHA": "a" * 40,
                "ENGINE_SHA": "b" * 40,
                "UPSTREAM_SHA": "e" * 40,
                "PROJECT_ID": "fixture",
                "PKG_TOKEN": "fixture",
                "RELEASE_TAG": "fixture",
                "GITHUB_REPOSITORY": "fixture/repo",
                "GITHUB_SHA": "d" * 40,
                "GITHUB_STEP_SUMMARY": str(root / "summary"),
                "EVENTS": str(root / "events"),
            }
            # Functions intercept every network mutation in the unmodified script.
            stubs = (
                'curl() { echo curl >> "$EVENTS"; }; gh() { echo gh >> "$EVENTS"; };\n'
            )
            result = subprocess.run(
                ["bash"],
                input=stubs + SCRIPT,
                text=True,
                cwd=root,
                env=env,
                capture_output=True,
                check=False,
            )
            events = (
                (root / "events").read_text().splitlines()
                if (root / "events").exists()
                else []
            )
            return result, events

    def test_source_gate_rejects_stale_engine(self):
        for stale in (False, True):
            with self.subTest(stale=stale), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                upstream = root / "upstream"
                engine = root / "engine"

                def git(*args):
                    return subprocess.check_output(
                        ["git", *map(str, args)], text=True, stderr=subprocess.DEVNULL
                    ).strip()

                git("init", "-b", "master", upstream)
                for key, value in (
                    ("user.name", "Fixture"),
                    ("user.email", "fixture@example.invalid"),
                ):
                    git("-C", upstream, "config", key, value)
                (upstream / "source").write_text("upstream")
                git("-C", upstream, "add", "source")
                git("-C", upstream, "commit", "-m", "initial")
                git("clone", upstream, engine)
                if stale:
                    (upstream / "source").write_text("new upstream")
                    git("-C", upstream, "commit", "-am", "advance upstream")
                prefixes = []
                for url, repo in (
                    ("https://gitlab.com/ncz-os/zoder.git", engine),
                    ("https://gitlab.com/ncz-os/zeroclaw.git", engine),
                    ("https://github.com/zeroclaw-labs/zeroclaw.git", upstream),
                ):
                    prefixes += ["-c", f"url.file://{repo}.insteadOf={url}"]
                stub = (
                    "git() { command git "
                    + " ".join(map(shlex.quote, prefixes))
                    + ' "$@"; };\n'
                )
                env = os.environ | {
                    "ENGINE_USER": "fixture",
                    "ENGINE_TOKEN": "fixture",
                    "ENGINE_REPO": "gitlab.com/ncz-os/zeroclaw.git",
                    "ENGINE_BRANCH": "master",
                    "GITHUB_REF": "refs/heads/master",
                    "GITHUB_SHA": "a" * 40,
                    "GITHUB_OUTPUT": str(root / "outputs"),
                    "GITHUB_STEP_SUMMARY": str(root / "summary"),
                }
                source_script = WORKFLOW["jobs"]["sources"]["steps"][-1]["run"]
                result = subprocess.run(
                    ["bash"],
                    input=stub + source_script,
                    text=True,
                    cwd=root,
                    env=env,
                    capture_output=True,
                    check=False,
                )
                if stale:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("has not incorporated upstream", result.stdout)
                    self.assertFalse((root / "outputs").exists())
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(
                        "upstream_sha=" + git("-C", upstream, "rev-parse", "HEAD"),
                        (root / "outputs").read_text(),
                    )

    def test_complete_matrix_publishes(self):
        result, events = self.run_publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(events.count("curl"), 48)
        self.assertEqual(events.count("gh"), 3)

    def test_bad_artifacts_never_publish(self):
        for defect in ("missing_target", "wrong_source", "corrupt_archive"):
            with self.subTest(defect=defect):
                result, events = self.run_publish(defect)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(events, [])


if __name__ == "__main__":
    unittest.main()
