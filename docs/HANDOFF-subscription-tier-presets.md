# HANDOFF — subscription tier presets + utilization estimation

**For:** the next engineer (or agent) implementing tier-preset-driven subscription
utilization in zoder + the tokenomics ClawHub plugin.
**Status:** the rolling-window quota engine is BUILT and shipped in both consumers;
this handoff specifies the layer on top of it — a curated **tier preset catalog**
plus a manual config that lets a user pick a known subscription tier instead of
hand-entering caps.
**Provenance:** MNEMOS `mem_1782828569027_109522` (API feasibility) +
`mem_1782745429098` / build/review doctrine. Triple-persisted per operating
directive #9 (MNEMOS + this versioned doc + git).

---

## 1. Why this exists (the constraint)

There is **no universal provider API** to query "how much of my subscription
plan have I used and when does it reset" (full findings: MNEMOS
`mem_1782828569027_109522`):

- **Anthropic** — rate-limit *response headers* per call; a Rate Limits API
  (2026-04-25) but **Admin-key only**; Claude.ai Max/Pro subscription caps are
  **not** exposed via API.
- **OpenAI** — response headers + `/v1/organization/costs` & usage, but
  **Admin-key** gated and **billed-usage**, not plan-reset.
- **MiniMax** — Token Plan is **5h-rolling + weekly**, **dashboard-only**.

So utilization must be **estimated locally**: measure consumption from the local
ledger against **known** plan caps. The caps are the missing piece — providers
do not publish them in a machine feed, and most do not publish exact numbers at
all. Hence a **curated tier-preset catalog** maintained by hand.

## 2. What already exists (build on this, do not rebuild)

The rolling-window engine is identical in both consumers:

- **zoder (Rust):** `crates/zoder-core/src/quota.rs` — `window_usage()` returns
  `{used, cap, pct, next_reset_utc, approaching(>=80%)}`; `plan_usage()`,
  `amortized_per_call()`. Config types in `config.rs`: `SubscriptionPlan` (per
  provider) → `QuotaWindow { name, hours, unit, cap }`. Surfaced by
  `zoder providers`.
- **tokenomics plugin (TS):** `src/subscriptions.ts` — `windowUsage()`,
  `planUsage()`, `amortizedPerCall()`, `buildSubscriptionReport()`,
  `loadSubscriptions()`/`sanitizeSubscriptions()`. Surfaced by the `?view=quota`
  route. Config: `<stateDir>/tokenomics/subscriptions.json`
  (template: `subscriptions.example.json`).

Both compute usage from the ledger; **the operator currently hand-enters every
`cap`.** This handoff removes that burden via presets.

## 3. Goal

Let the operator write a **minimal** config — pick a provider + a known tier —
and have the tool expand it into the tier's known windows/caps, then **estimate**
utilization (used/cap/pct, next reset, approaching) against those caps.

```jsonc
// instead of hand-entering every window/cap:
{ "plans": [ { "provider": "anthropic", "tier": "claude-max-20x" } ] }
// resolver expands `tier` → the catalog's windows for that tier.
```

## 4. Design directives

### 4.1 Shared, curated tier-preset catalog (single source of truth)
- New file in **ncz-os/zoder** main: `subscriptions/tiers.json`, **raw-pullable**
  (same pattern as `corpus/model_corpus.json` + `bench/known-good-swe.json`), so
  zoder, the tokenomics plugin, and any other consumer resolve from one place.
- **Schema** (per provider → list of tiers):
  ```jsonc
  {
    "version": 1,
    "as_of": "2026-06-30",
    "disclaimer": "ESTIMATES from known/observed service behavior. Providers change
                   limits without notice and rarely publish exact numbers. Treat
                   utilization as approximate; verify against your provider dashboard.",
    "providers": {
      "anthropic": {
        "tiers": {
          "claude-max-20x": {
            "monthly_fee_usd": 200,
            "confidence": "low",          // exact caps unpublished; community-observed
            "source": "observed",
            "windows": [
              { "name": "5h",     "hours": 5,   "unit": "messages", "cap": 900 },
              { "name": "weekly", "hours": 168, "unit": "messages", "cap": 8000 }
            ]
          }
        }
      },
      "minimax": { "tiers": { "token-plan-2": { "windows": [
        { "name": "5h", "hours": 5, "unit": "tokens", "cap": 20000000 },
        { "name": "weekly", "hours": 168, "unit": "tokens", "cap": 500000000 } ] } } }
    }
  }
  ```
- Every cap carries `confidence` (`published` | `observed` | `estimated`) and the
  block carries `as_of` + a `disclaimer`. **Do not present estimated utilization
  as authoritative.**

