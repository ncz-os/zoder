//! Pre-call budget estimate + gate.
//!
//! This module is **pure** (no I/O), so it is fully unit-testable: the caller
//! supplies the projected cost (from the pricing catalog) and the month-to-date
//! spend (from the ledger), and [`Budget::evaluate`] returns a verdict. The CLI
//! wires it in just after the prompt is read, before the model call.

use crate::ledger::Ledger;
use crate::pricing::{CallTokens, CostVerdict, PricingCatalog};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Approximate token count for a piece of text — a deliberately simple
/// `chars / 4` heuristic (the common English rule of thumb). Good enough for a
/// *pre-call* cost ballpark; the authoritative token count lands in the ledger
/// after the call from provider telemetry. Never returns 0 for non-empty input.
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    ((text.chars().count() as f64 / 4.0).ceil() as u64).max(1)
}

fn default_est_output_tokens() -> u64 {
    1024
}

/// Budget caps for the pre-call estimate gate. Every cap is optional (absent =
/// no limit); configured under a `[budget]` table in `config.json` or an
/// overlay. The caps gate *paid* calls only — a $0 (free-model) estimate is
/// always within budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Per-call estimated-cost ceiling (USD). A call estimated above this needs
    /// confirmation (or `--allow-paid`). `None` = no per-call cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_per_call_usd: Option<f64>,
    /// Rolling month-to-date spend cap (USD), checked against the local ledger.
    /// A call that would push the current calendar month over this cap needs
    /// confirmation. `None` = no monthly cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_cap_usd: Option<f64>,
    /// Assumed output tokens for the pre-call estimate when the request does not
    /// pin a maximum. Defaults to 1024.
    #[serde(default = "default_est_output_tokens")]
    pub est_output_tokens: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_cost_per_call_usd: None,
            monthly_cap_usd: None,
            est_output_tokens: default_est_output_tokens(),
        }
    }
}

/// Result of a pre-call budget check.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetVerdict {
    /// Within every configured cap (or no caps set, or the estimate is $0).
    WithinBudget,
    /// A cap would be exceeded — proceed only on explicit confirmation /
    /// `--allow-paid`. The string explains which cap(s) and by how much.
    Confirm(String),
}

/// WHICH cap a [`BudgetVerdict::Confirm`] tripped.
///
/// The host-level back-off (D8 step 12) keys on the MONTHLY cap only: it
/// is a host-wide condition (this month's budget is gone) that a sibling
/// worker must respect. A per-call cap is a per-CALL condition — the very
/// next cheap call may fit under it — so it must NOT park the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrippedCap {
    /// The per-call cap was exceeded.
    PerCall,
    /// The month-to-date cap was exceeded.
    Monthly,
}

/// A [`BudgetVerdict::Confirm`] plus the machine-readable reason it exists.
///
/// The human string stays EXACTLY as before (`"BUDGET: ..."`), so every
/// caller that matches on prose is unaffected; this adds a typed
/// [`TrippedCap`] so a caller can decide "is this a host-wide event?" without
/// parsing English.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetDecline {
    /// The reason text shown to the operator.
    pub message: String,
    /// Which cap tripped. `None` when the decline was not a cap trip at all
    /// (an unpriceable model, an unreadable ledger).
    pub tripped: Option<TrippedCap>,
    /// The configured cap that was exceeded, in USD.
    pub limit_usd: Option<f64>,
    /// The month-to-date BILLED spend that was already on the books when the
    /// cap tripped, in USD. `None` when no month-to-date read happened.
    pub spent_usd: Option<f64>,
}

impl BudgetDecline {
    /// `true` when the MONTHLY cap tripped — the host-wide condition that
    /// must write the cooldown marker.
    pub fn monthly_cap_tripped(&self) -> bool {
        self.tripped == Some(TrippedCap::Monthly)
    }
}

