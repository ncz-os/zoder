//! Model pricing catalog (department **chargeback**, not COGS).
//!
//! Rates are realized `$ / 1M tokens`, derived from your provider's billing
//! actuals (`cost / tokens` per model over a trailing window). free-tier
//! models price at ~$0 because they are not billed to the department. Refresh
//! with `zoder pricing refresh` (pulls provider actuals → this file).
//!
//! The catalog is the source of truth for `cost_usd` when the backend does not
//! report a billed cost itself, and it powers the avoided-spend headline in
//! `zoder report`.

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Monotonic per-process nonce so two overlapping `save()` calls in the same
/// process never collide on the temp path. Combined with `std::process::id()`
/// it makes each writer's temp file (`<stem>.json.tmp.<pid>.<nonce>`) unique,
/// so an interleaved write+rename can never promote a torn or foreign temp
/// over the live catalog. Mirrors `write_atomic` in `crates/model-health` and
/// `Corpus::save` (C5-4) -- self-contained, no cross-crate dependency.
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

/// The price list shipped with zoder, embedded in the binary.
///
/// The runtime catalog (`~/.zoder/pricing.json`) is the live source: it
/// tracks the provider's current prices and is refreshed daily. It can be
/// missing (a host that has never run the refresh), stale (written by an
/// older build), or missing an entry (the loader skips a malformed one).
/// In each of those cases a PAID model must not fall through to "unknown":
/// an unknown cost fails the budget gate closed and a $0 cost is a lie. The
/// shipped list is the floor -- the published list prices -- so a model it
/// knows is priced rather than unknown, and a model it does not know is
/// still unknown.
const BUNDLED_CATALOG_JSON: &str = include_str!("../../../pricing/catalog.json");

/// The shipped price list, as raw JSON.
///
/// The runtime builds its catalog from the public feeds, and the shipped
/// list is the fallback when they cannot be fetched. Exposing the raw JSON
/// lets a caller compare the two without reaching into this module.
pub fn bundled_catalog_json() -> &'static str {
    BUNDLED_CATALOG_JSON
}

/// The shipped price list, parsed once per process. A lookup only reaches it
/// when the runtime catalog does not know the model, so the parse is paid
/// for at most once and only when it is needed.
static BUNDLED_CATALOG: OnceLock<PricingCatalog> = OnceLock::new();

/// The shipped price list, parsed on first use.
fn bundled_catalog() -> &'static PricingCatalog {
    BUNDLED_CATALOG.get_or_init(|| {
        match serde_json::from_str::<PricingCatalog>(BUNDLED_CATALOG_JSON) {
            Ok(cat) => cat,
            Err(e) => {
                eprintln!("zoder: warning: the shipped price list could not be read ({e})");
                PricingCatalog::default()
            }
        }
    })
}

/// The token counts of one call, split the way the provider reports them.
///
/// `tokens_in` is the provider's TOTAL input count, which INCLUDES the
/// cache hits; `cache_hit_tokens` is the subset of it that was served from
/// the provider's prompt cache. The misses are the difference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallTokens {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_hit_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Legacy blended chargeback in USD per 1M tokens (input+output). Kept as a
    /// fallback for catalogs that don't carry the per-component rates below.
    #[serde(default)]
    pub usd_per_mtok: f64,
    /// Per-component rates in USD per 1M tokens (the public price-list shape:
    /// LiteLLM `input/output_cost_per_token`, OpenRouter `pricing.prompt/
    /// completion`, scaled to per-Mtok). When `input`/`output` are set they take
    /// precedence over the blended `usd_per_mtok`.
    #[serde(default)]
    pub input_usd_per_mtok: f64,
    #[serde(default)]
    pub output_usd_per_mtok: f64,
    #[serde(default)]
    pub cache_read_usd_per_mtok: f64,
    #[serde(default)]
    pub cache_write_usd_per_mtok: f64,
    #[serde(default)]
    pub reasoning_usd_per_mtok: f64,
    #[serde(default)]
    pub source: String,
    /// Optional off-peak (time-of-day) rates + the UTC window they apply in
    /// (e.g. DeepSeek's discounted off-peak window). When present and the call's
    /// UTC time is inside the window, the off-peak rate bills instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_peak: Option<OffPeak>,
    /// Optional peak (time-of-day) rates plus the UTC windows they apply in.
    /// A call served inside one of the windows bills the peak rates instead
    /// of the standard ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak: Option<Peak>,
}

/// Off-peak (time-of-day) rates + the UTC window they apply in.
/// `window_start_utc_min` is a real minute-of-day UTC in `[0, 1439]`.
/// `window_end_utc_min` is an EXCLUSIVE end-of-day boundary in `[0, 1440]`;
/// `1440` is a legitimate value meaning "the end of the day" (= 24:00 /
/// midnight), so a window `(0, 1440)` covers the entire day including the
/// last minute at 23:59 UTC. A window may wrap midnight (`start > end`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OffPeak {
    #[serde(default)]
    pub input_usd_per_mtok: f64,
    #[serde(default)]
    pub output_usd_per_mtok: f64,
    /// The cache-read rate inside the window. When it is absent the cache
    /// hits are charged at the window's input rate: overcharging a call is
    /// recoverable, undercharging it is not.
    #[serde(default)]
    pub cache_read_usd_per_mtok: f64,
    /// Minute-of-day UTC at which the off-peak window starts, inclusive.
    /// Range: `[0, 1439]`.
    #[serde(default)]
    pub window_start_utc_min: u32,
    /// Minute-of-day UTC at which the off-peak window ends, EXCLUSIVE.
    /// Range: `[0, 1440]`. `1440` = end-of-day (midnight), so a window
    /// `(0, 1440)` covers the full day including minute 1439 (23:59 UTC).
    #[serde(default)]
    pub window_end_utc_min: u32,
}

/// A UTC time-of-day window. `window_start_utc_min` is inclusive and
/// `window_end_utc_min` is exclusive, both minutes since midnight UTC; a
/// start greater than the end wraps past midnight.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TimeWindow {
    #[serde(default)]
    pub window_start_utc_min: u32,
    #[serde(default)]
    pub window_end_utc_min: u32,
    /// When true the window applies only on weekdays (Monday to Friday, UTC).
    #[serde(default)]
    pub weekdays_only: bool,
}

impl TimeWindow {
    /// True when `utc_min` (minutes-of-day UTC) falls in this window,
    /// handling a window that wraps past midnight.
    pub fn active_at(&self, utc_min: u32) -> bool {
        let (s, e) = (self.window_start_utc_min, self.window_end_utc_min);
        if s <= e {
            utc_min >= s && utc_min < e
        } else {
            utc_min >= s || utc_min < e
        }
    }
}

/// Peak (time-of-day) rates plus the windows they apply in.
///
/// A provider that bills a premium during busy hours lists the premium rates
/// here and the windows they cover; a call served inside a window bills the
/// premium, every other call the standard rates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Peak {
    /// The rate for input tokens inside a peak window.
    #[serde(default)]
    pub input_usd_per_mtok: f64,
    /// The rate for output tokens inside a peak window.
    #[serde(default)]
    pub output_usd_per_mtok: f64,
    /// The rate for cache-hit input tokens inside a peak window.
    #[serde(default)]
    pub cache_read_usd_per_mtok: f64,
    /// The windows the premium applies in.
    #[serde(default)]
    pub windows: Vec<TimeWindow>,
}

impl Peak {
    /// True when the call was served inside one of the peak windows.
    /// `weekday` is 0 = Monday .. 6 = Sunday (UTC).
    pub fn active_at(&self, utc_min: u32, weekday: u32) -> bool {
        self.windows
            .iter()
            .any(|w| w.active_at(utc_min) && (!w.weekdays_only || weekday < 5))
    }
}

impl OffPeak {
    /// True when `utc_min` (minutes-of-day UTC) falls in the off-peak window,
    /// handling a window that wraps past midnight.
    pub fn active_at(&self, utc_min: u32) -> bool {
        let (s, e) = (self.window_start_utc_min, self.window_end_utc_min);
        if s <= e {
            utc_min >= s && utc_min < e
        } else {
            utc_min >= s || utc_min < e
        }
    }
}

impl ModelPrice {
    /// True when any component (or the blended rate) is non-zero.
    pub fn is_priced(&self) -> bool {
        self.usd_per_mtok > 0.0 || self.input_usd_per_mtok > 0.0 || self.output_usd_per_mtok > 0.0
    }

    /// Cost in USD for an input/output token split. Uses the component rates
    /// when present, otherwise the blended rate over the combined token count.
    /// Operands are widened to `u128` so extreme token counts (e.g.
    /// `u64::MAX`) cannot panic with overflow checks or wrap to zero in
    /// release mode; cost is `f64` so the wider intermediate is just a
    /// defense against arithmetic overflow. Returns `0.0` for empty token
    /// splits (the catalog's "no work, no charge" convention).
    pub fn cost_io(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        // Empty input/output = no work; charge zero rather than treating
        // the call as a free passthrough at a paid rate. Free models
        // (input=output=0) and explicit-zero entries both yield 0 here.
        if tokens_in == 0 && tokens_out == 0 {
            return 0.0;
        }
        if self.input_usd_per_mtok > 0.0 || self.output_usd_per_mtok > 0.0 {
            let in_part = (tokens_in as u128) as f64 * self.input_usd_per_mtok;
            let out_part = (tokens_out as u128) as f64 * self.output_usd_per_mtok;
            (in_part + out_part) / 1_000_000.0
        } else {
            // Blended rate is per-token; convert the token total via u128
            // first so an overflow on the sum can't wrap and silently bill
            // $0 for a u64::MAX-sized call.
            let total = (tokens_in as u128).saturating_add(tokens_out as u128);
            self.usd_per_mtok * (total as f64) / 1_000_000.0
        }
    }

