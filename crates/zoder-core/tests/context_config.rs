//! `[context]` config block: the opt-in surface for the context levers.
//!
//! The 2026-10-25 DeepSeek export billed 10.6B input tokens against 50.1M
//! output, and one `zoder loop` spent 32.9M input for 110K output by resending
//! a full window on every turn. The levers that bound that resend are pure
//! functions in `zoder_core::context_opt`; this file pins the *config* half:
//! absent block = nothing changes, present block = the named profile's numbers
//! with any operator override applied, and a typo never silently becomes some
//! other provider's tuning.

use zoder_core::config::Config;
use zoder_core::context_opt::Levers;

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
    assert!(!cfg.context.enabled);
    assert!(cfg.context.profiles.is_empty());
    // Off is off: no levers are returned for any model, so a caller cannot
    // half-apply an unconfigured block.
    assert_eq!(cfg.context.levers_for("deepseek-flash"), None);
    assert_eq!(cfg.context.levers_for("MiniMax-M3"), None);
}

#[test]
fn an_enabled_block_without_profiles_sniffs_the_provider_family() {
    let cfg = config_from(&format!("{}\n[context]\nenabled = true\n", minimal()));
    assert_eq!(
        cfg.context.levers_for("deepseek-flash"),
        Some(Levers::DEEPSEEK)
    );
    assert_eq!(cfg.context.levers_for("MiniMax-M3"), Some(Levers::MINIMAX));
    assert_eq!(cfg.context.levers_for("qwen38"), Some(Levers::OTHER));
}

#[test]
fn a_named_profile_matches_through_a_provider_namespace() {
    let cfg = config_from(&format!("{}\n[context]\nenabled = true\n", minimal()));
    assert_eq!(
        cfg.context.levers_for("azure_ai/FW-DeepSeek-V4-Flash"),
        Some(Levers::DEEPSEEK)
    );
    assert_eq!(
        cfg.context.levers_for("minimax/MiniMax-M3.1-Flash"),
        Some(Levers::MINIMAX)
    );
}

#[test]
fn an_explicit_profile_overrides_only_the_fields_it_names() {
    let cfg = config_from(&format!(
        "{}\n[context]\nenabled = true\n[context.profiles.deepseek]\ncompact_after_tokens = 60000\n",
        minimal()
    ));
    let levers = cfg.context.levers_for("deepseek-flash").unwrap();
    assert_eq!(levers.compact_after_tokens, 60_000);
    // Everything not named keeps the DeepSeek profile's value.
    assert_eq!(
        levers.tool_output_cap_bytes,
        Levers::DEEPSEEK.tool_output_cap_bytes
    );
    assert_eq!(levers.elide_after_turns, Levers::DEEPSEEK.elide_after_turns);
    assert_eq!(levers.keep_recent_turns, Levers::DEEPSEEK.keep_recent_turns);
}

#[test]
fn compaction_can_be_disabled_per_provider_with_zero() {
    let cfg = config_from(&format!(
        "{}\n[context]\nenabled = true\n[context.profiles.minimax]\ncompact_after_tokens = 0\n",
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
        "{}\n[context]\nenabled = true\ndefault_profile = \"claude\"\n",
        minimal()
    ));
    // No `claude` built-in and no `profiles` entry: the catch-all applies.
    assert_eq!(cfg.context.levers_for("qwen38"), Some(Levers::OTHER));
}

#[test]
fn the_other_profile_is_the_catch_all_when_a_model_matches_nothing() {
    let cfg = config_from(&format!(
        "{}\n[context]\nenabled = true\n[context.profiles.other]\nelide_after_turns = 3\n",
        minimal()
    ));
    let levers = cfg.context.levers_for("qwen38").unwrap();
    assert_eq!(levers.elide_after_turns, 3);
    assert_eq!(levers.compact_after_tokens, 0, "OTHER never compacts");
}

#[test]
fn a_longer_profile_key_beats_a_shorter_one() {
    let cfg = config_from(&format!(
        "{}\n[context]\nenabled = true\n\
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
fn measurement_is_on_whenever_the_block_is_configured() {
    let absent = config_from(minimal());
    assert!(!absent.context.measure("deepseek-flash"));
    let enabled = config_from(&format!("{}\n[context]\nenabled = true\n", minimal()));
    assert!(enabled.context.measure("deepseek-flash"));
}

#[test]
fn a_config_without_context_round_trips_byte_for_byte() {
    let cfg = config_from(minimal());
    let text = toml::to_string(&cfg).unwrap();
    assert!(!text.contains("context"), "{text}");
}