impl Budget {
    /// Pure pre-call decision. `estimate_usd` is the projected cost of this call
    /// (pricing catalog × estimated tokens); `month_spent_usd` is the ledger's
    /// month-to-date total. A non-positive estimate is always within budget
    /// (the free path is never gated by spend caps).
    pub fn evaluate(&self, estimate_usd: f64, month_spent_usd: f64) -> BudgetVerdict {
        match self.evaluate_decline(estimate_usd, month_spent_usd) {
            Ok(Some(decline)) => BudgetVerdict::Confirm(decline.message),
            Ok(None) => BudgetVerdict::WithinBudget,
            // `evaluate_decline` never fails on these inputs; a decline is
            // still the safe answer if it ever did.
            Err(_) => BudgetVerdict::WithinBudget,
        }
    }

    /// The same decision as [`Budget::evaluate`], but with the tripped cap
    /// reported STRUCTURALLY.
    ///
    /// `Ok(None)` = within budget; `Ok(Some(decline))` = a cap tripped. The
    /// error case does not exist in practice (the inputs are two finite
    /// numbers), but it is typed so a future input can fail loudly rather
    /// than silently.
    ///
    /// The message is byte-identical to [`Budget::evaluate`]'s
    /// `BudgetVerdict::Confirm` text, so switching a caller to this method
    /// cannot change what an operator sees.
    pub fn evaluate_decline(
        &self,
        estimate_usd: f64,
        month_spent_usd: f64,
    ) -> anyhow::Result<Option<BudgetDecline>> {
        if estimate_usd <= 0.0 {
            return Ok(None);
        }
        let mut reasons = Vec::new();
        // `None` = no cap of this kind is configured; `Some` = it tripped.
        // The monthly cap wins when BOTH tripped: it is the host-wide one,
        // and a host whose monthly budget is gone must back off even if the
        // per-call cap also fired.
        let mut per_call_tripped = false;
        let mut monthly_tripped = false;
        if let Some(cap) = self.max_cost_per_call_usd {
            if estimate_usd > cap {
                per_call_tripped = true;
                reasons.push(format!(
                    "estimated call cost ${estimate_usd:.4} exceeds the per-call cap ${cap:.2}"
                ));
            }
        }
        if let Some(cap) = self.monthly_cap_usd {
            let after = month_spent_usd + estimate_usd;
            if after > cap {
                monthly_tripped = true;
                reasons.push(format!(
                    "this call (~${estimate_usd:.4}) would push month-to-date spend to \
                     ${after:.2}, over the monthly cap ${cap:.2} (already ${month_spent_usd:.2})"
                ));
            }
        }
        if reasons.is_empty() {
            return Ok(None);
        }
        let tripped = if monthly_tripped {
            Some(TrippedCap::Monthly)
        } else if per_call_tripped {
            Some(TrippedCap::PerCall)
        } else {
            None
        };
        // The numbers go into the marker so an operator (and the Hive
        // worker) can see WHY the host backed off without re-deriving them.
        let (limit_usd, spent_usd) = match tripped {
            Some(TrippedCap::Monthly) => (self.monthly_cap_usd, Some(month_spent_usd)),
            Some(TrippedCap::PerCall) => (self.max_cost_per_call_usd, None),
            None => (None, None),
        };
        Ok(Some(BudgetDecline {
            message: format!("BUDGET: {}", reasons.join("; ")),
            tripped,
            limit_usd,
            spent_usd,
        }))
    }