    /// Cost for an input/output split at a given UTC minute-of-day, charging the
    /// off-peak rate when its window is active, else the standard `cost_io`.
    pub fn cost_io_at(&self, tokens_in: u64, tokens_out: u64, utc_min: u32) -> f64 {
        if let Some(op) = &self.off_peak {
            // C6-P2: require BOTH off-peak components (> 0) to take the
            // off-peak branch. A partial pair (only one set) would zero the
            // missing component's charge in-window; falling through to
            // `cost_io` bills the standard peak rate instead. Defense-in-depth
            // -- the loader (`validate_model_price`) already rejects a partial
            // off-peak pair, so a partial pair should never reach here.
            if op.active_at(utc_min) && op.input_usd_per_mtok > 0.0 && op.output_usd_per_mtok > 0.0
            {
                let in_part = (tokens_in as u128) as f64 * op.input_usd_per_mtok;
                let out_part = (tokens_out as u128) as f64 * op.output_usd_per_mtok;
                return (in_part + out_part) / 1_000_000.0;
            }
        }
        self.cost_io(tokens_in, tokens_out)
    }

    /// Cost for an input/output split where part of the input was served from
    /// the provider's prompt cache.
    ///
    /// `tokens_in` is the provider's TOTAL input count, which INCLUDES the
    /// cache hits; `cache_hit_tokens` is the subset served from the cache.
    /// The hits bill at the cache-read rate and the rest at the miss rate. A
    /// catalog entry without a cache rate charges the hits at the miss rate:
    /// overcharging a call is recoverable, undercharging it is not.
    pub fn cost_io_with_cache(
        &self,
        tokens_in: u64,
        tokens_out: u64,
        cache_hit_tokens: u64,
    ) -> f64 {
        if tokens_in == 0 && tokens_out == 0 {
            return 0.0;
        }
        if self.input_usd_per_mtok > 0.0 || self.output_usd_per_mtok > 0.0 {
            self.cost_with_rates(
                tokens_in,
                tokens_out,
                cache_hit_tokens,
                self.input_usd_per_mtok,
                self.output_usd_per_mtok,
                self.cache_read_usd_per_mtok,
            )
        } else {
            // Blended rate is per-token; convert the token total via u128
            // first so an overflow on the sum can't wrap and silently bill
            // $0 for a u64::MAX-sized call.
            let total = (tokens_in as u128).saturating_add(tokens_out as u128);
            self.usd_per_mtok * (total as f64) / 1_000_000.0
        }
    }

    /// The shared arithmetic: the miss tokens at `in_rate`, the cache hits at
    /// `cache_rate` (the input rate when the catalog carries none) and the
    /// output tokens at `out_rate`.
    fn cost_with_rates(
        &self,
        tokens_in: u64,
        tokens_out: u64,
        cache_hit_tokens: u64,
        in_rate: f64,
        out_rate: f64,
        cache_rate: f64,
    ) -> f64 {
        // A provider can only report hits it actually served, so the hit
        // count is clamped to the input count.
        let hits = cache_hit_tokens.min(tokens_in);
        let misses = tokens_in - hits;
        let hit_rate = if cache_rate > 0.0 {
            cache_rate
        } else {
            in_rate
        };
        let in_part = (misses as u128) as f64 * in_rate + (hits as u128) as f64 * hit_rate;
        let out_part = (tokens_out as u128) as f64 * out_rate;
        (in_part + out_part) / 1_000_000.0
    }

    /// Cost for an input/output split served at a given time, honoring the
    /// peak and off-peak windows and the cache-read rate.
    pub fn cost_io_at_ts(
        &self,
        tokens_in: u64,
        tokens_out: u64,
        cache_hit_tokens: u64,
        ts: DateTime<Utc>,
    ) -> f64 {
        let utc_min = ts.hour() * 60 + ts.minute();
        if let Some(peak) = &self.peak {
            // A peak window needs both component rates (> 0), for the same
            // reason the off-peak branch does: a partial pair would zero the
            // missing component's charge inside the window.
            let weekday = ts.weekday().num_days_from_monday();
            if peak.active_at(utc_min, weekday)
                && peak.input_usd_per_mtok > 0.0
                && peak.output_usd_per_mtok > 0.0
            {
                return self.cost_with_rates(
                    tokens_in,
                    tokens_out,
                    cache_hit_tokens,
                    peak.input_usd_per_mtok,
                    peak.output_usd_per_mtok,
                    peak.cache_read_usd_per_mtok,
                );
            }
        }
        if let Some(op) = &self.off_peak {
            if op.active_at(utc_min) && op.input_usd_per_mtok > 0.0 && op.output_usd_per_mtok > 0.0
            {
                return self.cost_with_rates(
                    tokens_in,
                    tokens_out,
                    cache_hit_tokens,
                    op.input_usd_per_mtok,
                    op.output_usd_per_mtok,
                    op.cache_read_usd_per_mtok,
                );
            }
        }
        self.cost_io_with_cache(tokens_in, tokens_out, cache_hit_tokens)
    }
}

/// Numeric rate fields validated on load (each is USD per 1M tokens).
const NUMERIC_PRICE_FIELDS: [&str; 6] = [
    "usd_per_mtok",
    "input_usd_per_mtok",
    "output_usd_per_mtok",
    "cache_read_usd_per_mtok",
    "cache_write_usd_per_mtok",
    "reasoning_usd_per_mtok",
];

fn set_price_field(p: &mut ModelPrice, key: &str, n: f64) {
    match key {
        "usd_per_mtok" => p.usd_per_mtok = n,
        "input_usd_per_mtok" => p.input_usd_per_mtok = n,
        "output_usd_per_mtok" => p.output_usd_per_mtok = n,
        "cache_read_usd_per_mtok" => p.cache_read_usd_per_mtok = n,
        "cache_write_usd_per_mtok" => p.cache_write_usd_per_mtok = n,
        "reasoning_usd_per_mtok" => p.reasoning_usd_per_mtok = n,
        _ => {}
    }
}

/// Validate one model price entry. Numeric rate fields must be finite and
/// non-negative; a bad field is dropped (warned) instead of aborting the whole
/// catalog parse. Returns `None` (skip the model) when the value is not an
/// object or no valid rate field remains. Mirrors the TS `validateModelPrice`.
fn validate_model_price(
    model_id: &str,
    raw: &serde_json::Value,
    warn: &mut dyn FnMut(String),
) -> Option<ModelPrice> {
    let serde_json::Value::Object(obj) = raw else {
        warn(format!(
            "model \"{model_id}\": value is not an object, skipping"
        ));
        return None;
    };
    if let Some(src) = obj.get("source") {
        if !src.is_string() && !src.is_null() {
            warn(format!(
                "model \"{model_id}\": source is not a string, skipping field"
            ));
        }
    }
    let mut price = ModelPrice::default();
    let mut blended_present = false;
    let mut input_present = false;
    let mut output_present = false;
    for f in NUMERIC_PRICE_FIELDS {
        match obj.get(f) {
            None | Some(serde_json::Value::Null) => {}
            Some(v) => match v.as_f64() {
                Some(n) if n.is_finite() && n >= 0.0 => {
                    set_price_field(&mut price, f, n);
                    match f {
                        "usd_per_mtok" => blended_present = true,
                        "input_usd_per_mtok" => input_present = true,
                        "output_usd_per_mtok" => output_present = true,
                        _ => {}
                    }
                }
                // A finite negative value is the catalog's "unpriced / unknown"
                // sentinel (e.g. -1000000.0 for models with no published rate).
                // Drop the field silently — it is intentional, not malformed, so
                // it must not spam a warning on every report.
                Some(n) if n.is_finite() => {}
                _ => warn(format!(
                    "model \"{model_id}\": {f}={v} is not a finite number, skipping field"
                )),
            },
        }
    }
    let complete_component_pair = input_present && output_present;
    let partial_component_pair = input_present != output_present;
    if partial_component_pair || (!complete_component_pair && !blended_present) {
        warn(format!(
            "model \"{model_id}\": no complete input/output pair or blended rate, skipping model"
        ));
        return None;
    }
    price.source = obj
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Optional off-peak (time-of-day) block; absent/malformed → no off-peak.
    if let Some(op) = obj.get("off_peak").and_then(|v| v.as_object()) {
        let num = |k: &str| op.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
        // Clamp one window minute field to its inclusive maximum, warning on
        // out-of-range values (e.g. a typo like 1500, or an attacker-crafted
        // value larger than u32::MAX) instead of silently swallowing them.
        // `warn` is the callback the loader passes in so the warning is
        // captured by tests (and forwarded to stderr in production).
        //
        // window_start_utc_min is a real minute-of-day: valid in [0, 1439].
        // window_end_utc_min is an EXCLUSIVE end boundary: valid in [0, 1440],
        // where 1440 is the legitimate "end of day" sentinel (= 24:00 / midnight)
        // so a window (0, 1440) covers the full day INCLUDING minute 1439 (23:59
        // UTC). Previously the parser clamped BOTH fields with `.min(1439)`,
        // which silently destroyed the legitimate 1440 end-of-day value and
        // caused 23:59-UTC calls to be billed at the peak rate.
        //
        // SECURITY: the raw value is read as `u64` (because JSON integers are
        // arbitrary-width and an operator can supply `u32::MAX` or larger).
        // We MUST compare against the *original* `u64` against the allowed
        // range BEFORE casting to `u32` -- a naive `as u32` cast silently
        // truncates values above `u32::MAX`, and the truncated value can land
        // inside the valid range (e.g. `4_294_967_296_u64 as u32 == 0`,
        // which the per-field check `0 <= max` would happily accept). An
        // attacker who controls the catalog could use that path to inject
        // an arbitrary minute-of-day and reshape the off-peak window. We
        // also surface `u32::MAX`-exceeding values with a dedicated warning
        // so the audit trail records the *original* value, not the truncated
        // one -- otherwise the warning text would lie about what was loaded.
        fn clamp_window_min(
            op: &serde_json::Map<String, serde_json::Value>,
            model_id: &str,
            key: &str,
            max: u32,
            warn: &mut dyn FnMut(String),
        ) -> u32 {
            let Some(raw_u64) = op.get(key).and_then(|v| v.as_u64()) else {
                // Absent, non-numeric, or negative -- treat as 0. A negative
                // value is not representable as `u64`; fall back to the
                // missing-field default rather than rejecting the model.
                return 0;
            };
            // Step 1: catch values that don't fit in `u32` BEFORE we cast.
            // Without this check, `(raw_u64 as u32)` would silently truncate
            // and the truncated value could land in the valid range, bypassing
            // the warning entirely. We name the *original* value in the warning
            // so operators can see what was actually written to the catalog.
            if raw_u64 > u32::MAX as u64 {
                warn(format!(
                    "model \"{model_id}\": off_peak.{key}={raw_u64} exceeds u32::MAX ({}), out of range; clamping to {max}",
                    u32::MAX
                ));
                return max;
            }
            // Safe to narrow now: `raw_u64 <= u32::MAX`.
            let raw = raw_u64 as u32;
            // Step 2: catch values within `u32::MAX` but above the per-field
            // max (e.g. a typo like 1500 on the end field, whose max is 1440).
            // Keep the original `u32` in the warning so it matches what the
            // operator wrote (no need to print the upper bound here -- the
            // dedicated u32::MAX branch above already covers that case).
            if raw > max {
                warn(format!(
                    "model \"{model_id}\": off_peak.{key}={raw} > {max} (out of range), clamping to {max}"
                ));
                return max;
            }
            raw
        }
        let op_input = num("input_usd_per_mtok");
        let op_output = num("output_usd_per_mtok");
        // C6-P2: an off-peak block is only honored when BOTH the input AND the
        // output rate are present (> 0) -- mirroring the standard-rate
        // complete-pair check above. A partial pair (e.g. only input set) would
        // otherwise default the missing component to 0.0 and bill ALL of that
        // component's tokens at $0 in-window (peak $18 output silently vanishing
        // to $0). Treat a partial pair as NO off-peak so the call falls through
        // to the standard peak rate.
        if op_input > 0.0 && op_output > 0.0 {
            price.off_peak = Some(OffPeak {
                input_usd_per_mtok: op_input,
                output_usd_per_mtok: op_output,
                cache_read_usd_per_mtok: num("cache_read_usd_per_mtok"),
                window_start_utc_min: clamp_window_min(
                    op,
                    model_id,
                    "window_start_utc_min",
                    1439,
                    warn,
                ),
                window_end_utc_min: clamp_window_min(
                    op,
                    model_id,
                    "window_end_utc_min",
                    1440,
                    warn,
                ),
            });
        }
    }
    // Optional peak (time-of-day) block: the premium rates plus the windows
    // they apply in. Absent/malformed -> no peak.
    if let Some(pk) = obj.get("peak").and_then(|v| v.as_object()) {
        let num = |k: &str| pk.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
        let mut windows: Vec<TimeWindow> = Vec::new();
        if let Some(ws) = pk.get("windows").and_then(|v| v.as_array()) {
            for w in ws.iter().filter_map(|w| w.as_object()) {
                windows.push(TimeWindow {
                    window_start_utc_min: clamp_peak_window_min(
                        w,
                        model_id,
                        "window_start_utc_min",
                        1439,
                        warn,
                    ),
                    window_end_utc_min: clamp_peak_window_min(
                        w,
                        model_id,
                        "window_end_utc_min",
                        1440,
                        warn,
                    ),
                    weekdays_only: w
                        .get("weekdays_only")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                });
            }
        }
        if !windows.is_empty() {
            price.peak = Some(Peak {
                input_usd_per_mtok: num("input_usd_per_mtok"),
                output_usd_per_mtok: num("output_usd_per_mtok"),
                cache_read_usd_per_mtok: num("cache_read_usd_per_mtok"),
                windows,
            });
        }
    }
    Some(price)
}

