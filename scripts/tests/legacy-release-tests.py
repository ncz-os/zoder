#!/usr/bin/env python3
"""Offline unit tests for legacy release decisions and mirror verification."""
from __future__ import annotations

import hashlib
import importlib.util
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def load(name: str, path: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader
    spec.loader.exec_module(module)
    return module


legacy = load("legacy_release", "scripts/ci/legacy-release.py")
mirror = load("legacy_release_mirror", "scripts/ci/legacy-release-mirror.py")


class LegacyDecisionTests(unittest.TestCase):
    def test_no_previous_release_requires_release(self):
        self.assertTrue(legacy.needs_release("a" * 64, []))

    def test_unchanged_fingerprint_skips_release(self):
        releases = [{"tag_name": "legacy-x86_64-v2-20261117-12345678", "description": "Source fingerprint: " + "a" * 64}]
        self.assertFalse(legacy.needs_release("a" * 64, releases))

    def test_changed_fingerprint_requires_release(self):
        releases = [{"tag_name": "legacy-x86_64-v2-20261117-12345678", "description": "Source fingerprint: " + "a" * 64}]
        self.assertTrue(legacy.needs_release("b" * 64, releases))


class MirrorTests(unittest.TestCase):
    def test_checksum_match_and_mismatch(self):
        binary = b"smoke-tested legacy binary"
        sums = f"{hashlib.sha256(binary).hexdigest()}  zoder-x86_64-v2-linux\n".encode()
        self.assertTrue(mirror.checksum_matches(binary, sums))
        self.assertFalse(mirror.checksum_matches(binary + b"!", sums))

    def test_existing_tag_is_never_republished(self):
        release = {"tag_name": "legacy-x86_64-v2-20261117-12345678"}
        self.assertEqual(mirror.mirror([release], {release["tag_name"]}, "owner/repo"), [])

    def test_asset_links_are_selected_by_name(self):
        links = {"links": [{"name": "zoder-x86_64-v2-linux", "url": "https://example.invalid/binary"}, {"name": "SHA256SUMS", "url": "https://example.invalid/sums"}, {"name": "other", "url": "ignored"}]}
        self.assertEqual(set(mirror.asset_urls({"assets": links})), mirror.EXPECTED)

    def test_downloads_are_verified_before_gh_publish(self):
        binary = b"verified"
        sums = f"{hashlib.sha256(binary).hexdigest()}  zoder-x86_64-v2-linux\n".encode()
        release = {"tag_name": "legacy-x86_64-v2-20261117-12345678", "description": "notes", "assets": {"links": [
            {"name": "zoder-x86_64-v2-linux", "url": "binary-url"}, {"name": "SHA256SUMS", "url": "sums-url"}
        ]}}
        calls = []

        class Scratch:
            def __enter__(self):
                return "/not-created/legacy-mirror-test"
            def __exit__(self, *args):
                return False

        def fake_http(url):
            return 200, binary if url == "binary-url" else sums

        with patch.object(mirror, "http_request", fake_http), patch.object(mirror.tempfile, "TemporaryDirectory", return_value=Scratch()), patch.object(Path, "write_bytes", lambda *args: len(args[1])):
            result = mirror.mirror([release], set(), "owner/repo", gh=lambda args, **kwargs: calls.append(args))
        self.assertEqual(result, [release["tag_name"]])
        self.assertEqual(len(calls), 1)
        with patch.object(mirror, "http_request", lambda url: (200, b"bad" if url == "sums-url" else binary)), patch.object(mirror.tempfile, "TemporaryDirectory", side_effect=AssertionError("must verify first")):
            with self.assertRaisesRegex(RuntimeError, "checksum verification failed"):
                mirror.mirror([release], set(), "owner/repo")


if __name__ == "__main__":
    unittest.main()
