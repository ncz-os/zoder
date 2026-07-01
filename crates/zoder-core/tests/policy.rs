use zoder_core::{CallTelemetry, Config, Decision, ModelEntry, PolicyGate};

fn cfg() -> Config {
    Config::default_provider(std::path::Path::new("/tmp/zoder-test"))
}

fn free_model() -> ModelEntry {
    ModelEntry {
        id: "vendor/x".into(),
        free: true,
        route_candidate: true,
        kind: "chat".into(),
        ..Default::default()
    }
}

fn paid_model() -> ModelEntry {
    ModelEntry {
        id: "openai/gpt".into(),
        paid: true,
        ..Default::default()
    }
}

#[test]
fn paid_needs_confirmation_free_allows() {
    let g = PolicyGate::new(&cfg(), false, true);
    assert_eq!(g.check(&free_model(), false, false), Decision::Allow);
    assert!(matches!(
        g.check(&paid_model(), false, false),
        Decision::NeedConfirm(_)
    ));
    // A free model routed to a PAID provider also needs confirmation.
    assert!(matches!(
        g.check(&free_model(), true, false),
        Decision::NeedConfirm(_)
    ));

    // --allow-paid turns paid into Allow.
    let g2 = PolicyGate::new(&cfg(), true, true);
    assert_eq!(g2.check(&paid_model(), false, false), Decision::Allow);
}

#[test]
fn verify_free_strict_fails_closed_without_telemetry() {
    let g = PolicyGate::new(&cfg(), false, true);
    let t = CallTelemetry::default();
    assert!(g.verify_free(&free_model(), &t).is_err());

    // Lenient (strict_free=false) allows the no-telemetry case.
    let lenient = PolicyGate::new(&cfg(), false, false);
    assert!(lenient.verify_free(&free_model(), &t).is_ok());
}

#[test]
fn verify_free_rejects_cost_fallback_and_foreign_host() {
    let g = PolicyGate::new(&cfg(), false, true);

    let billed = CallTelemetry {
        cost_usd: Some(0.01),
        ..Default::default()
    };
    assert!(g.verify_free(&free_model(), &billed).is_err());

    let fellback = CallTelemetry {
        attempted_fallbacks: Some(1),
        api_base: Some("https://example.com".into()),
        ..Default::default()
    };
    assert!(g.verify_free(&free_model(), &fellback).is_err());

    let foreign = CallTelemetry {
        cost_usd: Some(0.0),
        api_base: Some("https://api.openai.com".into()),
        ..Default::default()
    };
    assert!(g.verify_free(&free_model(), &foreign).is_err());

    // A genuinely free call on an internal host passes.
    let ok = CallTelemetry {
        cost_usd: Some(0.0),
        api_base: Some("https://prod.free.example.com".into()),
        attempted_fallbacks: Some(0),
        ..Default::default()
    };
    assert!(g.verify_free(&free_model(), &ok).is_ok());
}

#[test]
fn host_spoofing_substring_is_rejected() {
    let g = PolicyGate::new(&cfg(), false, true);
    // Attacker host merely contains "example.com" as a substring.
    let spoof = CallTelemetry {
        cost_usd: Some(0.0),
        api_base: Some("https://example.com.evil.io".into()),
        ..Default::default()
    };
    assert!(g.verify_free(&free_model(), &spoof).is_err());
}
