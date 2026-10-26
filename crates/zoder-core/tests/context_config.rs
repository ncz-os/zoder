//! `[context]` config block: the opt-in surface for the context levers.
//!
//! The 2026-10-25 DeepSeek export billed 10.6B input tokens against 50.1M
//! output, and one `zoder loop` spent 32.9M input for 110K output by resending
//! a full window on every turn. The levers that bound that resend are pure
//! functions in `zoder_core::context_opt`; this file pins the *config* half:
//! absent block = nothing changes, present block = the named profile's numbers
//! with any operator override applied, and a typo never silently becomes some
//! other provider's tuning.
//!
//! The defaults are **profile-gated**: `standalone` (one operator's own
//! machine) and `fleet` (a shared, metered deployment) get different numbers
//! and, unless the operator says otherwise, a different on/off state.

use zoder_core::config::Config;
use zoder_core::context_opt::{ContextProfile, Levers};

fn config_from(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("config parses")
}

fn minimal() -> &'static str {
    r#"
default_provider = "p"
corpus_path = "/nonexistent/corpus.json"
ledger_path = "/nonexistent/ledger.jsonl"
health_path = "/nonexistent/health.json"
[[providers]]
id = "p"
base_url = "https://example.invalid/v1"
kind = "openai-chat"
auth = { type = "none" }
"#
}

#[test]
fn an_absent_context_block_changes_nothing() {
    let cfg = config_from(minimal());
    assert_eq!(cfg.context.enabled, None);
    assert!(cfg.context.profiles.is_empty());
    // Off is off: no levers are returned for any model, so a caller cannot
    // half-apply an unconfigured block.
    assert_eq!(cfg.context.levers_for("deepseek-flash"), None);
    assert_eq!(cfg.context.levers_for("MiniMax-M3"), None);
}

#[test]
fn the_standalone_profile_is_the_default_and_leaves_the_levers_off() {
    let cfg = config_from(minimal());
    assert_eq!(
        cfg.context.deployment_profile(),
        ContextProfile::Standalone,
        "an operator who says nothing gets the standalone defaults"
    );
    assert!(!cfg.context.levers_enabled());
    assert_eq!(cfg.context.levers_for("deepseek-flash"), None);
}

#[test]
fn the_fleet_profile_turns_the_levers_on_with_the_fleet_numbers() {
    let cfg = config_from(&format!("{}\n[context]\nprofile = \"fleet\"\n", minimal()));
    assert_eq!(cfg.context.deployment_profile(), ContextProfile::Fleet);
    assert!(cfg.context.levers_enabled());
    assert_eq!(
        cfg.context.levers_for("deepseek-flash"),
        Some(Levers::FLEET_DEEPSEEK)
    );
    assert_eq!(
        cfg.context.levers_for("MiniMax-M3"),
        Some(Levers::FLEET_MINIMAX)
    );
    assert_eq!(cfg.context.levers_for("qwen38"), Some(Levers::FLEET_OTHER));
}

#[test]
fn the_fleet_numbers_are_tighter_than_the_standalone_numbers() {
    // The whole point of the gate: a shared, metered deployment bounds the
    // working set harder than a single operator's machine does. Both sides
    // are resolved through the resolver, so this is the answer an operator
    // gets rather than a restatement of the constants.
    let fleet = Levers::for_model(ContextProfile::Fleet, "deepseek-flash");
    let standalone = Levers::for_model(ContextProfile::Standalone, "deepseek-flash");
    assert!(fleet.tool_output_cap_bytes < standalone.tool_output_cap_bytes);
    assert!(fleet.compact_after_tokens < standalone.compact_after_tokens);
    assert!(fleet.elide_after_turns < standalone.elide_after_turns);
    assert!(fleet.keep_recent_turns < standalone.keep_recent_turns);
    // Neither profile ever compacts a provider with no per-token bill.
    let fleet_other = Levers::for_model(ContextProfile::Fleet, "qwen38");
    let standalone_other = Levers::for_model(ContextProfile::Standalone, "qwen38");
    assert_eq!(fleet_other.compact_after_tokens, 0);
    assert_eq!(standalone_other.compact_after_tokens, 0);
}

