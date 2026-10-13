#!/usr/bin/env python3
"""Regression tests for public free-alias corpus classification."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "build-public-corpus.py"
spec = importlib.util.spec_from_file_location("build_public_corpus", SCRIPT)
if spec is None or spec.loader is None:
    raise RuntimeError(f"cannot load {SCRIPT}")
builder = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = builder
spec.loader.exec_module(builder)


def zero_price(source: str = "litellm") -> dict[str, object]:
    return {
        "input_usd_per_mtok": 0.0,
        "output_usd_per_mtok": 0.0,
        "source": source,
        "_explicit_zero": True,
        "_priced": True,
    }


class OpenRouterFreeAliasTests(unittest.TestCase):
    def test_live_zero_priced_free_alias_is_routable(self) -> None:
        model = "google/gemma-4-31b-it:free"
        econ = {model: zero_price()}
        response = {
            "data": [
                {
                    "id": model,
                    "pricing": {"prompt": "0", "completion": "0"},
                }
            ]
        }

        with mock.patch.object(builder, "fetch_json", return_value=response):
            builder.overlay_openrouter(econ)

        corpus = builder.build_corpus(econ, "2026-08-13")
        entry = corpus["models"][0]
        self.assertTrue(entry["free"])
        self.assertTrue(entry["route_candidate"])
        self.assertFalse(entry["paid"])
        self.assertIsNone(entry["gated_reason"])
        self.assertNotIn("_openrouter_free_alias", entry["economics"])

    def test_suffix_without_live_openrouter_proof_remains_blocked(self) -> None:
        model = "google/gemma-4-31b-it:free"
        corpus = builder.build_corpus(
            {model: zero_price()},
            "2026-08-13",
        )
        entry = corpus["models"][0]
        self.assertFalse(entry["free"])
        self.assertFalse(entry["route_candidate"])
        self.assertTrue(entry["paid"])
        self.assertIn("vendor is paid-only", entry["gated_reason"])

    def test_zero_priced_commercial_alias_without_free_suffix_remains_blocked(self) -> None:
        model = "google/gemini-placeholder"
        econ = {model: zero_price()}
        response = {
            "data": [
                {
                    "id": model,
                    "pricing": {"prompt": "0", "completion": "0"},
                }
            ]
        }

        with mock.patch.object(builder, "fetch_json", return_value=response):
            builder.overlay_openrouter(econ)

        corpus = builder.build_corpus(econ, "2026-08-13")
        entry = corpus["models"][0]
        self.assertFalse(entry["free"])
        self.assertFalse(entry["route_candidate"])
        self.assertTrue(entry["paid"])


if __name__ == "__main__":
    unittest.main()
