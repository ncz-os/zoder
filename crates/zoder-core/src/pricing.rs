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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process nonce so two overlapping `save()` calls in the same
/// process never collide on the temp path. Combined with `std::process::id()`
/// it makes each writer's temp file (`<stem>.json.tmp.<pid>.<nonce>`) unique,
/// so an interleaved write+rename can never promote a torn or foreign temp
/// over the live catalog. Mirrors `write_atomic` in `crates/model-health` and
/// `Corpus::save` (C5-4) -- self-contained, no cross-crate dependency.
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

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
}

/// Off-peak (time-of-day) rates + the UTC window they apply in. Minutes are
/// minutes-of-day UTC in `[0, 1440)`; a window may wrap midnight (`start > end`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OffPeak {
    #[serde(default)]
    pub input_usd_per_mtok: f64,
    #[serde(default)]
    pub output_usd_per_mtok: f64,
    #[serde(default)]
    pub window_start_utc_min: u32,
    #[serde(default)]
    pub window_end_utc_min: u32,
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
        let umin = |k: &str| (op.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as u32).min(1439);
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
                window_start_utc_min: umin("window_start_utc_min"),
                window_end_utc_min: umin("window_end_utc_min"),
            });
        }
    }
    Some(price)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingCatalog {
    #[serde(default)]
    pub generated: String,
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
        cat.generated = v
            .get("generated")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
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
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
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
