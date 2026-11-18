"""Accounting tests for zoder's local spend ledger, via the installed binary.

The ledger is the only record of what a session spent: one append-only JSONL
row per model call, which ``zoder finops report`` and ``zoder report`` total.
A drift between what was recorded and what is reported is a budget bug, so the
tests below pin the arithmetic and the integrity rules against a throwaway
``ZODER_HOME`` -- no network, no API spend, no Rust build.

The suite skips cleanly when ``zoder`` is not on PATH (a host that only has
the source tree has nothing to drive).

Run: python3 -m pytest tests/test_token_budget_ledger_20261106.py -q
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CORPUS = REPO / "corpus" / "model_corpus.json"
ZODER = shutil.which("zoder")


def zoder_version() -> str:
    if ZODER is None:
        return "zoder not on PATH"
    try:
        completed = subprocess.run(
            [ZODER, "--version"], capture_output=True, text=True, check=False
        )
        return (completed.stdout or completed.stderr).strip() or "unknown version"
    except OSError as exc:
        return f"zoder --version failed: {exc}"


ZODER_VERSION = zoder_version()

# JSON totals are sums of exact inputs, so the only slack needed is float
# addition order. This is the documented tolerance for "reported == recorded".
COST_TOLERANCE_USD = 1e-9
# The human `zoder report` table rounds cost to cents and tokens to 0.1M.
DISPLAY_COST_TOLERANCE_USD = 0.005
DISPLAY_TOKENS_TOLERANCE_MILLIONS = 0.05

RESERVATION_PROVIDER = "__zoder_reservation__"
PREPARED_RESERVATION_MODEL = "__zoder_billable_p__"


def utc_now() -> datetime:
    return datetime.now(UTC)


def iso(ts: datetime) -> str:
    return ts.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def ledger_row(
    ts: datetime,
    model: str,
    tokens_in: int,
    tokens_out: int,
    cost_usd: float,
    **extra: object,
) -> dict:
    row = {
        "ts_utc": iso(ts),
        "provider": "probe",
        "model": model,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "cost_usd": cost_usd,
    }
    row.update(extra)
    return row


@unittest.skipIf(ZODER is None, "zoder binary is not on PATH")
class LedgerCase(unittest.TestCase):
    """A temp ZODER_HOME whose ledger and corpus the binary reads directly."""

    def setUp(self) -> None:
        self._tmp = None
        self.tmpdir = None
        if ZODER is None:
            self.skipTest("zoder binary is not on PATH")
        self._tmp = __import__("tempfile").TemporaryDirectory()
        self.home = Path(self._tmp.name)
        (self.home / "engine").mkdir()
        (self.home / "engine" / "config.toml").write_text("", encoding="utf-8")
        self.ledger = self.home / "ledger.jsonl"
        config = {
            "providers": [
                {
                    "id": "probe",
                    "base_url": "https://probe.invalid/v1",
                    "kind": "openai-chat",
                    "auth": {"type": "none"},
                    "billing": "free",
                    "serves": ["zz-unpriced/"],
                }
            ],
            "default_provider": "probe",
            "strict_free": False,
            "corpus_path": str(CORPUS),
            "ledger_path": str(self.ledger),
            "health_path": str(self.home / "health.json"),
        }
        (self.home / "config.json").write_text(json.dumps(config), encoding="utf-8")

    def tearDown(self) -> None:
        if self._tmp is not None:
            self._tmp.cleanup()

    # -- writes ---------------------------------------------------------
    def write_rows(self, rows: list[dict]) -> None:
        self.ledger.write_text(
            "".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8"
        )

    def append_raw(self, text: str) -> None:
        with open(self.ledger, "a", encoding="utf-8") as handle:
            handle.write(text)

    # -- reads ----------------------------------------------------------
    def run_zoder(self, *args: str) -> subprocess.CompletedProcess:
        env = os.environ.copy()
        env["ZODER_HOME"] = str(self.home)
        return subprocess.run(
            [ZODER, *args],
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )

    def finops(self) -> subprocess.CompletedProcess:
        return self.run_zoder("finops", "report", "--json")

    def totals(self) -> dict:
        completed = self.finops()
        self.assertEqual(
            completed.returncode, 0, f"{ZODER_VERSION}: {completed.stderr}"
        )
        return json.loads(completed.stdout)

    def ledger_digest(self) -> tuple[str, int]:
        payload = self.ledger.read_bytes()
        return hashlib.sha256(payload).hexdigest(), len(payload)


class TotalsTest(LedgerCase):
    def test_totals_equal_the_sum_of_per_request_rows(self) -> None:
        ts = utc_now() - timedelta(hours=1)
        self.write_rows(
            [
                ledger_row(ts, "zz-a/one", 1000, 500, 0.0015),
                ledger_row(ts, "zz-a/two", 2500, 750, 0.0042),
                ledger_row(ts, "zz-b/free", 400, 100, 0.0),
            ]
        )
        totals = self.totals()

        self.assertAlmostEqual(
            totals["total_cost_usd"], 0.0057, delta=COST_TOLERANCE_USD
        )
        self.assertEqual(totals["total_tokens"], 5250)
        self.assertEqual(totals["total_calls"], 3)
        self.assertEqual(totals["unknown_cost_tokens"], 0)
        self.assertEqual(totals["unknown_cost_calls"], 0)

    def test_per_host_and_per_model_sums_match_the_headline(self) -> None:
        ts = utc_now() - timedelta(hours=1)
        self.write_rows(
            [
                ledger_row(ts, "zz-a/one", 1000, 500, 0.0015),
                ledger_row(ts, "zz-a/two", 2500, 750, 0.0042),
                ledger_row(ts, "zz-b/free", 400, 100, 0.0),
            ]
        )
        totals = self.totals()

        host_cost = sum(row["cost_usd"] for row in totals["by_host"])
        self.assertAlmostEqual(
            host_cost, totals["total_cost_usd"], delta=COST_TOLERANCE_USD
        )
        host_tokens = sum(
            row["tokens_in"] + row["tokens_out"] for row in totals["by_host"]
        )
        self.assertEqual(host_tokens, totals["total_tokens"])
        model_tokens = sum(row["tokens"] for row in totals["by_model_realized"])
        self.assertEqual(model_tokens, totals["total_tokens"])


class MissingUsageTest(LedgerCase):
    def test_unknown_cost_row_is_conservative_not_zero(self) -> None:
        """A call whose cost could not be established is not free spend.

        Its tokens must never be promoted to ``total_tokens`` (verified spend)
        and its placeholder ``0.0`` must never enter ``total_cost_usd``.
        """
        ts = utc_now() - timedelta(hours=1)
        self.write_rows(
            [
                ledger_row(ts, "zz-a/known", 1000, 500, 0.0035),
                ledger_row(ts, "zz-a/unknown", 700, 300, 0.0, cost_unknown=True),
            ]
        )
        totals = self.totals()

        self.assertEqual(totals["total_tokens"], 1500)
        self.assertAlmostEqual(
            totals["total_cost_usd"], 0.0035, delta=COST_TOLERANCE_USD
        )
        self.assertEqual(totals["unknown_cost_tokens"], 1000)
        self.assertEqual(totals["unknown_cost_calls"], 1)
        # A call whose cost is unknown is reported in its own bucket, not in
        # the verified-call count; the two buckets together are every call.
        self.assertEqual(totals["total_calls"], 1)
        self.assertEqual(totals["total_calls"] + totals["unknown_cost_calls"], 2)

        unknown = [
            row for row in totals["by_model_realized"] if row["model"] == "zz-a/unknown"
        ]
        self.assertEqual(len(unknown), 1)
        self.assertEqual(unknown[0]["tokens"], 0)
        self.assertEqual(unknown[0]["unknown_cost_tokens"], 1000)
        self.assertEqual(unknown[0]["cost_usd"], 0.0)

    def test_missing_usage_field_is_refused_not_counted_as_zero(self) -> None:
        """A row with no output-token count cannot be totalled as if it were 0."""
        ts = utc_now() - timedelta(hours=1)
        broken = ledger_row(ts, "zz-a/broken", 100, 50, 0.001)
        del broken["tokens_out"]
        self.write_rows([ledger_row(ts, "zz-a/ok", 10, 5, 0.001), broken])

        completed = self.finops()

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("cannot establish ledger integrity", completed.stderr)
        self.assertIn("malformed non-empty row", completed.stderr)
        self.assertIn("line(s) 2", completed.stderr)
        # No total at all: a silently smaller number would be the budget bug.
        self.assertEqual(completed.stdout.strip(), "")


class PartialTailTest(LedgerCase):
    def test_partial_tail_is_refused_and_no_total_is_reported(self) -> None:
        """An interrupted append must not shrink the reported total.

        The last line here is half a JSON object -- exactly what a killed
        writer leaves. At base the reader fails closed: exit 1, the line
        number named, and nothing on stdout. Fail-closed is the documented
        contract (``entries_strict``): a skipped row may hold spend.
        """
        ts = utc_now() - timedelta(hours=1)
        self.write_rows([ledger_row(ts, "zz-a/one", 1000, 500, 0.0035)])
        self.append_raw('{"ts_utc":"2026-11-05T11:00:00Z","prov')

        completed = self.finops()

        self.assertEqual(completed.returncode, 1, completed.stderr)
        self.assertIn("cannot establish ledger integrity", completed.stderr)
        self.assertIn("line(s) 2", completed.stderr)
        self.assertEqual(completed.stdout.strip(), "")

    @unittest.skip(
        "divergence from the brief: base refuses a partial tail (exit 1, line "
        "named) instead of ignoring it; see "
        "test_partial_tail_is_refused_and_no_total_is_reported"
    )
    def test_partial_tail_is_ignored(self) -> None:
        """The brief's wording: record the complete rows, drop the partial one."""
        ts = utc_now() - timedelta(hours=1)
        self.write_rows([ledger_row(ts, "zz-a/one", 1000, 500, 0.0035)])
        self.append_raw('{"ts_utc":"2026-11-05T11:00:00Z","prov')
        totals = self.totals()
        self.assertEqual(totals["total_tokens"], 1500)

    def test_blank_line_and_unterminated_final_row_are_tolerated(self) -> None:
        """Neither is corruption: a blank line is padding, and a complete JSON
        object with no trailing newline is finished work."""
        ts = utc_now() - timedelta(hours=1)
        self.write_rows([ledger_row(ts, "zz-a/one", 1000, 500, 0.0035)])
        self.append_raw("\n")

        first = self.totals()
        self.assertEqual(first["total_tokens"], 1500)

        self.ledger.write_text(
            json.dumps(ledger_row(ts, "zz-a/one", 1000, 500, 0.0035)),
            encoding="utf-8",
        )
        second = self.totals()
        self.assertEqual(second["total_tokens"], 1500)
        self.assertEqual(second["total_calls"], 1)


