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

    let cat = PricingCatalog::load(&path);
    assert!(cat.models.contains_key("good"));
    assert!(cat.models.contains_key("free")); // explicit 0 is a valid rate
    assert!(!cat.models.contains_key("partial")); // missing output rate is unknown, not $0
    assert!(!cat.models.contains_key("neg")); // negative field dropped -> no rate -> skipped
    assert!(!cat.models.contains_key("notobj"));
    assert!((cat.baseline_usd_per_mtok - 1.5).abs() < 1e-9);
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
    let cat = PricingCatalog::load(&path);
    assert!(cat.models.is_empty());
}
