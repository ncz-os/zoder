//! Integration tests for the agent-descriptor crate. These read the
//! checked-in artifacts (`schema/*.json`) and exercise them through the
//! public API: round-trip, JSON-Schema validation, and the descriptor-id
//! format invariant.

use agent_descriptor::{
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