#[test]
fn an_explicit_enabled_overrides_the_profile_in_both_directions() {
    let on = config_from(&format!(
        "{}\n[context]\nprofile = \"standalone\"\nenabled = true\n",
        minimal()
    ));
    assert!(on.context.levers_enabled());
    assert_eq!(
        on.context.levers_for("deepseek-flash"),
        Some(Levers::STANDALONE_DEEPSEEK),
        "the standalone NUMBERS apply, not the fleet's"
    );

    let off = config_from(&format!(
        "{}\n[context]\nprofile = \"fleet\"\nenabled = false\n",
        minimal()
    ));
    assert!(!off.context.levers_enabled());
    assert_eq!(off.context.levers_for("deepseek-flash"), None);
}

#[test]
fn an_enabled_standalone_block_sniffs_the_provider_family() {
    let cfg = config_from(&format!("{}\n[context]\nenabled = true\n", minimal()));
    assert_eq!(
        cfg.context.levers_for("deepseek-flash"),
        Some(Levers::STANDALONE_DEEPSEEK)
    );
    assert_eq!(
        cfg.context.levers_for("MiniMax-M3"),
        Some(Levers::STANDALONE_MINIMAX)
    );
    assert_eq!(
        cfg.context.levers_for("qwen38"),
        Some(Levers::STANDALONE_OTHER)
    );
}

#[test]
fn a_named_profile_matches_through_a_provider_namespace() {
    let cfg = config_from(&format!("{}\n[context]\nprofile = \"fleet\"\n", minimal()));
    assert_eq!(
        cfg.context.levers_for("azure_ai/FW-DeepSeek-V4-Flash"),
        Some(Levers::FLEET_DEEPSEEK)
    );
    assert_eq!(
        cfg.context.levers_for("minimax/MiniMax-M3.1-Flash"),
        Some(Levers::FLEET_MINIMAX)
    );
}

#[test]
fn an_explicit_profile_overrides_only_the_fields_it_names() {
    let cfg = config_from(&format!(
        "{}\n[context]\nprofile = \"fleet\"\n[context.profiles.deepseek]\ncompact_after_tokens = 60000\n",
        minimal()
    ));
    let levers = cfg.context.levers_for("deepseek-flash").unwrap();
    assert_eq!(levers.compact_after_tokens, 60_000);
    // Everything not named keeps the DeepSeek profile's value. The base is
    // fetched through the resolver rather than the constant, so this compares
    // the CONFIG's resolution against the resolver's own answer.
    let base = Levers::named(ContextProfile::Fleet, "deepseek").unwrap();
    assert_eq!(levers.tool_output_cap_bytes, base.tool_output_cap_bytes);
    assert_eq!(levers.elide_after_turns, base.elide_after_turns);
    assert_eq!(levers.keep_recent_turns, base.keep_recent_turns);
    assert_eq!(levers.max_context_tokens, base.max_context_tokens);
}

#[test]
fn the_window_can_be_set_per_profile() {
    let cfg = config_from(&format!(
        "{}\n[context]\nprofile = \"fleet\"\n[context.profiles.deepseek]\nmax_context_tokens = 64000\n",
        minimal()
    ));
    assert_eq!(
        cfg.context
            .levers_for("deepseek-flash")
            .unwrap()
            .max_context_tokens,
        64_000
    );
}

#[test]
fn compaction_can_be_disabled_per_provider_with_zero() {
    let cfg = config_from(&format!(
        "{}\n[context]\nprofile = \"fleet\"\n[context.profiles.minimax]\ncompact_after_tokens = 0\n",
        minimal()
    ));
    assert_eq!(
        cfg.context
            .levers_for("MiniMax-M3")
            .unwrap()
            .compact_after_tokens,
        0
    );
}

#[test]
fn an_unknown_profile_name_falls_back_to_the_other_profile_not_a_guess() {
    let cfg = config_from(&format!(
        "{}\n[context]\nprofile = \"fleet\"\ndefault_profile = \"claude\"\n",
        minimal()
    ));
    // No `claude` built-in and no `profiles` entry: the catch-all applies.
    assert_eq!(cfg.context.levers_for("qwen38"), Some(Levers::FLEET_OTHER));
}

#[test]
fn the_other_profile_is_the_catch_all_when_a_model_matches_nothing() {
    let cfg = config_from(&format!(
        "{}\n[context]\nprofile = \"fleet\"\n[context.profiles.other]\nelide_after_turns = 3\n",
        minimal()
    ));
    let levers = cfg.context.levers_for("qwen38").unwrap();
    assert_eq!(levers.elide_after_turns, 3);
    assert_eq!(
        levers.compact_after_tokens, 0,
        "OTHER never compacts, whatever the deployment profile"
    );
}

