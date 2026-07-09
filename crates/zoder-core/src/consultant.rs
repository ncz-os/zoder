//! Model consultant: rank every routable model by the same signals the router
//! already uses (capability, ELO, health) so a human can pick one without
//! digging through the corpus.
//!
//! This is the data layer behind the future `model consultant` pane; it does
//! no scoring of its own — it reuses the canonical `ModelEntry` helpers
//! (`routable`, `code_capability`, `swe_elo`, `value_score`, `arena_label`) and
//! only orders, filters, and annotates with health state.

use std::cmp::Ordering;

use crate::corpus::Corpus;
use crate::health::HealthStore;

/// Knobs for [`consult`].
#[derive(Debug, Clone, Default)]
pub struct ConsultOptions {
    /// Honor `free_only` as an explicit filter even though [`crate::corpus::ModelEntry::routable`]
    /// already implies `free` (so the flag is redundant but kept for clarity at
    /// the call site — e.g. "user asked for free models only").
    pub free_only: bool,
    /// Cap on the number of rows returned. `None` keeps them all.
    pub limit: Option<usize>,
}

/// One row in the consultant advisory. Sorted best-first by [`consult`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct Advisory {
    /// 1-based rank, assigned after sorting.
    pub rank: usize,
    pub model_id: String,
    pub family: String,
    pub free: bool,
    pub code_capability: Option<f64>,
    pub swe_elo: Option<f64>,
    pub value_score: Option<f64>,
    pub arena_label: Option<String>,
    pub latency_class: Option<String>,
    pub breaker_open: bool,
    pub available: bool,
}

