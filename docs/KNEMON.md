# KNEMON — Subscription-Utilization Intelligence

*Part of `zoder-core`. Self-contained (no external service). Keyed per-account.*

## What it is

KNEMON is the layer that makes zoder **maximize the LLM subscriptions you already
pay for**. Most agentic CLIs meter only their own single subscription. KNEMON models
your **portfolio** of subs (MiniMax Max, OpenAI-codex OAuth, Anthropic Max, …), tracks
each account's live rolling-window headroom, and **actively routes build work to the
most-underused paid-for capacity** while guarding against overrun — all from a local
JSON state file, no server.

> Flat-rate subscription capacity that resets unused is wasted forever. KNEMON's job is
> to consume it — deliberately, up to a safe target — before falling back to metered/free.

## Architecture (5 layers)

| Layer | Module | Role |
|---|---|---|
| L1 | `config.rs` (`QuotaWindow`) | **Declarative** per-account plan schema — any sub in JSON |
| L2 | `utilization.rs` (`WindowView`/`AccountView`, `TelemetryHealth`) | Unified per-window runtime view + observability/health |
| L3A | `provider.rs` + `utilization.rs::from_headers` | Capture header-fed telemetry (codex/anthropic) |
| L3B | `provider.rs` + `record_counter` + `subscriptions/tiers.json` | Counter-fed telemetry (MiniMax; no headers) |
| L4 | `utilization.rs::decide_account` + `scenarios.rs` | Two-sided decision + idle-ranked routing |
| L5 | `main.rs` (`finops report`) | Per-account utilization report |

## The honesty model (the core design principle)

Providers expose their limits inconsistently. KNEMON never fabricates a number it
doesn't have. Every window carries an **observability** level and a **telemetry health**:

- **`Full`** — cap AND used known → a real `used_percent` (e.g. MiniMax monthly counter, cap 5.1B tokens).
- **`PercentOnly`** — `used_percent` known from a vendor header, absolute cap unknown (codex/anthropic).
- **Unknown** — no `used_percent` at all (e.g. MiniMax 5h/weekly: no header, cap not published) → **headroom, never a gate.**
- Health: `Fresh` (<5m) · `Stale` (5–60m, 0.8× weight) · `Degraded` (>60m, excluded).

Rule: a window with no usable observation contributes **headroom**, it never fabricates
a percentage and never falsely gates. Stale telemetry degrades to headroom, not to a gate.

## Declarative config — define any subscription in JSON

`QuotaWindow { name, hours, unit(Tokens|Requests|Messages|Sessions), cap:Option<f64>,
models:Option<Vec<String>>, observability, reset }`. `cap:None` = unknown/percent-only.
`models` scopes a window to model globs (Anthropic-style per-model caps). Adding a new
provider is a JSON edit — no recompile. Presets live in `subscriptions/tiers.json`
(overridable at `$ZODER_HOME/subscriptions/tiers.json`) with an estimates-disclaimer
and per-tier `confidence`/`source`.

```jsonc
// MiniMax Max
"windows": [
  {"name":"5h","hours":5,"observability":"percent_only"},
  {"name":"weekly","hours":168,"observability":"percent_only"},
  {"name":"monthly","hours":720,"unit":"tokens","cap":5100000000,"observability":"counter","reset":"calendar_monthly"}
]
// OpenAI-codex: two header-fed windows.  Anthropic: unified + per-model {"models":["*opus*"]}.
```

## The decision — `decide_account`

Over an account's windows:
1. **Observable** = has `used_percent` AND health ≠ Degraded.
2. **Binding window** = `argmax(used_percent × health_weight)` — only ONE window binds (no double-gating).
3. **Reset-relaxation** — a window ≥ `cap_guard` that resets within `reset_imminence_threshold` (10%) of its cycle does NOT gate.
4. Bands on the binding used%: `< use_target (80%)` → **PreferSub** (drive utilization); `< cap_guard (85%)` → PreferSub (hysteresis); `≥ cap_guard` → `Block`→FallBackToFree / `Chargeback`.
5. **`strength`** (= binding used%) — the router ranks sub-accounts by *ascending* strength, so build work drains the **most-underused** sub first.

## The report — `zoder finops report`

A "Subscription utilization" section per account: each window (used% / `percent-only` /
`unknown` — never fabricated), observability, health, headroom; the binding window +
verdict + strength; and one hint: **IDLE → preferring for build work** / NEAR TARGET /
AT CAP → falling back / no telemetry yet. Content-free (no prompts/responses); JSON
output stays pure-data.

## Guarantees

- **Self-contained** — all state in `~/.zoder/utilization.json`; no MNEMOS/network dependency.
- **Content-free** — only counts/percentages/reset windows are ever recorded or shown.
- **Best-effort capture** — a telemetry parse/IO error is debug-logged and swallowed; it can never fail a request.
- **Honest** — unknown ≠ zero; estimates are labelled; verify against your provider dashboard.