/// Clamp one window minute field of a peak block to its allowed range,
/// warning on out-of-range values instead of silently swallowing them.
///
/// The raw value is read as `u64` (JSON integers are arbitrary-width), so it
/// is compared against the allowed range BEFORE it is narrowed to `u32`: a
/// naive `as u32` cast would silently truncate a value above `u32::MAX` into
/// the valid range and bypass the warning entirely.
fn clamp_peak_window_min(
    block: &serde_json::Map<String, serde_json::Value>,
    model_id: &str,
    key: &str,
    max: u32,
    warn: &mut dyn FnMut(String),
) -> u32 {
    let Some(raw_u64) = block.get(key).and_then(|v| v.as_u64()) else {
        // Absent, non-numeric, or negative -- treat as 0.
        return 0;
    };
    if raw_u64 > u32::MAX as u64 {
        warn(format!(
            "model \"{model_id}\": peak.{key}={raw_u64} exceeds u32::MAX ({}), out of range; clamping to {max}",
            u32::MAX
        ));
        return max;
    }
    let raw = raw_u64 as u32;
    if raw > max {
        warn(format!(
            "model \"{model_id}\": peak.{key}={raw} > {max} (out of range), clamping to {max}"
        ));
        return max;
    }
    raw
}

/// Returns true when `model_id` is a known OpenRouter meta-routing alias that
/// genuinely has no fixed per-token rate (it routes to whichever model OpenRouter
/// picks).  These entries are intentionally missing input/output pricing data, so
/// the loader can skip them without emitting a repeated "malformed" warning.
///
/// IMPORTANT: this is an **explicit list**, not a prefix match.  If a brand-new
/// model happens to be added with the same prefix but is genuinely malformed, it
/// will still pass through `validate_model_price` and produce a warning — so real
/// data-quality problems are never silently swallowed.
pub fn is_known_unpriceable_model(model_id: &str) -> bool {
    matches!(
        model_id.to_ascii_lowercase().as_str(),
        "openrouter/auto"
            | "openrouter/bodybuilder"
            | "openrouter/fusion"
            | "openrouter/pareto-code"
    )
}

fn is_false(b: &bool) -> bool {
    !b
}

/// The data's own date, read from a pricing/corpus file's metadata — NEVER
/// from the file's modification time (which changes on every re-write or
/// copy and says nothing about how old the data is).
///
/// Takes the `generated` field (a `"YYYY-MM-DD"` date or an RFC3339
/// timestamp whose first 10 characters are the date) and the optional
/// `generated_at` field (an RFC3339 timestamp with `Z`). When BOTH parse,
/// `generated_at` wins, because it is the writer's more precise claim.
/// Garbage, empty, or missing inputs yield `None`.
pub fn data_date(generated: &str, generated_at: &str) -> Option<chrono::NaiveDate> {
    if !generated_at.is_empty() {
        if let Ok(d) = parse_data_date(generated_at) {
            return Some(d);
        }
    }
    if !generated.is_empty() {
        if let Ok(d) = parse_data_date(generated) {
            return Some(d);
        }
    }
    None
}

fn parse_data_date(value: &str) -> Result<chrono::NaiveDate, ()> {
    // RFC3339 with an explicit offset ("Z" or ±HH:MM) — take the date part.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(dt.date_naive());
    }
    // "YYYY-MM-DD" (optionally followed by a space+time we do not need).
    let ten: String = value.chars().take(10).collect();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&ten, "%Y-%m-%d") {
        // Guard against "12345-01-02" style strings: the 11th char must be
        // end-of-string, a space, 'T', or 'Z' for the date prefix to count.
        if value.len() == 10
            || matches!(value.chars().nth(10), Some(c) if c == ' ' || c == 'T' || c == 'Z' || c == '+')
        {
            return Ok(d);
        }
    }
    Err(())
}

/// Whether `~/.zoder/pricing.json` or `~/.zoder/model_corpus.json` declares
/// `"managed": true` at its top level, i.e. it was installed by its
/// installer and must not be re-pulled or overwritten by zoder. Reads the
/// file directly (not through the catalog/corpus loaders, which ignore the
/// flag on purpose) so the marker works even when the binary is older or
/// newer than the config.
pub fn file_is_managed(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    v.get("managed").and_then(|m| m.as_bool()).unwrap_or(false)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingCatalog {
    /// Top-level `"managed": true` in the pricing file: the file is managed
    /// by its installer and must never be re-pulled or overwritten by zoder.
    /// Read via [`file_is_managed`] on the raw file; carried here so a loaded
    /// catalog can answer without a second file read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub managed: bool,
    /// Raw bytes of a file that parsed as JSON but is not a pricing catalog
    /// (missing `models` map). Preserved so an unforced `save` of such a
    /// file is a byte-identical no-op instead of clobbering it with an empty
    /// catalog. Never serialized.
    #[serde(skip)]
    pub raw: Option<String>,
    #[serde(default)]
    pub generated: String,
    /// Provenance label of the file's data (e.g. the name of the price
    /// database an installer pulled it from). `None` round-trips as absent
    /// so a file without the field never gains one; an empty string
    /// (deliberately written) round-trips as `""`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Full RFC3339 generation timestamp (with `Z`), when the writer had
    /// one. Wins over `generated` in [`data_date`] when both parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub models: HashMap<String, ModelPrice>,
    /// Frontier baseline `$ / 1M tok` for the avoided-spend estimate.
    #[serde(default)]
    pub baseline_usd_per_mtok: f64,
    #[serde(default)]
    pub baseline_model: String,
}

impl Default for PricingCatalog {
    fn default() -> Self {
        Self {
            managed: false,
            raw: None,
            generated: String::new(),
            source: None,
            generated_at: None,
            window: String::new(),
            models: HashMap::new(),
            baseline_usd_per_mtok: 0.0,
            baseline_model: String::new(),
        }
    }
}

/// Stable verdict from [`PricingCatalog::classify_cost`]: separates "free
/// because the catalog explicitly says so" from "free because we have no
/// data". Treating unknown metered models as free silently (the old
/// `cost()` -> `0.0` behavior) lets a paid custom model whose catalog
/// entry has been deleted, renamed, or never written slip past the
/// pre-call budget gate as $0 — Finding #11. The enum forces the
/// caller to acknowledge the difference: `Free` is honest; `Unknown`
/// means "could not price, do not charge, do not trust as free".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CostVerdict {
    /// Catalog has an explicit non-zero rate and the math came out
    /// finite and non-negative.
    Priced(f64),
    /// Catalog entry has all-zero rates — the model is documented as
    /// free (free-tier, internal, OpenAI free preview, etc.).
    Free,
    /// Either no catalog entry, or the rate(s) were malformed enough
    /// to not pin a value. Cost is reported as 0 to keep the math
    /// safe, but the caller MUST NOT treat this as Free — the
    /// pre-call budget gate fails closed on `Unknown`.
    Unknown,
}

