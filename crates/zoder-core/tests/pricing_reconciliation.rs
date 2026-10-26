//! Reconcile the shipped price list against a provider console export.
//!
//! The fixture is a provider console export (per-day, per-token-type usage
//! plus the per-day bill) with the account id and the API key columns
//! removed: only the date, the model, the token type, the token count and the
//! unit price the provider charged remain.
//!
//! It is the ground truth for the rate card. The test re-derives the
//! console's per-day bill from the console's token counts using ONLY the
//! shipped price list, so a rate card that has drifted from the provider
//! cannot pass: every console row's unit price must match a band in the
//! shipped price list exactly, and the day must add up to the console's bill.
//!
//! The console bills a busy-hours premium during two weekday windows and the
//! standard rate outside them, so a single day can carry both bands. The
//! fixture records the band implicitly, in the unit price.

use std::collections::BTreeMap;
use std::path::Path;

use zoder_core::pricing::PricingCatalog;

const CONSOLE_USAGE: &str = include_str!("data/console-usage-2026-10.csv");
const CONSOLE_COST: &str = include_str!("data/console-cost-2026-10.csv");

/// The model the export covers.
const MODEL: &str = "deepseek-flash";

/// The three rates of one band: cache read, input miss, output.
#[derive(Debug, Clone, Copy)]
struct Band {
    cache_read: f64,
    input: f64,
    output: f64,
}

impl Band {
    /// The band rate for one token type.
    fn rate(&self, token_type: &str) -> f64 {
        match token_type {
            "input_cache_hit_tokens" => self.cache_read,
            "input_cache_miss_tokens" => self.input,
            "output_tokens" => self.output,
            other => panic!("unknown token type {other:?}"),
        }
    }
}

/// The shipped price list: the file the runtime catalog is built from.
///
/// The file is copied into a temp directory with its mode pinned before it
/// is loaded, so the test exercises the catalog's content and not the
/// loader's group/world-writable rejection, which depends on the host
/// umask (and therefore on the checkout, not on this code).
fn shipped_catalog() -> PricingCatalog {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pricing/catalog.json");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("catalog.json");
    std::fs::copy(&source, &path).expect("the shipped price list is readable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let catalog = PricingCatalog::load(&path);
    assert!(
        catalog.lookup(MODEL).is_some(),
        "the shipped price list must know {MODEL}"
    );
    catalog
}

/// The console's per-day usage rows, grouped by day.
fn usage_by_day() -> BTreeMap<String, Vec<(String, u64, f64)>> {
    let mut days: BTreeMap<String, Vec<(String, u64, f64)>> = BTreeMap::new();
    let mut lines = CONSOLE_USAGE.lines();
    let header = lines.next().expect("the usage fixture has a header");
    assert!(header.starts_with("date,model,token_type"));
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        assert_eq!(cols.len(), 5, "unexpected fixture row {line:?}");
        assert_eq!(cols[1], MODEL, "the fixture covers one model");
        let tokens: u64 = cols[3].parse().expect("a token count");
        let unit_price: f64 = cols[4].parse().expect("a unit price");
        days.entry(cols[0].to_string()).or_default().push((
            cols[2].to_string(),
            tokens,
            unit_price,
        ));
    }
    days
}

/// The console's per-day bill.
fn console_cost_by_day() -> BTreeMap<String, f64> {
    let mut days: BTreeMap<String, f64> = BTreeMap::new();
    let mut lines = CONSOLE_COST.lines();
    let header = lines.next().expect("the cost fixture has a header");
    assert!(header.starts_with("date,model,cost_usd"));
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        assert_eq!(cols.len(), 3, "unexpected fixture row {line:?}");
        let cost: f64 = cols[2].parse().expect("a cost");
        days.insert(cols[0].to_string(), cost);
    }
    days
}

/// The standard and the busy-hours bands of the shipped price list.
fn bands() -> (Band, Band) {
    let catalog = shipped_catalog();
    let price = catalog.lookup(MODEL).expect("the model is priced");
    let standard = Band {
        cache_read: price.cache_read_usd_per_mtok,
        input: price.input_usd_per_mtok,
        output: price.output_usd_per_mtok,
    };
    let peak_block = price
        .peak
        .as_ref()
        .expect("the shipped price list must carry the busy-hours band");
    let peak = Band {
        cache_read: peak_block.cache_read_usd_per_mtok,
        input: peak_block.input_usd_per_mtok,
        output: peak_block.output_usd_per_mtok,
    };
    (standard, peak)
}

/// The shipped price list reproduces the console's per-day bill from the
/// console's token counts, on every day of the export.
#[test]
fn the_shipped_price_list_reproduces_the_console_bill() {
    let (standard, peak) = bands();
    let costs = console_cost_by_day();
    let days = usage_by_day();
    assert!(!days.is_empty(), "the fixture must carry days");

    let mut checked = 0usize;
    for (day, rows) in &days {
        let console_cost = costs
            .get(day)
            .unwrap_or_else(|| panic!("the console bill has no row for {day}"));
        let mut total = 0.0f64;
        for (token_type, tokens, unit_price) in rows {
            // The console's unit price is per token; the rate card is per
            // million tokens.
            let charged = unit_price * 1_000_000.0;
            let band = if charged == standard.rate(token_type) {
                &standard
            } else if charged == peak.rate(token_type) {
                &peak
            } else {
                panic!(
                    "{day}: the console charged {charged} per million tokens for {token_type}, \
                     which is neither the standard ({}) nor the busy-hours ({}) rate",
                    standard.rate(token_type),
                    peak.rate(token_type)
                )
            };
            // Bill from the shipped rate card, not from the console's price.
            total += (*tokens as f64) * band.rate(token_type) / 1_000_000.0;
        }
        assert!(
            (total - console_cost).abs() < 1e-9,
            "{day}: the shipped price list bills {total}, the console billed {console_cost}"
        );
        checked += 1;
    }
    assert_eq!(checked, costs.len(), "every console day must be checked");
}

/// The busy-hours band is exactly twice the standard band, on every
/// component: the cache read, the input miss and the output.
#[test]
fn the_busy_hours_band_is_exactly_twice_the_standard_band() {
    let (standard, peak) = bands();
    assert_eq!(peak.cache_read, standard.cache_read * 2.0);
    assert_eq!(peak.input, standard.input * 2.0);
    assert_eq!(peak.output, standard.output * 2.0);
}

/// The cache-read rate is carried by the shipped price list and is cheaper
/// than the miss rate: without it a cache-heavy call bills as if every token
/// had missed the cache.
#[test]
fn the_cache_read_rate_is_carried_and_cheaper_than_the_miss_rate() {
    let (standard, _) = bands();
    assert!(
        standard.cache_read > 0.0,
        "the cache-read rate must be known"
    );
    assert!(standard.cache_read < standard.input);
}

/// The console's peak rows land on the busy-hours band and the rest on the
/// standard one, so both bands are exercised by the fixture.
#[test]
fn the_fixture_exercises_both_bands() {
    let (standard, peak) = bands();
    let mut standard_rows = 0usize;
    let mut peak_rows = 0usize;
    for rows in usage_by_day().values() {
        for (token_type, _tokens, unit_price) in rows {
            let charged = unit_price * 1_000_000.0;
            if charged == standard.rate(token_type) {
                standard_rows += 1;
            } else if charged == peak.rate(token_type) {
                peak_rows += 1;
            }
        }
    }
    assert!(
        standard_rows > 0,
        "the fixture must carry standard-rate rows"
    );
    assert!(peak_rows > 0, "the fixture must carry busy-hours rows");
}
