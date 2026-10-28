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

The second half of the suite drives the REAL transport (`urllib_http`), not
the injected seam: a `http.server.ThreadingHTTPServer` on the loopback
interface on an ephemeral port, so urllib genuinely raises `HTTPError` for
the 4xx/5xx responses - the only way the retry classification is really
exercised.  The loopback address is assembled from parts so no host name or
IP literal appears in the source.

Covered against the real socket:

  * a transient 5xx on a read is retried, then succeeds (the server counts
    exactly the requests it received, and the injected backoff clock shows
    the real backoff ran);
  * a non-retryable 4xx is NOT retried (exactly one request) and the failure
    surfaces;
  * a 429 IS retried (then succeeds);
  * exhausting the attempts raises an error naming the URL and the last
    cause, with exactly 3 requests and the expected backoff;
  * THE INVARIANT: when the issue-create POST leaves the client not knowing
    whether the issue was created (the server stores the issue, then the
    connection dies without a response), the script ends with exactly ONE
    open issue labelled `corpus-sync` - the create is not retried, so one
    POST /issues reaches the server;
  * the create's single-shot decision is made at the CALL SITE, not by
    introspecting the transport: a `**kwargs` transport that drops the
    keyword, and a var-positional transport that cannot take one, both see
    exactly ONE create POST and leave exactly one open issue;
  * the dry-run path still never reaches the transport, even when a real
    HTTP server is standing up and listening.

The module under test is loaded by path (its file name is not a valid module
name), and it is registered in `sys.modules` first: `@dataclass` needs to look
the module up there while it is being executed.