impl PricingCatalog {
    /// Maximum trusted pricing-catalog size. Larger files are rejected before the
    /// body is read: a tampered/oversized catalog can't be trusted to drive the
    /// chargeback baseline.
    const MAX_PRICE_BYTES: u64 = 2_097_152; // 2 MiB

    /// Load the catalog, or an empty catalog if absent/corrupt/insecure/oversized
    /// (never fatal: an unpriced run simply reports $0 chargeback rather than
    /// crashing). The file is opened once and validated via that descriptor
    /// (size, regular-file, and — on Unix — no group/world write) to avoid a
    /// TOCTOU race, then each model entry is validated independently so one
    /// malformed entry can't blank the whole catalog.
    pub fn load(path: &Path) -> Self {
        use std::io::Read;
        let mut cat = Self::default();
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return cat, // absent → empty, silently (not an error)
        };
        let meta = match f.metadata() {
            Ok(m) => m,
            Err(_) => return cat,
        };
        if !meta.is_file() {
            eprintln!(
                "zoder: warning: pricing catalog {} rejected — not a regular file; using empty",
                path.display()
            );
            return cat;
        }
        if meta.len() > Self::MAX_PRICE_BYTES {
            eprintln!("zoder: warning: pricing catalog {} rejected — {} bytes exceeds {} limit; using empty", path.display(), meta.len(), Self::MAX_PRICE_BYTES);
            return cat;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let bad = 0o020 | 0o002; // S_IWGRP | S_IWOTH
            if meta.mode() & bad != 0 {
                eprintln!("zoder: warning: pricing catalog {} rejected — insecure mode {:03o} (must not be group- or world-writable); using empty", path.display(), meta.mode() & 0o777);
                return cat;
            }
        }
        let mut s = String::new();
        if f.read_to_string(&mut s).is_err() {
            return cat;
        }
        let v: serde_json::Value = match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "zoder: warning: pricing catalog {} unreadable ({e}); using empty",
                    path.display()
                );
                return cat;
            }
        };
        // When the file is valid JSON but NOT a pricing catalog (missing
        // `models` map), remember the raw bytes: a save without --force
        // would otherwise replace an unknown-but-valid file with an empty
        // catalog and destroy it.
        let is_catalog = v.get("models").and_then(|m| m.as_object()).is_some();
        if !is_catalog {
            cat.raw = Some(s);
        }
        cat.generated = v
            .get("generated")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        cat.managed = v.get("managed").and_then(|m| m.as_bool()).unwrap_or(false);
        cat.source = v.get("source").and_then(|x| x.as_str()).map(str::to_string);
        cat.generated_at = v
            .get("generated_at")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        cat.window = v
            .get("window")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        cat.baseline_usd_per_mtok = v
            .get("baseline_usd_per_mtok")
            .and_then(|x| x.as_f64())
            .filter(|n| n.is_finite() && *n >= 0.0)
            .unwrap_or(0.0);
        cat.baseline_model = v
            .get("baseline_model")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(models) = v.get("models").and_then(|m| m.as_object()) {
            let mut skipped = 0usize;
            let mut warn = |m: String| eprintln!("zoder: warning: {m}");
            for (k, raw) in models {
                // Known unpriceable meta-routing aliases have no fixed rate by design
                // (they route to whichever model OpenRouter picks).  Skip them
                // silently — only entries that aren't in the explicit list go through
                // the full validation path so real data-quality problems still warn.
                if is_known_unpriceable_model(k) {
                    continue;
                }
                match validate_model_price(k, raw, &mut warn) {
                    Some(price) => {
                        cat.models.insert(k.clone(), price);
                    }
                    None => skipped += 1,
                }
            }
            if skipped > 0 {
                eprintln!(
                    "zoder: warning: skipped {skipped} malformed model entr{} in {}",
                    if skipped == 1 { "y" } else { "ies" },
                    path.display()
                );
            }
        }
        cat
    }

    /// Atomic write (unique temp file + rename).
    ///
    /// C6-P1 (= S19): the temp filename carries the process id AND a monotonic
    /// nonce (`<stem>.json.tmp.<pid>.<nonce>`) so two overlapping `pricing
    /// refresh` runs -- a single refresh does two `save()`s, to
    /// `~/.zoder/pricing.json` and the engine dir -- can never share a temp
    /// path. Otherwise an interleaved write+rename could promote a torn or
    /// foreign temp over the live catalog, and a later `load()` of that torn
    /// file would fall back to an EMPTY catalog, collapsing all cost
    /// classification to $0. The temp is removed on any error so a failed write
    /// never litters the dir with a half-written file a later reader could pick
    /// up. Mirrors the `write_atomic` pattern in `crates/model-health/src/lib.rs`
    /// and `Corpus::save` (kept self-contained here -- no cross-crate dep).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        // A file that loaded as JSON but not as a catalog was never written
        // by zoder: an unforced save must not replace it with an empty
        // catalog. Write the raw bytes back (a byte-identical no-op) and
        // stop. Callers that mean to replace it (e.g. `pricing refresh
        // --force`) build a fresh catalog whose `raw` is `None`.
        if let Some(raw) = &self.raw {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, raw)?;
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let data = serde_json::to_vec_pretty(self)?;
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));
        // Write to the unique temp; on any failure remove it so it can never be
        // renamed over the live catalog or left behind torn.
        if let Err(e) = std::fs::write(&tmp, &data) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        // The loader refuses a group- or world-writable catalog, so the file
        // this writes must not be one. `fs::write` inherits the process
        // umask, and a lax umask (0002, as on many hosts and CI runners)
        // leaves the temp at 0664 -- a catalog the loader would then reject
        // as empty, collapsing every rate to unknown. Strip the group and
        // other write bits so a saved catalog is loadable again.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if let Ok(meta) = std::fs::metadata(&tmp) {
                let mode = meta.mode() & 0o7777 & !0o022;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
            }
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// The age of this catalog in days, as of `today`, from `generated`
    /// (`YYYY-MM-DD`). `None` when the date is missing or unparsable — an
    /// unknown age is never reported as rot.
    pub fn age_days(&self, today: chrono::NaiveDate) -> Option<i64> {
        let built = crate::corpus::parse_corpus_date(&self.generated)?;
        Some((today - built).num_days())
    }

    /// A one-line warning when this catalog is older than `max_days`, or
    /// `None` when it is fresh or its age is unknown. `max_days <= 0`
    /// disables the check.
    ///
    /// Mirrors [`crate::corpus::Corpus::staleness_warning`]: prefixed
    /// `warning:`, printed on stderr only, and it names the command that fixes
    /// it. Stale prices silently mis-state spend, so the operator should hear
    /// about it the same way they hear about a stale corpus.
    pub fn staleness_warning(&self, today: chrono::NaiveDate, max_days: i64) -> Option<String> {
        if max_days <= 0 {
            return None;
        }
        let days = self.age_days(today)?;
        if days <= max_days {
            return None;
        }
        let built = crate::corpus::parse_corpus_date(&self.generated)?;
        Some(format!(
            "warning: pricing catalog is {days} days old (generated {built}). \
             Refresh it with: zoder pricing sync"
        ))
    }

    /// Classify the cost of a single call. Distinct from
    /// [`PricingCatalog::cost`] (which collapses everything to `f64`)
    /// because `Unknown` must be visible: a metered custom model absent
    /// from the catalog returns `Unknown`, and only an explicit
    /// all-zero entry returns `Free`. Cost is `0.0` in both the `Free`
    /// and `Unknown` branches so callers that only care about the
    /// number still get something safe to log; callers that need to
    /// gate on it must check the enum tag.
    pub fn classify_cost(
        &self,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        ts: Option<DateTime<Utc>>,
    ) -> CostVerdict {
        let Some(p) = self.lookup(model) else {
            return CostVerdict::Unknown;
        };
        // Explicit zero: a catalog entry is present but every
        // component is zero. Document this as `Free` (not
        // `Unknown`) — the operator deliberately wrote the entry to
        // pin the model at $0.
        let has_rate =
            p.usd_per_mtok > 0.0 || p.input_usd_per_mtok > 0.0 || p.output_usd_per_mtok > 0.0;
        if !has_rate {
            return CostVerdict::Free;
        }
        let cost = match ts {
            Some(t) => p.cost_io_at(tokens_in, tokens_out, minutes_of_day_utc(t)),
            None => p.cost_io(tokens_in, tokens_out),
        };
        // Validate the arithmetic: a NaN/inf cost is a bug we must
        // not surface as Priced — Finding #24. Negative cost would
        // be a refund-shaped error; treat as Unknown so the budget
        // gate fails closed rather than treating it as a credit.
        if !cost.is_finite() || cost < 0.0 {
            return CostVerdict::Unknown;
        }
        CostVerdict::Priced(cost)
    }

    /// Look up a model price, tolerating id vs display-name drift: exact,
    /// then case-insensitive, then leaf/suffix match (`host/leaf` -> `leaf`).
    /// The price for `model`, falling back to the price list shipped with
    /// zoder when the runtime catalog does not know it.
    ///
    /// The runtime catalog always wins: it is the live source. The shipped
    /// list is the floor, so a host whose runtime catalog is missing, stale
    /// or missing an entry still prices a paid model instead of reporting it
    /// unknown (which fails the budget gate closed) or free (which is a lie).
    pub fn lookup_with_fallback(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(price) = self.lookup(model) {
            return Some(price);
        }
        bundled_catalog().lookup(model)
    }

    /// Classify the cost of one call, with the cache-hit tokens the provider
    /// reported.
    ///
    /// `cache_hit_tokens` is a subset of `tokens_in`: the provider's total
    /// input count, which includes the hits. The hits bill at the cache-read
    /// rate and the rest at the miss rate, so a cache-heavy call is not
    /// billed as if every token had missed the cache.
    pub fn classify_cost_with_cache(
        &self,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        cache_hit_tokens: u64,
        ts: Option<DateTime<Utc>>,
    ) -> CostVerdict {
        let Some(price) = self.lookup_with_fallback(model) else {
            return CostVerdict::Unknown;
        };
        if !price.is_priced() {
            return CostVerdict::Free;
        }
        let cost = match ts {
            Some(t) => price.cost_io_at_ts(tokens_in, tokens_out, cache_hit_tokens, t),
            None => price.cost_io_with_cache(tokens_in, tokens_out, cache_hit_tokens),
        };
        if !cost.is_finite() || cost < 0.0 {
            return CostVerdict::Unknown;
        }
        CostVerdict::Priced(cost)
    }

    pub fn lookup(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.models.get(model) {
            return Some(p);
        }
        let ml = model.to_ascii_lowercase();
        let leaf = ml.rsplit('/').next().unwrap_or(&ml);
        // C6-P3: a leaf/suffix fallback can match several distinct keys (e.g.
        // `openai/gpt-4o` and `azure/gpt-4o` both leaf `gpt-4o`). Iterating a
        // HashMap yields an arbitrary winner, so the returned price would be
        // non-deterministic across runs. Resolve the collision deterministically
        // AND fail-safe: pick the match with the highest effective rate
        // (conservative for chargeback -- never undercharge), tie-broken by the
        // lexicographically-first key so the result is stable regardless of map
        // iteration order.
        let rate = |p: &ModelPrice| -> f64 {
            let mut r = p.usd_per_mtok;
            if p.input_usd_per_mtok > r {
                r = p.input_usd_per_mtok;
            }
            if p.output_usd_per_mtok > r {
                r = p.output_usd_per_mtok;
            }
            r
        };
        let mut best: Option<(&String, &ModelPrice)> = None;
        for (k, v) in self.models.iter() {
            let kl = k.to_ascii_lowercase();
            let key_leaf = kl.rsplit('/').next().unwrap_or(&kl);
            if kl == ml || key_leaf == leaf {
                best = Some(match best {
                    None => (k, v),
                    Some((bk, bv)) => {
                        let (bv_r, v_r) = (rate(bv), rate(v));
                        if v_r > bv_r || (v_r == bv_r && k.as_str() < bk.as_str()) {
                            (k, v)
                        } else {
                            (bk, bv)
                        }
                    }
                });
            }
        }
        best.map(|(_, v)| v)
    }

    /// Chargeback for a call (legacy `f64` interface — collapses
    /// `Free`/`Unknown` to `0.0`). New code should use
    /// [`PricingCatalog::classify_cost`] so the unknown-vs-free
    /// distinction is observable at the budget gate (Finding #11).
    pub fn cost(&self, model: &str, tokens_in: u64, tokens_out: u64) -> f64 {
        self.cost_at(model, tokens_in, tokens_out, None)
    }

    /// Time-of-day-aware chargeback. When `ts` is `Some`, the off-peak
    /// window (if any) is honored so a DeepSeek call at 20:00 UTC uses
    /// the configured $0.14/$0.21 off-peak rates instead of always
    /// charging peak — Finding #23.
    pub fn cost_at(
        &self,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        ts: Option<DateTime<Utc>>,
    ) -> f64 {
        match self.classify_cost(model, tokens_in, tokens_out, ts) {
            CostVerdict::Priced(v) => v,
            CostVerdict::Free | CostVerdict::Unknown => 0.0,
        }
    }

    /// True when the model has a non-zero chargeback rate (a paid cloud model).
    pub fn is_billed(&self, model: &str) -> bool {
        self.lookup(model).map(|p| p.is_priced()).unwrap_or(false)
    }

    /// Avoided spend: `tokens` priced at the frontier baseline. This is the
    /// "if these free tokens had run on a paid frontier model" estimate.
    pub fn avoided(&self, tokens: u64) -> f64 {
        self.baseline_usd_per_mtok * (tokens as f64 / 1_000_000.0)
    }
}