### 4.2 Config resolution (both consumers)
- A plan entry may now be one of:
  1. **Explicit** — `windows: [...]` (today's behavior; unchanged).
  2. **Preset** — `{ provider, tier }` → resolver loads the tier's windows from
     the catalog.
  3. **Preset + overrides** — `{ provider, tier, windows: [...] }` → start from
     the preset, then **explicit windows override by `name`** (operator tunes one
     cap without re-declaring the rest).
- Resolution happens at load time → the existing `windowUsage()` path is
  unchanged downstream. Keep the resolver pure + tolerant (unknown tier →
  warn + fall back to explicit windows or skip the plan; never throw).

### 4.3 "Estimate utilization from DAILY limits"
- Some plans publish a **daily** message/request limit rather than a rolling
  window. Support it directly: a window with `hours: 24` (a rolling 24h) or add
  an optional `period: "calendar-day-utc"` variant if a fixed-midnight reset is
  needed (rolling-24h is the simpler default and matches the existing engine).
- The estimator is unchanged: `used(trailing window) / cap`. For a daily limit,
  `hours: 24`, `cap = daily limit`. Document that rolling-24h ≈ a daily cap with a
  sliding reset (good enough for "approaching" warnings).

### 4.4 Known service behaviors to seed the initial catalog
Encode these with the right `confidence`. Verify/refresh each at implementation
time (they drift):
- **Anthropic Claude.ai** — Pro, Max 5x, Max 20x: **5-hour rolling session** limit
  + **weekly** limit; exact message counts vary by model and are **not officially
  published** → `confidence: observed`, conservative caps.
- **OpenAI ChatGPT** — Plus / Pro / Team / Business: per-window message caps
  (e.g. GPT-class messages per 3h + weekly); `confidence: observed`.
- **MiniMax Token Plan** — per-tier **5h + weekly TOKEN** caps (structure is
  documented; per-tier numbers from the dashboard/pricing page);
  `confidence: published` where the page states them.
- **DeepSeek** — note: DeepSeek's special behavior is **off-peak pricing**
  (already handled in `pricing/peak-pricing.json`), not a subscription quota.

### 4.5 Implementation steps
**zoder (Rust):**
1. Add a `subscription_tiers` loader (read `subscriptions/tiers.json`, seeded at
   install + refreshable like the corpus). New `TierCatalog` type.
2. Add an optional `tier: Option<String>` to `config::SubscriptionPlan`; add a
   resolver `resolve_plan(plan, catalog) -> Vec<QuotaWindow>` (preset → windows,
   explicit overrides by name). Call it where `plan_usage()` reads `plan.windows`.
3. `zoder providers` already prints windows + next-reset + approaching — add the
   tier name + `as_of`/confidence note to the header line.

**tokenomics plugin (TS):**
1. Add `loadTiers()` (bundled default `tiers.json` + optional override path).
2. Add `tier?: string` to `SubscriptionPlan`; resolve in `sanitizeSubscriptions`
   / a new `resolvePlans(config, catalog)` before `buildSubscriptionReport()`.
3. `?view=quota` payload gains `tier`, `as_of`, `confidence` per provider.

**Shared:** the catalog is curated + committed (not in the daily machine feed).
Add it to the `corpus-sync` repo but **do not auto-overwrite** it — it is
hand-maintained. Consider a staleness check: warn when `as_of` is > 90 days old.

### 4.6 Maintenance / WATCH
- Subscription tiers change without notice. Add a periodic review (quarterly, or
  when a provider announces plan changes) to refresh `tiers.json` + bump `as_of`.
- Keep `confidence` honest; downgrade to `estimated` when a number is a guess.

## 5. Acceptance criteria
- A config of `{ provider, tier }` alone yields a correct per-window utilization
  report in both zoder (`zoder providers`) and the plugin (`?view=quota`).
- Explicit `windows` still work and override preset windows by `name`.
- Unknown tier → graceful warning, no crash.
- Reports label utilization as **estimated** with `as_of` + a verify-dashboard
  note; never imply an authoritative live quota.
- Catalog is the single raw-pullable source; both consumers read it identically.

## 6. Open questions for the operator
- Calendar-day (fixed midnight) reset vs rolling-24h — needed for any provider?
  (Default: rolling-24h.)
- Should the catalog ship as a bundled default in each consumer (offline-safe)
  AND raw-pull a refresh, or raw-pull only? (Recommend: bundle a default + refresh,
  like the corpus.)
- Anthropic/OpenAI exact caps are unpublished — acceptable to ship `observed`
  estimates with the disclaimer, or gate those tiers behind operator-entered caps?
