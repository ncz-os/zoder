#!/usr/bin/env python3
"""Turn a failing or stale scheduled corpus-sync into exactly one tracked issue.

The daily GitLab schedule publishes the model corpus, the pricing catalog and
the benchmark overlay.  Between 2026-10-14 and 2026-10-26 that job failed
every single day and nobody noticed for 13 days: the published data stayed at
2026-10-13 while every client kept routing on a two-week-old index.  A red
pipeline is not monitoring unless somebody is watching it.

This script runs after every *scheduled* pipeline (see the `corpus-sync-alert`
job in `.gitlab-ci.yml`) and turns the failure into a durable signal:

  * two consecutive failures of the `corpus-sync` job, or
  * a freshness report that says the benchmark overlay is older than 7 days /
    flagged stale, or the prices are not ok

become ONE open issue labelled `corpus-sync`.  While the problem persists the
script adds notes to that same issue (it never opens a second one); the first
healthy scheduled run adds a final note and closes the issue itself.

Only the GitLab REST API and the standard library are used - no third-party
dependencies - so it runs in a plain `python:3.12` container.

The delivery path has its own retry, because the script's one job is to be
the last line of monitoring: a single transient blip from the GitLab API
(5xx, 429, dropped connection, read timeout) must not decide the fate of the
alert.  `urllib_http` therefore retries the same failure classes the house
fetchers retry (`scripts/rsc_util.py::fetch`, `scripts/fetch-arena.py`'s
`fetch_json`): 3 attempts total, `2**attempt` seconds of backoff between
them; every other 4xx is answered and NOT retried.  The issue-create POST is
the one call that is not idempotent - a blind retry after a lost response
would open a second `corpus-sync` issue and break `act()`'s invariant - so
the create is never retried (design (a)): a transient failure there raises
`DeliveryError`, the CI job shows "ALERT NOT DELIVERED", and the next
scheduled run opens the issue.

Environment (all GitLab predefined variables plus one token):

  CI_API_V4_URL        e.g. https://gitlab.com/api/v4
  CI_PROJECT_ID        the project id (numeric or URL-encoded path)
  CI_PIPELINE_ID       the pipeline that ran the sync
  CI_PIPELINE_URL      the human-readable pipeline URL (quoted in the issue)
  CI_DEFAULT_BRANCH    the branch the schedule publishes to
  CORPUS_ALERT_TOKEN   masked project variable, `api` scope (preferred)
  GITLAB_PUSH_TOKEN    fallback token

`--dry-run` prints the decision and the API calls that would be made without
making any of them.

Public surface
--------------
This module is deliberately small and flat; everything the CI job and the
offline tests use is listed here:

    ISSUE_TITLE       "corpus-sync: scheduled data sync is failing or stale"
    LABEL             "corpus-sync"          (the one label the issue carries)
    TOKEN_VARS        ("CORPUS_ALERT_TOKEN", "GITLAB_PUSH_TOKEN")
    DEFAULT_API_URL   "https://gitlab.com/api/v4"
    FRESHNESS_PATH    "corpus/freshness.json" (the pipeline artifact)
    MAX_OVERLAY_AGE_DAYS  7

    class GitLabClient   - the REST client.
                          planned_calls: every call the script intends to make
                          sent_calls:    the calls that actually went out
                          (empty in a dry run - the transport is not reached)
    class SyncState      - the gathered facts (job status, freshness report).
    class Decision       - state in {"ALERT", "HEALTHY", "QUIET", "UNKNOWN"}.
    class DeliveryError  - raised when the alert cannot be delivered.

    def decide(state) -> Decision
    def act(client, decision, state) -> str
        The invariant: after `act` there is at most ONE open issue labelled
        `corpus-sync`. On a healthy run every open alert (primary and
        duplicates alike) is noted and closed; on an alert run the primary
        is noted and the duplicates are folded into it.
    def run(env, http=None, dry_run=False, freshness_path=FRESHNESS_PATH) -> int
        The whole decision + action. Returns the process exit code:
          0 - the alert was delivered, or was not needed, or this was a dry
              run (a dry run sends nothing, so it never fails: it prints the
              decision and the planned calls).
          1 - the alert was NOT delivered: no token, or a failing API call.
              In that case it prints
              "corpus-sync-alert: ALERT NOT DELIVERED: <reason>".
        EVERY path returns an int (0 or 1) - never None: `sys.exit(run(...))`
        with a None would exit 0 and mask a real failure.
        `freshness_path` defaults to FRESHNESS_PATH; a missing report is not
        an error - the staleness facts are simply unknown.
    def run_dry(env, http=None, freshness_path=FRESHNESS_PATH) -> int
        The dry-run path: it never authenticates, never reaches the
        transport, prints the decision and the planned calls, and always
        exits 0.
"""