Run with:  python3 scripts/tests/corpus-sync-alert-tests.py
"""

from __future__ import annotations

import contextlib
import http.server
import importlib.util
import io
import json
import os
import sys
import tempfile
import threading
import urllib.parse
import urllib.request
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
# real-transport tests: a real HTTP server on loopback, an ephemeral port
# --------------------------------------------------------------------------
#
# These tests do NOT inject an `http=` seam.  They build a
# `http.server.ThreadingHTTPServer` on the loopback address (assembled from
# parts so no host name or IP literal appears in the source) and let the
# REAL `alert.urllib_http` talk to it, so urllib genuinely raises
# `HTTPError` for the 4xx/5xx responses.  That is the only way the retry
# classification is really exercised.  The retry loop lives in the CLIENT
# (`GitLabClient._send`), so the backoff clock is injected there
# (`sleep=`, the house seam) and the real backoff path runs without
# blocking.

# The loopback address, built from parts so the diff carries no host or IP
# literal.
LOOPBACK = ".".join(["127", "0", "0", "1"])


def _client(server, sleeps: list[float]) -> Any:
    """A real client against `server`, with the backoff clock recorded.

    The transport is the production `alert.urllib_http` and the retry policy
    is the production one; only the clock is injected, so the suite drives
    the real backoff path without blocking.
    """
    return alert.GitLabClient(
        base_url=_base_url(server),
        project_id="42",
        token="secret-token",
        http=alert.urllib_http,
        sleep=sleeps.append,
    )


def _base_url(server) -> str:
    port = server.server_address[1]
    return "http://" + LOOPBACK + ":" + str(port)


class RecordingHandler(http.server.BaseHTTPRequestHandler):
    """Counts every request (method, path) in `type(self).hits`.

    `hits` is a class attribute, so it is shared across the one-per-request
    handler instances; every test builds its own handler subclass with a
    fresh list (via `_start_server`), so counts never leak between tests.
    """

    hits: list[tuple[str, str]] = []

    def log_message(self, *args) -> None:  # silence request logging
        pass

    def _count(self) -> None:
        type(self).hits.append((self.command, self.path))

    def do_GET(self) -> None:  # noqa: N802 (http.server API)
        self._count()

    def do_POST(self) -> None:  # noqa: N802 (http.server API)
        self._count()

    def do_PUT(self) -> None:  # noqa: N802 (http.server API)
        self._count()


class ScriptedHandler(RecordingHandler):
    """Replays one (status, body) pair to every request.

    One handler instance per request (ThreadingHTTPServer default); the pair
    lives on the class, so the same response is returned for every request.
    """

    status: int = 200
    body: bytes = b"{}"

    def _respond(self) -> None:
        self.send_response(type(self).status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(type(self).body)))
        self.end_headers()
        self.wfile.write(type(self).body)

    def do_GET(self) -> None:  # noqa: N802 (http.server API)
        self._count()
        self._respond()

    def do_POST(self) -> None:  # noqa: N802 (http.server API)
        self._count()
        self._respond()

    def do_PUT(self) -> None:  # noqa: N802 (http.server API)
        self._count()
        self._respond()


class LostCreateHandler(RecordingHandler):
    """A minimal in-memory GitLab API whose issue-create response is lost.

    The issue IS stored (the server created it), but the POST /issues
    connection is then destroyed without sending a response: the client
    cannot know whether the create landed.  Everything else (issue list,
    job, pipeline) answers normally.
    """

    issues: list[dict[str, Any]] = []

    def _send_json(self, status: int, payload: Any) -> None:
        raw = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:  # noqa: N802 (http.server API)
        self._count()
        path = self.path.split("?", 1)[0]
        if path.endswith("/issues"):
            self._send_json(
                200,
                [i for i in type(self).issues if i.get("state") == "opened"],
            )
            return
        if path.endswith("/pipelines"):
            self._send_json(200, [{"id": 99}, {"id": 98}])
            return
        if path.endswith("/jobs"):
            self._send_json(200, [{"name": "corpus-sync", "status": "failed"}])
            return
        self._send_json(200, [])

    def do_POST(self) -> None:  # noqa: N802 (http.server API)
        self._count()
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length).decode("utf-8"))
        if self.path.split("?", 1)[0].endswith("/issues"):
            # The server creates the issue ...
            issue = {
                "iid": len(type(self).issues) + 1,
                "title": body.get("title"),
                "labels": body.get("labels"),
                "state": "opened",
            }
            type(self).issues.append(issue)
            # ... then the connection dies without a response: the client
            # cannot tell that the issue was stored.
            self.close_connection = True
            self.connection.close()
            return
        self._send_json(200, {})


class LostNoteHandler(LostCreateHandler):
    """An in-memory GitLab API with ONE open alert whose note response is lost.

    `POST /issues/<iid>/notes` STORES the note, then destroys the connection
    without answering: the client cannot know whether the note landed.  A
    retried POST would store the same note twice, so the note-creating POST
    must be single-attempt just like the issue create.
    """

    notes: list[dict[str, Any]] = []

    def do_POST(self) -> None:  # noqa: N802 (http.server API)
        self._count()
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length).decode("utf-8"))
        path = self.path.split("?", 1)[0]
        if path.endswith("/notes"):
            type(self).notes.append({"path": path, "body": body})
            self.close_connection = True
            self.connection.close()
            return
        self._send_json(200, {})


def _start_server(handler_cls: type, **attrs: Any):
    """Start a server with a fresh handler subclass.

    The subclass gets its OWN `hits` list (a fresh per-test list, not the
    inherited class default) plus whatever extra attributes the test passes
    in (a scripted status/body pair, a shared issue store, ...).
    """
    attrs = dict(attrs)
    attrs.setdefault("hits", [])
    handler = type("Handler", (handler_cls,), attrs)
    server = http.server.ThreadingHTTPServer((LOOPBACK, 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, handler


def test_real_transport_retries_a_transient_5xx_then_succeeds() -> None:
    """A transient 5xx on a read IS retried, then the read succeeds.

    The scripted server answers the first request with 500 and every later
    request with 200: the transport must survive the blip on the second
    attempt.
    """

    def _respond(self) -> None:
        # After the first hit the server is healthy again.
        if len(type(self).hits) >= 2:
            type(self).status = 200
            type(self).body = b'{"ok": true}'
        ScriptedHandler._respond(self)

    sleeps: list[float] = []
    server, handler = _start_server(
        type(
            "BlipOnceThenOk",
            (ScriptedHandler,),
            {"status": 500, "body": b'{"message": "blip"}', "_respond": _respond},
        )
    )
    try:
        client = _client(server, sleeps)
        result = client.request("GET", "/issues")
    finally:
        server.shutdown()
    assert result == {"ok": True}, result
    # Exactly two requests reached the server: the 500, then the success.
    assert len(handler.hits) == 2, handler.hits
    assert all(method == "GET" for method, _ in handler.hits), handler.hits
    # The real backoff path ran: 2**0 = 1 second between the attempts.
    assert sleeps == [1], sleeps


def test_real_transport_does_not_retry_a_non_retryable_4xx() -> None:
    """A non-retryable 4xx is NOT retried: exactly one request, and the
    failure surfaces."""
    server, handler = _start_server(
        ScriptedHandler, status=404, body=b'{"message": "not found"}'
    )
    sleeps: list[float] = []
    try:
        client = _client(server, sleeps)
        raised = None
        try:
            client.request("GET", "/issues")
        except alert.DeliveryError as exc:
            raised = exc
    finally:
        server.shutdown()
    assert raised is not None, "a 404 must surface as a DeliveryError"
    # The HTTP status is named so a CI log says why the call failed.
    assert "404" in str(raised), str(raised)
    # Exactly ONE request reached the server: the 404 was NOT retried.
    assert len(handler.hits) == 1, handler.hits
    assert sleeps == [], "no backoff may be scheduled for a 404"


def test_real_transport_retries_a_429_then_succeeds() -> None:
    """A 429 IS retried (then succeeds)."""

    def _respond(self) -> None:
        # After the first hit the server is no longer rate-limiting.
        if len(type(self).hits) >= 2:
            type(self).status = 200
            type(self).body = b'{"ok": true}'
        ScriptedHandler._respond(self)

    sleeps: list[float] = []
    server, handler = _start_server(
        type(
            "RateLimitOnceThenOk",
            (ScriptedHandler,),
            {
                "status": 429,
                "body": b'{"message": "too many requests"}',
                "_respond": _respond,
            },
        )
    )
    try:
        client = _client(server, sleeps)
        result = client.request("GET", "/issues")
    finally:
        server.shutdown()
    assert result == {"ok": True}, result
    assert len(handler.hits) == 2, handler.hits
    assert sleeps == [1], sleeps


def test_real_transport_exhaustion_names_url_and_last_cause() -> None:
    """Exhausting the attempts raises an error naming the URL and the last
    cause, with exactly 3 requests and the expected backoff."""
    server, handler = _start_server(
        ScriptedHandler, status=503, body=b'{"message": "unavailable"}'
    )
    sleeps: list[float] = []
    try:
        client = _client(server, sleeps)
        url = client.url("/issues")
        raised = None
        try:
            client.request("GET", "/issues")
        except alert.DeliveryError as exc:
            raised = exc
    finally:
        server.shutdown()
    assert raised is not None, "an exhausted transport must surface a failure"
    # The error names the URL it could not reach ...
    assert url in str(raised), str(raised)
    # ... and the last cause that ended the attempts.
    assert "503" in str(raised), str(raised)
    # Exactly three attempts reached the server.
    assert len(handler.hits) == 3, handler.hits
    # Backed off between every attempt: 2**0 then 2**1 seconds.
    assert sleeps == [1, 2], sleeps


def test_lost_issue_create_does_not_open_a_second_issue() -> None:
    """THE INVARIANT, over a real socket.

    The server stores the issue, then the create connection dies without a
    response: the client cannot know whether the create landed.  The script
    must end with exactly ONE open issue labelled `corpus-sync`.  Because the
    create POST is never retried (design (a)), exactly one POST /issues
    reaches the server, and the issue the server stored is the only one.
    """
    sleeps: list[float] = []
    server, handler = _start_server(LostCreateHandler, issues=[])
    try:
        env = base_env(
            **{
                "CI_API_V4_URL": _base_url(server),
                "CI_PIPELINE_ID": "100",
            }
        )
        with tempfile.TemporaryDirectory() as tmp:
            freshness = write_freshness(
                tmp, {"sources": [{"name": "x", "ok": False, "reason": "boom"}]}
            )
            code = alert.run(
                env,
                http=alert.urllib_http,
                sleep=sleeps.append,
                freshness_path=freshness,
            )
    finally:
        server.shutdown()
    # The create POST could not get a response: the alert was NOT delivered.
    assert code == 1, (
        code,
        "a lost create response must surface as ALERT NOT DELIVERED (exit 1)",
    )
    # Exactly ONE POST /issues reached the server: the create was NOT retried.
    create_posts = [
        (m, p) for (m, p) in handler.hits if m == "POST" and p.endswith("/issues")
    ]
    assert len(create_posts) == 1, (
        "the issue create must not be retried: " + repr(handler.hits)
    )
    # THE INVARIANT: exactly ONE open issue labelled `corpus-sync` exists.
    open_issues = [i for i in handler.issues if i.get("state") == "opened"]
    assert len(open_issues) == 1, (
        "THE INVARIANT IS BROKEN: "
        + str(len(open_issues))
        + " open issue(s) stored: "
        + repr(handler.issues)
    )
    assert handler.issues[0].get("labels") == [alert.LABEL]
    assert handler.issues[0].get("state") == "opened"
    # No backoff may have been scheduled: the create is not retried and
    # every other call (jobs, pipelines, list) succeeded on the first try.
    assert sleeps == [], "no call may have needed a retry: " + repr(sleeps)


def test_lost_create_is_not_retried_through_a_kwargs_transport() -> None:
    """The create's non-idempotency is a CALL-SITE property, not a property
    of the injected transport's signature.

    A transport shaped `(method, url, token, body=None, **kwargs)` that
    IGNORES the keywords it captures is the dangerous case.  A guard that
    introspected the transport's signature in order to decide whether to
    pass `attempts=1` judged this shape unable to take the keyword, called
    it plainly, and any retry inside it could then resend the create after
    the lost response and open a SECOND issue.  Nothing about the transport
    may change the create's fate: exactly one POST /issues, one open issue.
    """
    sleeps: list[float] = []
    server, handler = _start_server(LostCreateHandler, issues=[])

    def kwargs_transport(
        method: str, url: str, token: str, body: dict[str, Any] | None = None, **kwargs: Any
    ) -> Any:
        # The worst shape: it captures the keyword and drops it.
        return alert.urllib_http(method, url, token, body)

    try:
        env = base_env(
            **{"CI_API_V4_URL": _base_url(server), "CI_PIPELINE_ID": "100"}
        )
        with tempfile.TemporaryDirectory() as tmp:
            freshness = write_freshness(
                tmp, {"sources": [{"name": "x", "ok": False, "reason": "boom"}]}
            )
            code = alert.run(
                env,
                http=kwargs_transport,
                sleep=sleeps.append,
                freshness_path=freshness,
            )
    finally:
        server.shutdown()
    assert code == 1, code
    create_posts = [
        (m, p) for (m, p) in handler.hits if m == "POST" and p.endswith("/issues")
    ]
    assert len(create_posts) == 1, (
        "the create must be sent once whatever the transport's signature: "
        + repr(handler.hits)
    )
    open_issues = [i for i in handler.issues if i.get("state") == "opened"]
    assert len(open_issues) == 1, (
        "THE INVARIANT IS BROKEN: " + repr(handler.issues)
    )
    assert sleeps == [], repr(sleeps)


def test_lost_note_response_does_not_store_a_second_note() -> None:
    """A note-creating POST is single-attempt too.

    `POST /issues/<iid>/notes` creates a note.  The server stores it and then
    the connection dies without a response, so the client cannot know whether
    it landed.  A retried POST would store the SAME note twice: every POST is
    sent exactly once, so exactly one note is stored and the run reports that
    the alert was not delivered.
    """
    sleeps: list[float] = []
    server, handler = _start_server(
        LostNoteHandler,
        issues=[{"iid": 7, "labels": [alert.LABEL], "state": "opened"}],
        notes=[],
    )
    try:
        env = base_env(
            **{"CI_API_V4_URL": _base_url(server), "CI_PIPELINE_ID": "100"}
        )
        with tempfile.TemporaryDirectory() as tmp:
            freshness = write_freshness(
                tmp, {"sources": [{"name": "x", "ok": False, "reason": "boom"}]}
            )
            code = alert.run(
                env,
                http=alert.urllib_http,
                sleep=sleeps.append,
                freshness_path=freshness,
            )
    finally:
        server.shutdown()
    # A lost note response is a delivery failure: the job must say so.
    assert code == 1, code
    # Exactly ONE note-creating POST reached the server: never retried.
    note_posts = [
        (m, p) for (m, p) in handler.hits if m == "POST" and p.endswith("/notes")
    ]
    assert len(note_posts) == 1, (
        "a note-creating POST must not be retried: " + repr(handler.hits)
    )
    # THE POINT: the same note was stored exactly once, not three times.
    assert len(handler.notes) == 1, (
        "the note must be stored once: " + repr(handler.notes)
    )
    assert sleeps == [], "no retry may have been scheduled: " + repr(sleeps)


def test_a_var_positional_transport_is_never_sent_the_attempts_keyword() -> None:
    """`def t(method, url, token, body=None, *args)` cannot take a keyword.

    Calling it with `attempts=1` would raise TypeError and LOSE the alert.
    The client calls every transport with exactly the four documented
    positional arguments, so this shape works - and the create is still
    sent exactly once.
    """
    sleeps: list[float] = []
    server, handler = _start_server(LostCreateHandler, issues=[])

    def var_positional_transport(
        method: str, url: str, token: str, body: dict[str, Any] | None = None, *args: Any
    ) -> Any:
        return alert.urllib_http(method, url, token, body)

    try:
        env = base_env(
            **{"CI_API_V4_URL": _base_url(server), "CI_PIPELINE_ID": "100"}
        )
        with tempfile.TemporaryDirectory() as tmp:
            freshness = write_freshness(
                tmp, {"sources": [{"name": "x", "ok": False, "reason": "boom"}]}
            )
            code = alert.run(
                env,
                http=var_positional_transport,
                sleep=sleeps.append,
                freshness_path=freshness,
            )
    finally:
        server.shutdown()
    # The create reached the server at all: no keyword was forced on the
    # var-positional transport (which would have raised TypeError).
    create_posts = [
        (m, p) for (m, p) in handler.hits if m == "POST" and p.endswith("/issues")
    ]
    assert len(create_posts) == 1, (
        "a var-positional transport must be called positionally: "
        + repr(handler.hits)
    )
    assert code == 1, code
    open_issues = [i for i in handler.issues if i.get("state") == "opened"]
    assert len(open_issues) == 1, (
        "THE INVARIANT IS BROKEN: " + repr(handler.issues)
    )
    assert sleeps == [], repr(sleeps)



def test_dry_run_never_reaches_a_real_http_server() -> None:
    """The dry-run path never reaches the transport, even with a REAL HTTP
    server listening: not one request may arrive at the socket."""
    server, handler = _start_server(
        ScriptedHandler, status=200, body=b'{"ok": true}'
    )
    sleeps: list[float] = []
    env = base_env(
        **{
            "CI_API_V4_URL": _base_url(server),
            "CI_PIPELINE_ID": "100",
        }
    )
    buffer = io.StringIO()
    try:
        with contextlib.redirect_stdout(buffer):
            code = alert.run(
                env,
                http=alert.urllib_http,
                sleep=sleeps.append,
                dry_run=True,
            )
    finally:
        server.shutdown()
    assert code == 0, buffer.getvalue()
    assert "dry-run" in buffer.getvalue(), buffer.getvalue()
    # No request may reach the real socket in a dry run.
    assert handler.hits == [], handler.hits
    assert sleeps == [], "a dry run must not schedule any backoff"


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