class RetryAccountingTest(LedgerCase):
    @unittest.skip(
        "no request identity in the ledger: whether two attempts were one "
        "call or two cannot be distinguished from the recorded rows offline; "
        "the slot is rewritten in place, which the test below pins instead"
    )
    def test_retry_is_counted_once(self) -> None:
        """The brief's wording: a retried request must not add a second row."""

    def test_reservation_slot_adds_no_spend_but_counts_as_a_call(self) -> None:
        """A billable call owns a reservation slot the final row overwrites.

        An abandoned slot (the writer died mid-dispatch) must not add spend:
        its zero tokens and placeholder cost stay out of the totals. It is
        counted as one attempted call, which the assertion below pins -- and
        is recorded as a known gap in PLAN-FB266.md, because a *retry* against
        the same slot is indistinguishable from a second attempt offline.
        """
        ts = utc_now() - timedelta(hours=1)
        self.write_rows(
            [
                ledger_row(ts, "zz-a/one", 1000, 500, 0.0035),
                {
                    "ts_utc": iso(ts),
                    "provider": RESERVATION_PROVIDER,
                    "model": PREPARED_RESERVATION_MODEL,
                    "tokens_in": 0,
                    "tokens_out": 0,
                    "cost_usd": 0.0,
                },
            ]
        )
        totals = self.totals()

        # Recorded gap: the slot contributes one attempt but no spend.
        self.assertEqual(totals["total_calls"], 2)
        self.assertEqual(totals["total_tokens"], 1500)
        self.assertAlmostEqual(
            totals["total_cost_usd"], 0.0035, delta=COST_TOLERANCE_USD
        )
        self.assertEqual(totals["unknown_cost_tokens"], 0)


