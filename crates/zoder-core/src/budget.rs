//! Pre-call budget estimate + gate.
//!
//! This module is **pure** (no I/O), so it is fully unit-testable: the caller
//! supplies the projected cost (from the pricing catalog) and the month-to-date
//! spend (from the ledger), and [`Budget::evaluate`] returns a verdict. The CLI
//! wires it in just after the prompt is read, before the model call.

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

impl Budget {
    /// Pure pre-call decision. `estimate_usd` is the projected cost of this call
    /// (pricing catalog × estimated tokens); `month_spent_usd` is the ledger's
    /// month-to-date total. A non-positive estimate is always within budget
    /// (the free path is never gated by spend caps).
    pub fn evaluate(&self, estimate_usd: f64, month_spent_usd: f64) -> BudgetVerdict {
        if estimate_usd <= 0.0 {
            return BudgetVerdict::WithinBudget;
        }
        let mut reasons = Vec::new();
        if let Some(cap) = self.max_cost_per_call_usd {
            if estimate_usd > cap {
                reasons.push(format!(
                    "estimated call cost ${estimate_usd:.4} exceeds the per-call cap ${cap:.2}"
                ));
            }
        }
        if let Some(cap) = self.monthly_cap_usd {
            let after = month_spent_usd + estimate_usd;
            if after > cap {
                reasons.push(format!(
                    "this call (~${estimate_usd:.4}) would push month-to-date spend to \
                     ${after:.2}, over the monthly cap ${cap:.2} (already ${month_spent_usd:.2})"
                ));
            }
        }
        if reasons.is_empty() {
            BudgetVerdict::WithinBudget
        } else {
            BudgetVerdict::Confirm(format!("BUDGET: {}", reasons.join("; ")))
        }
    }
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