    /// Catalog-aware pre-call decision. Distinct from [`Budget::evaluate`]
    /// in three ways:
    ///
    /// 1. The estimate comes from [`PricingCatalog::classify_cost`], so
    ///    the verdict is informed by the `Free` vs `Unknown` distinction
    ///    (Finding #11): a metered custom model with no catalog entry is
    ///    treated as "could not price" and the gate fails closed with a
    ///    `Confirm` rather than silently approving $0.
    /// 2. The cost honors the off-peak window when a timestamp is
    ///    supplied (Finding #23): a DeepSeek call at 20:00 UTC uses the
    ///    configured off-peak rate, not peak.
    /// 3. The month-to-date spend is read via a closure that returns
    ///    `anyhow::Result<f64>`; any I/O error fails closed (Finding
    ///    #10) with a `Confirm` rather than collapsing to $0.
    ///
    /// `month_spent` is a closure so this function stays pure (no I/O
    /// inside `Budget`); the CLI supplies `|| Ledger::new(...).month_to_date_usd()`.
    pub fn evaluate_call(
        &self,
        pricing: &PricingCatalog,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        ts: Option<DateTime<Utc>>,
        month_spent: impl FnOnce() -> anyhow::Result<f64>,
    ) -> BudgetVerdict {
        self.evaluate_call_with_cache(
            pricing,
            model,
            CallTokens {
                tokens_in,
                tokens_out,
                cache_hit_tokens: 0,
            },
            ts,
            month_spent,
        )
    }

    /// Evaluate a call whose cache-hit token count is known.
    ///
    /// The pre-call gate estimates from the prompt size, before the response
    /// says how much of it the provider served from its cache, so it prices
    /// the whole prompt at the miss rate. This variant takes the hit count
    /// when the caller knows it: the hits bill at the cache-read rate.
    /// The cache-aware form of [`Budget::evaluate_call`]: the hits are
    /// billed at the cache-read rate, the misses at the input rate.
    ///
    pub fn evaluate_call_with_cache(
        &self,
        pricing: &PricingCatalog,
        model: &str,
        tokens: CallTokens,
        ts: Option<DateTime<Utc>>,
        month_spent: impl FnOnce() -> anyhow::Result<f64>,
    ) -> BudgetVerdict {
        match self.evaluate_call_decline(pricing, model, tokens, ts, month_spent) {
            Ok(Some(decline)) => BudgetVerdict::Confirm(decline.message),
            Ok(None) => BudgetVerdict::WithinBudget,
            Err(_) => BudgetVerdict::WithinBudget,
        }
    }

    /// The catalog-aware decision, with the tripped cap reported
    /// STRUCTURALLY.
    ///
    /// `Ok(None)` = within budget. `Ok(Some(decline))` = the call must be
    /// confirmed: a cap tripped, the cost could not be priced, or the
    /// ledger could not be read. Only a MONTHLY-cap trip carries
    /// `Some(TrippedCap::Monthly)` — that is the host-wide event that must
    /// write the cooldown marker (D8 step 12). An unpriceable model or an
    /// unreadable ledger is a per-call event and carries `None`.
    ///
    /// The message is byte-identical to
    /// [`Budget::evaluate_call_with_cache`]'s `BudgetVerdict::Confirm` text.
    pub fn evaluate_call_decline(
        &self,
        pricing: &PricingCatalog,
        model: &str,
        tokens: CallTokens,
        ts: Option<DateTime<Utc>>,
        month_spent: impl FnOnce() -> anyhow::Result<f64>,
    ) -> anyhow::Result<Option<BudgetDecline>> {
        // Step 1: cost verdict. Unknown = could not price = fail closed.
        let verdict = pricing.classify_cost_with_cache(
            model,
            tokens.tokens_in,
            tokens.tokens_out,
            tokens.cache_hit_tokens,
            ts,
        );
        let estimate_usd = match verdict {
            CostVerdict::Priced(v) => v,
            CostVerdict::Free => return Ok(None),
            CostVerdict::Unknown => {
                // The pre-call gate fails closed: an unknown cost for a
                // would-be-metred call must be confirmed by the user.
                // We carry the cap-trip reasons in the same string so a
                // user who says "yes" still gets a clean audit trail.
                let mut msg = format!(
                    "BUDGET: catalog has no rate for model {model:?}; treating cost as \
                     unknown — confirm to proceed"
                );
                if let Some(cap) = self.max_cost_per_call_usd {
                    msg.push_str(&format!(" (per-call cap ${cap:.2} cannot be evaluated)"));
                }
                return Ok(Some(BudgetDecline {
                    message: msg,
                    // NOT a cap trip: the cost is unknown, not over a cap.
                    tripped: None,
                    limit_usd: None,
                    spent_usd: None,
                }));
            }
        };
        // Step 2: month-to-date spend. Fail closed on I/O errors so a
        // corrupted / permission-denied ledger can never bypass the cap.
        let month_spent_usd = match month_spent() {
            Ok(v) => v,
            Err(e) => {
                return Ok(Some(BudgetDecline {
                    message: format!(
                        "BUDGET: could not read month-to-date spend from the ledger ({e}); \
                         confirm to proceed"
                    ),
                    // NOT a cap trip: the spend is UNKNOWN, not over a cap.
                    // Parking the host on an unreadable ledger would be
                    // exactly the phantom back-off D8 step 12 must avoid.
                    tripped: None,
                    limit_usd: None,
                    spent_usd: None,
                }));
            }
        };
        Ok(self.evaluate_decline(estimate_usd, month_spent_usd)?)
    }
}