/// Minutes-of-day UTC in `[0, 1440)` for a timestamp. Used by
/// [`PricingCatalog::classify_cost`] to pick the off-peak rate when one
/// is configured. Defined at module scope so the public surface stays
/// inside `PricingCatalog::classify_cost` (callers don't need to know
/// the conversion).
fn minutes_of_day_utc(ts: DateTime<Utc>) -> u32 {
    let secs = ts.timestamp();
    if secs < 0 {
        0
    } else {
        ((secs as u64 % 86_400) / 60) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU32, Ordering as TestOrdering};

    fn deepseek_chat() -> ModelPrice {
        ModelPrice {
            input_usd_per_mtok: 0.28,
            output_usd_per_mtok: 0.42,
            off_peak: Some(OffPeak {
                input_usd_per_mtok: 0.14,
                output_usd_per_mtok: 0.21,
                cache_read_usd_per_mtok: 0.07,
                window_start_utc_min: 990, // 16:30 UTC
                window_end_utc_min: 30,    // 00:30 UTC (wraps midnight)
            }),
            ..Default::default()
        }
    }

    #[test]
    fn off_peak_window_wraps_midnight() {
        let op = deepseek_chat().off_peak.unwrap();
        assert!(op.active_at(990)); // 16:30 start, inclusive
        assert!(op.active_at(1200)); // 20:00, inside
        assert!(op.active_at(0)); // 00:00, past midnight, inside
        assert!(op.active_at(29)); // 00:29, inside
        assert!(!op.active_at(30)); // 00:30 end, exclusive
        assert!(!op.active_at(600)); // 10:00, daytime peak
        assert!(!op.active_at(989)); // 16:29, just before window
    }

    /// Off-peak window end-of-day: `window_end_utc_min: 1440` is the legitimate
    /// exclusive "end of day" sentinel (= 24:00 / midnight) so a window
    /// `(0, 1440)` must cover the FULL day INCLUDING minute 1439 (23:59 UTC).
    /// Before the fix the loader silently clamped any parsed minute to 1439,
    /// collapsing this exact minute back into the peak window and billing
    /// operators' last-minute-of-day calls at peak. The downstream
    /// `active_at` comparison is already `<` (exclusive end), so honoring
    /// `1440` is sufficient to restore the contract.
    #[test]
    fn off_peak_window_end_of_day_1440_covers_last_minute() {
        let raw = serde_json::json!({
            "input_usd_per_mtok": 6.0,
            "output_usd_per_mtok": 18.0,
            "off_peak": {
                "input_usd_per_mtok": 0.50,
                "output_usd_per_mtok": 1.50,
                "window_start_utc_min": 0,
                "window_end_utc_min": 1440
            }
        });
        let mut warnings = Vec::new();
        let p = validate_model_price("allday/op", &raw, &mut |w| warnings.push(w))
            .expect("complete pair with end-of-day window must load");
        // The end boundary must be preserved verbatim, not silently clamped.
        let op = p.off_peak.clone().expect("off-peak kept");
        assert_eq!(
            op.window_end_utc_min, 1440,
            "window_end_utc_min must be preserved as 1440 (got {})",
            op.window_end_utc_min
        );
        // The whole day is in-window, including the last minute at 23:59 UTC.
        // That is THE exact minute that was misclassified before the fix
        // (clamped to 1439, so 23:59 fell outside the would-be-all-day window
        // and got billed at peak).
        assert!(op.active_at(0)); // 00:00 start, inclusive
        assert!(op.active_at(720)); // 12:00
        assert!(op.active_at(1438)); // 23:58
        assert!(op.active_at(1439)); // 23:59
        let cost = p.cost_io_at(1_000_000, 1_000_000, 1439);
        assert!(
            (cost - 2.0).abs() < 1e-9,
            "23:59 UTC call must bill off-peak, got {cost}"
        );
    }

    /// Out-of-range window minutes (e.g. a typo like 1500) must surface as a
    /// warning AND clamp to the field's valid max, instead of being silently
    /// rewritten -- matching how other out-of-range numeric config values
    /// elsewhere in this loader are surfaced (warn, don't swallow).
    #[test]
    fn off_peak_window_out_of_range_minutes_warn_and_clamp() {
        // 1500 on end is above the legitimate exclusive end (1440).
        let raw = serde_json::json!({
            "input_usd_per_mtok": 1.0,
            "output_usd_per_mtok": 2.0,
            "off_peak": {
                "input_usd_per_mtok": 0.10,
                "output_usd_per_mtok": 0.20,
                "window_start_utc_min": 0,
                "window_end_utc_min": 1500
            }
        });
        let mut warnings = Vec::new();
        let p = validate_model_price("typo/op", &raw, &mut |w| warnings.push(w))
            .expect("model with typo'd end must still load (just warned)");
        let op = p.off_peak.expect("off-peak kept");
        assert_eq!(op.window_end_utc_min, 1440, "end clamped to 1440");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("window_end_utc_min=1500")),
            "expected a warning naming the out-of-range value, got {:?}",
            warnings
        );

        // 2000 on start is above the real-minute max (1439).
        let raw2 = serde_json::json!({
            "input_usd_per_mtok": 1.0,
            "output_usd_per_mtok": 2.0,
            "off_peak": {
                "input_usd_per_mtok": 0.10,
                "output_usd_per_mtok": 0.20,
                "window_start_utc_min": 2000,
                "window_end_utc_min": 1440
            }
        });
        let mut warnings2 = Vec::new();
        let p2 = validate_model_price("typo2/op", &raw2, &mut |w| warnings2.push(w))
            .expect("model with typo'd start must still load (just warned)");
        let op2 = p2.off_peak.expect("off-peak kept");
        assert_eq!(op2.window_start_utc_min, 1439, "start clamped to 1439");
        assert!(
            warnings2
                .iter()
                .any(|w| w.contains("window_start_utc_min=2000")),
            "expected a warning naming the out-of-range start, got {:?}",
            warnings2
        );
    }

    /// SECURITY: window minute values larger than `u32::MAX` (4_294_967_295)
    /// must NOT be silently truncated by a naive `as u32` cast before the
    /// out-of-range check. JSON integers are arbitrary-width so an attacker
    /// who controls the catalog can supply e.g. `4_294_967_296` (= `u32::MAX
    /// + 1`), which casts to `0_u32` — and `0 <= 1440`, so the previous
    /// code happily accepted it as a valid end-of-day-style minute and
    /// emitted NO warning. Worse, picking a value like `2 * u32::MAX + 1`
    /// truncates to `1`, letting the attacker pin the off-peak start at
    /// minute-of-day `1` with no audit trail.
    ///
    /// The fix compares the *original* `u64` against `u32::MAX` BEFORE
    /// casting and against the per-field `max` AFTER casting. A
    /// `u32::MAX`-exceeding value must (a) trigger a warning naming the
    /// *original* un-truncated value (so the audit trail is honest), and
    /// (b) clamp to the per-field max instead of becoming a sneaky-valid
    /// minute.
    #[test]
    fn off_peak_window_rejects_u32_max_exceeding_values_before_truncation() {
        // Helper: build a minimal valid off-peak catalog entry with a
        // caller-supplied window minute value.
        let entry = |start: serde_json::Value, end: serde_json::Value| {
            serde_json::json!({
                "input_usd_per_mtok": 1.0,
                "output_usd_per_mtok": 2.0,
                "off_peak": {
                    "input_usd_per_mtok": 0.10,
                    "output_usd_per_mtok": 0.20,
                    "window_start_utc_min": start,
                    "window_end_utc_min": end
                }
            })
        };

        // --- Case 1: `u32::MAX + 1 = 4_294_967_296`. ---
        // This is THE attack value: the OLD code cast it to 0_u32, then
        // saw `0 <= 1440` and accepted it as a valid window_end with no
        // warning. We must catch it.
        // Note: `format!("{}", n_u64)` renders without underscores, so the
        // warning text reads `4294967296`, not `4_294_967_296`.
        let raw = entry(serde_json::json!(0), serde_json::json!(4_294_967_296_u64));
        let mut warnings = Vec::new();
        let p = validate_model_price("huge-end/op", &raw, &mut |w| warnings.push(w))
            .expect("model with huge end must still load (just warned)");
        let op = p.off_peak.expect("off-peak kept");
        assert_eq!(
            op.window_end_utc_min, 1440,
            "u32::MAX+1 on end must clamp to 1440, not silently truncate to 0"
        );
        // The warning must name the ORIGINAL value, not the truncated one.
        // The OLD code would have either (a) printed no warning at all
        // (truncation landed inside the valid range), or (b) printed the
        // truncated value. Either way the audit trail was useless.
        assert!(
            warnings.iter().any(|w| w.contains("4294967296")
                && w.contains("window_end_utc_min")
                && w.contains("exceeds u32::MAX")),
            "expected a warning naming the ORIGINAL value 4294967296 \
             and 'exceeds u32::MAX'; got {:?}",
            warnings
        );

        // --- Case 2: `2 * u32::MAX + 1 = 8_589_934_591` truncates to 1. ---
        // The OLD code would have silently accepted `start = 1` and
        // enabled off-peak coverage from 00:01 UTC onwards with no
        // warning whatsoever. The fix must catch it.
        let raw2 = entry(
            serde_json::json!(8_589_934_591_u64),
            serde_json::json!(1440),
        );
        let mut warnings2 = Vec::new();
        let p2 = validate_model_price("huge-start/op", &raw2, &mut |w| warnings2.push(w))
            .expect("model with huge start must still load (just warned)");
        let op2 = p2.off_peak.expect("off-peak kept");
        assert_eq!(
            op2.window_start_utc_min, 1439,
            "u32::MAX*2+1 on start must clamp to 1439, not silently \
             truncate to 1"
        );
        assert!(
            warnings2.iter().any(|w| w.contains("8589934591")
                && w.contains("window_start_utc_min")
                && w.contains("exceeds u32::MAX")),
            "expected a warning naming the ORIGINAL value 8589934591 \
             and 'exceeds u32::MAX'; got {:?}",
            warnings2
        );

        // --- Case 3: reviewer's example, `5_000_000_000`. ---
        // Truncates to 705_032_704_u32, which is > 1440, so the OLD code
        // DID warn -- but with the truncated value in the message
        // (`window_end_utc_min=705032704`), hiding the actual magnitude
        // from the operator. The fix must show the original value AND
        // route through the dedicated `u32::MAX` branch.
        let raw3 = entry(serde_json::json!(0), serde_json::json!(5_000_000_000_u64));
        let mut warnings3 = Vec::new();
        let p3 = validate_model_price("reviewer/op", &raw3, &mut |w| warnings3.push(w))
            .expect("model with 5B end must still load (just warned)");
        let op3 = p3.off_peak.expect("off-peak kept");
        assert_eq!(op3.window_end_utc_min, 1440, "end clamped to 1440");
        assert!(
            warnings3
                .iter()
                .any(|w| w.contains("5000000000") && w.contains("exceeds u32::MAX")),
            "expected a warning naming the ORIGINAL value 5000000000 \
             and 'exceeds u32::MAX'; got {:?}",
            warnings3
        );
        // And specifically the truncated value must NOT appear in the
        // warning text -- that would mean we lost precision before warning.
        assert!(
            !warnings3.iter().any(|w| w.contains("705032704")),
            "warning must not show truncated value 705032704; got {:?}",
            warnings3
        );

        // --- Case 4: exact `u32::MAX = 4_294_967_295` on start. ---
        // This is the boundary case: `u32::MAX as u32 == u32::MAX == 4294967295`,
        // which is `> max (1439)`, so it MUST fall into the per-field
        // out-of-range branch (the second warning template), NOT the
        // `exceeds u32::MAX` branch. The fix must NOT misclassify a
        // representable `u32` value as overflowing.
        let raw4 = entry(serde_json::json!(u32::MAX as u64), serde_json::json!(1440));
        let mut warnings4 = Vec::new();
        let p4 = validate_model_price("boundary/op", &raw4, &mut |w| warnings4.push(w))
            .expect("model with u32::MAX start must still load (just warned)");
        let op4 = p4.off_peak.expect("off-peak kept");
        assert_eq!(
            op4.window_start_utc_min, 1439,
            "u32::MAX on start must clamp to 1439 via the per-field branch"
        );
        // The warning should use the per-field template (no `exceeds u32::MAX`).
        assert!(
            warnings4
                .iter()
                .any(|w| w.contains("window_start_utc_min=4294967295") && w.contains("> 1439")),
            "u32::MAX must take the per-field out-of-range branch, got {:?}",
            warnings4
        );
        assert!(
            !warnings4.iter().any(|w| w.contains("exceeds u32::MAX")),
            "u32::MAX is representable and must NOT trigger the overflow branch; \
             got {:?}",
            warnings4
        );

        // --- Case 5: a negative integer in JSON. ---
        // serde_json::Value::as_u64 returns None for negatives; we treat
        // that as "missing/0" (matching the original semantics) rather
        // than rejecting the model. This is the softest path; the test
        // pins the behavior so a future refactor can't quietly start
        // accepting negatives as large unsigned values.
        let raw5 = entry(serde_json::json!(-5), serde_json::json!(1440));
        let mut warnings5 = Vec::new();
        let p5 = validate_model_price("negative/op", &raw5, &mut |w| warnings5.push(w))
            .expect("model with negative start must still load (no warning)");
        let op5 = p5.off_peak.expect("off-peak kept");
        assert_eq!(
            op5.window_start_utc_min, 0,
            "negative JSON int on window minute must default to 0 (no cast)"
        );
        assert!(
            warnings5.is_empty(),
            "negative values must not produce a warning (defaulted silently); \
             got {:?}",
            warnings5
        );
    }

    #[test]
    fn cost_io_at_charges_off_peak_in_window_else_standard() {
        let p = deepseek_chat();
        // 1M in + 1M out: standard = 0.28 + 0.42 = 0.70; off-peak = 0.14 + 0.21 = 0.35.
        let std = p.cost_io_at(1_000_000, 1_000_000, 600);
        let off = p.cost_io_at(1_000_000, 1_000_000, 1200);
        assert!((std - 0.70).abs() < 1e-9, "peak {std}");
        assert!((off - 0.35).abs() < 1e-9, "off-peak {off}");
        // No off_peak block → always standard.
        let flat = ModelPrice {
            input_usd_per_mtok: 1.0,
            ..Default::default()
        };
        assert!((flat.cost_io_at(1_000_000, 0, 1200) - 1.0).abs() < 1e-9);
    }

    /// Finding #11: explicit zero = `Free`, missing model = `Unknown`.
    /// Only the unknown-shaped verdict has to fail the budget gate closed;
    /// a deliberately-zeroed entry is an honest "this is free" statement.
    #[test]
    fn classify_cost_distinguishes_free_from_unknown() {
        let mut cat = PricingCatalog::default();
        // Explicit zero entry: every component is zero → Free.
        cat.models.insert(
            "free-preview".to_string(),
            ModelPrice {
                input_usd_per_mtok: 0.0,
                output_usd_per_mtok: 0.0,
                ..Default::default()
            },
        );
        // Paid entry: a metered custom model the catalog knows about.
        cat.models.insert(
            "metered-paid".to_string(),
            ModelPrice {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 2.0,
                ..Default::default()
            },
        );
        assert_eq!(
            cat.classify_cost("free-preview", 1_000_000, 0, None),
            CostVerdict::Free
        );
        assert_eq!(
            cat.classify_cost("metered-paid", 1_000_000, 1_000_000, None),
            CostVerdict::Priced(3.0)
        );
        // Missing from the catalog → Unknown, not Free.
        assert_eq!(
            cat.classify_cost("metered-but-missing", 1_000_000, 0, None),
            CostVerdict::Unknown
        );
        // The legacy `cost()` collapses Unknown and Free to 0.0 so old
        // callers keep their behavior; the new `classify_cost` returns
        // the tagged verdict so the gate can fail closed.
        assert_eq!(cat.cost("metered-but-missing", 1_000_000, 0), 0.0);
        assert_eq!(cat.cost("free-preview", 1_000_000, 0), 0.0);
        assert_eq!(cat.cost("metered-paid", 1_000_000, 0), 1.0);
    }

    #[test]
    fn lookup_does_not_suffix_match_a_different_model_name() {
        let mut cat = PricingCatalog::default();
        cat.models.insert(
            "gpt-4o".into(),
            ModelPrice {
                input_usd_per_mtok: 1.0,
                ..Default::default()
            },
        );
        assert!(cat.lookup("openai/gpt-4o").is_some());
        assert!(cat.lookup("custom/cheap-gpt-4o").is_none());
        assert_eq!(
            cat.classify_cost("custom/cheap-gpt-4o", 1_000, 0, None),
            CostVerdict::Unknown
        );
    }

    /// Finding #23: `cost_at` honors the configured off-peak window so
    /// a DeepSeek-style call inside the window uses off-peak rates, not
    /// peak. `cost()` always uses peak.
    #[test]
    fn cost_at_honors_off_peak_window_per_timestamp() {
        let mut cat = PricingCatalog::default();
        cat.models
            .insert("deepseek/deepseek-chat".to_string(), deepseek_chat());
        // 20:00 UTC → off-peak window active.
        let t_off = Utc.with_ymd_and_hms(2026, 7, 5, 20, 0, 0).unwrap();
        // 10:00 UTC → daytime peak.
        let t_peak = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let off = cat.cost_at("deepseek/deepseek-chat", 1_000_000, 1_000_000, Some(t_off));
        let peak = cat.cost_at("deepseek/deepseek-chat", 1_000_000, 1_000_000, Some(t_peak));
        assert!((off - 0.35).abs() < 1e-9, "off-peak {off}");
        assert!((peak - 0.70).abs() < 1e-9, "peak {peak}");
        // `cost()` (no timestamp) always uses peak — the legacy contract.
        assert!((cat.cost("deepseek/deepseek-chat", 1_000_000, 1_000_000) - 0.70).abs() < 1e-9);
    }

    /// Finding #24: blended pricing on `tokens_in = u64::MAX`,
    /// `tokens_out = 1` must not panic (overflow checks) or wrap to $0
    /// in release mode. The `u128` intermediate forces saturation at the
    /// arithmetic boundary, but the resulting `f64` cost may be
    /// non-finite — in which case `classify_cost` reports `Unknown` so
    /// the gate fails closed rather than billing $0.
    #[test]
    fn blended_pricing_does_not_overflow_or_wrap_extreme_tokens() {
        let p = ModelPrice {
            usd_per_mtok: 1.0,
            ..Default::default()
        };
        let c = p.cost_io(u64::MAX, 1);
        // The exact f64 may be huge or non-finite depending on the
        // rounding path; what we MUST guarantee is that it's not
        // silently zero. Either a non-zero finite value (the cost is
        // huge but the math didn't lie) or a non-finite value (which
        // classify_cost reports as Unknown so the gate fails closed).
        assert!(
            c > 0.0 || !c.is_finite(),
            "cost must not silently wrap to zero (got {c})"
        );
    }

    // A unique per-test temp dir under the OS temp root (no external tempfile
    // dep in this crate's tests).
    fn tmpdir() -> std::path::PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, TestOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zoder_pricing_test_{}_{}_{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// C6-P1 (S19): `save()` writes through a UNIQUE temp file and leaves no
    /// stray `.json.tmp` -- neither the legacy deterministic `<stem>.json.tmp`
    /// nor any per-writer temp survives. A leftover foreign/torn temp is exactly
    /// what could get renamed over the live catalog, collapsing all pricing to
    /// $0 on the next load.
    #[test]
    fn save_uses_unique_temp_and_leaves_no_stray_tmp() {
        let dir = tmpdir();
        let path = dir.join("pricing.json");
        let mut cat = PricingCatalog::default();
        cat.models.insert(
            "vendor/model".to_string(),
            ModelPrice {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 2.0,
                ..Default::default()
            },
        );
        cat.save(&path).unwrap();

        // Test-only: the load security gate rejects any group- or
        // world-writable catalog. Under a lax ambient umask (e.g. 0002,
        // as on many CI runners) `save()`'s fs::write produces a 0664
        // file, which would trip the gate and make `load` return the
        // empty-catalog fallback -- a false failure of THIS test, which
        // is about temp-file hygiene, not the perms gate. Normalize the
        // catalog to 0600 so the reload below exercises the intended path
        // regardless of the caller's umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        // The catalog is present and reloads with its content intact (NOT the
        // empty-catalog fallback a torn temp would produce).
        assert!(path.exists());
        let reloaded = PricingCatalog::load(&path);
        assert_eq!(reloaded.models.len(), 1);
        assert!(reloaded.models.contains_key("vendor/model"));

        // Two more saves in-process must not collide on a temp path and must
        // leave nothing behind.
        cat.save(&path).unwrap();
        cat.save(&path).unwrap();

        // No file matching `.json.tmp` (deterministic or per-writer) survives.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".json.tmp"),
                "stray temp file left behind: {name}"
            );
        }
        // Belt-and-suspenders: the legacy deterministic temp path never exists.
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// C6-P2: a PARTIAL off-peak entry (only the input rate set) must NOT zero
    /// the output charge in-window. The loader rejects the partial pair, so an
    /// in-window call bills output at the PEAK output rate (off-peak ignored).
    /// A COMPLETE off-peak pair still applies both off-peak components.
    #[test]
    fn partial_off_peak_does_not_zero_output_complete_pair_still_applies() {
        // Partial off-peak: only the input rate is present. Peak = 6.0 in +
        // 18.0 out per Mtok.
        let raw_partial = serde_json::json!({
            "input_usd_per_mtok": 6.0,
            "output_usd_per_mtok": 18.0,
            "off_peak": {
                "input_usd_per_mtok": 0.50,
                // output_usd_per_mtok deliberately ABSENT -> partial pair
                "window_start_utc_min": 0,
                "window_end_utc_min": 1440
            }
        });
        let mut warnings = Vec::new();
        let p = validate_model_price("partial/op", &raw_partial, &mut |w| warnings.push(w))
            .expect("standard rates are complete, model must load");
        // The partial off-peak pair is discarded entirely.
        assert!(
            p.off_peak.is_none(),
            "partial off-peak pair must be treated as no off-peak"
        );
        // In-window call (any minute -- the would-be window is all day) bills
        // output at the PEAK rate, not $0. 1M out at peak $18 = 18.0.
        let cost = p.cost_io_at(0, 1_000_000, 720);
        assert!(
            (cost - 18.0).abs() < 1e-9,
            "output must bill at peak $18, got {cost}"
        );

        // A COMPLETE off-peak pair still applies BOTH off-peak components.
        let raw_complete = serde_json::json!({
            "input_usd_per_mtok": 6.0,
            "output_usd_per_mtok": 18.0,
            "off_peak": {
                "input_usd_per_mtok": 0.50,
                "output_usd_per_mtok": 1.50,
                "window_start_utc_min": 0,
                "window_end_utc_min": 1440
            }
        });
        let mut warnings2 = Vec::new();
        let pc = validate_model_price("complete/op", &raw_complete, &mut |w| warnings2.push(w))
            .expect("complete pairs must load");
        assert!(pc.off_peak.is_some(), "complete off-peak pair must be kept");
        // 1M in + 1M out off-peak = 0.50 + 1.50 = 2.00.
        let off = pc.cost_io_at(1_000_000, 1_000_000, 720);
        assert!((off - 2.0).abs() < 1e-9, "complete off-peak {off}");

        // Defense-in-depth: even if a partial off-peak somehow existed on the
        // struct, cost_io_at falls through to peak rather than zeroing output.
        let hand_partial = ModelPrice {
            input_usd_per_mtok: 6.0,
            output_usd_per_mtok: 18.0,
            off_peak: Some(OffPeak {
                input_usd_per_mtok: 0.50,
                output_usd_per_mtok: 0.0, // partial: output missing
                cache_read_usd_per_mtok: 0.0,
                window_start_utc_min: 0,
                window_end_utc_min: 1440,
            }),
            ..Default::default()
        };
        let hp = hand_partial.cost_io_at(0, 1_000_000, 720);
        assert!(
            (hp - 18.0).abs() < 1e-9,
            "cost_io_at guard must fall through to peak, got {hp}"
        );
    }

    /// C6-P3: a leaf-collision query resolves DETERMINISTICALLY and fail-safe --
    /// the MAX-rate match is returned, and the result is identical across
    /// repeated calls regardless of HashMap iteration order.
    #[test]
    fn leaf_collision_returns_deterministic_max_price() {
        let mut cat = PricingCatalog::default();
        cat.models.insert(
            "openai/gpt-4o".to_string(),
            ModelPrice {
                input_usd_per_mtok: 2.50,
                output_usd_per_mtok: 10.0,
                ..Default::default()
            },
        );
        cat.models.insert(
            "azure/gpt-4o".to_string(),
            ModelPrice {
                input_usd_per_mtok: 5.0,
                output_usd_per_mtok: 15.0,
                ..Default::default()
            },
        );
        // The query leaf `gpt-4o` collides with both; the higher-rate azure
        // entry (input 5.0) must win, stably.
        let mut seen = None;
        for _ in 0..64 {
            let p = cat.lookup("vertex/gpt-4o").expect("leaf match");
            let r = p.input_usd_per_mtok;
            match seen {
                None => seen = Some(r),
                Some(prev) => assert!(
                    (prev - r).abs() < 1e-12,
                    "lookup non-deterministic across calls: {prev} vs {r}"
                ),
            }
        }
        assert!(
            (seen.unwrap() - 5.0).abs() < 1e-9,
            "must return the MAX-rate (azure) match, got {}",
            seen.unwrap()
        );
    }
}