from __future__ import annotations

import argparse
import inspect
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

LABEL = "corpus-sync"
ISSUE_TITLE = "corpus-sync: scheduled data sync is failing or stale"
DEFAULT_API_URL = "https://gitlab.com/api/v4"
TOKEN_VARS = ("CORPUS_ALERT_TOKEN", "GITLAB_PUSH_TOKEN")
MAX_OVERLAY_AGE_DAYS = 7
FRESHNESS_PATH = "corpus/freshness.json"
HTTP_TIMEOUT = 30.0

# How many previous scheduled pipelines are inspected when counting
# consecutive failures.  The outage this guards against lasted 13 days; ten
# is enough to say "it has been broken for a while" without walking the whole
# schedule history on every run.
MAX_LOOKBACK = 10


class DeliveryError(RuntimeError):
    """The alert could not be delivered (no token, or an API call failed)."""


def not_delivered(reason: str) -> str:
    return f"corpus-sync-alert: ALERT NOT DELIVERED: {reason}"


# --------------------------------------------------------------------------
# state gathering
# --------------------------------------------------------------------------


@dataclass
class SyncState:
    """Everything the decision depends on, gathered defensively."""

    pipeline_url: str = ""
    this_failed: bool | None = None
    prev_failed: bool | None = None
    consecutive_failures: int | None = None
    failing_sources: list[dict[str, Any]] = field(default_factory=list)
    prices_ok: bool | None = None
    prices_reason: str = ""
    overlay_age_days: int | None = None
    overlay_stale: bool | None = None
    overlay_built_at: str = ""


def _as_bool(value: Any) -> bool | None:
    if isinstance(value, bool):
        return value
    return None


def _as_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str):
        try:
            return int(value.strip())
        except ValueError:
            return None
    return None


