use chrono::{DateTime, NaiveDateTime, Utc};
use zoder_core::{Entry, FinOpsTags, Ledger, Period};

fn ts(s: &str) -> DateTime<Utc> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .unwrap()
        .and_utc()
}

fn entry(t: &str, model: &str, cost: f64, tin: u64, tout: u64) -> Entry {
    Entry {
        ts_utc: ts(t),
        provider: "default".into(),
        model: model.into(),
        host: model
            .split_once('/')
            .map(|(h, _)| h.to_string())
            .unwrap_or_default(),
        tokens_in: tin,
        tokens_out: tout,
        cost_usd: cost,
        cost_unknown: false,
        calls: 1,
        violation: None,
        tags: FinOpsTags::default(),
    }
}

#[test]
fn legacy_entry_without_unknown_flag_defaults_to_known_cost() {
    let raw = r#"{"ts_utc":"2026-01-01T00:00:00Z","provider":"p","model":"m","tokens_in":1,"tokens_out":2,"cost_usd":0.0}"#;
    let parsed: Entry = serde_json::from_str(raw).unwrap();
    assert!(!parsed.cost_unknown);
}

#[test]
fn rollup_and_by_model_and_date_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    let led = Ledger::new(&path);

    led.record(&entry("2026-01-10 10:00:00", "a", 0.0, 100, 50))
        .unwrap();
    led.record(&entry("2026-01-10 11:00:00", "b", 0.5, 200, 80))
        .unwrap();
    led.record(&entry("2026-02-01 09:00:00", "a", 1.0, 10, 5))
        .unwrap();

    // Monthly rollup: two buckets.
    let monthly = led.rollup(Period::Month).unwrap();
    assert_eq!(monthly.len(), 2);
    assert_eq!(monthly.get("2026-01").unwrap().calls, 2);
    assert!((monthly.get("2026-01").unwrap().cost_usd - 0.5).abs() < 1e-9);
    assert!((monthly.get("2026-02").unwrap().cost_usd - 1.0).abs() < 1e-9);

    // by_model groups across time.
    let bm = led.by_model(None, None).unwrap();
    assert_eq!(bm.get("a").unwrap().calls, 2);
    assert_eq!(bm.get("b").unwrap().calls, 1);

    // Date window excludes February.
    let since = ts("2026-01-01 00:00:00");
    let until = ts("2026-01-31 23:59:59");
    let jan = led
        .rollup_in(Period::Month, Some(since), Some(until))
        .unwrap();
    assert_eq!(jan.len(), 1);
    assert_eq!(jan.get("2026-01").unwrap().calls, 2);
}

#[test]
fn tolerant_read_skips_garbage_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    let led = Ledger::new(&path);
    led.record(&entry("2026-01-10 10:00:00", "a", 0.0, 1, 1))
        .unwrap();
    // Append a mangled line.
    std::fs::write(
        &path,
        format!(
            "{}{}",
            std::fs::read_to_string(&path).unwrap(),
            "{not json\n"
        ),
    )
    .unwrap();
    let entries = led.entries().unwrap();
    assert_eq!(entries.len(), 1);
}