/// Compare two `Option<f64>` with `Some > None` and `f64::total_cmp` for the
/// `Some` arm. Never calls `.unwrap()` on `partial_cmp` — `f64` is fully
/// ordered here so the comparator is total.
fn cmp_opt_desc(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less, // a (Some) is better -> a sorts first
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Rank every routable model in `corpus`, annotated with breaker state from
/// `health`. Best first.
pub fn consult(corpus: &Corpus, health: &HealthStore, opts: &ConsultOptions) -> Vec<Advisory> {
    let mut rows: Vec<Advisory> = corpus
        .models
        .iter()
        .filter(|m| m.routable())
        // free_only is implied by routable() (routable requires free), but we
        // apply it explicitly so the caller's intent is honored verbatim and
        // the filter is visible at the call site.
        .filter(|m| !opts.free_only || m.free)
        .map(|m| {
            let breaker_open = health.breaker_open(&m.id);
            // A model can be skip-for-now (Unauthorized/Unprovisioned/Capacity)
            // while its breaker is closed: those classifications are
            // breaker-neutral (W1). Fold that into availability so consult does
            // not advertise a guaranteed-failed model as available.
            let skipped = health.is_skipped_by_classification(&m.id);
            Advisory {
                rank: 0, // assigned after sorting
                model_id: m.id.clone(),
                family: m.family.clone(),
                free: m.free,
                code_capability: m.code_capability(),
                swe_elo: m.swe_elo(),
                value_score: m.value_score(),
                arena_label: m.arena_label(),
                latency_class: m.latency_class.clone(),
                breaker_open,
                available: !breaker_open && !skipped,
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        // a) availability: available (breaker NOT open) first.
        match (a.available, b.available) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        // b) code_capability desc (None last).
        match cmp_opt_desc(a.code_capability, b.code_capability) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
        // c) swe_elo desc (None last).
        match cmp_opt_desc(a.swe_elo, b.swe_elo) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
        // d) final tie-break: id ascending (lexicographic, total order).
        a.model_id.cmp(&b.model_id)
    });

    if let Some(n) = opts.limit {
        rows.truncate(n);
    }

    // Assign 1-based rank now that the order is final.
    for (i, row) in rows.iter_mut().enumerate() {
        row.rank = i + 1;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{BenchScore, Capability, ModelEntry};

    /// Build a routable entry (free + chat + route_candidate) with an optional
    /// SWE-bench Verified capability score.
    fn model(id: &str, cap: Option<f64>) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            family: "test".to_string(),
            free: true,
            route_candidate: true,
            kind: "chat".to_string(),
            capability: cap.map(|a| Capability {
                swe_verified: Some(BenchScore {
                    acc: Some(a),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Open the breaker for a model id by recording enough failures.
    fn open_breaker(health: &mut HealthStore, id: &str) {
        for _ in 0..10 {
            health.record_failure(id, "boom");
        }
        assert!(health.breaker_open(id), "fixture: breaker should be open");
    }

    fn opts() -> ConsultOptions {
        ConsultOptions::default()
    }

    #[test]
    fn healthy_high_cap_ranks_above_healthy_low_cap() {
        let high = model("z-high", Some(90.0));
        let low = model("a-low", Some(40.0));
        let corpus = Corpus {
            models: vec![low.clone(), high.clone()],
            ..Default::default()
        };
        let health = HealthStore::default();

        let rows = consult(&corpus, &health, &opts());

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model_id, "z-high");
        assert_eq!(rows[0].rank, 1);
        assert_eq!(rows[0].code_capability, Some(90.0));
        assert_eq!(rows[1].model_id, "a-low");
        assert_eq!(rows[1].rank, 2);
        // Both healthy -> both available.
        assert!(rows.iter().all(|r| r.available && !r.breaker_open));
    }

    #[test]
    fn breaker_open_sinks_below_every_available_model() {
        // Breaker-open model has the HIGHEST capability, but health wins.
        let poisoned = model("a-poisoned", Some(99.0));
        let medium = model("m-medium", Some(70.0));
        let low = model("z-low", Some(30.0));

        let corpus = Corpus {
            models: vec![poisoned.clone(), medium.clone(), low.clone()],
            ..Default::default()
        };

        let mut health = HealthStore::default();
        open_breaker(&mut health, "a-poisoned");

        let rows = consult(&corpus, &health, &opts());

        assert_eq!(rows.len(), 3);
        // All three are routable, but the poisoned one is unavailable, so it
        // must rank last regardless of its superior capability.
        assert_eq!(rows[0].model_id, "m-medium");
        assert_eq!(rows[1].model_id, "z-low");
        assert_eq!(rows[2].model_id, "a-poisoned");
        assert!(rows[2].breaker_open);
        assert!(!rows[2].available);
        assert!(rows[0].available && rows[1].available);
    }

    #[test]
    fn skip_class_model_is_unavailable_despite_closed_breaker() {
        // C2-2 regression on the consult path: a model stamped Unauthorized
        // (401) is skip-for-now but breaker-neutral (W1), so its breaker stays
        // CLOSED. consult must still report available=false, or it advertises a
        // guaranteed-failed model as usable.
        let skipped = model("a-skipped", Some(99.0));
        let healthy = model("m-healthy", Some(70.0));
        let corpus = Corpus {
            models: vec![skipped.clone(), healthy.clone()],
            ..Default::default()
        };

        let mut health = HealthStore::default();
        health.record_classified_failure(
            "a-skipped",
            "401 Unauthorized",
            "prov",
            crate::health::Classification::Unauthorized,
        );
        // The trap: the breaker did NOT open.
        assert!(
            !health.breaker_open("a-skipped"),
            "precondition: 401 must be breaker-neutral"
        );

        let rows = consult(&corpus, &health, &opts());
        let skipped_row = rows.iter().find(|r| r.model_id == "a-skipped").unwrap();
        assert!(
            !skipped_row.breaker_open,
            "breaker must stay closed for a 401 model"
        );
        assert!(
            !skipped_row.available,
            "a skip-classified model must be reported unavailable"
        );
        // The healthy model outranks it despite lower capability.
        assert_eq!(rows[0].model_id, "m-healthy");
        assert!(rows[0].available);
    }

    #[test]
    fn free_only_and_non_routable_are_excluded() {
        // Routable free chat candidate (baseline).
        let good = model("good", Some(80.0));
        // Non-chat kind -> not routable.
        let embedding = ModelEntry {
            id: "embed-1".into(),
            kind: "embedding".into(),
            ..model("embed-1", Some(95.0))
        };
        // Not flagged as a route candidate.
        let not_candidate = ModelEntry {
            id: "not-cand".into(),
            route_candidate: false,
            ..model("not-cand", Some(95.0))
        };
        // Explicitly paid (fails the free predicate).
        let paid = ModelEntry {
            id: "paid".into(),
            free: false,
            ..model("paid", Some(95.0))
        };

        let corpus = Corpus {
            models: vec![
                good.clone(),
                embedding,
                not_candidate,
                paid,
                good.clone(), // dup id is fine; just a sanity belt-and-braces
            ],
            ..Default::default()
        };
        let health = HealthStore::default();

        // free_only = false: still only routable entries survive (routable()
        // itself is the gate).
        let rows_all = consult(&corpus, &health, &opts());
        assert_eq!(rows_all.len(), 2, "both copies of `good` are routable");
        assert!(rows_all.iter().all(|r| r.model_id == "good"));

        // free_only = true: same set, but verified as an explicit filter too.
        let rows_free = consult(
            &corpus,
            &health,
            &ConsultOptions {
                free_only: true,
                ..Default::default()
            },
        );
        assert_eq!(rows_free.len(), 2);
        assert!(rows_free.iter().all(|r| r.free));
    }

    #[test]
    fn limit_truncates_result() {
        let corpus = Corpus {
            models: vec![
                model("c", Some(60.0)),
                model("a", Some(80.0)),
                model("b", Some(70.0)),
            ],
            ..Default::default()
        };
        let health = HealthStore::default();

        let rows = consult(
            &corpus,
            &health,
            &ConsultOptions {
                free_only: false,
                limit: Some(2),
            },
        );
        assert_eq!(rows.len(), 2);
        // Sorted by capability desc -> a (80), b (70).
        assert_eq!(rows[0].model_id, "a");
        assert_eq!(rows[1].model_id, "b");
        // Rank reflects post-truncation position, still 1-based.
        assert_eq!(rows[0].rank, 1);
        assert_eq!(rows[1].rank, 2);
    }

    #[test]
    fn equal_scores_break_tie_by_id_ascending() {
        let corpus = Corpus {
            models: vec![
                model("zeta", Some(50.0)),
                model("alpha", Some(50.0)),
                model("mu", Some(50.0)),
            ],
            ..Default::default()
        };
        let health = HealthStore::default();

        let rows = consult(&corpus, &health, &opts());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].model_id, "alpha");
        assert_eq!(rows[1].model_id, "mu");
        assert_eq!(rows[2].model_id, "zeta");
        // And the order is identical across calls (deterministic).
        let rows2 = consult(&corpus, &health, &opts());
        assert_eq!(
            rows.iter().map(|r| &r.model_id).collect::<Vec<_>>(),
            rows2.iter().map(|r| &r.model_id).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn none_capability_sorts_below_any_some() {
        let with_cap = model("has-cap", Some(60.0));
        let no_cap = model("no-cap", None);
        let corpus = Corpus {
            models: vec![no_cap.clone(), with_cap.clone()],
            ..Default::default()
        };
        let health = HealthStore::default();

        let rows = consult(&corpus, &health, &opts());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model_id, "has-cap");
        assert_eq!(rows[0].code_capability, Some(60.0));
        assert_eq!(rows[1].model_id, "no-cap");
        assert_eq!(rows[1].code_capability, None);
        assert_eq!(rows[1].rank, 2);

        // Also: the Some value wins even when it is SMALLER than the
        // alternative... but here we have no other Some to compare against,
        // so build that case explicitly: small Some must beat None.
        let small_some = model("small", Some(1.0));
        let corpus2 = Corpus {
            models: vec![no_cap.clone(), small_some.clone()],
            ..Default::default()
        };
        let rows2 = consult(&corpus2, &health, &opts());
        assert_eq!(rows2[0].model_id, "small");
        assert_eq!(rows2[1].model_id, "no-cap");
    }
}