#[test]
fn a_longer_profile_key_beats_a_shorter_one() {
    let cfg = config_from(&format!(
        "{}\n[context]\nprofile = \"fleet\"\n\
         [context.profiles.deepseek]\ncompact_after_tokens = 48000\n\
         [context.profiles.deepseek-v4]\ncompact_after_tokens = 24000\n",
        minimal()
    ));
    assert_eq!(
        cfg.context
            .levers_for("deepseek-v4-flash")
            .unwrap()
            .compact_after_tokens,
        24_000
    );
    assert_eq!(
        cfg.context
            .levers_for("deepseek-flash")
            .unwrap()
            .compact_after_tokens,
        48_000
    );
}

#[test]
fn an_unknown_key_inside_a_profile_is_rejected() {
    let err = toml::from_str::<Config>(&format!(
        "{}\n[context]\nenabled = true\n[context.profiles.deepseek]\ncompact_after_token = 1\n",
        minimal()
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("compact_after_token"), "{err}");
}

#[test]
fn an_unknown_deployment_profile_name_is_rejected() {
    let err = toml::from_str::<Config>(&format!(
        "{}\n[context]\nprofile = \"cluster\"\n",
        minimal()
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("cluster"), "{err}");
}

#[test]
fn measurement_is_on_whenever_the_block_is_configured() {
    let absent = config_from(minimal());
    assert!(!absent.context.measure("deepseek-flash"));
    let enabled = config_from(&format!("{}\n[context]\nenabled = true\n", minimal()));
    assert!(enabled.context.measure("deepseek-flash"));
    // Even a block that only names a deployment profile turns measurement on:
    // proving a lever helped needs the numbers before the lever is switched on.
    let fleet = config_from(&format!("{}\n[context]\nprofile = \"fleet\"\n", minimal()));
    assert!(fleet.context.measure("deepseek-flash"));
}

#[test]
fn a_config_without_context_round_trips_byte_for_byte() {
    let cfg = config_from(minimal());
    let text = toml::to_string(&cfg).unwrap();
    assert!(!text.contains("context"), "{text}");
}

/// The window is a per-provider and per-model knob, not a global one: a
/// provider can serve models with different windows, and a model entry is
/// more specific than its provider.
#[test]
fn the_window_resolves_per_model_then_per_provider() {
    let cfg = config_from(
        r#"
default_provider = "p"
corpus_path = "/nonexistent/corpus.json"
ledger_path = "/nonexistent/ledger.jsonl"
health_path = "/nonexistent/health.json"
[[providers]]
id = "p"
base_url = "https://example.invalid/v1"
kind = "openai-chat"
auth = { type = "none" }
max_context_tokens = 131072

[models.alias]
model = "some-model"
max_context_tokens = 64000
"#,
    );
    assert_eq!(cfg.max_context_tokens_for("p", "some-model"), Some(64_000));
    assert_eq!(
        cfg.max_context_tokens_for("p", "other-model"),
        Some(131_072)
    );
    // A provider that states no window states none: the caller must not trim
    // a prompt to fit a window nobody published.
    let bare = config_from(minimal());
    assert_eq!(bare.max_context_tokens_for("p", "any"), None);
}

/// The per-model window is scoped to the provider that serves it: one model
/// id can be served by two providers, and the two can give it different
/// windows. An entry that names no provider matches any, which is the
/// historical behaviour.
#[test]
fn a_per_model_window_is_scoped_to_its_provider() {
    let cfg = config_from(
        r#"
default_provider = "p"
corpus_path = "/nonexistent/corpus.json"
ledger_path = "/nonexistent/ledger.jsonl"
health_path = "/nonexistent/health.json"
[[providers]]
id = "p"
base_url = "https://api.example.invalid/v1"
kind = "openai-chat"
auth = { type = "none" }
max_context_tokens = 131072

[[providers]]
id = "q"
base_url = "https://api2.example.invalid/v1"
kind = "openai-chat"
auth = { type = "none" }
max_context_tokens = 32768

[models.owned]
model = "some-model"
provider = "q"
max_context_tokens = 16384

[models.unowned]
model = "other-model"
max_context_tokens = 8192
"#,
    );
    // The entry names its provider: it answers for that provider only.
    assert_eq!(cfg.max_context_tokens_for("q", "some-model"), Some(16_384));
    assert_eq!(cfg.max_context_tokens_for("p", "some-model"), Some(131_072));
    // The entry names no provider: it answers for any.
    assert_eq!(cfg.max_context_tokens_for("p", "other-model"), Some(8_192));
    assert_eq!(cfg.max_context_tokens_for("q", "other-model"), Some(8_192));
}