def read_freshness(path: str) -> dict[str, Any]:
    """Read the freshness report published as a pipeline artifact.

    Every field is optional: the report schema changed while this script was
    being written, so an older report (or no report at all) must degrade to
    "unknown", never to a crash and never to a false "everything is fine".
    """
    try:
        with open(path, "r", encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def state_from_freshness(report: dict[str, Any]) -> SyncState:
    """Fold a freshness report into the parts of the state it can answer."""
    state = SyncState()

    prices = report.get("prices")
    if isinstance(prices, dict):
        state.prices_ok = _as_bool(prices.get("ok"))
        reason = prices.get("reason") or prices.get("error")
        if isinstance(reason, str):
            state.prices_reason = reason.strip()

    overlay = report.get("overlay")
    if isinstance(overlay, dict):
        state.overlay_age_days = _as_int(overlay.get("age_days"))
        state.overlay_stale = _as_bool(overlay.get("stale"))
        built_at = overlay.get("built_at")
        if isinstance(built_at, str):
            state.overlay_built_at = built_at.strip()

    # Top-level duplicates (older/newer report shape) fill in the gaps.
    if state.overlay_age_days is None:
        state.overlay_age_days = _as_int(report.get("overlay_age_days"))
    if state.overlay_stale is None:
        state.overlay_stale = _as_bool(report.get("overlay_stale"))

    sources = report.get("sources")
    if isinstance(sources, list):
        for entry in sources:
            if not isinstance(entry, dict):
                continue
            ok = _as_bool(entry.get("ok"))
            if ok is False:
                state.failing_sources.append(
                    {
                        "name": str(entry.get("name") or "unknown"),
                        "kind": str(entry.get("kind") or "unknown"),
                        "reason": str(entry.get("reason") or "no reason given"),
                    }
                )
    return state


# --------------------------------------------------------------------------
# GitLab API client
# --------------------------------------------------------------------------

# The transport signature: (method, url, token, body) -> decoded JSON.
#
# It is spelled with `...` on purpose: the alias is evaluated at import time,
# and a `dict[str, Any] | None` there would need Python 3.10+. The script runs
# in a `python:3.12` CI image, but a reviewer (or a laptop) with an older
# interpreter must still be able to import it and run the offline tests.
HttpFn = Callable[..., Any]


# The GitLab API answers transient 5xx / 429 and drops connections, and the
# alert's own delivery path is the thing this script monitors: one blip must
# not decide the fate of the alert.  Retry the same failure classes the house
# fetchers retry (`scripts/rsc_util.py::fetch`, `scripts/fetch-arena.py`'s
# `fetch_json`): 3 attempts total, `2**attempt` seconds of backoff between
# them.  Every other 4xx is answered and NOT retried - the upstream said no,
# retrying only burns time.
HTTP_ATTEMPTS = 3


def _http_call_once(
    method: str,
    url: str,
    token: str,
    body: dict[str, Any] | None = None,
) -> Any:
    """One urllib round-trip against the GitLab API.

    Raises `urllib.error.HTTPError` for any 4xx/5xx response and
    `urllib.error.URLError` / `OSError` for a dropped or timed-out
    connection: the retry policy in `urllib_http` owns the classification.
    """
    data = None
    headers = {"PRIVATE-TOKEN": token, "Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT) as response:
        raw = response.read().decode("utf-8", "replace")
    if not raw.strip():
        return {}
    return json.loads(raw)


def _is_retryable(exc: Exception) -> bool:
    """House retry classification: 5xx, 429, and network-level failures.

    Every other `HTTPError` is a decision by the upstream - retrying it only
    burns time - so it is not retryable.
    """
    if isinstance(exc, urllib.error.HTTPError):
        return exc.code >= 500 or exc.code == 429
    # URLError is an OSError subclass, as are TimeoutError and the plain
    # connection/timeout errors urllib raises.
    return isinstance(exc, OSError)


def urllib_http(
    method: str,
    url: str,
    token: str,
    body: dict[str, Any] | None = None,
    attempts: int = HTTP_ATTEMPTS,
    sleep: Callable[[float], None] = time.sleep,
) -> Any:
    """Default HTTP transport: a plain urllib call against the GitLab API,
    retried with backoff on transient failures.

    `attempts` is the TOTAL number of tries (default 3) and `sleep` is the
    backoff clock (house seam: `rsc_util.fetch(..., sleep=time.sleep)`),
    injected so the test suite can drive the real backoff path without
    blocking.  When the attempts run out, the error names the URL and the
    LAST cause, so a CI log says which call failed and why.

    NOTE: this is the shared transport for every API call, including the
    issue-create POST.  The create POST must never be retried blindly - a
    retry after a lost response opens a SECOND `corpus-sync` issue and breaks
    `act()`'s invariant.  The create path therefore passes `attempts=1`
    (see `GitLabClient.request`); everything else retries freely.
    """
    last: Exception | None = None
    for attempt in range(attempts):
        try:
            return _http_call_once(method, url, token, body)
        except Exception as exc:
            if not _is_retryable(exc):
                raise
            last = exc
            if attempt + 1 < attempts:
                sleep(2**attempt)
    if last is None:  # pragma: no cover - defensive
        raise RuntimeError(f"{method} {url} failed after {attempts} attempts")
    raise RuntimeError(
        f"{method} {url} failed after {attempts} attempts ({last})"
    ) from last


class GitLabClient:
    """A very small GitLab REST client that records every call it makes.

    Two lists matter, and they are not the same thing:

    * `planned_calls` - every call the script intends to make. In dry-run mode
      this is exactly what gets printed and NOTHING is sent.
    * `sent_calls` - the calls that actually went out over the network. It
      stays empty in dry-run mode: the transport is never reached.
    """

    def __init__(
        self,
        base_url: str,
        project_id: str,
        token: str,
        http: HttpFn | None = None,
        dry_run: bool = False,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.project_id = project_id
        self.token = token
        self.http = http or urllib_http
        self.dry_run = dry_run
        self.planned_calls: list[str] = []
        self.sent_calls: list[str] = []
        # Whether the transport accepts the extra `attempts` keyword.  The
        # real `urllib_http` does; an injected `HttpFn` fake (the test seam)
        # takes exactly (method, url, token, body).  Decided once, up front:
        # the issue-create call passes `attempts=1` only when the transport
        # can take it, so the create is never blindly retried and an
        # injected transport is never called with a keyword it rejects.
        self._http_takes_attempts = self._transport_accepts_attempts(self.http)

    @staticmethod
    def _transport_accepts_attempts(http: HttpFn) -> bool:
        """Whether `http` can take the `attempts=` keyword.

        The real `urllib_http` can.  An injected `HttpFn` is judged by its
        signature: an explicit `attempts` parameter or a `*args` capture both
        work; anything else is called with the four documented positional
        arguments only.  A fake that cannot be introspected at all is treated
        as incompatible (the conservative choice: it simply does not get the
        extra keyword).
        """
        try:
            params = inspect.signature(http).parameters
        except (TypeError, ValueError):  # pragma: no cover - builtins etc.
            return False
        if "attempts" in params:
            return True
        return any(p.kind is p.VAR_POSITIONAL for p in params.values())

    def url(self, path: str, params: dict[str, Any] | None = None) -> str:
        quoted = urllib.parse.quote(str(self.project_id), safe="")
        url = f"{self.base_url}/projects/{quoted}{path}"
        if params:
            query = urllib.parse.urlencode(
                {k: v for k, v in params.items() if v is not None}
            )
            if query:
                url = f"{url}?{query}"
        return url

    def request(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        params: dict[str, Any] | None = None,
    ) -> Any:
        """Plan (and, unless this is a dry run, send) one API call.

        A dry run records the call in `planned_calls` and returns `None`
        WITHOUT touching the transport: `self.http` is only reachable from
        the `else` branch below.

        The issue-create POST (`POST /issues`) is the one call in the script
        that is NOT idempotent: if the server creates the issue but the
        response is lost, a blind retry opens a SECOND `corpus-sync` issue
        and breaks `act()`'s invariant.  So the create is sent with
        `attempts=1` - it is never retried (design (a)): a transient failure
        there raises `DeliveryError`, the CI job shows "ALERT NOT DELIVERED",
        and the next scheduled run opens the issue.  Every other call (the
        idempotent list / notes / close) retries freely.
        """
        url = self.url(path, params)
        self.planned_calls.append(f"{method} {path}")
        if self.dry_run:
            # Planned, not sent: the transport is never reached.
            return None
        self.sent_calls.append(f"{method} {path}")
        # The issue create is never retried (design (a)); every other call
        # gets the transport's default retry policy.  The `attempts` keyword
        # is only passed to transports that accept it - the real
        # `urllib_http` does, an injected `HttpFn` seam may not.
        if method == "POST" and path == "/issues" and self._http_takes_attempts:
            try:
                return self.http(method, url, self.token, body, attempts=1)
            except DeliveryError:
                raise
            except Exception as exc:
                raise DeliveryError(f"{method} {path} failed: {exc}") from exc
        try:
            return self.http(method, url, self.token, body)
        except DeliveryError:
            raise
        except Exception as exc:
            raise DeliveryError(f"{method} {path} failed: {exc}") from exc


def corpus_sync_status(client: GitLabClient, pipeline_id: str) -> str | None:
    """The status of the `corpus-sync` job in a pipeline, if it ran."""
    jobs = client.request("GET", f"/pipelines/{pipeline_id}/jobs")
    if not isinstance(jobs, list):
        return None
    for job in jobs:
        if isinstance(job, dict) and job.get("name") == "corpus-sync":
            status = job.get("status")
            return str(status) if status is not None else None
    return None


def is_failed(status: str | None) -> bool:
    return status == "failed"


def scheduled_pipelines(
    client: GitLabClient, branch: str, current_pipeline_id: str
) -> list[dict[str, Any]]:
    """Scheduled pipelines on the default branch, newest first."""
    pipelines = client.request(
        "GET",
        "/pipelines",
        params={
            "source": "schedule",
            "ref": branch,
            "order_by": "id",
            "sort": "desc",
            "per_page": MAX_LOOKBACK + 1,
        },
    )
    if not isinstance(pipelines, list):
        return []
    return [
        p
        for p in pipelines
        if isinstance(p, dict) and str(p.get("id")) != str(current_pipeline_id)
    ]


def gather_state(
    client: GitLabClient,
    env: dict[str, str],
    freshness_path: str,
) -> SyncState:
    """Collect everything the decision needs."""
    state = state_from_freshness(read_freshness(freshness_path))
    state.pipeline_url = env.get("CI_PIPELINE_URL", "")

    pipeline_id = env.get("CI_PIPELINE_ID", "")
    branch = env.get("CI_DEFAULT_BRANCH", "")
    if not pipeline_id or not branch:
        return state

    this_status = corpus_sync_status(client, pipeline_id)
    state.this_failed = is_failed(this_status) if this_status is not None else None

    previous = scheduled_pipelines(client, branch, pipeline_id)
    consecutive = 0
    prev_failed: bool | None = None
    for index, pipeline in enumerate(previous):
        pipeline_id_prev = str(pipeline.get("id") or "")
        if not pipeline_id_prev:
            break
        status = corpus_sync_status(client, pipeline_id_prev)
        if status is None:
            break
        failed = is_failed(status)
        if index == 0:
            prev_failed = failed
        if failed:
            consecutive += 1
        else:
            break
        if consecutive >= MAX_LOOKBACK:
            break

    state.prev_failed = prev_failed
    if state.this_failed is not None:
        state.consecutive_failures = consecutive + 1 if state.this_failed else 0
    else:
        state.consecutive_failures = None
    return state


# --------------------------------------------------------------------------
# decision
# --------------------------------------------------------------------------


@dataclass
class Decision:
    state: str  # "ALERT" | "HEALTHY" | "QUIET" | "UNKNOWN"
    reason: str


def decide(state: SyncState) -> Decision:
    """Turn the gathered state into one of four outcomes.

    ALERT   - something is wrong and somebody has to know.
    HEALTHY - the sync worked and the data is current.
    QUIET   - one failure only (no consecutive failures yet), nothing to do.
    UNKNOWN - the state could not be determined (no report, no API answer).
    """
    if state.this_failed and state.prev_failed:
        return Decision("ALERT", "2 consecutive failures")

    if state.overlay_stale is True:
        return Decision("ALERT", "stale")
    if (
        state.overlay_age_days is not None
        and state.overlay_age_days > MAX_OVERLAY_AGE_DAYS
    ):
        return Decision(
            "ALERT", f"stale (overlay is {state.overlay_age_days} days old)"
        )
    if state.prices_ok is False:
        return Decision("ALERT", "stale (prices are not ok)")

    if state.this_failed is False and state.prices_ok is True:
        return Decision("HEALTHY", "sync succeeded and the data is current")

    if state.this_failed is True:
        return Decision("QUIET", "one failure, not consecutive")

    return Decision("UNKNOWN", "could not determine the sync state")


# --------------------------------------------------------------------------
# rendering
# --------------------------------------------------------------------------


def _yes_no(value: bool | None) -> str:
    if value is None:
        return "unknown"
    return "yes" if value else "no"


def render_details(state: SyncState, decision: Decision) -> str:
    """The issue / note body: what broke, how long, and what to do."""
    lines: list[str] = []
    lines.append("The scheduled corpus sync is failing or its data is stale.")
    lines.append("")
    lines.append(f"Pipeline: {state.pipeline_url or 'unknown'}")
    lines.append(f"Decision: {decision.state} ({decision.reason})")

    consecutive = state.consecutive_failures
    if consecutive is None:
        lines.append("Consecutive corpus-sync failures: unknown")
    else:
        lines.append(f"Consecutive corpus-sync failures: {consecutive}")

    if state.failing_sources:
        lines.append("")
        lines.append("Failing sources:")
        for source in state.failing_sources:
            lines.append(f"- {source['name']} ({source['kind']}): {source['reason']}")
    else:
        lines.append("")
        lines.append("Failing sources: none reported")

    lines.append("")
    lines.append(f"Prices ok: {_yes_no(state.prices_ok)}")
    if state.prices_reason:
        lines.append(f"Prices reason: {state.prices_reason}")

    if state.overlay_age_days is None:
        lines.append("Overlay age: unknown")
    else:
        lines.append(f"Overlay age: {state.overlay_age_days} days")
    lines.append(f"Overlay stale: {_yes_no(state.overlay_stale)}")
    if state.overlay_built_at:
        lines.append(f"Overlay built at: {state.overlay_built_at}")

    lines.append("")
    lines.append(
        "_Opened automatically by `scripts/ci/corpus-sync-alert.py`; the next "
        "healthy scheduled run closes this issue again._"
    )
    return "\n".join(lines)


def render_recovery_details(state: SyncState) -> str:
    """The closing note: the problem is over, and what proved it."""
    lines = [
        (
            "The scheduled corpus sync recovered: the sync job succeeded and the "
            "freshness report is current again."
        ),
        "",
        f"Pipeline: {state.pipeline_url or 'unknown'}",
        f"Prices ok: {_yes_no(state.prices_ok)}",
    ]
    if state.overlay_age_days is None:
        lines.append("Overlay age: unknown")
    else:
        lines.append(f"Overlay age: {state.overlay_age_days} days")
    lines.append(f"Overlay stale: {_yes_no(state.overlay_stale)}")
    lines.append("")
    lines.append(
        "_Closed automatically by `scripts/ci/corpus-sync-alert.py`; a new "
        "issue is opened if the sync breaks again._"
    )
    return "\n".join(lines)


# --------------------------------------------------------------------------
# action
# --------------------------------------------------------------------------


def open_issues(client: GitLabClient) -> list[dict[str, Any]]:
    """The open issues carrying the `corpus-sync` label, oldest first.

    An entry without a usable `iid` is dropped: every write below addresses an
    issue by its `iid`, and a malformed entry must not produce a request for
    `/issues/None/notes`.
    """
    issues = client.request(
        "GET",
        "/issues",
        params={"state": "opened", "labels": LABEL, "per_page": 100},
    )
    if not isinstance(issues, list):
        return []
    usable = [
        i for i in issues if isinstance(i, dict) and _as_int(i.get("iid")) is not None
    ]
    return sorted(usable, key=lambda i: _as_int(i.get("iid")) or 0)


def act(
    client: GitLabClient,
    decision: Decision,
    state: SyncState,
) -> str:
    """Perform the decision. Returns a short description of what was done.

    Invariant: after this function there is at most ONE open issue labelled
    `corpus-sync`. Every other open issue with that label is closed - on an
    ALERT run and on a HEALTHY run alike. The HEALTHY branch therefore does
    NOT return early: it closes every open alert (each one gets a note
    first), whether there is a "primary" issue or not.
    """
    if decision.state in ("QUIET", "UNKNOWN"):
        return "nothing to do"

    issues = open_issues(client)
    primary = issues[0] if issues else None
    duplicates = issues[1:] if issues else []

    if decision.state == "ALERT":
        body = render_details(state, decision)
        if primary is None:
            client.request(
                "POST",
                "/issues",
                body={"title": ISSUE_TITLE, "body": body, "labels": [LABEL]},
            )
            action = "opened a new issue"
        else:
            iid = primary.get("iid")
            client.request("POST", f"/issues/{iid}/notes", body={"body": body})
            action = "added a note to the open issue"
    else:  # HEALTHY
        body = render_recovery_details(state)
        if primary is None:
            # No open issue at all: there is nothing to close and nothing to
            # note. The duplicate sweep below still runs (and finds nothing).
            action = "nothing to do"
        else:
            iid = primary.get("iid")
            client.request("POST", f"/issues/{iid}/notes", body={"body": body})
            client.request("PUT", f"/issues/{iid}", body={"state_event": "close"})
            action = "noted the recovery and closed the open issue"

    # "Keep exactly ONE open issue": fold duplicates back into the canonical
    # one instead of leaving a pile of half-alert issues behind. This runs on
    # EVERY path (alert, healthy, with or without a primary issue): a stale
    # duplicate that stayed open is exactly the failure this job exists to
    # prevent.
    for duplicate in duplicates:
        iid = duplicate.get("iid")
        client.request(
            "POST",
            f"/issues/{iid}/notes",
            body={
                "body": (
                    "Duplicate `corpus-sync` alert: folded into the canonical "
                    "issue for this label."
                )
            },
        )
        client.request("PUT", f"/issues/{iid}", body={"state_event": "close"})
    if duplicates:
        action += f" (closed {len(duplicates)} duplicate issue(s))"
    return action


# --------------------------------------------------------------------------
# entry point
# --------------------------------------------------------------------------


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Alert on a failing or stale scheduled corpus sync."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the decision and the planned API calls without making them",
    )
    parser.add_argument(
        "--freshness",
        default=os.environ.get("CORPUS_FRESHNESS_PATH", FRESHNESS_PATH),
        help=f"path of the freshness report (default: {FRESHNESS_PATH})",
    )
    return parser.parse_args(argv)


def token_from_env(env: dict[str, str]) -> tuple[str | None, str | None]:
    for name in TOKEN_VARS:
        value = (env.get(name) or "").strip()
        if value:
            return value, name
    return None, None


def note_missing_freshness(freshness_path: str) -> None:
    """Say so when the freshness report is not there.

    The report is an artifact of the corpus-sync job. When it is missing, the
    staleness facts are simply unknown - never "fine" - and the operator
    should be told which file was looked for.
    """
    if not os.path.isfile(freshness_path):
        print(
            f"corpus-sync-alert: no freshness report at {freshness_path}; "
            "the staleness facts are unknown"
        )


def run_dry(
    env: dict[str, str],
    http: HttpFn | None = None,
    freshness_path: str = FRESHNESS_PATH,
) -> int:
    """Print the decision and the planned API calls. Send nothing.

    This is the whole dry-run contract, in one place:

    * it never authenticates and never reaches the transport - the client is
      built with `dry_run=True`, so every request is only *planned*;
    * it always exits 0. A dry run has nothing to deliver, so it has nothing
      to fail at: it must never mark the CI job failed.
    """
    print("corpus-sync-alert: dry-run: no API call will be made")
    token, _token_var = token_from_env(env)
    if token is None:
        print(
            "corpus-sync-alert: dry-run: no token set "
            f"({' or '.join(TOKEN_VARS)}); a real run would report "
            "ALERT NOT DELIVERED"
        )
    project_id = env.get("CI_PROJECT_ID") or ""
    if not project_id:
        # A dry run only prints a plan; it needs no project to talk to.
        project_id = "unknown"
        print(
            "corpus-sync-alert: dry-run: CI_PROJECT_ID is not set; "
            "the planned paths are shown without a project"
        )
    note_missing_freshness(freshness_path)

    client = GitLabClient(
        base_url=env.get("CI_API_V4_URL") or DEFAULT_API_URL,
        project_id=project_id,
        # The token is never sent in a dry run; it only fills the client's
        # signature so the planned URLs look exactly like the real ones.
        token=token or "dry-run",
        http=http,
        dry_run=True,
    )

    state = gather_state(client, env, freshness_path)
    decision = decide(state)
    act(client, decision, state)

    print(f"corpus-sync-alert: decision {decision.state} ({decision.reason})")
    print(
        "corpus-sync-alert: dry-run: the decision above uses the freshness "
        "report only; the pipeline state needs a real run"
    )
    for call in client.planned_calls:
        print(f"corpus-sync-alert: would call {call}")
    return 0


def run(
    env: dict[str, str],
    http: HttpFn | None = None,
    dry_run: bool = False,
    freshness_path: str = FRESHNESS_PATH,
) -> int:
    """The whole decision + action. Returns the process exit code.

    Exit codes:
      0 - the alert was delivered, or was not needed, or this was a dry run.
      1 - the alert could NOT be delivered (no token, or a failing API call).
          A dry run never returns 1: it sends nothing, so it has nothing to
          deliver and nothing to fail at (see `run_dry`).
    """
    if dry_run:
        # A dry run is a separate path on purpose: it must never reach the
        # transport, and it must never exit non-zero.
        return run_dry(env, http=http, freshness_path=freshness_path)

    token, token_var = token_from_env(env)
    if token is None:
        # A real run without a token can deliver nothing. Say so loudly and
        # fail: the CI job is `allow_failure`, so this shows up as a warning
        # instead of hiding the problem.
        print(not_delivered(f"no {' or '.join(TOKEN_VARS)} in the environment"))
        return 1

    note_missing_freshness(freshness_path)

    project_id = env.get("CI_PROJECT_ID") or ""
    if not project_id:
        print(not_delivered("CI_PROJECT_ID is not set"))
        return 1

    client = GitLabClient(
        base_url=env.get("CI_API_V4_URL") or DEFAULT_API_URL,
        project_id=project_id,
        token=token,
        http=http,
        dry_run=False,
    )

    try:
        state = gather_state(client, env, freshness_path)
        decision = decide(state)
        action = act(client, decision, state)
    except DeliveryError as exc:
        # The alert could not be delivered (a failing API call): exit 1 so
        # the CI job shows the warning instead of hiding it.
        print(not_delivered(str(exc)))
        return 1

    print(f"corpus-sync-alert: decision {decision.state} ({decision.reason})")
    print(f"corpus-sync-alert: {action}")
    if token_var:
        print(f"corpus-sync-alert: authenticated with ${token_var}")
    # Delivered, or there was nothing to deliver: exit 0.
    return 0


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    code = run(
        dict(os.environ),
        dry_run=args.dry_run,
        freshness_path=args.freshness,
    )
    if not isinstance(code, int) or isinstance(code, bool):
        # `sys.exit(None)` would exit 0 and mask a failure. Fail loudly
        # instead: an exit code is not an exit code, it is a bug.
        print(not_delivered(f"run() returned {code!r} instead of an exit code"))
        return 1
    return code


if __name__ == "__main__":
    sys.exit(main())
