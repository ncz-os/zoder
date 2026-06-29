//! Enterprise cost snapshot — org/personal billed YTD + full-year projection,
//! cached locally so `zoder report` stays instant and offline.
//!
//! Local-first by design: the per-call rollups come from the spend ledger
//! (`ledger.rs`); this adds the *authoritative billed YTD* + a full-year
//! projection on top, **when a snapshot cache exists**. The snapshot is a
//! vendor-neutral JSON file (`$ZODER_HOME/cost_snapshot.json`) produced by a
//! config-driven enterprise cost source — e.g. an org's billing/cost-portal
//! export. zoder only *consumes* it here; how it is populated (which endpoint,
//! which auth) is a deployment/config concern, never hard-coded in the tool.

use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One calendar month's billed cost.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MonthCost {
    pub month: String,
    pub cost_usd: f64,
}

/// Billed cost for one scope (e.g. the whole org, or just the signed-in user).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopeStat {
    #[serde(default)]
    pub ytd_cost_usd: f64,
    #[serde(default)]
    pub ytd_tokens: u64,
    /// One entry per calendar month, oldest first. The last entry may be the
    /// (partial) current month.
    #[serde(default)]
    pub monthly: Vec<MonthCost>,
}

impl ScopeStat {
    /// Day-scaled full-year estimate: `ytd / fraction_of_year_elapsed`.
    pub fn project_linear(&self, frac_year: f64) -> f64 {
        if frac_year <= 0.0 {
            0.0
        } else {
            self.ytd_cost_usd / frac_year
        }
    }

    /// Run-rate full-year estimate from the last FULL calendar month × 12
    /// (captures acceleration the linear scale misses). Falls back to the last
    /// available month when only one exists. Returns `(projection, month)`.
    pub fn project_runrate(&self) -> Option<(f64, String)> {
        if self.monthly.len() >= 2 {
            let m = &self.monthly[self.monthly.len() - 2];
            Some((m.cost_usd * 12.0, m.month.clone()))
        } else {
            self.monthly
                .last()
                .map(|m| (m.cost_usd * 12.0, m.month.clone()))
        }
    }
}

/// Cached enterprise cost snapshot. Vendor-neutral on-disk shape; the source
/// that writes it is config-driven (see module docs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSnapshot {
    #[serde(default)]
    pub generated: String,
    #[serde(default)]
    pub year: i32,
    /// The signed-in user's scope, when the source exposes it.
    #[serde(default)]
    pub personal: Option<ScopeStat>,
    /// The org-wide scope, when the credential can see it.
    #[serde(default)]
    pub org: Option<ScopeStat>,
}

impl CostSnapshot {
    /// Load the snapshot from disk; `None` when absent or unparseable.
    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Persist the snapshot as pretty JSON.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Fraction of the current UTC year elapsed (leap-year aware).
    pub fn frac_year_elapsed() -> f64 {
        let now = Utc::now();
        let y = now.year();
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let days = if leap { 366.0 } else { 365.0 };
        now.ordinal() as f64 / days
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_projection_scales_ytd_by_year_fraction() {
        let s = ScopeStat {
            ytd_cost_usd: 300.0,
            ytd_tokens: 0,
            monthly: vec![],
        };
        // Half the year elapsed -> ~2x the YTD.
        assert!((s.project_linear(0.5) - 600.0).abs() < 1e-9);
        // Guard against divide-by-zero at year start.
        assert_eq!(s.project_linear(0.0), 0.0);
    }

    #[test]
    fn runrate_uses_last_full_month_times_twelve() {
        let s = ScopeStat {
            ytd_cost_usd: 0.0,
            ytd_tokens: 0,
            monthly: vec![
                MonthCost {
                    month: "2026-04".into(),
                    cost_usd: 100.0,
                },
                MonthCost {
                    month: "2026-05".into(),
                    cost_usd: 0.0, // partial current month — must be ignored
                },
            ],
        };
        let (proj, month) = s.project_runrate().expect("has months");
        assert_eq!(month, "2026-04");
        assert!((proj - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn snapshot_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cost_snapshot.json");
        let snap = CostSnapshot {
            generated: "2026-06-28".into(),
            year: 2026,
            org: Some(ScopeStat {
                ytd_cost_usd: 1234.5,
                ytd_tokens: 9,
                monthly: vec![MonthCost {
                    month: "2026-06".into(),
                    cost_usd: 200.0,
                }],
            }),
            personal: None,
        };
        snap.save(&p).unwrap();
        let back = CostSnapshot::load(&p).expect("loads");
        assert_eq!(back.year, 2026);
        assert!((back.org.unwrap().ytd_cost_usd - 1234.5).abs() < 1e-9);
        assert!(CostSnapshot::load(&dir.path().join("missing.json")).is_none());
    }
}