#[cfg(test)]
mod cache_aware_pricing_tests {
    use super::*;
    use chrono::TimeZone;

    fn price(input: f64, output: f64, cache: f64) -> ModelPrice {
        ModelPrice {
            input_usd_per_mtok: input,
            output_usd_per_mtok: output,
            cache_read_usd_per_mtok: cache,
            ..Default::default()
        }
    }

    #[test]
    fn cache_hits_bill_at_the_cache_rate_not_the_miss_rate() {
        let p = price(0.15, 0.60, 0.003);
        // 406,422,272 hits at $0.003/M, 5,927,864 misses at $0.15/M and
        // 1,482,797 output at $0.60/M: one day's bill as the provider
        // invoices it.
        let cost = p.cost_io_with_cache(406_422_272 + 5_927_864, 1_482_797, 406_422_272);
        assert!((cost - 2.998_124_616).abs() < 1e-9, "cost was {cost}");
        // The same split priced as if every token missed the cache bills far
        // more -- this is the overcharge the ledger used to record.
        let naive = p.cost_io(406_422_272 + 5_927_864, 1_482_797);
        assert!(naive > cost * 10.0, "naive {naive} vs cache-aware {cost}");
    }

    #[test]
    fn a_catalog_without_a_cache_rate_never_undercharges() {
        let p = price(0.15, 0.60, 0.0);
        let cost = p.cost_io_with_cache(1_000_000, 0, 1_000_000);
        assert!((cost - 0.15).abs() < 1e-12, "cost was {cost}");
    }

