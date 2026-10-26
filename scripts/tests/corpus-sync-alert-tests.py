#!/usr/bin/env python3
"""Offline tests for `scripts/ci/corpus-sync-alert.py`.

The GitLab API is faked with an injected HTTP function, so the tests never
touch the network and never need a token.

What is covered:

  * two consecutive failures of `corpus-sync`  -> one issue is opened
  * one failure after a success              -> nothing happens
  * an open issue plus a new alert           -> a note, never a second issue
  * a healthy run with an open issue         -> a note and the issue is closed
  * a healthy run with several open issues   -> every one of them is closed
  * an 8-day-old overlay                     -> an alert
  * prices not ok                           -> an alert naming the source
  * no token                                 -> "ALERT NOT DELIVERED", exit 1
  * `--dry-run`                              -> no API call, and a printed plan

The module under test is loaded by path (its file name is not a valid module
name), and it is registered in `sys.modules` first: `@dataclass` needs to look
the module up there while it is being executed.

Run with:  python3 scripts/tests/corpus-sync-alert-tests.py
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import sys
import tempfile
import urllib.parse
from typing import Any

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
SCRIPT = os.path.join(REPO, "scripts", "ci", "corpus-sync-alert.py")

_spec = importlib.util.spec_from_file_location("corpus_sync_alert", SCRIPT)
assert _spec and _spec.loader, f"cannot load {SCRIPT}"
alert = importlib.util.module_from_spec(_spec)
sys.modules["corpus_sync_alert"] = alert
_spec.loader.exec_module(alert)


class FakeApi:
    """A minimal in-memory GitLab API.

    `jobs` maps a pipeline id to the status of its `corpus-sync` job;
    `issues` is the list of issues (open ones are those without a state);
    every request is recorded in `calls`.
    """

    def __init__(
        self,
        jobs: dict[str, str] | None = None,
        issues: list[dict[str, Any]] | None = None,
        pipelines: list[dict[str, Any]] | None = None,
    ) -> None:
        self.jobs = jobs or {}
        self.issues = list(issues or [])
        self.pipelines = list(pipelines or [])
        self.calls: list[str] = []
        self.requests: list[dict[str, Any]] = []

    # -- the injected transport -------------------------------------------
    def http(
        self,
        method: str,
        url: str,
        token: str,
        body: dict[str, Any] | None = None,
    ) -> Any:
        parsed = urllib.parse.urlparse(url)
        path = parsed.path
        self.calls.append(f"{method} {path}")
        self.requests.append({"method": method, "path": path, "body": body})

        if path.endswith("/jobs"):
            pipeline_id = path.rsplit("/", 2)[-2]
            status = self.jobs.get(pipeline_id)
            if status is None:
                return []
            return [{"name": "corpus-sync", "id": 1, "status": status}]

        if path.endswith("/pipelines"):
            return list(self.pipelines)

        if path.endswith("/issues") and method == "GET":
            return [i for i in self.issues if i.get("state") != "closed"]

        if path.endswith("/issues") and method == "POST":
            issue = {
                "iid": len(self.issues) + 1,
                "title": (body or {}).get("title"),
                "body": (body or {}).get("body"),
                "labels": (body or {}).get("labels"),
                "state": "opened",
            }
            self.issues.append(issue)
            return issue

        if path.endswith("/notes") and method == "POST":
            iid = path.split("/issues/")[1].split("/")[0]
            note = {
                "iid": len(self.issues) + 1,
                "issue": iid,
                "body": (body or {}).get("body"),
            }
            self.notes = getattr(self, "notes", [])
            self.notes.append(note)
            return note

        if method == "PUT":
            iid = path.rsplit("/", 1)[-1]
            for issue in self.issues:
                if str(issue.get("iid")) == str(iid):
                    issue["state"] = (body or {}).get("state_event", "closed")
            return {"iid": int(iid) if str(iid).isdigit() else 0}

        return {}

    # -- helpers ------------------------------------------------------------
    def notes_for(self, iid: Any) -> list[dict[str, Any]]:
        return [n for n in getattr(self, "notes", []) if str(n["issue"]) == str(iid)]

    def open_issues(self) -> list[dict[str, Any]]:
        return [i for i in self.issues if i.get("state") == "opened"]


def base_env(**extra: str) -> dict[str, str]:
    env = {
        "CI_API_V4_URL": "https://gitlab.example/api/v4",
        "CI_PROJECT_ID": "42",
        "CI_PIPELINE_ID": "100",
        "CI_PIPELINE_URL": "https://gitlab.example/proj/-/pipelines/100",
        "CI_DEFAULT_BRANCH": "master",
        "CORPUS_ALERT_TOKEN": "secret-token",
    }
    env.update(extra)
    return env


def write_freshness(directory: str, report: dict[str, Any]) -> str:
    path = os.path.join(directory, "freshness.json")
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(report, handle)
    return path


FRESH_OK = {
    "schema": 2,
    "ts": "2026-10-26T04:00:00Z",
    "sources": [
        {"name": "arena", "kind": "benchmark", "ok": True},
        {"name": "litellm", "kind": "pricing", "ok": True},
    ],
    "prices": {"ok": True},
    "overlay": {"stale": False, "built_at": "2026-10-25", "age_days": 1},
}


# --------------------------------------------------------------------------
# tests
# --------------------------------------------------------------------------


def test_two_consecutive_failures_open_an_issue() -> None:
    api = FakeApi(
        jobs={"100": "failed", "99": "failed", "98": "success"},
        pipelines=[{"id": 100}, {"id": 99}, {"id": 98}],
    )
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    created = [
        r
        for r in api.requests
        if r["method"] == "POST" and r["path"].endswith("/issues")
    ]
    assert len(created) == 1, api.calls
    body = created[0]["body"]
    assert body["title"] == "corpus-sync: scheduled data sync is failing or stale"
    assert body["labels"] == ["corpus-sync"]
    assert "https://gitlab.example/proj/-/pipelines/100" in body["body"]
    assert "Consecutive corpus-sync failures: 2" in body["body"]
    assert len(api.open_issues()) == 1


def test_one_failure_after_a_success_does_nothing() -> None:
    api = FakeApi(
        jobs={"100": "failed", "99": "success"},
        pipelines=[{"id": 100}, {"id": 99}],
    )
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    assert api.open_issues() == [], api.calls
    assert not [r for r in api.requests if r["method"] in ("POST", "PUT")], api.calls


def test_open_issue_plus_new_alert_adds_a_note_only() -> None:
    existing = {
        "iid": 7,
        "title": alert.ISSUE_TITLE,
        "state": "opened",
        "labels": ["corpus-sync"],
    }
    api = FakeApi(
        jobs={"100": "failed", "99": "failed"},
        pipelines=[{"id": 100}, {"id": 99}],
        issues=[existing],
    )
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    assert len(api.open_issues()) == 1, api.calls
    assert len(api.issues) == 1, "no second issue may be opened"
    notes = api.notes_for(7)
    assert len(notes) == 1, api.calls
    assert "Consecutive corpus-sync failures: 2" in notes[0]["body"]
    assert not [
        r
        for r in api.requests
        if r["method"] == "POST" and r["path"].endswith("/issues")
    ]


def test_healthy_with_an_open_issue_notes_and_closes_it() -> None:
    existing = {
        "iid": 7,
        "title": alert.ISSUE_TITLE,
        "state": "opened",
        "labels": ["corpus-sync"],
    }
    api = FakeApi(
        jobs={"100": "success"},
        pipelines=[{"id": 100}],
        issues=[existing],
    )
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    notes = api.notes_for(7)
    assert len(notes) == 1, api.calls
    assert "recovered" in notes[0]["body"].lower()
    assert api.open_issues() == [], "the issue must be closed again"


def test_stale_overlay_of_eight_days_alerts() -> None:
    api = FakeApi(jobs={"100": "success"}, pipelines=[{"id": 100}])
    report = json.loads(json.dumps(FRESH_OK))
    report["overlay"] = {"stale": True, "built_at": "2026-10-18", "age_days": 8}
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, report)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    created = [
        r
        for r in api.requests
        if r["method"] == "POST" and r["path"].endswith("/issues")
    ]
    assert len(created) == 1, api.calls
    body = created[0]["body"]
    assert "Overlay age: 8 days" in body["body"], body["body"]
    assert "Overlay stale: yes" in body["body"], body["body"]


def test_prices_not_ok_alerts_and_names_the_source() -> None:
    api = FakeApi(jobs={"100": "success"}, pipelines=[{"id": 100}])
    report = json.loads(json.dumps(FRESH_OK))
    report["prices"] = {"ok": False, "reason": "upstream refused"}
    report["sources"] = [
        {
            "name": "litellm",
            "kind": "pricing",
            "ok": False,
            "reason": "upstream refused",
        }
    ]
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, report)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    created = [
        r
        for r in api.requests
        if r["method"] == "POST" and r["path"].endswith("/issues")
    ]
    assert len(created) == 1, api.calls
    body = created[0]["body"]
    assert "litellm (pricing): upstream refused" in body["body"], body["body"]
    assert "Prices ok: no" in body["body"], body["body"]


def test_healthy_closes_every_open_duplicate() -> None:
    """A healthy run leaves NO open `corpus-sync` issue behind.

    Even when several alerts are open (a duplicated alert, a leftover from a
    previous run), the healthy run notes and closes each of them: "keep
    exactly one open issue" must not leave a stale duplicate open.
    """
    issues = [
        {"iid": 7, "title": alert.ISSUE_TITLE, "state": "opened"},
        {"iid": 8, "title": alert.ISSUE_TITLE, "state": "opened"},
    ]
    api = FakeApi(jobs={"100": "success"}, pipelines=[{"id": 100}], issues=issues)
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    assert api.open_issues() == [], api.calls
    for iid in (7, 8):
        assert api.notes_for(iid), f"issue {iid} got no note: {api.calls}"


def test_alert_notes_the_primary_and_closes_the_duplicate() -> None:
    issues = [
        {"iid": 7, "title": alert.ISSUE_TITLE, "state": "opened"},
        {"iid": 8, "title": alert.ISSUE_TITLE, "state": "opened"},
    ]
    api = FakeApi(
        jobs={"100": "failed", "99": "failed"},
        pipelines=[{"id": 100}, {"id": 99}],
        issues=issues,
    )
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    assert len(api.issues) == 2, "no new issue may be opened"
    assert len(api.open_issues()) == 1, api.calls
    assert api.notes_for(7), api.calls
    assert api.notes_for(8), "the duplicate must be noted before it is closed"


def test_dry_run_without_a_project_id_still_prints_a_plan() -> None:
    """A dry run needs no project id and no token: it only prints a plan."""
    api = FakeApi()
    env = base_env()
    env.pop("CI_PROJECT_ID")
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        code = alert.run(env, http=api.http, dry_run=True)

    assert code == 0, buffer.getvalue()
    assert api.calls == [], api.calls
    assert "dry-run" in buffer.getvalue(), buffer.getvalue()


def test_a_malformed_issue_without_an_iid_is_ignored() -> None:
    """An entry without an `iid` must not produce a `/issues/None/...` call."""
    api = FakeApi(
        jobs={"100": "success"},
        pipelines=[{"id": 100}],
        issues=[{"title": alert.ISSUE_TITLE, "state": "opened"}],
    )
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        code = alert.run(base_env(), http=api.http, freshness_path=freshness)

    assert code == 0, code
    assert not [r for r in api.requests if "None" in r["path"]], api.calls


def test_dry_run_without_a_token_still_exits_zero() -> None:
    """A dry run sends nothing, so a missing token is not a failure.

    It prints the note ("a real run would report ALERT NOT DELIVERED") and
    the plan, and exits 0 - a dry run must never mark the CI job failed.
    """
    api = FakeApi()
    env = base_env()
    env.pop("CORPUS_ALERT_TOKEN")
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        code = alert.run(env, http=api.http, dry_run=True)

    assert code == 0, buffer.getvalue()
    printed = buffer.getvalue()
    assert "dry-run" in printed, printed
    assert "no token set" in printed, printed
    assert api.calls == [], "a dry run must not reach the transport"


def test_run_defaults_the_freshness_path_and_tolerates_a_missing_file() -> None:
    """`freshness_path` is optional: the default is used, and a missing
    report is not an error (the staleness facts are simply unknown)."""
    api = FakeApi(jobs={"100": "success"}, pipelines=[{"id": 100}])
    env = base_env()
    buffer = io.StringIO()
    # No freshness_path argument at all: the default must be used, and the
    # call must not raise.
    with contextlib.redirect_stdout(buffer):
        code = alert.run(env, http=api.http)

    assert code == 0, buffer.getvalue()

    # A report that is not there is handled, not fatal.
    missing = os.path.join(tempfile.gettempdir(), "zoder-no-such-freshness-report.json")
    if os.path.exists(missing):  # pragma: no cover - defensive
        os.remove(missing)
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        code = alert.run(env, http=api.http, freshness_path=missing)

    assert code == 0, buffer.getvalue()
    assert "no freshness report" in buffer.getvalue(), buffer.getvalue()


def test_dry_run_never_reaches_the_transport() -> None:
    """Even a transport that would fail is never called in a dry run."""

    def exploding_http(method, url, token, body):
        raise AssertionError(f"a dry run must not call the API: {method} {url}")

    env = base_env()
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        code = alert.run(env, http=exploding_http, dry_run=True)

    assert code == 0, buffer.getvalue()
    assert "dry-run" in buffer.getvalue(), buffer.getvalue()


def test_run_always_returns_an_integer_exit_code() -> None:
    """Every path of `run()` returns an int exit code - never `None`.

    `sys.exit(run(...))` with a `None` would exit 0 and mask a failure, so
    the contract is pinned here for the success, no-token, dry-run and
    API-failure paths.
    """

    def failing_http(method, url, token, body):
        raise OSError("the API is unreachable")

    api = FakeApi(jobs={"100": "success"}, pipelines=[{"id": 100}])
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)

        code = alert.run(base_env(), http=api.http, freshness_path=freshness)
        assert isinstance(code, int) and not isinstance(code, bool), repr(code)
        assert code == 0, "a successful run exits 0"

        env = base_env()
        env.pop("CORPUS_ALERT_TOKEN")
        code = alert.run(env, http=api.http, freshness_path=freshness)
        assert isinstance(code, int) and not isinstance(code, bool), repr(code)
        assert code == 1, "a run that cannot deliver exits 1"

        code = alert.run(
            base_env(), http=api.http, dry_run=True, freshness_path=freshness
        )
        assert isinstance(code, int) and not isinstance(code, bool), repr(code)
        assert code == 0, "a dry run exits 0"

        code = alert.run(base_env(), http=failing_http, freshness_path=freshness)
        assert isinstance(code, int) and not isinstance(code, bool), repr(code)
        assert code == 1, "a failing API call exits 1"


def test_no_token_reports_alert_not_delivered_and_exits_1() -> None:
    api = FakeApi(jobs={"100": "failed", "99": "failed"})
    env = base_env()
    env.pop("CORPUS_ALERT_TOKEN")
    code = alert.run(env, http=api.http, freshness_path="does-not-exist.json")
    assert code == 1, code
    assert api.calls == [], "no API call may be made without a token"


def test_module_exposes_the_public_surface_the_job_depends_on() -> None:
    """The constants, attributes and signature the alert job relies on.

    A reviewer (or a future refactor) must be able to see that the script
    really provides them: the issue title, the label, the planned/sent call
    lists, and a `run()` that takes the freshness path and a dry-run flag.
    """
    import inspect

    assert alert.ISSUE_TITLE == "corpus-sync: scheduled data sync is failing or stale"
    assert alert.LABEL == "corpus-sync"
    assert alert.DEFAULT_API_URL
    assert alert.FRESHNESS_PATH == "corpus/freshness.json"
    assert "CORPUS_ALERT_TOKEN" in alert.TOKEN_VARS
    assert "GITLAB_PUSH_TOKEN" in alert.TOKEN_VARS

    client = alert.GitLabClient(
        base_url="https://gitlab.example/api/v4",
        project_id="42",
        token="secret-token",
        http=FakeApi().http,
    )
    assert client.planned_calls == []
    assert client.sent_calls == []

    signature = inspect.signature(alert.run)
    for name in ("env", "http", "dry_run", "freshness_path"):
        assert name in signature.parameters, signature


def test_dry_run_client_plans_calls_without_sending_them() -> None:
    """The client itself: a dry-run plans the calls and sends none of them."""
    api = FakeApi()
    client = alert.GitLabClient(
        base_url="https://gitlab.example/api/v4",
        project_id="42",
        token="secret-token",
        http=api.http,
        dry_run=True,
    )
    client.request("GET", "/issues", params={"state": "opened"})
    client.request("POST", "/issues", body={"title": "x"})

    assert client.planned_calls == ["GET /issues", "POST /issues"]
    assert client.sent_calls == [], "a dry-run must not reach the transport"
    assert api.calls == [], "no HTTP request may be made in dry-run mode"


def test_dry_run_writes_nothing_and_reports_the_plan() -> None:
    api = FakeApi(jobs={"100": "failed", "99": "failed"})
    buffer = io.StringIO()
    with tempfile.TemporaryDirectory() as tmp:
        freshness = write_freshness(tmp, FRESH_OK)
        with contextlib.redirect_stdout(buffer):
            code = alert.run(
                base_env(), http=api.http, dry_run=True, freshness_path=freshness
            )

    assert code == 0, code
    assert api.calls == [], api.calls
    assert api.requests == [], api.calls
    printed = buffer.getvalue()
    assert "dry-run" in printed, printed
    assert "would call" in printed, printed


def main() -> int:
    tests = [
        value for name, value in sorted(globals().items()) if name.startswith("test_")
    ]
    failures = 0
    for test in tests:
        try:
            test()
        except AssertionError as exc:
            failures += 1
            print(f"FAIL {test.__name__}: {exc}")
        except Exception as exc:  # noqa: BLE001
            failures += 1
            print(f"ERROR {test.__name__}: {type(exc).__name__}: {exc}")
        else:
            print(f"ok   {test.__name__}")
    print(f"\n{len(tests) - failures}/{len(tests)} tests passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