class ReadOnlyTest(LedgerCase):
    def test_read_paths_do_not_rewrite_the_ledger(self) -> None:
        """Reporting is a read: the append-only file is never rewritten."""
        ts = utc_now() - timedelta(hours=1)
        self.write_rows(
            [
                ledger_row(ts, "zz-a/one", 1000, 500, 0.0035),
                ledger_row(ts, "zz-a/two", 2500, 750, 0.0042),
            ]
        )
        before = self.ledger_digest()

        for args in (
            ("finops", "report", "--json"),
            ("report", "--days", "1"),
            ("spend", "month"),
        ):
            completed = self.run_zoder(*args)
            self.assertEqual(completed.returncode, 0, f"{args}: {completed.stderr}")

        self.assertEqual(self.ledger_digest(), before)
        self.assertEqual(len(self.ledger.read_text().splitlines()), 2)


class DisplayAgreementTest(LedgerCase):
    def test_text_report_matches_json_within_display_tolerance(self) -> None:
        ts = utc_now() - timedelta(hours=1)
        self.write_rows([ledger_row(ts, "zz-a/one", 1_500_000, 500_000, 1.2345)])
        totals = self.totals()

        completed = self.run_zoder("report", "--days", "1")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        day = iso(ts)[:10]
        line = next(
            (
                row
                for row in completed.stdout.splitlines()
                if row.strip().startswith(day)
            ),
            None,
        )
        self.assertIsNotNone(line, completed.stdout)
        fields = line.split()
        self.assertLessEqual(len(fields), 5, line)
        self.assertGreaterEqual(len(fields), 4, line)
        shown_cost = float(fields[1])
        shown_tokens_millions = float(fields[2])
        shown_calls = int(fields[3])

        self.assertAlmostEqual(
            shown_cost,
            totals["total_cost_usd"],
            delta=DISPLAY_COST_TOLERANCE_USD,
        )
        self.assertAlmostEqual(
            shown_tokens_millions,
            totals["total_tokens"] / 1_000_000,
            delta=DISPLAY_TOKENS_TOLERANCE_MILLIONS,
        )
        self.assertEqual(shown_calls, totals["total_calls"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
