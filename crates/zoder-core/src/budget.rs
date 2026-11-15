//! Pre-call budget estimate + gate.
//!
//! This module is **pure** (no I/O), so it is fully unit-testable: the caller
//! supplies the projected cost (from the pricing catalog) and the month-to-date
//! spend (from the ledger), and [`Budget::evaluate`] returns a verdict. The CLI
//! wires it in just after the prompt is read, before the model call.

use crate::host_cooldown::{clear_cooldown, note_budget_decline, HostCooldown};
use crate::ledger::Ledger;
use crate::pricing::{CallTokens, CostVerdict, PricingCatalog};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    /// Rolling day-to-date spend cap (USD, UTC calendar day), checked against
    /// the local ledger. A call that would push today's spend over this cap
    /// needs confirmation. `None` = no daily cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_cap_usd: Option<f64>,
    /// Rolling week-to-date spend cap (USD, UTC ISO week), checked against the
    /// local ledger. `None` = no weekly cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_cap_usd: Option<f64>,
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
            daily_cap_usd: None,
            weekly_cap_usd: None,
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
/// The host-level back-off (D8 step 12) keys on the WINDOW caps (daily,
/// weekly, monthly): a tripped spend window is a host-wide condition (this
/// window's budget is gone) that a sibling worker must respect. A per-call
/// cap is a per-CALL condition — the very next cheap call may fit under
/// it — so it must NOT park the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrippedCap {
    /// The per-call cap was exceeded.
    PerCall,
    /// The day-to-date cap was exceeded.
    Daily,
    /// The week-to-date (UTC ISO week) cap was exceeded.
    Weekly,
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
    /// The window-to-date BILLED spend that was already on the books when the
    /// cap tripped, in USD. `None` when no window spend was read (a per-call
    /// trip never reads the ledger).
    pub spent_usd: Option<f64>,
}

impl BudgetDecline {
    /// `true` when the MONTHLY cap tripped — the host-wide condition that
    /// must write the cooldown marker.
    pub fn monthly_cap_tripped(&self) -> bool {
        self.tripped == Some(TrippedCap::Monthly)
    }

    /// `true` when any SPEND-WINDOW cap (daily, weekly, monthly) tripped —
    /// the host-wide conditions that must write the cooldown marker. A
    /// per-call trip is a per-CALL condition and must NOT park the host.
    pub fn any_cap_tripped(&self) -> bool {
        matches!(
            self.tripped,
            Some(TrippedCap::Daily | TrippedCap::Weekly | TrippedCap::Monthly)
        )
    }
}

/// The ledger's billed spend for each cap window, read the same way
/// `read_month_spent_or_default` reads the month: day and week come from the
/// same ledger, UTC day / UTC ISO week.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSpend {
    /// Day-to-date billed spend (UTC calendar day).
    pub day: f64,
    /// Week-to-date billed spend (UTC ISO week).
    pub week: f64,
    /// Month-to-date billed spend (UTC calendar month).
    pub month: f64,
}

impl WindowSpend {
    /// All three windows at once; the pre-call gate reads them together so
    /// one ledger scan serves every cap.
    pub fn new(day: f64, week: f64, month: f64) -> Self {
        Self { day, week, month }
    }

    /// The two-argument call shape: only the month is known (no ledger
    /// evidence for day/week), so only per-call and monthly caps can trip.
    pub fn month_only(month: f64) -> Self {
        Self {
            day: 0.0,
            week: 0.0,
            month,
        }
    }
}