    #[test]
    fn the_hit_count_cannot_exceed_the_input_count() {
        let p = price(0.15, 0.60, 0.003);
        let cost = p.cost_io_with_cache(1_000_000, 0, 5_000_000);
        assert!((cost - 0.003).abs() < 1e-12, "cost was {cost}");
    }

    #[test]
    fn a_peak_window_bills_the_peak_rates() {
        let mut p = price(0.15, 0.60, 0.003);
        p.peak = Some(Peak {
            input_usd_per_mtok: 0.30,
            output_usd_per_mtok: 1.20,
            cache_read_usd_per_mtok: 0.006,
            windows: vec![TimeWindow {
                window_start_utc_min: 60,
                window_end_utc_min: 240,
                weekdays_only: true,
            }],
        });
        // Tuesday 02:00 UTC is inside the window.
        let peak_ts = Utc.with_ymd_and_hms(2026, 10, 27, 2, 0, 0).unwrap();
        let cost = p.cost_io_at_ts(1_000_000, 1_000_000, 0, peak_ts);
        assert!((cost - 1.50).abs() < 1e-12, "cost was {cost}");
        // The same minute on a Sunday is outside it: weekdays only.
        let weekend_ts = Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap();
        let cost = p.cost_io_at_ts(1_000_000, 1_000_000, 0, weekend_ts);
        assert!((cost - 0.75).abs() < 1e-12, "cost was {cost}");
    }

