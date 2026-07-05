use chrono::{DateTime, NaiveDateTime, Utc};
use zoder_core::{
    build_report, build_report_from_entries, Entry, FinOpsTags, Gran, Ledger, PricingCatalog,
};

fn ts(s: &str) -> DateTime<Utc> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .unwrap()
        .and_utc()
}

fn entry(t: &str, provider: &str, model: &str, cost: f64, tin: u64, tout: u64) -> Entry {
    Entry {
        ts_utc: ts(t),
        provider: provider.into(),
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
fn unknown_cost_is_neither_free_nor_spend() {
    let mut unknown = entry(
        "2026-06-10 10:00:00",
        "unpriced-provider",
        "unpriced/model",
        0.0,
        400,
        600,
    );
    unknown.cost_unknown = true;
    let pricing = PricingCatalog::default();
    let report = build_report_from_entries(
        &[unknown],
        &pricing,
        ts("2026-06-01 00:00:00"),
        ts("2026-07-01 00:00:00"),
        Gran::Day,
        "June",
    )
    .unwrap();
    assert_eq!(report.total_tokens, 1_000);
    assert_eq!(report.total_calls, 1);
    assert_eq!(report.total_cost_usd, 0.0);
    assert_eq!(report.free_tokens, 0);
    assert_eq!(report.billed_tokens, 0);
    assert_eq!(report.unknown_cost_tokens, 1_000);
    assert!(report.by_model[0].cost_unknown);
}

/// With no pricing catalog, the avoided-spend / counterfactual baseline is
/// derived from the highest effective $/Mtok among the paid models actually used.
#[test]
fn derived_baseline_from_observed_paid_spend_when_catalog_absent() {
    let dir = tempfile::tempdir().unwrap();
    let led = Ledger::new(&dir.path().join("ledger.jsonl"));
    // paid: $0.60 over 1,000,000 tokens => effective 0.60 $/Mtok
    led.record(&entry(
        "2026-06-10 10:00:00",
        "enterprise-gw",
        "paid",
        0.60,
        600_000,
        400_000,
    ))
    .unwrap();
    // free: 500,000 tokens at $0
    led.record(&entry(
        "2026-06-10 11:00:00",
        "enterprise-gw",
        "free",
        0.0,
        300_000,
        200_000,
    ))
    .unwrap();

    let pricing = PricingCatalog::default(); // baseline 0 => derive
    let since = ts("2026-06-01 00:00:00");
    let until = ts("2026-06-30 23:59:59");
    let rep = build_report(&led, &pricing, since, until, Gran::Day, "june").unwrap();

    assert_eq!(rep.baseline_model, "paid");
    assert!(
        (rep.baseline_usd_per_mtok - 0.60).abs() < 1e-6,
        "rate {}",
        rep.baseline_usd_per_mtok
    );
    // free 500k @ 0.60/Mtok = 0.30
    assert!(
        (rep.avoided_usd - 0.30).abs() < 1e-6,
        "avoided {}",
        rep.avoided_usd
    );
    // all 1.5M @ 0.60/Mtok = 0.90
    assert!(
        (rep.counterfactual_usd - 0.90).abs() < 1e-6,
        "cf {}",
        rep.counterfactual_usd
    );
    assert_eq!(rep.free_tokens, 500_000);
    assert_eq!(rep.billed_tokens, 1_000_000);
}

/// An explicit catalog baseline takes precedence over the derived one.
#[test]
fn catalog_baseline_overrides_derivation() {
    let dir = tempfile::tempdir().unwrap();
    let led = Ledger::new(&dir.path().join("ledger.jsonl"));
    led.record(&entry(
        "2026-06-10 10:00:00",
        "enterprise-gw",
        "paid",
        0.60,
        600_000,
        400_000,
    ))
    .unwrap();

    let pricing = PricingCatalog {
        baseline_usd_per_mtok: 2.0,
        baseline_model: "frontier".into(),
        ..PricingCatalog::default()
    };

    let since = ts("2026-06-01 00:00:00");
    let until = ts("2026-06-30 23:59:59");
    let rep = build_report(&led, &pricing, since, until, Gran::Day, "june").unwrap();

    assert_eq!(rep.baseline_model, "frontier");
    assert!((rep.baseline_usd_per_mtok - 2.0).abs() < 1e-9);
}

/// `--vendor <name>` filters the ledger to entries whose `provider` is in the
/// vendor's provider set, then `build_report_from_entries` recomputes totals,
/// buckets, and the counterfactual over the filtered slice. The headline
/// numbers must reflect the vendor alone — not the whole ledger.
#[test]
fn vendor_filter_recomputes_totals() {
    let dir = tempfile::tempdir().unwrap();
    let led = Ledger::new(&dir.path().join("ledger.jsonl"));

    // enterprise-gw traffic: 1 paid call ($0.60 over 1M tok), 1 free call (500k tok).
    led.record(&entry(
        "2026-06-10 10:00:00",
        "enterprise-gw",
        "llama-3.1-nemotron",
        0.60,
        600_000,
        400_000,
    ))
    .unwrap();
    led.record(&entry(
        "2026-06-10 11:00:00",
        "enterprise-gw",
        "llama-3.1-nemotron",
        0.0,
        300_000,
        200_000,
    ))
    .unwrap();

    // OpenAI traffic that MUST be excluded by the filter.
    led.record(&entry(
        "2026-06-10 12:00:00",
        "openai",
        "gpt-4o",
        5.00,
        600_000,
        400_000,
    ))
    .unwrap();

    let pricing = PricingCatalog::default(); // baseline derived from observed paid spend
    let since = ts("2026-06-01 00:00:00");
    let until = ts("2026-06-30 23:59:59");

    // Whole ledger: includes the $5 GPT-4o call.
    let full = build_report(&led, &pricing, since, until, Gran::Day, "june").unwrap();
    assert_eq!(full.total_calls, 3);
    assert!((full.total_cost_usd - 5.60).abs() < 1e-9);

    // Filtered to enterprise only: gpt-4o excluded.
    let entries: Vec<Entry> = led
        .entries_in(Some(since), Some(until))
        .unwrap()
        .into_iter()
        .filter(|e| e.provider == "enterprise-gw")
        .collect();
    let enterprise =
        build_report_from_entries(&entries, &pricing, since, until, Gran::Day, "june").unwrap();
    assert_eq!(enterprise.total_calls, 2);
    assert!(
        (enterprise.total_cost_usd - 0.60).abs() < 1e-9,
        "enterprise total"
    );
    assert_eq!(enterprise.billed_tokens, 1_000_000);
    assert_eq!(enterprise.free_tokens, 500_000);
    // Baseline is derived from the only paid model name (`llama-3.1-nemotron`),
    // whose total observed rate is $0.60 over 1.5M tok = $0.40/Mtok. The
    // avoided-spend and counterfactual headlines are over the *filtered* slice
    // (not the full ledger that contains gpt-4o).
    assert!(
        (enterprise.baseline_usd_per_mtok - 0.40).abs() < 1e-6,
        "enterprise baseline {}",
        enterprise.baseline_usd_per_mtok
    );
    assert!(
        (enterprise.avoided_usd - 0.20).abs() < 1e-6,
        "enterprise avoided {}",
        enterprise.avoided_usd
    );
    assert!(
        (enterprise.counterfactual_usd - 0.60).abs() < 1e-6,
        "enterprise counterfactual {}",
        enterprise.counterfactual_usd
    );
}

/// The by-host rollup sums a publisher's traffic across *every* provider that
/// served it — the complement of `--vendor`. `meta/...` served by both
/// OpenRouter (paid) and enterprise (free) is one `meta` host row with both calls;
/// a `--host meta` scope recomputes totals over just that publisher's slice.
#[test]
fn host_rollup_sums_across_providers() {
    let dir = tempfile::tempdir().unwrap();
    let led = Ledger::new(&dir.path().join("ledger.jsonl"));
    led.record(&entry(
        "2026-06-10 10:00:00",
        "openrouter",
        "meta/llama-3.3-70b-instruct",
        0.012,
        1_000,
        500,
    ))
    .unwrap();
    led.record(&entry(
        "2026-06-11 10:00:00",
        "enterprise-gw",
        "meta/llama-3.3-70b-instruct",
        0.0,
        2_000,
        800,
    ))
    .unwrap();
    led.record(&entry(
        "2026-06-12 10:00:00",
        "openrouter",
        "anthropic/claude-3.5-sonnet",
        0.045,
        4_000,
        1_200,
    ))
    .unwrap();

    let pricing = PricingCatalog::default();
    let since = ts("2026-06-01 00:00:00");
    let until = ts("2026-06-30 23:59:59");
    let rep = build_report(&led, &pricing, since, until, Gran::Day, "june").unwrap();

    let meta = rep
        .by_host
        .iter()
        .find(|h| h.host == "meta")
        .expect("meta host row present");
    assert_eq!(meta.calls, 2, "both providers' meta traffic summed");
    assert!((meta.cost_usd - 0.012).abs() < 1e-9);
    assert!(meta.billed, "one of the meta calls was paid");
    assert!(rep.by_host.iter().any(|h| h.host == "anthropic"));

    // `--host meta` scope: recompute totals over just the meta publisher.
    let entries: Vec<Entry> = led
        .entries_in(Some(since), Some(until))
        .unwrap()
        .into_iter()
        .filter(|e| e.effective_host() == "meta")
        .collect();
    let meta_rep =
        build_report_from_entries(&entries, &pricing, since, until, Gran::Day, "june").unwrap();
    assert_eq!(meta_rep.total_calls, 2);
    assert!((meta_rep.total_cost_usd - 0.012).abs() < 1e-9);
    assert_eq!(meta_rep.by_host.len(), 1);
    assert_eq!(meta_rep.by_host[0].host, "meta");
}

/// Un-prefixed model ids must not create a phantom "" host bucket; they still
/// count in totals and by_model.
#[test]
fn unprefixed_models_have_no_host_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let led = Ledger::new(&dir.path().join("ledger.jsonl"));
    led.record(&entry(
        "2026-06-10 10:00:00",
        "openai",
        "gpt-4o",
        5.0,
        600_000,
        400_000,
    ))
    .unwrap();
    let pricing = PricingCatalog::default();
    let since = ts("2026-06-01 00:00:00");
    let until = ts("2026-06-30 23:59:59");
    let rep = build_report(&led, &pricing, since, until, Gran::Day, "june").unwrap();
    assert_eq!(rep.total_calls, 1);
    assert!(
        rep.by_host.is_empty(),
        "no '/' in model id => no host bucket"
    );
}