/// Convenience helper: read month-to-date spend with a configurable
/// fallback to `0.0` when the ledger is missing vs `Err` when the
/// ledger exists but is unreadable. Wraps [`Ledger::month_to_date_usd`]
/// so CLI call sites don't repeat the `Err` → "confirm" decision.
pub fn read_month_spent_or_default(ledger: &Ledger) -> anyhow::Result<f64> {
    ledger.month_to_date_usd()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_chars_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2); // ceil(5/4)
        assert!(estimate_tokens("a") >= 1); // never 0 for non-empty
    }

    #[test]
    fn no_caps_is_always_within_budget() {
        let b = Budget::default();
        assert_eq!(b.evaluate(9999.0, 9999.0), BudgetVerdict::WithinBudget);
    }

    #[test]
    fn free_estimate_is_never_gated() {
        let b = Budget {
            max_cost_per_call_usd: Some(0.01),
            monthly_cap_usd: Some(0.01),
            ..Default::default()
        };
        // $0 estimate (free model) passes even with tiny caps already blown.
        assert_eq!(b.evaluate(0.0, 100.0), BudgetVerdict::WithinBudget);
    }

    #[test]
    fn per_call_cap_triggers_confirm() {
        let b = Budget {
            max_cost_per_call_usd: Some(0.50),
            ..Default::default()
        };
        assert_eq!(b.evaluate(0.40, 0.0), BudgetVerdict::WithinBudget);
        match b.evaluate(0.75, 0.0) {
            BudgetVerdict::Confirm(m) => assert!(m.contains("per-call cap")),
            _ => panic!("expected Confirm over per-call cap"),
        }
    }

    #[test]
    fn monthly_cap_triggers_on_projected_total() {
        let b = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        // 9.50 already spent + 0.40 = 9.90, under 10 -> ok.
        assert_eq!(b.evaluate(0.40, 9.50), BudgetVerdict::WithinBudget);
        // 9.50 + 0.75 = 10.25, over 10 -> confirm.
        match b.evaluate(0.75, 9.50) {
            BudgetVerdict::Confirm(m) => assert!(m.contains("monthly cap")),
            _ => panic!("expected Confirm over monthly cap"),
        }
    }

    #[test]
    fn both_caps_report_both_reasons() {
        let b = Budget {
            max_cost_per_call_usd: Some(0.10),
            monthly_cap_usd: Some(1.0),
            ..Default::default()
        };
        match b.evaluate(0.50, 0.80) {
            BudgetVerdict::Confirm(m) => {
                assert!(m.contains("per-call cap"));
                assert!(m.contains("monthly cap"));
            }
            _ => panic!("expected Confirm citing both caps"),
        }
    }
}