    #[test]
    fn the_shipped_price_list_prices_a_model_the_runtime_catalog_does_not_know() {
        let empty = PricingCatalog::default();
        // The runtime catalog on a host that has never run the refresh.
        assert!(empty.lookup("deepseek-flash").is_none());
        assert!(empty.lookup_with_fallback("deepseek-flash").is_some());
    }
}

#[cfg(test)]
mod staleness_tests {
    //! Stale prices mis-state spend silently, so the catalog gets the same
    //! age/warn treatment as the model corpus.
    use super::*;

    fn catalog(generated: &str) -> PricingCatalog {
        PricingCatalog {
            generated: generated.to_string(),
            ..Default::default()
        }
    }

    fn day(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("valid test date")
    }

    #[test]
    fn fresh_catalog_has_no_warning() {
        let c = catalog("2026-10-26");
        assert_eq!(Some(0), c.age_days(day("2026-10-26")));
        assert!(c.staleness_warning(day("2026-10-26"), 7).is_none());
    }

    #[test]
    fn eight_days_old_points_at_pricing_sync() {
        let c = catalog("2026-10-18");
        let w = c.staleness_warning(day("2026-10-26"), 7).expect("warning");
        assert!(
            w.starts_with("warning: pricing catalog is 8 days old"),
            "{w}"
        );
        assert!(w.contains("generated 2026-10-18"), "{w}");
        assert!(w.contains("zoder pricing sync"), "{w}");
    }

    #[test]
    fn seven_days_is_fine_and_zero_disables() {
        let c = catalog("2026-10-19");
        assert!(c.staleness_warning(day("2026-10-26"), 7).is_none());
        let old = catalog("2026-01-01");
        assert!(
            old.staleness_warning(day("2026-10-26"), 0).is_none(),
            "0 disables"
        );
        assert!(old.staleness_warning(day("2026-10-26"), 7).is_some());
    }

    #[test]
    fn missing_generated_never_warns() {
        for g in ["", "  ", "not-a-date"] {
            let c = catalog(g);
            assert_eq!(None, c.age_days(day("2026-10-26")), "{g}");
            assert!(c.staleness_warning(day("2026-10-26"), 7).is_none(), "{g}");
        }
    }
}

#[cfg(test)]
mod pricing_source_metadata_tests {
    use super::*;

    /// Write a catalog file with a mode the loader accepts. The loader
    /// refuses a group- or world-writable catalog, and `fs::write` inherits
    /// the process umask, so on a host with a lax umask (0002, as on many
    /// hosts and CI runners) the file would land at 0664 and be rejected as
    /// empty.
    fn write_catalog(path: &std::path::Path, body: &[u8]) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn data_date_parses_plain_date() {
        assert_eq!(
            data_date("2026-10-14", ""),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap())
        );
    }

    #[test]
    fn data_date_parses_rfc3339_timestamp() {
        assert_eq!(
            data_date("2026-10-14T05:00:00Z", ""),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap())
        );
    }

    #[test]
    fn data_date_generated_at_wins_when_both_parse() {
        assert_eq!(
            data_date("2026-01-01", "2026-10-14T05:00:00Z"),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap())
        );
    }

    #[test]
    fn data_date_generated_used_when_generated_at_garbage() {
        assert_eq!(
            data_date("2026-10-14", "not-a-date"),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap())
        );
    }

    #[test]
    fn data_date_garbage_and_empty_are_none() {
        assert_eq!(data_date("garbage", ""), None);
        assert_eq!(data_date("", ""), None);
        assert_eq!(data_date("", "garbage"), None);
        assert_eq!(data_date("2026-13-99", ""), None);
    }

    /// A pricing file that predates the `generated_at` field must load and
    /// re-save WITHOUT inventing new top-level keys: no `generated_at`, no
    /// `managed`. (Full byte-equality of the re-save is not asserted: the
    /// loader normalizes the per-model rate map, which is pre-existing
    /// behavior — the regression this test pins is the new fields.)
    #[test]
    fn pricing_file_without_new_fields_round_trips_without_new_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        let original = br#"{
    "generated": "2026-10-13",
    "source": "litellm+openrouter (public)",
    "models": {
        "gpt-4o": {
            "input_usd_per_mtok": 0.025,
            "output_usd_per_mtok": 0.1
        }
    }
}"#;
        write_catalog(&path, original);
        let cat = PricingCatalog::load(&path);
        assert_eq!(cat.generated, "2026-10-13");
        // The top-level `source` the file carries must survive the load.
        assert_eq!(cat.source.as_deref(), Some("litellm+openrouter (public)"));
        assert!(cat.generated_at.is_none());
        assert!(!cat.managed);
        assert!(
            cat.raw.is_none(),
            "a valid catalog must not be treated as raw"
        );
        cat.save(&path).unwrap();
        let resaved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !resaved.contains("generated_at"),
            "save must not invent a generated_at key the file never had: {resaved}"
        );
        assert!(
            !resaved.contains("\"managed\""),
            "save must not invent a managed key the file never had: {resaved}"
        );
        assert!(
            resaved.contains("\"source\": \"litellm+openrouter (public)\""),
            "save must keep the top-level source it loaded: {resaved}"
        );
    }

    /// A pricing file WITHOUT the top-level `source` field must re-save
    /// without the loader inventing one: an absent key stays absent.
    #[test]
    fn pricing_file_without_source_field_round_trips_without_new_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        let original = br#"{
    "generated": "2026-10-13",
    "models": {
        "gpt-4o": {
            "input_usd_per_mtok": 0.025,
            "output_usd_per_mtok": 0.1
        }
    }
}"#;
        write_catalog(&path, original);
        let cat = PricingCatalog::load(&path);
        assert!(cat.source.is_none());
        cat.save(&path).unwrap();
        let resaved = std::fs::read_to_string(&path).unwrap();
        // Top-level keys: the re-save must not invent `source`,
        // `generated_at`, or `managed`. (Per-model keys are normalized by
        // the loader — pre-existing behavior — so only top-level is asserted.)
        let root: serde_json::Value = serde_json::from_str(&resaved).unwrap();
        assert!(
            root.get("source").is_none(),
            "save must not invent a top-level source key: {resaved}"
        );
        assert!(
            root.get("generated_at").is_none(),
            "save must not invent a generated_at key: {resaved}"
        );
        assert!(
            root.get("managed").is_none(),
            "save must not invent a managed key: {resaved}"
        );
        assert_eq!(
            root.get("generated").and_then(|v| v.as_str()),
            Some("2026-10-13"),
            "existing keys must survive"
        );
    }

    #[test]
    fn pricing_file_with_new_fields_round_trips_and_is_managed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        let original = br#"{
    "managed": true,
    "generated": "2026-10-14",
    "generated_at": "2026-10-14T05:00:00Z",
    "source": "an internal price database",
    "models": {}
}"#;
        write_catalog(&path, original);
        let cat = PricingCatalog::load(&path);
        assert!(cat.managed);
        assert_eq!(cat.source.as_deref(), Some("an internal price database"));
        assert_eq!(cat.generated_at.as_deref(), Some("2026-10-14T05:00:00Z"));
        assert_eq!(
            data_date(&cat.generated, cat.generated_at.as_deref().unwrap_or("")),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap())
        );
        assert!(file_is_managed(&path));
        cat.save(&path).unwrap();
        let resaved = std::fs::read_to_string(&path).unwrap();
        assert!(resaved.contains("\"managed\": true"));
        assert!(resaved.contains("\"generated_at\": \"2026-10-14T05:00:00Z\""));
        assert!(resaved.contains("\"source\": \"an internal price database\""));
        assert!(file_is_managed(&path));
    }

    #[test]
    fn file_is_managed_false_for_missing_garbage_or_unmanaged() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(!file_is_managed(&missing));
        let garbage = dir.path().join("garbage.json");
        std::fs::write(&garbage, "not json").unwrap();
        assert!(!file_is_managed(&garbage));
        let unmanaged = dir.path().join("pricing.json");
        std::fs::write(&unmanaged, r#"{"managed": false, "models": {}}"#).unwrap();
        assert!(!file_is_managed(&unmanaged));
    }
}