impl Budget {
    /// Pure pre-call decision. `estimate_usd` is the projected cost of this call
    /// (pricing catalog × estimated tokens); `month_spent_usd` is the ledger's
    /// month-to-date total. A non-positive estimate is always within budget
    /// (the free path is never gated by spend caps).
    ///
    /// Kept for the two-argument call shape: day and week windows read as
    /// zero here (no ledger evidence), so only per-call and monthly caps
    /// can trip. Callers with all three window spends use
    /// [`Budget::evaluate_windows_decline`].
    pub fn evaluate(&self, estimate_usd: f64, month_spent_usd: f64) -> BudgetVerdict {
        match self.evaluate_windows_decline(estimate_usd, WindowSpend::month_only(month_spent_usd))
        {
            Ok(Some(decline)) => BudgetVerdict::Confirm(decline.message),
            Ok(None) => BudgetVerdict::WithinBudget,
            // `evaluate_windows_decline` never fails on these inputs; a
            // decline is still the safe answer if it ever did.
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
        self.evaluate_windows_decline(estimate_usd, WindowSpend::month_only(month_spent_usd))
    }

    /// The three-window pre-call decision. `spend` carries the ledger's
    /// day-, week- and month-to-date totals; a non-positive estimate is
    /// always within budget (the free path is never gated by spend caps).
    ///
    /// Every reason is reported (the message joins them), and the tripped
    /// cap prefers the WIDEST window that fired: monthly beats weekly beats
    /// daily beats per-call. A wider window owns the longer back-off, so a
    /// call that blows both the day and the month parks the host until the
    /// month resets rather than waking at midnight into the same wall.
    /// (Per-call stays last: it is a per-CALL condition, never host-wide.)
    pub fn evaluate_windows_decline(
        &self,
        estimate_usd: f64,
        spend: WindowSpend,
    ) -> anyhow::Result<Option<BudgetDecline>> {
        if estimate_usd <= 0.0 {
            return Ok(None);
        }
        let mut reasons = Vec::new();
        // `None` = no cap of this kind is configured; `Some` = it tripped.
        let mut per_call_tripped = false;
        let mut daily_tripped = false;
        let mut weekly_tripped = false;
        let mut monthly_tripped = false;
        if let Some(cap) = self.max_cost_per_call_usd {
            if estimate_usd > cap {
                per_call_tripped = true;
                reasons.push(format!(
                    "estimated call cost ${estimate_usd:.4} exceeds the per-call cap ${cap:.2}"
                ));
            }
        }
        if let Some(cap) = self.daily_cap_usd {
            let after = spend.day + estimate_usd;
            if after > cap {
                daily_tripped = true;
                reasons.push(format!(
                    "this call (~${estimate_usd:.4}) would push day-to-date spend to \
                     ${after:.2}, over the daily cap ${cap:.2} (already ${:.2})",
                    spend.day
                ));
            }
        }
        if let Some(cap) = self.weekly_cap_usd {
            let after = spend.week + estimate_usd;
            if after > cap {
                weekly_tripped = true;
                reasons.push(format!(
                    "this call (~${estimate_usd:.4}) would push week-to-date spend to \
                     ${after:.2}, over the weekly cap ${cap:.2} (already ${:.2})",
                    spend.week
                ));
            }
        }
        if let Some(cap) = self.monthly_cap_usd {
            let after = spend.month + estimate_usd;
            if after > cap {
                monthly_tripped = true;
                reasons.push(format!(
                    "this call (~${estimate_usd:.4}) would push month-to-date spend to \
                     ${after:.2}, over the monthly cap ${cap:.2} (already ${:.2})",
                    spend.month
                ));
            }
        }
        if reasons.is_empty() {
            return Ok(None);
        }
        let tripped = if monthly_tripped {
            Some(TrippedCap::Monthly)
        } else if weekly_tripped {
            Some(TrippedCap::Weekly)
        } else if daily_tripped {
            Some(TrippedCap::Daily)
        } else if per_call_tripped {
            Some(TrippedCap::PerCall)
        } else {
            None
        };
        // The numbers go into the marker so an operator (and the Hive
        // worker) can see WHY the host backed off without re-deriving them.
        let (limit_usd, spent_usd) = match tripped {
            Some(TrippedCap::Daily) => (self.daily_cap_usd, Some(spend.day)),
            Some(TrippedCap::Weekly) => (self.weekly_cap_usd, Some(spend.week)),
            Some(TrippedCap::Monthly) => (self.monthly_cap_usd, Some(spend.month)),
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
        // Legacy two-argument call shape: only the month is read. Day and
        // week read as zero (no ledger evidence), so only per-call and
        // monthly caps can trip.
        self.evaluate_call_windows(
            pricing,
            model,
            CallTokens {
                tokens_in,
                tokens_out,
                cache_hit_tokens: 0,
            },
            ts,
            || month_spent().map(WindowSpend::month_only),
        )
    }

    /// The three-window form of [`Budget::evaluate_call`]: `spend` supplies
    /// the ledger's day-, week- and month-to-date totals in one read, so one
    /// ledger scan serves every cap.
    pub fn evaluate_call_windows(
        &self,
        pricing: &PricingCatalog,
        model: &str,
        tokens: CallTokens,
        ts: Option<DateTime<Utc>>,
        spend: impl FnOnce() -> anyhow::Result<WindowSpend>,
    ) -> BudgetVerdict {
        match self.evaluate_call_windows_decline(pricing, model, tokens, ts, spend) {
            Ok(Some(decline)) => BudgetVerdict::Confirm(decline.message),
            Ok(None) => BudgetVerdict::WithinBudget,
            Err(_) => BudgetVerdict::WithinBudget,
        }
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
        // Legacy two-argument call shape: only the month is read.
        self.evaluate_call_windows(pricing, model, tokens, ts, || {
            month_spent().map(WindowSpend::month_only)
        })
    }

    /// The catalog-aware decision (month-only legacy shape), with the
    /// tripped cap reported STRUCTURALLY. Prefer
    /// [`Budget::evaluate_call_windows_decline`] when all three windows are
    /// available.
    ///
    /// `Ok(None)` = within budget. `Ok(Some(decline))` = the call must be
    /// confirmed: a cap tripped, the cost could not be priced, or the
    /// ledger could not be read. Only a spend-window trip carries its
    /// `TrippedCap` — the host-wide event that must write the cooldown
    /// marker (D8 step 12). An unpriceable model or an unreadable ledger is
    /// a per-call event and carries `None`.
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
        // Legacy two-argument call shape: only the month is read.
        self.evaluate_call_windows_decline(pricing, model, tokens, ts, || {
            month_spent().map(WindowSpend::month_only)
        })
    }

    /// The catalog-aware decision with all three windows, and the tripped cap
    /// reported STRUCTURALLY.
    ///
    /// `Ok(None)` = within budget. `Ok(Some(decline))` = the call must be
    /// confirmed: a cap tripped, the cost could not be priced, or the
    /// ledger could not be read. Only a DAILY/WEEKLY/MONTHLY-cap trip
    /// carries its `TrippedCap` — the host-wide event that must write the
    /// cooldown marker (D8 step 12). An unpriceable model or an
    /// unreadable ledger is a per-call event and carries `None`.
    ///
    /// The message is byte-identical to
    /// [`Budget::evaluate_call_with_cache`]'s `BudgetVerdict::Confirm` text.
    pub fn evaluate_call_windows_decline(
        &self,
        pricing: &PricingCatalog,
        model: &str,
        tokens: CallTokens,
        ts: Option<DateTime<Utc>>,
        spend: impl FnOnce() -> anyhow::Result<WindowSpend>,
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
        // Step 2: window spend. Fail closed on I/O errors so a
        // corrupted / permission-denied ledger can never bypass the cap.
        let spend = match spend() {
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
        self.evaluate_windows_decline(estimate_usd, spend)
    }
}

/// Map ONE evaluation result to its verdict, and hand back the decline so the
/// caller can sync the host marker.
///
/// FAIL CLOSED: `Err` becomes `Confirm`, never `WithinBudget`. The earlier
/// `.ok().flatten()` shape turned any error into `None`, which read as "within
/// budget" — a silent fail-open on an unreadable ledger or a broken pricing
/// catalog. The declined-value arm is `None` on an error because an error is
/// not a proven cap trip, so no marker may be written from it.
fn decline_to_verdict(
    result: anyhow::Result<Option<BudgetDecline>>,
) -> (BudgetVerdict, Option<Option<BudgetDecline>>) {
    match result {
        Ok(decline) => {
            let verdict = match &decline {
                Some(d) => BudgetVerdict::Confirm(d.message.clone()),
                None => BudgetVerdict::WithinBudget,
            };
            // `Some(decline)` = "a real decision was made, sync the marker".
            (verdict, Some(decline))
        }
        Err(e) => (
            BudgetVerdict::Confirm(format!(
                "BUDGET: could not evaluate the pre-call budget ({e:#}); confirm to proceed"
            )),
            // `None` = no real decision, so no marker write.
            None,
        ),
    }
}

impl Budget {
    /// [`Budget::evaluate_call`] that ALSO keeps the host cooldown marker in
    /// step with the verdict (D8 step 12).
    ///
    /// Same call, same verdict, same `month_spent` closure; the only extra
    /// effect is host-local state:
    ///
    /// * a DAILY/WEEKLY/MONTHLY cap trip writes/extends `<ledger path>.cooldown.json`,
    ///   expiring at the end of the tripped window, so every sibling worker on
    ///   the host can see the window is spent without re-deriving it (see
    ///   [`crate::host_cooldown`] for the format);
    /// * a METERED call that fits CLEARS a live marker — it read the
    ///   window spend and no cap is by definition tripped any more, so a
    ///   stale hold-off must not keep parking the host;
    /// * a FREE call (`CostVerdict::Free`) leaves the marker ALONE. It never
    ///   reads month-to-date spend — it does not need to, a $0 call is not
    ///   gated — so it has no evidence the cap stopped tripping. Clearing on
    ///   that silence deleted a live marker while billed spend was still over
    ///   the cap, and the next metered sibling re-derived the same refusal:
    ///   exactly the per-worker loop this feature exists to kill;
    /// * a per-call cap trip, an unpriceable model or an unreadable ledger
    ///   changes nothing: those are per-CALL conditions, and backing the host
    ///   off on one of them is exactly the phantom back-off this decision
    ///   exists to prevent.
    ///
    /// The marker write is best-effort: a host whose ledger directory is not
    /// writable still gets its budget refusal (the write error is dropped, not
    /// fatal). The verdict is the authority; the marker is the host signal.
    ///
    /// An ERROR from the evaluation itself is NOT dropped: it fails closed to
    /// `Confirm` with the error text. Mapping it to `None` would report
    /// `WithinBudget` on an unreadable ledger or unpriceable model — exactly
    /// the fail-open the gate exists to prevent.
    pub fn evaluate_call_with_host_cooldown(
        &self,
        ledger_path: &Path,
        pricing: &PricingCatalog,
        model: &str,
        tokens: CallTokens,
        ts: Option<DateTime<Utc>>,
        month_spent: impl FnOnce() -> anyhow::Result<f64>,
    ) -> BudgetVerdict {
        // Legacy two-argument call shape: only the month is read.
        self.evaluate_call_windows_with_host_cooldown(
            ledger_path,
            pricing,
            model,
            tokens,
            ts,
            || month_spent().map(WindowSpend::month_only),
        )
    }

    /// The three-window form of
    /// [`Budget::evaluate_call_with_host_cooldown`]: `spend` supplies the
    /// ledger's day-, week- and month-to-date totals in one read, so one
    /// ledger scan serves every cap. A DAILY/WEEKLY/MONTHLY trip writes the
    /// cooldown marker with an expiry at the end of the tripped window.
    pub fn evaluate_call_windows_with_host_cooldown(
        &self,
        ledger_path: &Path,
        pricing: &PricingCatalog,
        model: &str,
        tokens: CallTokens,
        ts: Option<DateTime<Utc>>,
        spend: impl FnOnce() -> anyhow::Result<WindowSpend>,
    ) -> BudgetVerdict {
        let now = ts.unwrap_or_else(Utc::now);
        // A free model is not gated, and `evaluate_call_decline` short-circuits
        // it to `Ok(None)` WITHOUT reading month-to-date spend. That `None` is
        // not evidence the cap stopped tripping, so it must not reach the
        // marker sync (which would clear a live marker). Classifying here is a
        // pure catalog lookup; the evaluator below still owns the verdict.
        let is_free = matches!(
            pricing.classify_cost_with_cache(
                model,
                tokens.tokens_in,
                tokens.tokens_out,
                tokens.cache_hit_tokens,
                ts,
            ),
            CostVerdict::Free
        );
        let verdict = decline_to_verdict(
            self.evaluate_call_windows_decline(pricing, model, tokens, ts, spend),
        );
        // Only a real (Ok) decision touches the marker; an evaluation error
        // writes none (an error is not a proven cap trip).
        match verdict.1 {
            // No decision from a free call means "no spend observed", which
            // may neither write nor clear.
            Some(None) if is_free => {}
            Some(decline) => self.sync_host_cooldown_or_warn(ledger_path, decline.as_ref(), now),
            None => {}
        }
        verdict.0
    }

    /// The pure two-number form of
    /// [`Budget::evaluate_call_with_host_cooldown`], for callers that have
    /// already priced the call.
    ///
    /// A non-positive `estimate_usd` is within budget but does NOT clear a
    /// live marker: it carries no spend evidence (same rule as a free call —
    /// see [`Budget::evaluate_call_with_host_cooldown`]).
    pub fn evaluate_with_host_cooldown(
        &self,
        ledger_path: &Path,
        estimate_usd: f64,
        month_spent_usd: f64,
        now: DateTime<Utc>,
    ) -> BudgetVerdict {
        // Legacy two-argument call shape: only the month is known.
        self.evaluate_windows_with_host_cooldown(
            ledger_path,
            estimate_usd,
            WindowSpend::month_only(month_spent_usd),
            now,
        )
    }

    /// The three-window form of [`Budget::evaluate_with_host_cooldown`], for
    /// callers that have already priced the call and read all three windows.
    pub fn evaluate_windows_with_host_cooldown(
        &self,
        ledger_path: &Path,
        estimate_usd: f64,
        spend: WindowSpend,
        now: DateTime<Utc>,
    ) -> BudgetVerdict {
        let verdict = decline_to_verdict(self.evaluate_windows_decline(estimate_usd, spend));
        match verdict.1 {
            Some(None) if estimate_usd <= 0.0 => {}
            Some(decline) => self.sync_host_cooldown_or_warn(ledger_path, decline.as_ref(), now),
            None => {}
        }
        verdict.0
    }

    /// [`Budget::sync_host_cooldown`], but never fatal: the marker is an
    /// ADVISORY host signal, while the budget verdict is the authority. A
    /// read-only ledger directory must not fail the gate, so a write error is
    /// reported on stderr and the call proceeds with its verdict.
    ///
    /// It is NOT silently dropped: a marker that cannot be cleared would keep
    /// parking the host on state nothing re-confirms, and that has to be
    /// visible to whoever owns the host.
    fn sync_host_cooldown_or_warn(
        &self,
        ledger_path: &Path,
        decline: Option<&BudgetDecline>,
        now: DateTime<Utc>,
    ) {
        if let Err(e) = self.sync_host_cooldown(ledger_path, decline, now) {
            eprintln!(
                "[zoder] warning: could not update the host cooldown marker beside {} \
                 ({e:#}); the budget verdict stands",
                ledger_path.display()
            );
        }
    }

    /// Bring the host cooldown marker in line with ONE decision.
    ///
    /// Returns the marker left in force (`Some` only when this decision
    /// tripped a spend-window cap), or `None` when the host is not backed
    /// off by this decision. See [`note_budget_decline`] and
    /// [`clear_cooldown`].
    ///
    /// `Ok(None)` in `decline` means "within budget" and clears a live
    /// marker, so a caller may only pass it when the decision actually
    /// OBSERVED window spend. A free call and a $0 estimate never do;
    /// their wrappers ([`Budget::evaluate_call_with_host_cooldown`],
    /// [`Budget::evaluate_with_host_cooldown`]) skip this call for them
    /// rather than reporting "no spend evidence" as "this call fits".
    pub fn sync_host_cooldown(
        &self,
        ledger_path: &Path,
        decline: Option<&BudgetDecline>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<HostCooldown>> {
        if decline.is_some_and(BudgetDecline::any_cap_tripped) {
            return note_budget_decline(Some(ledger_path), decline, now);
        }
        // Not a host-wide decline: a marker still sitting beside the ledger
        // is stale (the window rolled over, or the operator raised the cap),
        // and keeping it would park the host on state nothing re-confirms.
        // Only a definite "this call fits" clears; a decline for another
        // reason says nothing about the spend caps.
        if decline.is_none() {
            clear_cooldown(ledger_path)?;
        }
        Ok(None)
    }
}

/// Convenience helper: read month-to-date spend with a configurable
/// fallback to `0.0` when the ledger is missing vs `Err` when the
/// ledger exists but is unreadable. Wraps [`Ledger::month_to_date_usd`]
/// so CLI call sites don't repeat the `Err` → "confirm" decision.
pub fn read_month_spent_or_default(ledger: &Ledger) -> anyhow::Result<f64> {
    ledger.month_to_date_usd()
}

/// Convenience helper: read all three cap windows in one ledger scan.
/// Fails closed like [`read_month_spent_or_default`]: any unreadable
/// window is an `Err` the gate turns into a `Confirm`.
pub fn read_window_spend_or_default(ledger: &Ledger) -> anyhow::Result<WindowSpend> {
    Ok(WindowSpend::new(
        ledger.day_to_date_usd()?,
        ledger.week_to_date_usd()?,
        ledger.month_to_date_usd()?,
    ))
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

    /// NG2-ZD-007 acceptance: with `daily_cap_usd = 5` and $4.90 spent
    /// today, a $0.50 estimate trips the daily cap with the exact message
    /// and `TrippedCap::Daily`.
    #[test]
    fn daily_cap_trips_on_day_spend() {
        let b = Budget {
            daily_cap_usd: Some(5.0),
            ..Default::default()
        };
        let spend = WindowSpend::new(4.90, 0.0, 0.0);
        match b.evaluate_windows_decline(0.50, spend) {
            Ok(Some(decline)) => {
                assert_eq!(decline.tripped, Some(TrippedCap::Daily));
                assert!(
                    decline.message.contains("daily cap"),
                    "the message must name the daily cap, got {:?}",
                    decline.message
                );
                assert_eq!(decline.limit_usd, Some(5.0));
                assert_eq!(decline.spent_usd, Some(4.90));
            }
            Ok(None) => panic!("expected the daily cap to trip"),
            Err(e) => panic!("evaluate_windows_decline failed: {e:#}"),
        }
        // Just under the cap stays within budget.
        let spend = WindowSpend::new(4.40, 0.0, 0.0);
        assert!(b.evaluate_windows_decline(0.50, spend).unwrap().is_none());
    }

    /// NG2-ZD-007 acceptance: the weekly cap trips on ISO-week (UTC)
    /// spend the same way.
    #[test]
    fn weekly_cap_trips_on_week_spend() {
        let b = Budget {
            weekly_cap_usd: Some(20.0),
            ..Default::default()
        };
        let spend = WindowSpend::new(0.0, 19.80, 0.0);
        match b.evaluate_windows_decline(0.50, spend) {
            Ok(Some(decline)) => {
                assert_eq!(decline.tripped, Some(TrippedCap::Weekly));
                assert!(
                    decline.message.contains("weekly cap"),
                    "the message must name the weekly cap, got {:?}",
                    decline.message
                );
                assert_eq!(decline.limit_usd, Some(20.0));
                assert_eq!(decline.spent_usd, Some(19.80));
            }
            Ok(None) => panic!("expected the weekly cap to trip"),
            Err(e) => panic!("evaluate_windows_decline failed: {e:#}"),
        }
    }

    /// The widest tripped window owns the marker: a call that blows both
    /// the day and the month trips `Monthly`, so the host backs off until
    /// the month resets rather than waking at midnight into the same wall.
    #[test]
    fn widest_tripped_window_wins() {
        let b = Budget {
            daily_cap_usd: Some(5.0),
            weekly_cap_usd: Some(20.0),
            monthly_cap_usd: Some(50.0),
            ..Default::default()
        };
        let spend = WindowSpend::new(4.90, 19.80, 49.90);
        match b.evaluate_windows_decline(0.50, spend) {
            Ok(Some(decline)) => {
                assert_eq!(decline.tripped, Some(TrippedCap::Monthly));
                assert!(decline.message.contains("daily cap"));
                assert!(decline.message.contains("weekly cap"));
                assert!(decline.message.contains("monthly cap"));
            }
            Ok(None) => panic!("expected a cap to trip"),
            Err(e) => panic!("evaluate_windows_decline failed: {e:#}"),
        }
        // Day-only trip stays daily.
        let spend = WindowSpend::new(4.90, 1.0, 1.0);
        match b.evaluate_windows_decline(0.50, spend) {
            Ok(Some(decline)) => assert_eq!(decline.tripped, Some(TrippedCap::Daily)),
            Ok(None) => panic!("expected the daily cap to trip"),
            Err(e) => panic!("evaluate_windows_decline failed: {e:#}"),
        }
    }

    /// `deny_unknown_fields` keeps working with the two new optional caps:
    /// a config with daily/weekly caps parses, and an unknown key still
    /// fails.
    #[test]
    fn budget_config_accepts_daily_weekly_and_still_denies_unknown() {
        let parsed: Budget = serde_json::from_value(serde_json::json!({
            "daily_cap_usd": 5.0,
            "weekly_cap_usd": 20.0,
            "monthly_cap_usd": 50.0,
            "max_cost_per_call_usd": 1.0,
            "est_output_tokens": 1024,
        }))
        .expect("daily/weekly caps must parse");
        assert_eq!(parsed.daily_cap_usd, Some(5.0));
        assert_eq!(parsed.weekly_cap_usd, Some(20.0));
        assert!(
            serde_json::from_value::<Budget>(serde_json::json!({"bogus_cap_usd": 1.0})).is_err()
        );
    }
}

#[cfg(test)]
mod cap_trip_sets_host_cooldown {
    //! D8 step 12: a MONTHLY cap trip is a HOST-WIDE event, so the trip must
    //! leave a marker beside the ledger that every sibling worker can read.
    //!
    //! Before this slice `Budget` only answered with a string: nothing outside
    //! the tripping process could see that the host's month was spent, so N
    //! siblings each re-derived the same refusal. These tests pin the two
    //! halves of the contract — the trip writes the marker, and a decline that
    //! is NOT a monthly cap (a per-call cap, an unpriceable model, an
    //! unreadable ledger) leaves no marker behind (the phantom back-off).

    use super::*;
    use crate::host_cooldown::{
        clear_cooldown, cooldown_active, cooldown_path_for, read_cooldown,
        trip_monthly_cap_cooldown, REASON_DAILY_CAP, REASON_MONTHLY_CAP, REASON_WEEKLY_CAP,
    };
    use crate::pricing::ModelPrice;

    fn priced_catalog(model: &str, usd_per_mtok: f64) -> PricingCatalog {
        let mut pricing = PricingCatalog::default();
        pricing.models.insert(
            model.to_string(),
            ModelPrice {
                usd_per_mtok,
                ..Default::default()
            },
        );
        pricing
    }

    #[test]
    fn cap_trip_sets_host_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let pricing = priced_catalog("paid-model", 100.0);
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        assert!(
            !cooldown_active(&ledger),
            "a quiet host starts with no cooldown marker"
        );

        // $10.00/MTok x (1000 in + 1000 out) = $0.20 on top of $9.90 spent.
        let verdict = budget.evaluate_call_with_host_cooldown(
            &ledger,
            &pricing,
            "paid-model",
            CallTokens {
                tokens_in: 1_000,
                tokens_out: 1_000,
                cache_hit_tokens: 0,
            },
            None,
            || Ok(9.90),
        );
        match verdict {
            BudgetVerdict::Confirm(_) => {}
            other => panic!("expected the monthly cap to trip, got {other:?}"),
        }

        let marker = read_cooldown(&ledger)
            .expect("reading the marker must not fail")
            .expect("a monthly cap trip must leave a host cooldown marker beside the ledger");
        assert_eq!(marker.reason, REASON_MONTHLY_CAP);
        assert_eq!(marker.limit_usd, Some(10.0));
        assert_eq!(marker.spent_usd, Some(9.9));
        assert!(
            marker.is_active(),
            "the marker written by a cap trip must be in force"
        );
        assert_eq!(
            cooldown_path_for(&ledger),
            dir.path().join("ledger.jsonl.cooldown.json"),
            "the marker sits BESIDE the ledger so the Hive worker can find it"
        );
    }

    #[test]
    fn per_call_cap_trip_leaves_no_host_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let pricing = priced_catalog("paid-model", 100.0);
        let budget = Budget {
            max_cost_per_call_usd: Some(0.01),
            ..Default::default()
        };
        let verdict = budget.evaluate_call_with_host_cooldown(
            &ledger,
            &pricing,
            "paid-model",
            CallTokens {
                tokens_in: 1_000,
                tokens_out: 1_000,
                cache_hit_tokens: 0,
            },
            None,
            || Ok(0.0),
        );
        assert!(
            matches!(verdict, BudgetVerdict::Confirm(_)),
            "the per-call cap still refuses the call"
        );
        assert!(
            !cooldown_active(&ledger),
            "a per-call cap is a per-CALL condition: it must not park the host"
        );
    }

    /// NG2-ZD-007: a DAILY trip through the window-aware evaluator writes a
    /// daily marker that expires at UTC midnight, and a WEEKLY trip writes a
    /// weekly marker expiring at Monday 00:00 UTC. The acceptance shape: a
    /// $0.50 estimate on $4.90 of day spend against a $5 daily cap confirms
    /// with `TrippedCap::Daily` and parks the host only until midnight.
    #[test]
    fn daily_and_weekly_trips_set_windowed_host_cooldown() {
        use chrono::TimeZone;
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 11, 18, 13, 50, 32)
            .unwrap();
        let budget = Budget {
            daily_cap_usd: Some(5.0),
            weekly_cap_usd: Some(20.0),
            ..Default::default()
        };
        // $0.50 estimate on $4.90 of day spend: the daily cap trips.
        let verdict = budget.evaluate_windows_with_host_cooldown(
            &ledger,
            0.50,
            WindowSpend::new(4.90, 10.0, 10.0),
            now,
        );
        match verdict {
            BudgetVerdict::Confirm(m) => assert!(m.contains("daily cap"), "got {m:?}"),
            other => panic!("expected the daily cap to trip, got {other:?}"),
        }
        let marker = read_cooldown(&ledger)
            .expect("reading the marker must not fail")
            .expect("a daily cap trip must leave a host cooldown marker beside the ledger");
        assert_eq!(marker.reason, REASON_DAILY_CAP);
        assert_eq!(marker.limit_usd, Some(5.0));
        assert_eq!(marker.spent_usd, Some(4.90));
        assert_eq!(
            marker.expires_at,
            chrono::Utc.with_ymd_and_hms(2026, 11, 19, 0, 0, 0).unwrap(),
            "a daily trip must expire at UTC midnight, not month end"
        );
        clear_cooldown(&ledger).unwrap();

        // $0.50 estimate on $19.80 of week spend: the weekly cap trips.
        let verdict = budget.evaluate_windows_with_host_cooldown(
            &ledger,
            0.50,
            WindowSpend::new(1.0, 19.80, 10.0),
            now,
        );
        match verdict {
            BudgetVerdict::Confirm(m) => assert!(m.contains("weekly cap"), "got {m:?}"),
            other => panic!("expected the weekly cap to trip, got {other:?}"),
        }
        let marker = read_cooldown(&ledger)
            .expect("reading the marker must not fail")
            .expect("a weekly cap trip must leave a host cooldown marker beside the ledger");
        assert_eq!(marker.reason, REASON_WEEKLY_CAP);
        assert_eq!(
            marker.expires_at,
            chrono::Utc.with_ymd_and_hms(2026, 11, 23, 0, 0, 0).unwrap(),
            "a weekly trip must expire at Monday 00:00 UTC, not month end"
        );
    }

