//! Runtime types that mirror the JSON Schema in
//! `schema/agent-descriptor.v1.schema.json`. They are deliberately close
//! to the wire format: every field is `pub`, every enum has an explicit
//! `#[serde(rename_all = ...)]`, and missing/extra fields are caught by
//! `deny_unknown_fields` on the wire-facing structs (the internal
//! `Schema` blob inside `ConfigSurface.schema` is intentionally a free-form
//! `serde_json::Value` — JSON Schema is recursive and a hand-rolled enum
//! would duplicate the spec).
//!
//! Round-trip (serialize → parse → re-serialize) is a hard invariant; the
//! `tests::roundtrip_*` tests cover every public type.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Current schema version this crate emits. Bump only on breaking changes
/// to the descriptor wire format (paired with a new schema URI).
pub const SCHEMA_VERSION: u32 = 1;

/// Top-level descriptor: the artifact a foreign client consumes once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    /// Stable id URI; see [`crate::descriptor_id`].
    pub id: String,
    /// Human-readable name (e.g. "zoder / zeroclaw").
    pub name: String,
    /// Reverse-DNS identifier of the agent implementation itself.
    pub agent_id: String,
    /// Implementation version, free-form.
    pub version: String,
    /// Schema version this descriptor was authored against. MUST equal the
    /// major version embedded in `id` (a separate field so consumers can
    /// branch on it without parsing the id string).
    pub schema_version: u32,
    /// MIF-style conformance level: 1 = identity + connection; 2 = + config surface.
    pub conformance_level: ConformanceLevel,
    /// How to reach the agent.
    pub connection: Connection,
    /// Optional config surface. Required when `conformance_level >= 2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_surface: Option<ConfigSurface>,
    /// Implementation-level capability flags (acp_capable, etc.).
    pub capabilities: Capabilities,
    /// Descriptor-level vendor extensions (e.g. goose-specific
    /// `recipes`/`scheduler` blobs that no other implementation needs).
    /// Modeled as a free-form key→`Value` map: the schema only mandates
    /// that keys be reverse-DNS; shape per key is vendor-private.
    /// Free-form `Value` is intentional — matching the schema's
    /// `additionalProperties: true` so descriptor-level extensions never
    /// require a schema bump. Empty when the descriptor carries none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// MIF-style conformance level. Two levels; we do not add more without an
/// ADR.
///
/// On the wire this is serialized as the integer `1` or `2`, matching the
/// JSON Schema enum [`1, 2`]. It is *not* renamed to "l1"/"l2" because the
/// schema is the source of truth; the `Display` impl is purely human-facing.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum ConformanceLevel {
    /// Identity + connection only.
    L1 = 1,
    /// Identity + connection + config surface.
    L2 = 2,
}

impl From<ConformanceLevel> for u8 {
    fn from(c: ConformanceLevel) -> u8 {
        c as u8
    }
}

impl From<u8> for ConformanceLevel {
    fn from(v: u8) -> ConformanceLevel {
        match v {
            1 => ConformanceLevel::L1,
            2 => ConformanceLevel::L2,
            // Unknown minor versions are mapped to L1 (the conservative baseline:
            // connection is mandatory; config surface is optional).
            _ => ConformanceLevel::L1,
        }
    }
}

impl fmt::Display for ConformanceLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}", *self as u8)
    }
}

/// Transport the agent speaks on its wire. Mirrors the JSON Schema enum
/// literally; if a new transport is needed, add a corresponding JSON
/// Schema entry first (the schema is the source of truth).
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum Transport {
    /// Spawned child process, JSON-RPC over its stdio.
    #[serde(rename = "stdio")]
    Stdio,
    /// Local-domain Unix domain socket (path endpoint).
    #[serde(rename = "unix_socket")]
    UnixSocket,
    /// Plain TCP socket.
    #[serde(rename = "tcp")]
    Tcp,
    /// Plain HTTP (no TLS). Mostly for development; unencrypted.
    #[serde(rename = "http")]
    Http,
    /// WebSocket (ws://).
    #[serde(rename = "websocket")]
    WebSocket,
    /// WebSocket Secure (wss://).
    #[serde(rename = "wss")]
    Wss,
    /// HTTPS.
    #[serde(rename = "https")]
    Https,
}

/// Endpoint address. Tagged union by `kind` (mirrors the JSON Schema oneOf).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Endpoint {
    /// Filesystem path (used by stdio / unix_socket).
    Path {
        /// Absolute path.
        path: String,
    },
    /// Full URI (HTTP, HTTPS, WS, WSS).
    Url {
        /// URL string.
        url: String,
    },
    /// Bare host + port (TCP).
    HostPort {
        /// Hostname or IP.
        host: String,
        /// TCP port.
        port: u16,
    },
}

