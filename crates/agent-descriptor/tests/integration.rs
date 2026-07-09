//! Integration tests for the agent-descriptor crate. These read the
//! checked-in artifacts (`schema/*.json`) and exercise them through the
//! public API: round-trip, JSON-Schema validation, and the descriptor-id
//! format invariant.

use agent_descriptor::{
    consumer::{derive_transport, Error as ConsumerError},
    descriptor_id,
    validate::{validate_and_parse, validate_v1},
    AgentDescriptor, Capabilities, ConformanceLevel,
};
use serde_json::Value;
use std::fs;

/// Path to the checked-in v1 schema (crate-root relative to `CARGO_MANIFEST_DIR`).
const V1_SCHEMA: &str = "schema/agent-descriptor.v1.schema.json";

/// Path to the checked-in zoder descriptor artifact.
const ZODER_DESCRIPTOR: &str = "schema/zoder.descriptor.json";

fn read_workspace_path(rel: &str) -> String {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    let p = std::path::Path::new(&manifest_dir).join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn checked_in_zoder_descriptor_validates_against_v1_schema() {
    let descriptor_str = read_workspace_path(ZODER_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    validate_v1(&descriptor)
        .expect("checked-in `schema/zoder.descriptor.json` must validate against the v1 schema");
}

#[test]
fn checked_in_zoder_descriptor_parses_and_roundtrips() {
    let descriptor_str = read_workspace_path(ZODER_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor =
        validate_and_parse(&descriptor).expect("descriptor must validate + deserialize");

    // Re-serializing must yield byte-stable JSON (modulo formatting).
    let s1 = serde_json::to_string(&parsed).expect("serialize");
    let v1: Value = serde_json::from_str(&s1).unwrap();
    let v2 = descriptor;
    assert_eq!(v1, v2, "round-trip drifted");
}

#[test]
fn zoder_descriptor_is_l2_and_advertises_acp() {
    let descriptor_str = read_workspace_path(ZODER_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor =
        validate_and_parse(&descriptor).expect("descriptor must validate + deserialize");

    assert_eq!(parsed.conformance_level, ConformanceLevel::L2);
    assert!(
        parsed.capabilities.acp_capable,
        "zoder/zeroclaw advertises ACP support"
    );
    assert!(
        parsed.config_surface.is_some(),
        "L2 descriptor must include a config surface"
    );
}

#[test]
fn zoder_descriptor_id_matches_helper() {
    let descriptor_str = read_workspace_path(ZODER_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor =
        validate_and_parse(&descriptor).expect("descriptor must validate + deserialize");
    assert_eq!(parsed.id, descriptor_id("zoder", parsed.schema_version));
}

#[test]
fn v1_schema_itself_loads() {
    // Sanity: read the v1 schema and confirm the json parses. The actual
    // conformance assertions live in `validate_v1`-backed tests.
    let s = read_workspace_path(V1_SCHEMA);
    let _: Value = serde_json::from_str(&s).expect("v1 schema must parse as JSON");
}

#[test]
fn every_knob_in_descriptor_roundtrips() {
    let descriptor_str = read_workspace_path(ZODER_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor =
        validate_and_parse(&descriptor).expect("descriptor must validate + deserialize");

    let surface = parsed
        .config_surface
        .expect("L2 has a config surface; checked in validated above");
    assert!(
        !surface.knobs.is_empty(),
        "the zoder descriptor must enumerate knobs (the codegen crate \
         emits at least one knob per Config field)",
    );

    for knob in &surface.knobs {
        let v = serde_json::to_value(knob).expect("knob serializes");
        let back: agent_descriptor::Knob =
            serde_json::from_value(v.clone()).expect("knob deserializes");
        let back_v = serde_json::to_value(&back).unwrap();
        assert_eq!(v, back_v, "knob {} round-trip drifted", knob.name);
    }
}

#[test]
fn capabilities_extensions_roundtrip() {
    let descriptor_str = read_workspace_path(ZODER_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let _parsed: AgentDescriptor =
        validate_and_parse(&descriptor).expect("descriptor must validate + deserialize");

    // Capabilities.extensions is `BTreeMap<String, String>`; ensure JSON
    // serialization produces sorted keys, never `null` for an empty map.
    let s = serde_json::to_string(&Capabilities {
        acp_capable: true,
        extensions: Default::default(),
    })
    .expect("serialize");
    assert!(
        !s.contains("\"extensions\""),
        "empty extensions must be skipped"
    );
}

// ----------------------------------------------------------------------------
// Goose descriptor (slice 2): validation + structural asserts.
// ----------------------------------------------------------------------------

/// Path to the checked-in goose descriptor artifact (slice 2).
const GOOSE_DESCRIPTOR: &str = "schema/goose.descriptor.json";

#[test]
fn checked_in_goose_descriptor_validates_against_v1_schema() {
    let descriptor_str = read_workspace_path(GOOSE_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    validate_v1(&descriptor)
        .expect("checked-in `schema/goose.descriptor.json` must validate against the v1 schema");
}

#[test]
fn checked_in_goose_descriptor_parses_and_roundtrips() {
    let descriptor_str = read_workspace_path(GOOSE_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor =
        validate_and_parse(&descriptor).expect("descriptor must validate + deserialize");

    let s1 = serde_json::to_string(&parsed).expect("serialize");
    let v1: Value = serde_json::from_str(&s1).unwrap();
    let v2 = descriptor;
    assert_eq!(v1, v2, "goose round-trip drifted");
}

#[test]
fn goose_descriptor_is_l2_and_advertises_acp() {
    let descriptor_str = read_workspace_path(GOOSE_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor =
        validate_and_parse(&descriptor).expect("descriptor must validate + deserialize");

    assert_eq!(parsed.conformance_level, ConformanceLevel::L2);
    assert!(
        parsed.capabilities.acp_capable,
        "goose advertises ACP support (it IS the ACP reference engine)"
    );
    assert!(
        parsed.config_surface.is_some(),
        "L2 descriptor must include a config surface"
    );
}

#[test]
fn goose_descriptor_connection_is_stdio() {
    let descriptor_str = read_workspace_path(GOOSE_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor = validate_and_parse(&descriptor).expect("parses");

    use agent_descriptor::Transport;
    assert_eq!(
        parsed.connection.transport,
        Transport::Stdio,
        "goose is reached by spawning `goose acp` over stdio"
    );
    use agent_descriptor::Endpoint;
    match &parsed.connection.endpoint {
        Endpoint::Path { path } => assert_eq!(path, "goose"),
        other => panic!("goose endpoint must be Path, got {other:?}"),
    }
    assert!(
        parsed.connection.auth.is_none(),
        "stdio transport must carry no auth (schema enforces this)"
    );
}

#[test]
fn goose_descriptor_knobs_carry_provenance_in_description() {
    // The v1 schema does not model per-knob source as a separate field
    // (only `ConfigSurface.source` is required). The contract used by
    // descriptors in this repo: each knob's `description` is prefixed
    // with `[env]` or `[file: <ext>]` so a consumer can grep on it.
    let descriptor_str = read_workspace_path(GOOSE_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor = validate_and_parse(&descriptor).expect("parses");

    let surface = parsed
        .config_surface
        .expect("L2 descriptor must include a config surface");

    // We expect GOOSE_PROVIDER + GOOSE_MODEL + OPENAI_API_KEY +
    // ANTHROPIC_API_KEY to be env knobs; the rest to be file knobs.
    let env_knobs = [
        "GOOSE_PROVIDER",
        "GOOSE_MODEL",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
    ];

    for knob in &surface.knobs {
        let desc = knob.description.as_deref().unwrap_or("");
        let is_env = env_knobs.contains(&knob.name.as_str());
        let prefix = if is_env { "[env]" } else { "[file:" };
        assert!(
            desc.starts_with(prefix),
            "knob `{}` description must start with `{prefix}` (got: {desc:?})",
            knob.name
        );
    }
}

#[test]
fn goose_descriptor_recipes_and_scheduler_live_in_extensions() {
    // Per ADR-0001 + the descriptor's own comment, recipes/scheduling are
    // goose-specific extensions — they MUST NOT be modeled as core
    // knobs (no other conformant implementation needs them; uplifting
    // would require a new conformance level). Assert they are present
    // on the descriptor-level `extensions` blob.
    let descriptor_str = read_workspace_path(GOOSE_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");

    let top = descriptor
        .as_object()
        .expect("descriptor must be a JSON object");
    let ext = top
        .get("extensions")
        .expect("goose descriptor must have a top-level extensions blob")
        .as_object()
        .expect("extensions must be a JSON object");
    assert!(
        ext.contains_key("block.goose.recipes"),
        "goose descriptor must carry recipes under extensions.goose.recipes"
    );
    assert!(
        ext.contains_key("block.goose.scheduler"),
        "goose descriptor must carry scheduling under extensions.goose.scheduler"
    );
}

// ----------------------------------------------------------------------------
// Consumer (slice 2 part 3): a single, testable seam that projects both
// descriptors onto the engine transport shape uniformly.
// ----------------------------------------------------------------------------

#[test]
fn consumer_drives_goose_as_stdio() {
    // Load goose from disk end-to-end: read -> JSON-schema validate ->
    // typed parse -> consumer. Anything failing along the way surfaces as
    // a panic with a precise cause.
    let descriptor_str = read_workspace_path(GOOSE_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor = validate_and_parse(&descriptor).expect("parses + validates");

    let transport = derive_transport(&parsed).expect("goose descriptor must be driveable");
    match transport {
        acp_client::EngineTransport::Stdio { command, args, env } => {
            assert_eq!(command, "goose", "endpoint.path is the binary to spawn");
            assert_eq!(
                args,
                vec!["acp".to_string()],
                "stdio + acp_capable conventional args are `acp`"
            );
            assert!(
                env.is_empty(),
                "the consumer yields the SHAPE only; the credential/endpoint bridge \
                 (zoder -> OPENAI_* / OPENAI_HOST) is layered on top by callers via \
                 `acp_client::GooseProviderEnv`, never baked into the descriptor"
            );
        }
        other => panic!("goose descriptor must yield EngineTransport::Stdio, got {other:?}"),
    }
}

#[test]
fn consumer_drives_zeroclaw_as_unix_socket() {
    let descriptor_str = read_workspace_path(ZODER_DESCRIPTOR);
    let descriptor: Value = serde_json::from_str(&descriptor_str).expect("descriptor is JSON");
    let parsed: AgentDescriptor = validate_and_parse(&descriptor).expect("parses + validates");

    let transport = derive_transport(&parsed).expect("zoder descriptor must be driveable");
    match transport {
        acp_client::EngineTransport::UnixSocket(path) => {
            // The checked-in zoder descriptor names the conventional
            // socket path under $ZODER_HOME; the helper keeps it as a
            // plain string so the test doesn't drift across operators.
            assert_eq!(path.to_string_lossy(), "$ZODER_HOME/run/zoder.sock");
        }
        other => panic!("zoder descriptor must yield EngineTransport::UnixSocket, got {other:?}"),
    }
}

#[test]
fn consumer_rejects_unsupported_transport() {
    // Sanity: the consumer must NOT silently misdrive an engine it
    // cannot reach. Build a tiny descriptor naming `wss` and confirm
    // `derive_transport` returns `Error::UnsupportedTransport`.
    let desc = AgentDescriptor {
        id: descriptor_id("probe", 1),
        name: "probe".into(),
        agent_id: "ncz-os/probe".into(),
        version: "0.0.1".into(),
        schema_version: 1,
        conformance_level: ConformanceLevel::L1,
        connection: agent_descriptor::Connection {
            transport: agent_descriptor::Transport::Wss,
            endpoint: agent_descriptor::Endpoint::Url {
                url: "wss://example.invalid/acp".into(),
            },
            auth: None,
        },
        config_surface: None,
        capabilities: Capabilities {
            acp_capable: true,
            extensions: Default::default(),
        },
        extensions: Default::default(),
    };
    let err = derive_transport(&desc).expect_err("wss must be rejected");
    assert!(
        matches!(err, ConsumerError::UnsupportedTransport(_)),
        "expected UnsupportedTransport, got {err:?}"
    );
}

// ----------------------------------------------------------------------------
// C4-AD1 / C4-AD2 (schema<->struct authority reconciliation).
//
// Two validation entry points MUST agree on the same set of accepted
// documents:
//   * `validate_v1`        — JSON-Schema only (offered as a standalone check),
//   * `validate_and_parse` — JSON-Schema *then* typed serde deserialize.
//
// Historically the schema was laxer than the `ConfigSurface`/conformance
// contract: an L2 descriptor could omit `config_surface`, and a
// `config_surface` could omit `source`/`schema` — both passed `validate_v1`
// but the second failed the typed parse. These tests pin the reconciled,
// stricter behavior.
// ----------------------------------------------------------------------------

/// A minimal, schema-valid L1 descriptor (no config_surface) as a JSON value.
/// Callers mutate `conformance_level` / `config_surface` to build the cases.
fn minimal_l1_descriptor() -> Value {
    serde_json::json!({
        "id": "ncz-os:agent-descriptor:probe:v1",
        "name": "probe",
        "agent_id": "ncz-os/probe",
        "version": "0.0.1",
        "schema_version": 1,
        "conformance_level": 1,
        "connection": {
            "transport": "stdio",
            "endpoint": { "kind": "path", "path": "/usr/bin/probe" },
            "auth": null
        },
        "capabilities": { "acp_capable": true }
    })
}

#[test]
fn l1_without_config_surface_still_validates_and_parses() {
    let d = minimal_l1_descriptor();
    validate_v1(&d).expect("baseline L1 (no config_surface) must remain valid");
    validate_and_parse(&d).expect("baseline L1 must also parse");
}

#[test]
fn c4_ad1_l2_without_config_surface_now_fails_validate_v1() {
    // conformance_level 2 with NO config_surface: previously passed
    // validate_v1 (violating the ADR contract); must now FAIL the schema.
    let mut d = minimal_l1_descriptor();
    d["conformance_level"] = serde_json::json!(2);
    // (no config_surface key)

    let schema_err =
        validate_v1(&d).expect_err("C4-AD1: L2 without config_surface must fail validate_v1");
    // Both entry points must agree it is invalid.
    let parse_err = validate_and_parse(&d)
        .expect_err("C4-AD1: L2 without config_surface must fail validate_and_parse too");
    let _ = (schema_err, parse_err);
}

#[test]
fn c4_ad1_l2_with_null_config_surface_now_fails_validate_v1() {
    // The `config_surface: null` branch must not satisfy the L2 requirement.
    let mut d = minimal_l1_descriptor();
    d["conformance_level"] = serde_json::json!(2);
    d["config_surface"] = Value::Null;

    validate_v1(&d).expect_err("C4-AD1: L2 with config_surface:null must fail validate_v1");
}

#[test]
fn c4_ad2_config_surface_missing_source_schema_now_fails_validate_v1() {
    // `{"config_surface": {"knobs": []}}` previously passed validate_v1 but
    // failed the typed parse (serde requires `source` + `schema`). The schema
    // now requires them too, so BOTH entry points reject it consistently.
    let mut d = minimal_l1_descriptor();
    d["conformance_level"] = serde_json::json!(2);
    d["config_surface"] = serde_json::json!({ "knobs": [] });

    validate_v1(&d)
        .expect_err("C4-AD2: config_surface missing source/schema must now fail validate_v1");
    validate_and_parse(&d)
        .expect_err("C4-AD2: config_surface missing source/schema must fail validate_and_parse");
}

#[test]
fn c4_ad2_fully_valid_l2_descriptor_still_passes_both_entry_points() {
    // A complete L2 descriptor (config_surface with source + schema + knobs)
    // must pass validate_v1 AND deserialize — the two authorities agree.
    let mut d = minimal_l1_descriptor();
    d["conformance_level"] = serde_json::json!(2);
    d["config_surface"] = serde_json::json!({
        "source": "toml",
        "schema": { "type": "object" },
        "knobs": [
            {
                "name": "default_provider",
                "kind": "string",
                "required": true
            }
        ]
    });

    validate_v1(&d).expect("fully-valid L2 descriptor must pass validate_v1");
    let parsed = validate_and_parse(&d).expect("fully-valid L2 descriptor must parse");
    assert_eq!(parsed.conformance_level, ConformanceLevel::L2);
    assert!(parsed.config_surface.is_some());
}
