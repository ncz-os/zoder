//! JSON-Schema validation for descriptors (and the inline `config_surface.schema`).
//!
//! Used in two places:
//!   * the unit tests for the codegen output (`schema/zoder.descriptor.json`
//!     MUST validate against `schema/agent-descriptor.v1.schema.json`),
//!   * any consumer that wants to confirm a descriptor loaded from disk
//!     matches the format before treating it as authoritative.

use std::sync::OnceLock;

use serde_json::Value;
use thiserror::Error;

use crate::schema;

/// All errors that can occur while validating a descriptor.
#[derive(Debug, Error)]
pub enum ValidationError {
    /// The v1 schema itself failed to compile. Should be unreachable in
    /// practice; we surface it so the test suite catches drift.
    #[error("internal: v1 schema did not compile: {0}")]
    Compile(String),
    /// The descriptor JSON did not satisfy the schema. `path` is the JSON
    /// pointer to the offending value (when available).
    #[error("descriptor does not match v1 schema (path=`{path}`): {message}")]
    Schema {
        /// JSON pointer (e.g. `/connection/transport`) or `""` when unknown.
        path: String,
        /// Underlying validator message.
        message: String,
    },
}

/// Validate `descriptor` (already parsed to a `Value`) against the v1 schema.
///
/// Returns `Ok(())` if the descriptor conforms to the schema, or a
/// [`ValidationError::Schema`] pointing at the first violation found.
pub fn validate_v1(descriptor: &Value) -> Result<(), ValidationError> {
    let validator = validator().map_err(|e| ValidationError::Compile(e.to_string()))?;
    if let Err(errors) = validator.validate(descriptor) {
        // `jsonschema::ValidationError` exposes `instance_path` as a JSON
        // pointer (e.g. `/connection/transport`). Report the first error.
        if let Some(err) = errors.into_iter().next() {
            let path = err.instance_path.to_string();
            return Err(ValidationError::Schema {
                path: if path.is_empty() {
                    "<root>".into()
                } else {
                    path
                },
                message: err.to_string(),
            });
        }
    }
    Ok(())
}

/// Validate then return the [`crate::descriptor::AgentDescriptor`] if all
/// shape + JSON-schema checks pass. Convenience wrapper for the common
/// "load JSON, check it, deserialize" sequence.
pub fn validate_and_parse(descriptor: &Value) -> Result<crate::descriptor::AgentDescriptor, Error> {
    validate_v1(descriptor)?;
    serde_json::from_value(descriptor.clone()).map_err(Error::Deserialize)
}

/// Combined error type for [`validate_and_parse`].
#[derive(Debug, Error)]
pub enum Error {
    /// Schema violation (see [`ValidationError`]).
    #[error(transparent)]
    Schema(#[from] ValidationError),
    /// `serde_json` failed to deserialize the validated JSON.
    #[error("descriptor validates against schema but fails to deserialize: {0}")]
    Deserialize(#[from] serde_json::Error),
}

fn validator() -> Result<&'static jsonschema::JSONSchema, String> {
    static CELL: OnceLock<Result<jsonschema::JSONSchema, String>> = OnceLock::new();
    CELL.get_or_init(|| {
        let schema_value = schema::v1();
        jsonschema::JSONSchema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .compile(schema_value)
            .map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| e.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::KnobKind;
    use crate::{
        descriptor_id, Capabilities, ConfigSource, ConfigSurface, ConformanceLevel, Connection,
        Endpoint, Knob, Transport, SCHEMA_VERSION,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn minimal_descriptor(level: ConformanceLevel) -> Value {
        let desc = crate::AgentDescriptor {
            id: descriptor_id("zoder", SCHEMA_VERSION),
            name: "zoder / zeroclaw".into(),
            agent_id: "ncz-os/zoder".into(),
            version: "0.2.1".into(),
            schema_version: SCHEMA_VERSION,
            conformance_level: level,
            connection: Connection {
                transport: Transport::UnixSocket,
                endpoint: Endpoint::Path {
                    path: "/tmp/zoder.sock".into(),
                },
                auth: None,
            },
            config_surface: matches!(level, ConformanceLevel::L2).then(|| ConfigSurface {
                source: ConfigSource::Toml,
                default_path: Some("$ZODER_HOME/config.toml".into()),
                schema: json!({"type": "object"}),
                knobs: vec![Knob {
                    name: "default_provider".into(),
                    kind: KnobKind::String,
                    required: true,
                    default: None,
                    secret: false,
                    enum_values: None,
                    r#ref: None,
                    description: None,
                }],
            }),
            capabilities: Capabilities {
                acp_capable: true,
                extensions: BTreeMap::new(),
            },
            extensions: BTreeMap::new(),
        };
        serde_json::to_value(&desc).unwrap()
    }

    #[test]
    fn minimal_l1_validates() {
        let v = minimal_descriptor(ConformanceLevel::L1);
        validate_v1(&v).expect("L1 descriptor must validate");
    }

    #[test]
    fn minimal_l2_validates() {
        let v = minimal_descriptor(ConformanceLevel::L2);
        validate_v1(&v).expect("L2 descriptor must validate");
    }

    #[test]
    fn wrong_id_format_is_rejected() {
        let mut v = minimal_descriptor(ConformanceLevel::L1);
        v["id"] = json!("not-a-real-id");
        let err = validate_v1(&v).expect_err("wrong id format must fail");
        assert!(matches!(err, ValidationError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let mut v = minimal_descriptor(ConformanceLevel::L1);
        v.as_object_mut()
            .unwrap()
            .insert("bogus".into(), json!("hello"));
        let err = validate_v1(&v).expect_err("extra fields must fail");
        assert!(matches!(err, ValidationError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn schema_does_not_demand_config_surface_at_l1() {
        // L1: config_surface is allowed to be omitted entirely OR to be null.
        let mut v = minimal_descriptor(ConformanceLevel::L1);
        v.as_object_mut().unwrap().remove("config_surface");
        validate_v1(&v).expect("L1 should not require config_surface");
    }

    #[test]
    fn c4_ad1_l2_requires_config_surface() {
        // C4-AD1: an L2 descriptor with NO config_surface must fail the schema
        // (previously it passed, violating the ADR contract).
        let mut v = minimal_descriptor(ConformanceLevel::L2);
        v.as_object_mut().unwrap().remove("config_surface");
        let err = validate_v1(&v).expect_err("L2 without config_surface must fail validate_v1");
        assert!(matches!(err, ValidationError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn c4_ad1_l2_rejects_null_config_surface() {
        // C4-AD1: `config_surface: null` does not satisfy the L2 requirement.
        let mut v = minimal_descriptor(ConformanceLevel::L2);
        v["config_surface"] = json!(null);
        let err = validate_v1(&v).expect_err("L2 with config_surface:null must fail validate_v1");
        assert!(matches!(err, ValidationError::Schema { .. }), "got {err:?}");
    }

    #[test]
    fn c4_ad2_config_surface_requires_source_and_schema() {
        // C4-AD2: the schema now mirrors the ConfigSurface struct (serde
        // requires `source` + `schema`). A config_surface with only `knobs`
        // used to pass validate_v1 but fail the typed parse; now both agree.
        let mut v = minimal_descriptor(ConformanceLevel::L2);
        v["config_surface"] = json!({ "knobs": [] });
        validate_v1(&v)
            .expect_err("config_surface missing source/schema must now fail validate_v1");
        validate_and_parse(&v)
            .expect_err("config_surface missing source/schema must fail validate_and_parse");
    }
}
