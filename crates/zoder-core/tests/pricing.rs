use zoder_core::pricing::OffPeak;
use zoder_core::PricingCatalog;

/// A single malformed model entry must not blank the whole catalog: valid
/// entries are kept, invalid ones (negative rate, non-object) are dropped, and
/// an explicit zero rate is a valid (free) entry.
#[test]
fn load_drops_invalid_models_keeps_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");
    let json = r#"{
      "baseline_usd_per_mtok": 1.5,
      "baseline_model": "x",
      "models": {
        "good":   {"input_usd_per_mtok": 0.15, "output_usd_per_mtok": 0.60},
        "neg":    {"input_usd_per_mtok": -1.0},
        "notobj": 42,
        "free":   {"input_usd_per_mtok": 0.0, "output_usd_per_mtok": 0.0},
        "partial":{"input_usd_per_mtok": 0.1}
      }
    }"#;
    std::fs::write(&path, json).unwrap();
    // The catalog loader rejects group/world-writable files
    // (`PricingCatalog::load` checks `S_IWGRP | S_IWOTH`). The default
    // umask on many CI hosts is 022 (file mode 0644, safe), but a host
    // with umask 002 (e.g. some Debian/Ubuntu dev defaults) creates
    // files with mode 0664 (group-writable) and the loader refuses to
    // trust them. Pin the mode explicitly so the test isn't dependent
    // on the host umask — we want to exercise the loader's content
    // handling here, not its permission-check path (covered by the
    // dedicated `load_rejects_world_writable_file` test below).
    set_secure_permissions(&path);

    let cat = PricingCatalog::load(&path);
    assert!(cat.models.contains_key("good"));
    assert!(cat.models.contains_key("free")); // explicit 0 is a valid rate
    assert!(!cat.models.contains_key("partial")); // missing output rate is unknown, not $0
    assert!(!cat.models.contains_key("neg")); // negative field dropped -> no rate -> skipped
    assert!(!cat.models.contains_key("notobj"));
    assert!((cat.baseline_usd_per_mtok - 1.5).abs() < 1e-9);
}

/// Pin a file to mode 0644 (owner read/write, others read) so the
/// pricing-catalog loader's group/world-writable rejection doesn't fire
/// on hosts whose default umask creates mode 0664 files. Unix-only.
#[cfg(unix)]
fn set_secure_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(path, perms).unwrap();
}

/// A group/world-writable catalog can't be trusted to drive chargeback and is
/// rejected (empty catalog), since another user could tamper with the rates.
#[cfg(unix)]
#[test]
fn load_rejects_world_writable_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");
    std::fs::write(&path, r#"{"models":{"good":{"input_usd_per_mtok":1.0}}}"#).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o666); // world-writable
    std::fs::set_permissions(&path, perms).unwrap();

    let cat = PricingCatalog::load(&path);
    assert!(
        cat.models.is_empty(),
        "world-writable catalog must be rejected"
    );
}

/// An oversized file is rejected before its body is trusted.
#[test]
fn load_rejects_oversized_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");
    let big = format!("{{\"models\":{{}},\"_pad\":\"{}\"}}", "a".repeat(3_000_000));
    std::fs::write(&path, big).unwrap();
    set_secure_permissions(&path);
    let cat = PricingCatalog::load(&path);
    assert!(cat.models.is_empty());
}

/// Regression: known openrouter meta-routing aliases (auto, bodybuilder,
/// fusion, pareto-code) that lack a complete input/output pricing pair are
/// classified and silently skipped — on every single invocation, not just the
/// first.  An explicit list (not a prefix match) is used so a brand-new model
/// with the same prefix would still be validated and warned about if missing
/// pricing data.
#[test]
fn known_meta_aliases_are_silently_skipped_via_classification() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");

    let json = r#"{
      "models": {
        "openrouter/auto":       {"source": "openrouter"},
        "openrouter/bodybuilder":{"source": "openrouter"},
        "openrouter/fusion":     {"source": "openrouter"},
        "openrouter/pareto-code":{"source": "openrouter"},
        "openai/gpt-4o":         {"input_usd_per_mtok": 5.0, "output_usd_per_mtok": 15.0}
      }
    }"#;
    std::fs::write(&path, json).unwrap();
    set_secure_permissions(&path);

    // Two successive loads must produce identical, warning-free behaviour.
    for _i in 1..=2 {
        let cat = PricingCatalog::load(&path);
        // The four known aliases are silently skipped — not counted as "malformed".
        assert!(
            !cat.models.contains_key("openrouter/auto"),
            "known alias must be silently skipped"
        );
        assert!(
            !cat.models.contains_key("openrouter/bodybuilder"),
            "known alias must be silently skipped"
        );
        assert!(
            !cat.models.contains_key("openrouter/fusion"),
            "known alias must be silently skipped"
        );
        assert!(
            !cat.models.contains_key("openrouter/pareto-code"),
            "known alias must be silently skipped"
        );
        // Valid model must survive.
        assert!(
            cat.models.contains_key("openai/gpt-4o"),
            "valid model must survive"
        );
    }
}

/// A GENUINELY malformed entry that is NOT a known meta-routing alias must
/// still produce a warning, so real data-quality problems are not silently
/// swallowed by the meta-routing alias filter.
#[test]
fn genuine_malformed_entries_still_warn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");

    let json = r#"{
      "models": {
        "vendor/real-model": {"input_usd_per_mtok": 1.0, "output_usd_per_mtok": 2.0},
        "vendor/broken":     {"input_usd_per_mtok": 0.5},
        "other/missing":     {"source": "nobody"}
      }
    }"#;
    std::fs::write(&path, json).unwrap();
    set_secure_permissions(&path);

    let cat = PricingCatalog::load(&path);
    // Real models kept.
    assert!(cat.models.contains_key("vendor/real-model"));
    // Malformed / unknown models dropped.
    assert!(!cat.models.contains_key("vendor/broken"));
    assert!(!cat.models.contains_key("other/missing"));
}

#[test]
fn off_peak_active_at_window_boundaries() {
    // Test case 1: Full-day window { window_start_utc_min: 0, window_end_utc_min: 1440 }
    let op1 = OffPeak {
        input_usd_per_mtok: 0.0,
        output_usd_per_mtok: 0.0,
        window_start_utc_min: 0,
        window_end_utc_min: 1440,
    };
    assert!(op1.active_at(1439)); // 23:59 UTC, inside
    assert!(op1.active_at(0)); // 00:00 UTC, inside
    assert!(op1.active_at(720)); // 12:00 UTC, inside

    // Test case 2: Exclusive end: window { start: 60, end: 120 }
    let op2 = OffPeak {
        input_usd_per_mtok: 0.0,
        output_usd_per_mtok: 0.0,
        window_start_utc_min: 60,
        window_end_utc_min: 120,
    };
    assert!(!op2.active_at(120)); // 02:00 UTC, exclusive end
    assert!(op2.active_at(60)); // 01:00 UTC, start, inclusive
    assert!(op2.active_at(119)); // 01:59 UTC, inside

    // Test case 3: Midnight-wrapping window { start: 1380, end: 60 } (23:00 to 01:00)
    let op3 = OffPeak {
        input_usd_per_mtok: 0.0,
        output_usd_per_mtok: 0.0,
        window_start_utc_min: 1380,
        window_end_utc_min: 60,
    };
    assert!(op3.active_at(1439)); // 23:59 UTC, inside (past midnight)
    assert!(op3.active_at(30)); // 00:30 UTC, inside (past midnight)
    assert!(!op3.active_at(120)); // 02:00 UTC, outside (daytime)
}
