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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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
}

impl ModelPrice {
    /// True when any component (or the blended rate) is non-zero.
    pub fn is_priced(&self) -> bool {
        self.usd_per_mtok > 0.0 || self.input_usd_per_mtok > 0.0 || self.output_usd_per_mtok > 0.0
    }

    /// Cost in USD for an input/output token split. Uses the component rates
    /// when present, otherwise the blended rate over the combined token count.
    pub fn cost_io(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        if self.input_usd_per_mtok > 0.0 || self.output_usd_per_mtok > 0.0 {
            (tokens_in as f64 * self.input_usd_per_mtok
                + tokens_out as f64 * self.output_usd_per_mtok)
                / 1_000_000.0
        } else {
            self.usd_per_mtok * (tokens_in + tokens_out) as f64 / 1_000_000.0
        }
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
    let mut has_rate = false;
    for f in NUMERIC_PRICE_FIELDS {
        match obj.get(f) {
            None | Some(serde_json::Value::Null) => {}
            Some(v) => match v.as_f64() {
                Some(n) if n.is_finite() && n >= 0.0 => {
                    has_rate = true;
                    set_price_field(&mut price, f, n);
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
    if !has_rate {
        warn(format!(
            "model \"{model_id}\": no valid rate fields remain, skipping model"
        ));
        return None;
    }
    price.source = obj
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
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

    /// Atomic write (temp + rename).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Look up a model price, tolerating id vs display-name drift: exact,
    /// then case-insensitive, then leaf/suffix match (`host/leaf` -> `leaf`).
    pub fn lookup(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.models.get(model) {
            return Some(p);
        }
        let ml = model.to_ascii_lowercase();
        let leaf = ml.rsplit('/').next().unwrap_or(&ml);
        self.models.iter().find_map(|(k, v)| {
            let kl = k.to_ascii_lowercase();
            if kl == ml || kl == leaf || ml.ends_with(&kl) || kl.ends_with(leaf) {
                Some(v)
            } else {
                None
            }
        })
    }

    /// Chargeback for a call. Unknown/unpriced model → $0 (free-tier): we never
    /// invent a cost, so the ledger stays honest and the model stays free.
    pub fn cost(&self, model: &str, tokens_in: u64, tokens_out: u64) -> f64 {
        self.lookup(model)
            .map(|p| p.cost_io(tokens_in, tokens_out))
            .unwrap_or(0.0)
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