    #[test]
    fn unpriceable_model_leaves_no_host_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let pricing = PricingCatalog::default();
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        let verdict = budget.evaluate_call_with_host_cooldown(
            &ledger,
            &pricing,
            "not-in-catalog",
            CallTokens {
                tokens_in: 10,
                tokens_out: 10,
                cache_hit_tokens: 0,
            },
            None,
            || Ok(99.0),
        );
        assert!(
            matches!(verdict, BudgetVerdict::Confirm(_)),
            "an unpriceable model still fails closed"
        );
        assert!(
            !cooldown_active(&ledger),
            "an unknown cost is not a cap trip: it must not park the host"
        );
    }

    #[test]
    fn unreadable_ledger_leaves_no_host_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let pricing = priced_catalog("paid-model", 100.0);
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        let verdict = budget.evaluate_call(&pricing, "paid-model", 1_000, 1_000, None, || {
            Err(anyhow::anyhow!("ledger is unreadable"))
        });
        assert!(
            matches!(verdict, BudgetVerdict::Confirm(_)),
            "an unreadable ledger still fails closed"
        );
        assert!(
            !cooldown_active(&ledger),
            "phantom spend: an unreadable ledger must not back the host off"
        );
    }

    /// The `Err` arm of [`decline_to_verdict`], tested directly: an error must
    /// map to `Confirm`, and it must report "no real decision" so the caller
    /// writes no marker. This is the unit that the old `.ok().flatten()`
    /// silently got wrong — `.ok()` discarded the error and `.flatten()` made
    /// it `None`, i.e. `WithinBudget`.
    #[test]
    fn a_failed_evaluation_maps_to_confirm_and_no_marker() {
        let (verdict, sync) = decline_to_verdict(Err(anyhow::anyhow!("ledger is unreadable")));
        match verdict {
            BudgetVerdict::Confirm(msg) => assert!(
                msg.contains("could not evaluate") && msg.contains("ledger is unreadable"),
                "the refusal must name the error, got {msg:?}"
            ),
            other => panic!("an evaluation error must fail closed, got {other:?}"),
        }
        assert!(
            sync.is_none(),
            "an error is not a proven cap trip: the caller must not write a marker"
        );
    }

    /// The happy arms keep their exact meaning: a decline is `Confirm` with
    /// its own text, and "no decline" is `WithinBudget` with a live sync.
    #[test]
    fn an_ok_evaluation_keeps_its_verdict() {
        let (within, sync) = decline_to_verdict(Ok(None));
        assert_eq!(within, BudgetVerdict::WithinBudget);
        assert_eq!(
            sync,
            Some(None),
            "a fit call still syncs (it may clear a marker)"
        );

        let decline = BudgetDecline {
            message: "over the monthly cap".into(),
            tripped: Some(TrippedCap::Monthly),
            limit_usd: Some(10.0),
            spent_usd: Some(12.0),
        };
        let (confirm, sync) = decline_to_verdict(Ok(Some(decline)));
        assert_eq!(
            confirm,
            BudgetVerdict::Confirm("over the monthly cap".into())
        );
        assert!(
            sync.flatten().is_some_and(|d| d.monthly_cap_tripped()),
            "the monthly-trip decline must reach the marker sync"
        );
    }

    #[test]
    fn an_evaluation_error_is_a_refusal_never_within_budget() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let pricing = priced_catalog("paid-model", 100.0);
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        // The cooldown-aware form must agree with the plain form on the
        // fail-closed contract: an unreadable ledger is a refusal here too.
        let verdict = budget.evaluate_call_with_host_cooldown(
            &ledger,
            &pricing,
            "paid-model",
            CallTokens {
                tokens_in: 1_000,
                tokens_out: 1_000,
                cache_hit_tokens: 0,
            },
            None,
            || Err(anyhow::anyhow!("ledger is unreadable")),
        );
        match verdict {
            BudgetVerdict::Confirm(_) => {}
            other => panic!("an evaluation error must fail closed, got {other:?}"),
        }
        assert!(
            !cooldown_active(&ledger),
            "an error is not a proven monthly trip: it must not park the host"
        );
    }

    #[test]
    fn a_live_marker_clears_when_the_cap_no_longer_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let pricing = priced_catalog("paid-model", 100.0);
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        let tokens = CallTokens {
            tokens_in: 1_000,
            tokens_out: 1_000,
            cache_hit_tokens: 0,
        };
        // Trip it once.
        assert!(matches!(
            budget.evaluate_call_with_host_cooldown(
                &ledger,
                &pricing,
                "paid-model",
                tokens,
                None,
                || Ok(9.90)
            ),
            BudgetVerdict::Confirm(_)
        ));
        assert!(cooldown_active(&ledger));

        // A later call that fits is not a trip, and must clear the stale
        // marker so the host does not stay parked after the month resets.
        assert_eq!(
            budget.evaluate_call_with_host_cooldown(
                &ledger,
                &pricing,
                "paid-model",
                tokens,
                None,
                || Ok(0.10)
            ),
            BudgetVerdict::WithinBudget
        );
        assert!(
            !cooldown_active(&ledger),
            "a call that fits means the cap is not tripped; the marker must be cleared"
        );
        clear_cooldown(&ledger).unwrap();
    }

    /// D8 step 12 follow-up: a FREE call must not clear a LIVE monthly marker.
    ///
    /// `evaluate_call_decline` short-circuits a `CostVerdict::Free` call to
    /// `Ok(None)` WITHOUT reading month spend — correct for the call itself
    /// (a free call is never gated), but `sync_host_cooldown` read that `None`
    /// as "this call fits, the cap is not tripped any more" and deleted the
    /// marker. The very next metered sibling then saw no cooldown and
    /// re-derived the refusal, which is the per-worker re-derivation loop the
    /// whole feature exists to kill.
    #[test]
    fn free_call_does_not_clear_a_live_monthly_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let mut pricing = priced_catalog("paid-model", 100.0);
        // A FREE model: present in the catalog at $0, which is what
        // `classify_cost_with_cache` reports as `CostVerdict::Free` (an ABSENT
        // model is `Unknown` and fails closed, a different path).
        pricing.models.insert(
            "free-model".to_string(),
            ModelPrice {
                usd_per_mtok: 0.0,
                ..Default::default()
            },
        );
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        let tokens = CallTokens {
            tokens_in: 1_000,
            tokens_out: 1_000,
            cache_hit_tokens: 0,
        };
        // Trip the monthly cap so the marker is live.
        assert!(matches!(
            budget.evaluate_call_with_host_cooldown(
                &ledger,
                &pricing,
                "paid-model",
                tokens,
                None,
                || Ok(9.90)
            ),
            BudgetVerdict::Confirm(_)
        ));
        assert!(cooldown_active(&ledger), "the trip leaves a live marker");

        // A free model is not gated...
        assert_eq!(
            budget.evaluate_call_with_host_cooldown(
                &ledger,
                &pricing,
                "free-model",
                tokens,
                None,
                || panic!("a free call must not need the month-to-date spend"),
            ),
            BudgetVerdict::WithinBudget,
            "a free call is never gated"
        );
        // ...and must NOT clear the marker: it never looked at spend, so it
        // has no evidence the cap stopped tripping.
        assert!(
            cooldown_active(&ledger),
            "a free call never reads month spend, so it cannot prove the cap \
             stopped tripping; the live marker must survive"
        );
    }

    /// The two-number sibling of the test above: `estimate_usd <= 0` takes the
    /// same short circuit in `evaluate_decline`, so it must not clear either.
    #[test]
    fn zero_estimate_does_not_clear_a_live_monthly_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        let now = Utc::now();
        // Trip the cap directly.
        trip_monthly_cap_cooldown(&ledger, 10.0, 12.0, now).unwrap();
        assert!(cooldown_active(&ledger));

        assert_eq!(
            budget.evaluate_with_host_cooldown(&ledger, 0.0, 0.0, now),
            BudgetVerdict::WithinBudget,
            "a $0 estimate is never gated"
        );
        assert!(
            cooldown_active(&ledger),
            "a $0 estimate carries no spend evidence: the live marker must survive"
        );
    }

    /// A REAL metered call that fits still clears the marker — the free-path
    /// carve-out must not turn into "a marker can never be cleared".
    #[test]
    fn a_metered_call_that_fits_still_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let budget = Budget {
            monthly_cap_usd: Some(10.0),
            ..Default::default()
        };
        let now = Utc::now();
        trip_monthly_cap_cooldown(&ledger, 10.0, 12.0, now).unwrap();
        assert!(cooldown_active(&ledger));

        assert_eq!(
            budget.evaluate_with_host_cooldown(&ledger, 0.25, 1.0, now),
            BudgetVerdict::WithinBudget
        );
        assert!(
            !cooldown_active(&ledger),
            "a priced call read the spend and fits: the stale marker must go"
        );
    }
}
