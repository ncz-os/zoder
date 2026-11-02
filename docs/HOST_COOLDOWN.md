# Host cooldown: the spend-cap back-off marker

Decision D8 step 12. One host-local file that says **"this host tripped its
monthly spend cap — do not start another metered call"**, written when the
monthly cap trips and read by every worker that starts afterwards.

Source of truth: `crates/zoder-core/src/host_cooldown.rs`. This page is the
contract; that module is the implementation. Nothing here is zoder-specific on
the READ side — it is plain JSON on disk, so the Hive worker's budget back-off
(FB64) reads the same signal with no zoder dependency.

## Why it exists

The monthly cap is a per-HOST condition, but the refusal used to be per-WORKER:
each sibling re-read the ledger, re-derived the same "over budget" verdict,
re-prompted and re-refused. One incident counted 49 identical refusals for one
cap trip. The refusal is right — the cap is real — but only the FIRST trip has
anything to do; the siblings should take the free route instead of the same
wall.

Two properties make the signal trustworthy rather than another way to wedge a
host:

- **The read path fails OPEN.** A missing, expired, corrupt, oversized or
  unknown-version marker means "no cooldown". A broken sidecar can degrade the
  optimisation; it can never park the host.
- **Only a MONTHLY cap trip writes it.** A per-call cap, an unpriceable model
  and an unreadable ledger are per-CALL conditions (the next cheap call may fit
  under the per-call cap) — backing off the host on one of those is the phantom
  back-off this decision exists to prevent. The distinction is typed, not
  parsed out of prose: `budget::TrippedCap::Monthly`.

## File

```
<ledger path>.cooldown.json
```

Sits BESIDE the ledger (the suffix is appended to the ledger's own file name,
like the `.lock` companion and the `.checkpoint` sidecar), so pointing the
ledger somewhere else moves the marker with it:

| ledger                      | marker                               |
| --------------------------- | ------------------------------------ |
| `~/.zoder/ledger.jsonl`     | `~/.zoder/ledger.jsonl.cooldown.json` |
| `/srv/z/ledger.json`        | `/srv/z/ledger.json.cooldown.json`    |

## Format (version 1)

A single JSON object. Keys, all required except where noted:

| key          | type             | meaning                                                       |
| ------------ | ---------------- | ------------------------------------------------------------- |
| `version`    | integer          | format version, currently `1`                                  |
| `reason`     | string           | short machine-ish reason; `"monthly_cap"` for a spend-cap trip |
| `detail`     | string (default `""`) | human-readable detail (cap, spend); never parsed for a decision |
| `limit_usd`  | number (optional) | the configured monthly cap in USD                             |
| `spent_usd`  | number (optional) | month-to-date **billed** spend in USD when the cap tripped    |
| `created_at` | string, RFC 3339 | UTC instant the marker was written                            |
| `expires_at` | string, RFC 3339 | UTC instant the back-off lapses                               |

Example:

```json
{
  "version": 1,
  "reason": "monthly_cap",
  "detail": "monthly spend cap reached: $12.50 spent of $10.00 cap",
  "limit_usd": 10.0,
  "spent_usd": 12.5,
  "created_at": "2026-11-01T13:50:32.118Z",
  "expires_at": "2026-12-01T00:00:00Z"
}
```

`expires_at` defaults to the first instant of the next UTC calendar month —
the cap's own reset — so one trip holds the host off until the cap actually
rolls over.

## The reader contract (what a sibling must implement)

1. Read `<ledger path>.cooldown.json`. Any error reading, parsing, or sizing
   the file: **treat as no cooldown**. Do not retry-loop on it.
2. `version` must equal `1`. Anything else: **no cooldown** (a marker from a
   future build is not misread).
3. If `now >= expires_at`: **no cooldown**. The marker is inert; removing it
   is optional and best-effort.
4. Otherwise the host is in cooldown: do not start metered routes; fall
   through to cost-neutral (free / subscription) routes.

A Python reader needs nothing from zoder:

```python
import json, datetime, pathlib

def host_cooldown(ledger_path: str) -> dict | None:
    """The live cooldown marker beside `ledger_path`, or None (fail open)."""
    try:
        raw = pathlib.Path(ledger_path + ".cooldown.json").read_text()
        marker = json.loads(raw)
        if marker.get("version") != 1:
            return None
        expires = datetime.datetime.fromisoformat(
            marker["expires_at"].replace("Z", "+00:00")
        )
        if datetime.datetime.now(datetime.timezone.utc) >= expires:
            return None
        return marker
    except Exception:
        return None
```

## The writer contract (what zoder does)

- Entry point: `Budget::evaluate_call_with_host_cooldown` (and the two-number
  `Budget::evaluate_with_host_cooldown`) — the pre-call budget gate `zoder
  oneshot` and `zoder exec`/agentic already run.
- A monthly cap trip **writes or extends** the marker beside
  `cfg.ledger_path`. A second trip never SHORTENS a live marker, so concurrent
  trips cannot cut each other's hold-off short.
- A call that **fits** clears a live marker: the cap is by definition not
  tripped any more, and a stale hold-off must not keep parking the host after
  the month resets.
- Any other decline (per-call cap, unpriceable model, unreadable ledger)
  changes nothing.
- The write is atomic (temp file + `rename`) so a concurrent reader never sees
  a half-written marker, and best-effort at the call site: a read-only ledger
  directory still yields the budget refusal, just without the host signal.

## Where it shows up

- `zoder status` prints one line when the host is in cooldown (`host cooldown
  ACTIVE (monthly_cap) … metered routes skipped until …`); `zoder status
  --json` carries the same object under `host_cooldown`.
- The agentic chain skips metered links while the marker is live and continues
  with the cost-neutral links (the last live link is never skipped — the run
  reports the cap instead of silently doing nothing).
- Sibling workers read it through `Router::with_ledger_cooldown` /
  `Route::prefer_cost_neutral`.

## Clearing it by hand

```sh
rm ~/.zoder/ledger.jsonl.cooldown.json
```

Safe at any time: the marker only ever *removes* metered routes, never
adds them, and an expired marker is inert anyway. Raising the cap or starting
a new month clears it on the next call that fits.

## Tests

- `cap_trip_sets_host_cooldown` (`crates/zoder-core/src/budget.rs`) — a
  monthly trip writes the marker with the cap and the spend; a per-call trip,
  an unpriceable model and an unreadable ledger leave none; a call that fits
  clears a stale one.
- `sibling_prefers_local_route_during_cooldown`
  (`crates/zoder-core/src/router.rs`) — a marker written by one worker sends
  the next one to the cost-neutral route; a corrupt marker changes nothing.