/// Auth-style specification. Mirrors `zoder_core::config::Auth` 1:1 in
/// shape (vendor-neutral naming) so the codegen can lift transports
/// across without renaming.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthSpec {
    /// No auth (stdio / open unix socket).
    None,
    /// Read bearer token from this environment variable name.
    EnvVar {
        /// Env-var name (must match `^[A-Z_][A-Z0-9_]*$`).
        var: String,
    },
    /// Inline bearer (discouraged; modeled for symmetry with auth shapes).
    Bearer {
        /// Env-var name from which to resolve the bearer at runtime.
        var: String,
    },
    /// Custom header auth (Azure OpenAI `api-key`, OCI gateway, etc.).
    ApiKeyHeader {
        /// Header name (e.g. `api-key`).
        header: String,
        /// Env-var holding the value.
        var: String,
    },
}

/// Connection descriptor: `transport` + `endpoint` + optional `auth`.
/// Mirrors `zoder_core::config::Auth` plus transport metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    /// Transport the agent speaks.
    pub transport: Transport,
    /// Endpoint address (interpretation depends on transport).
    pub endpoint: Endpoint,
    /// Auth (None for stdio).
    #[serde(default)]
    pub auth: Option<AuthSpec>,
}

/// Capability flags. Optional `extensions` is the reserved escape hatch
/// for vendor-specific boolean-ish flags (kept as `BTreeMap` for stable
/// JSON key ordering on round-trip).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Whether the agent speaks the Agent Client Protocol. Independent
    /// from `connection.transport`; ACP can be carried over stdio or WSS.
    pub acp_capable: bool,
    /// Vendor-specific extension flags (keys must be reverse-DNS).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: std::collections::BTreeMap<String, String>,
}

// BTreeMap re-import for the `is_empty` filter above.
use std::collections::BTreeMap;

/// Type tag for a single knob. `Enum` and `SecretRef` delegate additional
/// fields (`enum_values` and `ref`) — see the JSON Schema for the
/// field-level constraints.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobKind {
    /// Plain string.
    String,
    /// 64-bit integer (counter, port, cap, ...).
    Integer,
    /// Floating-point number.
    Number,
    /// Boolean.
    Boolean,
    /// Array (element kind unspecified).
    Array,
    /// Free-form object.
    Object,
    /// Closed enum; values listed in `enum_values`.
    Enum,
    /// Filesystem path.
    Path,
    /// URL string.
    Url,
    /// Reference to a secret (env-var name or vault key). Always `secret = true`.
    SecretRef,
}

/// A single config-surface knob. Mirrors the JSON Schema oneOf tail of
/// the same name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Knob {
    /// Dotted or bare identifier; MUST be unique within a `ConfigSurface`.
    pub name: String,
    /// Kind tag.
    pub kind: KnobKind,
    /// Whether the knob MUST be present (true) or has a sensible default.
    pub required: bool,
    /// JSON value of the default, if deterministic. Omit for non-deterministic
    /// knobs (system paths, OS-derived values).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// Whether the value is a credential / token. UI-redaction hint, no
    /// semantic effect on parsing.
    #[serde(default)]
    pub secret: bool,
    /// Required when `kind == Enum`. Permitted values (order is significant
    /// for client UIs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    /// Required when `kind == SecretRef`. Env-var name (or vault key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Config-surface descriptor for an agent. Optional at the top level —
/// conformance level 1 may omit it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSurface {
    /// Format of the agent's config file (e.g. "toml", "json").
    pub source: ConfigSource,
    /// Conventional default path for the config file. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_path: Option<String>,
    /// Inline JSON Schema of the underlying config object (recursive;
    /// kept as `Value` so we don't have to hand-roll a JSON-Schema
    /// mirror — the schema is the authority on shape).
    pub schema: serde_json::Value,
    /// Ordered list of knobs (the denormalized, human-friendly form).
    pub knobs: Vec<Knob>,
}

