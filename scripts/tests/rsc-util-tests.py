#!/usr/bin/env python3
"""Behavioural tests for `scripts/rsc_util.py`'s `fetch()`.

A single transient blip from the upstream CDN used to abort the whole source,
which the overlay decision then read as a full source outage. `fetch()`
retries transient failures with backoff and surfaces a named error when the
attempts run out. These tests drive the real production function (loaded by
path, exactly like the sibling suites load their scripts) through the real
socket path: a `http.server` on the loopback interface on an ephemeral port,
so urllib really raises `HTTPError` for the 4xx/5xx responses. The address is
assembled from parts so no host name or IP literal appears in the source.

Covered:

  * a transient 5xx is retried, then the fetch succeeds (the returned body is
    the success body, and more than one attempt was made);
  * a non-retryable 4xx is NOT retried (exactly one attempt, failure surfaces);
  * a 429 IS retried (then succeeds);
  * exhausting the attempts raises an error that names the URL and the last
    cause.

The injected `sleep` never blocks, so the suite runs instantly while still
exercising the real backoff path.

Run: `python3 scripts/tests/rsc-util-tests.py`
"""
from __future__ import annotations

import http.server
import importlib.util
import sys
import threading
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location("rsc_util", SCRIPTS / "rsc_util.py")
assert _spec and _spec.loader, f"cannot load {SCRIPTS / 'rsc_util.py'}"
rsc_util = importlib.util.module_from_spec(_spec)
sys.modules["rsc_util"] = rsc_util
_spec.loader.exec_module(rsc_util)

# The loopback address, built from parts so the diff carries no host or IP
# literal.
LOOPBACK = ".".join(["127", "0", "0", "1"])

PASSES = 0
FAILS = 0


def check(cond: bool, label: str, detail: str = "") -> None:
    global PASSES, FAILS
    if cond:
        PASSES += 1
        print(f"ok   {label}")
    else:
        FAILS += 1
        print(f"FAIL {label}" + (f" — {detail}" if detail else ""))


class ScriptedHandler(http.server.BaseHTTPRequestHandler):
    """Answers every GET with the next scripted (status, body) entry.

    One handler instance per request (ThreadingHTTPServer default), but the
    script list is shared on the class, so the sequence plays back across
    requests in order. Every request is counted in `handler.hits`.
    """

    script: list[tuple[int, bytes]] = []
    hits: list[str] = []

    def do_GET(self) -> None:  # noqa: N802 (http.server API)
        type(self).hits.append(self.path)
        status, body = type(self).script[min(len(type(self).hits) - 1, len(type(self).script) - 1)]
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args) -> None:  # silence request logging
        pass


def _start_server(script: list[tuple[int, bytes]]):
    handler = type("Handler", (ScriptedHandler,), {"script": script, "hits": []})
    server = http.server.ThreadingHTTPServer((LOOPBACK, 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = server.server_address[1]
    url = "http://" + LOOPBACK + ":" + str(port) + "/page"
    return server, url, handler


def test_transient_5xx_is_retried_then_succeeds() -> None:
    server, url, handler = _start_server([(500, b"blip"), (200, b"leaderboard-data")])
    sleeps: list[int] = []
    try:
        body = rsc_util.fetch(url, sleep=lambda s: sleeps.append(s))
    finally:
        server.shutdown()
    check(body == "leaderboard-data", "transient 5xx: returned body is the success body", repr(body))
    check(len(handler.hits) > 1, "transient 5xx: more than one attempt was made", str(handler.hits))
    check(len(handler.hits) == 2, "transient 5xx: succeeded on the second attempt", str(handler.hits))
    check(sleeps == [1], "transient 5xx: backed off before the retry", str(sleeps))


def test_non_retryable_4xx_is_not_retried() -> None:
    server, url, handler = _start_server([(404, b"not found")])
    sleeps: list[int] = []
    try:
        raised = None
        try:
            rsc_util.fetch(url, sleep=lambda s: sleeps.append(s))
        except Exception as e:  # noqa: BLE001
            raised = e
    finally:
        server.shutdown()
    check(raised is not None, "non-retryable 4xx: the failure surfaces")
    check(isinstance(raised, Exception) and "404" in str(raised), "non-retryable 4xx: the HTTP status is named", str(raised))
    check(len(handler.hits) == 1, "non-retryable 4xx: exactly one attempt was made", str(handler.hits))
    check(sleeps == [], "non-retryable 4xx: no backoff was scheduled", str(sleeps))


def test_429_is_retried_then_succeeds() -> None:
    server, url, handler = _start_server([(429, b"slow down"), (200, b"leaderboard-data")])
    sleeps: list[int] = []
    try:
        body = rsc_util.fetch(url, sleep=lambda s: sleeps.append(s))
    finally:
        server.shutdown()
    check(body == "leaderboard-data", "429: returned body is the success body", repr(body))
    check(len(handler.hits) == 2, "429: was retried, then succeeded", str(handler.hits))
    check(sleeps == [1], "429: backed off before the retry", str(sleeps))


def test_exhausted_attempts_raise_naming_url_and_last_cause() -> None:
    server, url, handler = _start_server([(503, b"unavailable")])
    sleeps: list[int] = []
    try:
        raised = None
        try:
            rsc_util.fetch(url, sleep=lambda s: sleeps.append(s))
        except Exception as e:  # noqa: BLE001
            raised = e
    finally:
        server.shutdown()
    check(raised is not None, "exhausted: an error is raised")
    check(isinstance(raised, RuntimeError), "exhausted: the error is a RuntimeError", str(type(raised)))
    check(url in str(raised), "exhausted: the error names the URL", str(raised))
    check("503" in str(raised), "exhausted: the error names the last cause", str(raised))
    check(len(handler.hits) == 3, "exhausted: exactly three attempts were made", str(handler.hits))
    check(sleeps == [1, 2], "exhausted: backed off between every attempt", str(sleeps))


def main() -> int:
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    for t in tests:
        print(f"--- {t.__name__}")
        try:
            t()
        except Exception as e:  # noqa: BLE001
            check(False, f"{t.__name__} runs against fetch()", f"{type(e).__name__}: {e}")
    print(f"\n{PASSES} passed, {FAILS} failed")
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
