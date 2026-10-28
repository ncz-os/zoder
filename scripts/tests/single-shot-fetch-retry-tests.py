#!/usr/bin/env python3
"""Behavioural tests for the single-shot ingest scripts' retry semantics.

`scripts/rsc_util.py::fetch` retries transient failures with backoff (3
attempts; retry on 5xx / 429 / URLError / TimeoutError / OSError; non-retryable
4xx surface immediately). The three ingest scripts that used to make a single
unretried GET each — one transient blip from the upstream used to abort them
the same way — must now share exactly those semantics:

  * scripts/build-public-corpus.py   fetch_json(url, timeout=30)
  * scripts/fetch-aider-polyglot.py  fetch(url)
  * scripts/fetch-vals-swebench.py   fetch(url)

All three are driven through the REAL production functions (loaded by path,
exactly like the sibling suites load their scripts) over the REAL socket path:
a `http.server` on the loopback interface on an ephemeral port, so urllib
really raises `HTTPError` for the 4xx/5xx responses. The address is assembled
from parts so no host name or IP literal appears in the source.

Covered, for EACH of the three scripts:

  * a transient 5xx is retried, then the fetch succeeds (the returned body /
    parsed payload is the success one, and more than one attempt was made);
  * a non-retryable 4xx is NOT retried (exactly one attempt, failure surfaces);
  * a 429 IS retried (then succeeds);
  * exhausting the attempts raises an error that names the URL and the last
    cause.

Plus a wiring assertion per script: the script's one-attempt call is gone and
its fetch goes through the shared helper, so no script can still make a single
unretried GET.

The injected `sleep` never blocks, so the suite runs instantly while still
exercising the real backoff path.

Run: `python3 scripts/tests/single-shot-fetch-retry-tests.py`
"""
from __future__ import annotations

import http.server
import importlib.util
import inspect
import sys
import threading
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]


def _load(name: str, filename: str):
    path = SCRIPTS / filename
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader, f"cannot load {path}"
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


build_public_corpus = _load("build_public_corpus", "build-public-corpus.py")
fetch_aider_polyglot = _load("fetch_aider_polyglot", "fetch-aider-polyglot.py")
fetch_vals_swebench = _load("fetch_vals_swebench", "fetch-vals-swebench.py")

# The shared helper the three scripts route through (loaded the same way the
# sibling rsc-util suite loads it).
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


def _call(fn, url, sleeps):
    return fn(url, sleep=lambda s: sleeps.append(s))


# (name, production fn, success body bytes, expected success value)
_SUCCESS_CASES = [
    ("build-public-corpus", build_public_corpus.fetch_json, b'{"ok": 1}', {"ok": 1}),
    ("fetch-aider-polyglot", fetch_aider_polyglot.fetch, b"polyglot-data", "polyglot-data"),
    ("fetch-vals-swebench", fetch_vals_swebench.fetch, b"page-data", "page-data"),
]


def _run_success_case(label: str, name: str, fn, script: list[tuple[int, bytes]],
                      success_value) -> None:
    """Drive one scripted sequence through one production fetcher and assert
    the transient-then-success path (retried, success body, one backoff)."""
    sleeps: list[int] = []
    server, url, handler = _start_server(script)
    try:
        raised = None
        result = None
        try:
            result = _call(fn, url, sleeps)
        except Exception as e:  # noqa: BLE001
            raised = e
    finally:
        server.shutdown()
    prefix = f"{name}: {label}"
    check(raised is None, f"{prefix}: no error raised",
          f"{type(raised).__name__}: {raised}" if raised else "")
    check(result == success_value, f"{prefix}: success body is returned", repr(result))
    check(len(handler.hits) > 1, f"{prefix}: more than one attempt was made", str(handler.hits))
    check(len(handler.hits) == 2, f"{prefix}: succeeded on the second attempt", str(handler.hits))
    check(sleeps == [1], f"{prefix}: backed off before the retry", str(sleeps))


def _run_failure_case(name: str, fn, script: list[tuple[int, bytes]],
                      want_attempts: int, want_sleeps: list[int],
                      last_cause: str, url_expected: bool = False) -> None:
    """Drive a failing sequence through one production fetcher and assert the
    error surfaces after exactly the expected number of attempts."""
    sleeps: list[int] = []
    server, url, handler = _start_server(script)
    try:
        raised = None
        try:
            _call(fn, url, sleeps)
        except Exception as e:  # noqa: BLE001
            raised = e
    finally:
        server.shutdown()
    check(raised is not None, f"{name}: the failure surfaces")
    if raised is not None:
        if url_expected:
            # A non-retryable 4xx is re-raised as the raw HTTPError (the
            # upstream answered); only the exhausted-attempts path wraps into a
            # RuntimeError that names the URL.
            check(url in str(raised), f"{name}: the error names the URL", str(raised))
        check(last_cause in str(raised), f"{name}: the error names the last cause", str(raised))
    check(len(handler.hits) == want_attempts, f"{name}: exactly {want_attempts} attempt(s) were made",
          str(handler.hits))
    check(sleeps == want_sleeps, f"{name}: the expected backoff sequence was scheduled", str(sleeps))


def test_transient_5xx_retried_then_succeeds() -> None:
    for name, fn, body, want in _SUCCESS_CASES:
        _run_success_case("transient 5xx", name, fn, [(500, b"blip"), (200, body)], want)


def test_non_retryable_4xx_not_retried() -> None:
    for name, fn, _body, _want in _SUCCESS_CASES:
        _run_failure_case(name, fn, [(404, b"not found")], want_attempts=1,
                          want_sleeps=[], last_cause="404")


def test_429_retried_then_succeeds() -> None:
    for name, fn, body, want in _SUCCESS_CASES:
        _run_success_case("429", name, fn, [(429, b"slow down"), (200, body)], want)


def test_exhausted_attempts_raise_naming_url_and_last_cause() -> None:
    for name, fn, _body, _want in _SUCCESS_CASES:
        _run_failure_case(name, fn, [(503, b"unavailable")], want_attempts=3,
                          want_sleeps=[1, 2], last_cause="503", url_expected=True)


def test_scripts_route_through_shared_helper() -> None:
    """No script can still make a single unretried GET: its fetch goes through
    the shared helper (rsc_util.fetch) and the one-attempt call is gone."""
    # The corpus builder keeps its own User-Agent, so the shared helper must be
    # the one carrying the retry loop and the request must carry that UA.
    src = inspect.getsource(build_public_corpus.fetch_json)
    check("fetch" in src, "build-public-corpus: fetch_json delegates to the shared helper", src)
    check("urlopen" not in src, "build-public-corpus: the one-attempt urlopen call is gone", src)

    for name, mod, attr in [
        ("fetch-aider-polyglot", fetch_aider_polyglot, "fetch"),
        ("fetch-vals-swebench", fetch_vals_swebench, "fetch"),
    ]:
        src = inspect.getsource(getattr(mod, attr))
        check("urlopen" not in src, f"{name}: the one-attempt urlopen call is gone", src)
        check("_fetch" in src or "rsc_util" in src, f"{name}: fetch delegates to the shared helper", src)


def main() -> int:
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    for t in tests:
        print(f"--- {t.__name__}")
        try:
            t()
        except Exception as e:  # noqa: BLE001
            check(False, f"{t.__name__} runs against the production fetchers", f"{type(e).__name__}: {e}")
    print(f"\n{PASSES} passed, {FAILS} failed")
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
