//! `agent-descriptor` — vendor-neutral Agent Descriptor (slice 1 foundation).
//!
//! See `docs/adr/0001-agent-descriptor-and-acp-posture.md`. This crate
//! defines the descriptor format (MIF-inspired: serde types + a checked-in
//! JSON Schema with a stable `$id`) and the codegen entry points used by
//! `src/bin/codegen-zoder.rs` to emit a static descriptor for the
//! zoder/zeroclaw implementation.
//!
//! The crate itself is runtime-light: serde types, the static schema
//! reachable via [`schema`], and helpers. The heavyweight codegen
//! (walking `schemars::schema::Schema` for `zoder_core::config::Config`)
//! lives behind a binary in `src/bin/` and is NOT pulled in by anything
//! that just wants to read or write descriptors at runtime.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod consumer;
pub mod descriptor;
pub mod schema;
pub mod validate;

pub use consumer::{derive_transport, Error as ConsumerError};
pub use descriptor::{
    AgentDescriptor, AuthSpec, Capabilities, ConfigSource, ConfigSurface, ConformanceLevel,
    Connection, Endpoint, Knob, KnobKind, Transport, SCHEMA_VERSION,
};

/// The schema-URI all v1 descriptors must carry. Stable; bump only when
/// the descriptor format itself changes (which would invalidate `schema_version`
/// and require a new `$id`).
pub const DESCRIPTOR_SCHEMA_V1_ID: &str = "https://ncz-os.dev/schemas/agent-descriptor/v1.json";

/// The vendor-namespace prefix used by `AgentDescriptor.id` for descriptors
/// emitted by this project (e.g. `ncz-os:agent-descriptor:zoder:v1`).
pub const VENDOR_NAMESPACE: &str = "ncz-os";

/// Convenience constructor: an empty `<vendor-namespace>:agent-descriptor:<slug>:v<major>` id.
pub fn descriptor_id(slug: &str, schema_version_major: u32) -> String {
    format!(
        "{VENDOR_NAMESPACE}:agent-descriptor:{slug}:v{schema_version_major}",
        VENDOR_NAMESPACE = VENDOR_NAMESPACE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptor_id_format_is_stable() {
        assert_eq!(
            descriptor_id("zoder", 1),
            "ncz-os:agent-descriptor:zoder:v1"
        );
    }

    #[test]
    fn schema_id_matches_constant() {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../schema/agent-descriptor.v1.schema.json"))
                .expect("schema file is valid JSON");
        assert_eq!(
            value["$id"].as_str(),
            Some(DESCRIPTOR_SCHEMA_V1_ID),
            "static schema $id drifted from DESCRIPTOR_SCHEMA_V1_ID",
        );
    }

    #[test]
    fn schema_has_required_top_keys() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../schema/agent-descriptor.v1.schema.json"))
                .unwrap();
        for key in ["$schema", "$id", "title", "type", "required", "properties"] {
            assert!(v.get(key).is_some(), "schema missing top-level key {key}");
        }
        assert_eq!(
            v["$schema"],
            json!("https://json-schema.org/draft/2020-12/schema")
        );
    }
}