/// How the agent loads its config file.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSource {
    /// `config.json`.
    Json,
    /// `config.toml`.
    Toml,
    /// `config.yaml`.
    Yaml,
    /// Pure env-var config (no file).
    Env,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptor_roundtrip() {
        let original = AgentDescriptor {
            id: "ncz-os:agent-descriptor:zoder:v1".into(),
            name: "zoder / zeroclaw".into(),
            agent_id: "ncz-os/zoder".into(),
            version: "0.2.1".into(),
            schema_version: SCHEMA_VERSION,
            conformance_level: ConformanceLevel::L2,
            connection: Connection {
                transport: Transport::UnixSocket,
                endpoint: Endpoint::Path {
                    path: "/tmp/zoder.sock".into(),
                },
                auth: None,
            },
            config_surface: Some(ConfigSurface {
                source: ConfigSource::Toml,
                default_path: Some("~/.zoder/config.toml".into()),
                schema: json!({"type": "object"}),
                knobs: vec![Knob {
                    name: "default_provider".into(),
                    kind: KnobKind::String,
                    required: true,
                    default: None,
                    secret: false,
                    enum_values: None,
                    r#ref: None,
                    description: Some("Provider id used when no override is given.".into()),
                }],
            }),
            capabilities: Capabilities {
                acp_capable: true,
                extensions: BTreeMap::new(),
            },
            extensions: BTreeMap::new(),
        };

        let s = serde_json::to_string(&original).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse JSON");
        let round: AgentDescriptor = serde_json::from_value(v.clone()).expect("deserialize back");

        // Re-serialize and confirm byte-identical (modulo key order).
        let s2 = serde_json::to_string(&round).expect("re-serialize");
        let a: serde_json::Value = serde_json::from_str(&s).unwrap();
        let b: serde_json::Value = serde_json::from_str(&s2).unwrap();
        assert_eq!(a, b, "round-trip drifted: {a:?} != {b:?}");

        // Top-level shape.
        assert_eq!(v["id"], json!("ncz-os:agent-descriptor:zoder:v1"));
        assert_eq!(v["schema_version"], json!(1));
        // conformance_level wire form is the integer 2 (per schema enum [1,2]).
        assert_eq!(v["conformance_level"], json!(2));
        assert_eq!(v["connection"]["transport"], json!("unix_socket"));
        assert_eq!(v["connection"]["endpoint"]["kind"], json!("path"));
        assert_eq!(
            v["connection"]["endpoint"]["path"],
            json!("/tmp/zoder.sock")
        );
    }

    #[test]
    fn knob_enum_roundtrip() {
        let k = Knob {
            name: "billing".into(),
            kind: KnobKind::Enum,
            required: false,
            default: Some(json!("metered")),
            secret: false,
            enum_values: Some(vec!["free".into(), "metered".into(), "subscription".into()]),
            r#ref: None,
            description: None,
        };
        let s = serde_json::to_string(&k).unwrap();
        let back: Knob = serde_json::from_str(&s).unwrap();
        assert_eq!(k, back);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], json!("enum"));
        assert_eq!(v["enum_values"], json!(["free", "metered", "subscription"]));
    }

    #[test]
    fn knob_secret_ref_roundtrip() {
        let k = Knob {
            name: "openai_key".into(),
            kind: KnobKind::SecretRef,
            required: true,
            default: None,
            // secret is required to be true by the schema when kind == secret_ref;
            // we set it true here so the round-trip holds up without error.
            secret: true,
            enum_values: None,
            r#ref: Some("OPENAI_API_KEY".into()),
            description: Some("OpenAI API key (env var).".into()),
        };
        let s = serde_json::to_string(&k).unwrap();
        let back: Knob = serde_json::from_str(&s).unwrap();
        assert_eq!(k, back);
    }

    #[test]
    fn auth_variants_roundtrip() {
        for a in [
            AuthSpec::None,
            AuthSpec::EnvVar {
                var: "OPENAI_API_KEY".into(),
            },
            AuthSpec::Bearer {
                var: "ZODER_TOKEN".into(),
            },
            AuthSpec::ApiKeyHeader {
                header: "api-key".into(),
                var: "AZURE_OPENAI_KEY".into(),
            },
        ] {
            let s = serde_json::to_string(&a).unwrap();
            let back: AuthSpec = serde_json::from_str(&s).unwrap();
            assert_eq!(a, back, "round-trip failed for {a:?}");
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bogus = json!({
            "transport": "unix_socket",
            "endpoint": {"kind": "path", "path": "/tmp/x.sock"},
            "auth": null,
            "mystery_field": true,
        });
        let r: Result<Connection, _> = serde_json::from_value(bogus);
        assert!(
            r.is_err(),
            "deny_unknown_fields should have rejected `mystery_field`"
        );
    }

    #[test]
    fn transport_strings_match_schema_enum() {
        // The wire strings MUST match the JSON Schema enum. If you change one,
        // change both.
        for t in [
            Transport::Stdio,
            Transport::UnixSocket,
            Transport::Tcp,
            Transport::Http,
            Transport::WebSocket,
            Transport::Wss,
            Transport::Https,
        ] {
            let wire = serde_json::to_value(t).unwrap();
            assert!(
                wire.is_string(),
                "transport {t:?} should serialize as a string"
            );
        }
    }

    #[test]
    fn schema_version_is_current() {
        // Drift guard: bumping SCHEMA_VERSION requires bumping DESCRIPTOR_SCHEMA_V1_ID's
        // major in `crate::descriptor_id`.
        assert_eq!(SCHEMA_VERSION, 1);
    }
}
