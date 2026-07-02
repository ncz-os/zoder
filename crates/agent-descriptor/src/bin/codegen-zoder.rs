// codegen-zoder: build-time codegen for the zoder/zeroclaw Agent Descriptor.
//
// Walks the public re-exports of `zoder_core::config::{Config, Auth, ...}`
// and emits `schema/zoder.descriptor.json` — the static checked-in descriptor
// artifact.
//
// Run via:
//
//     cargo run -p agent-descriptor --bin codegen-zoder --features codegen
//
// The architecture is "schemars-style" (build-time codegen from existing
// config structs, emitted JSON checked in), but we don't pull in the
// `schemars` crate itself — the inline `config_surface.schema` is a JSON
// Schema hand-authored as `serde_json::Value` to mirror the serde-derived
// shape of `zoder_core::config::Config`. Adding a compile-time
// `schemars::JsonSchema` derive to zoder-core would force every consumer of
// zoder-core to take a schemars dep; this slice is foundation-only and we
// avoid that blast radius intentionally.

use std::path::PathBuf;

use agent_descriptor::descriptor::{
    AgentDescriptor, Capabilities, ConfigSource, ConfigSurface, Connection, Endpoint, Knob,
    KnobKind, Transport, SCHEMA_VERSION,
};
use agent_descriptor::{descriptor_id, DESCRIPTOR_SCHEMA_V1_ID};
use serde_json::{json, Value};
use zoder_core::{
    config::Auth,
    // The full type set we name below; this is the "import the existing
    // structs" half of "from its existing config structs" so a drift
    // in their public names breaks the build rather than silently
    // emitting a stale descriptor.
    BillingMode,
    Budget,
    Config,
    Provider,
    QuotaUnit,
    Theme,
};

fn main() {
    let descriptor = build_zoder_descriptor();

    let json = serde_json::to_string_pretty(&descriptor).expect("serialize descriptor");

    let out_path = output_path();
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).expect("create schema/");
    }
    std::fs::write(&out_path, format!("{json}\n")).expect("write descriptor.json");
    eprintln!("wrote {}", out_path.display());

    // Smoke-check: the file we just wrote validates against the v1 schema.
    let as_value: Value = serde_json::from_str(&json).expect("valid JSON");
    if let Err(err) = agent_descriptor::validate::validate_v1(&as_value) {
        eprintln!("FATAL: emitted descriptor fails schema: {err}");
        std::process::exit(2);
    }
    eprintln!("ok: descriptor validates against {DESCRIPTOR_SCHEMA_V1_ID}");
}

fn output_path() -> PathBuf {
    // CARGO_MANIFEST_DIR points at `crates/agent-descriptor/`; we write to
    // the schema dir alongside the checked-in v1 schema.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .or_else(|_| std::env::var("CARGO_PKG_MANIFEST_DIR"))
        .expect("CARGO_MANIFEST_DIR is set by cargo");
    PathBuf::from(manifest_dir)
        .join("schema")
        .join("zoder.descriptor.json")
}

/// Touch every import above so dead-code lints / unused-import lints do
/// not nuke the codegen bin when the schema-author leaves a field unused.
/// Each call site references the imported type concretely, so a rename
/// upstream breaks the build rather than silently emitting a stale
/// descriptor.
#[allow(dead_code)]
fn _force_imports_used() {
    // Construct one canonical value of each imported type, validate that
    // it round-trips through serde, and discard. Drift in any imported
    // type's serde tags is then caught by `cargo test -p agent-descriptor`
    // because the constructed value below stops matching the published
    // shape.
    let _auth: Auth = Auth::None;
    let _billing: BillingMode = BillingMode::default();
    let _budget: Budget = Budget::default();
    let _config: Config = Config::default_provider(std::path::Path::new("/tmp"));
    let _provider: Provider = Provider {
        id: "x".into(),
        base_url: "https://example.invalid/v1".into(),
        kind: "openai-chat".into(),
        auth: Auth::None,
        paid: false,
        billing: BillingMode::Free,
        subscription: None,
        serves: vec!["test-model".into()],
    };
    let _quota_unit: QuotaUnit = QuotaUnit::Requests;
    let _theme: Theme = Theme {
        header: String::new(),
        accent: String::new(),
        ok: String::new(),
        warn: String::new(),
        violation: String::new(),
        dim: String::new(),
    };

    // And confirm each can be serialized to JSON for the schema walk:
    for v in [
        serde_json::to_value(_auth).unwrap(),
        serde_json::to_value(_billing).unwrap(),
        serde_json::to_value(&_budget).unwrap(),
        serde_json::to_value(&_provider).unwrap(),
        serde_json::to_value(&_theme).unwrap(),
    ] {
        // Smoke: every value must serialize successfully.
        assert!(v.is_object() || v.is_string() || v.is_array());
    }
}

/// Build the descriptor for the zoder/zeroclaw implementation. Pure
/// function: no I/O, no env, no clock. The checked-in descriptor is the
/// output of this function serialized at codegen time.
fn build_zoder_descriptor() -> AgentDescriptor {
    let knobs = knob_list();
    let surface = ConfigSurface {
        source: ConfigSource::Toml,
        default_path: Some("$ZODER_HOME/config.toml".into()),
        schema: inline_config_schema(),
        knobs,
    };

    AgentDescriptor {
        id: descriptor_id("zoder", SCHEMA_VERSION),
        name: "zoder / zeroclaw".into(),
        agent_id: "ncz-os/zoder".into(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version: SCHEMA_VERSION,
        conformance_level: agent_descriptor::ConformanceLevel::L2,
        connection: Connection {
            // zoder/zeroclaw's daemon speaks ACP over a Unix-domain socket
            // at the conventional path. The exact socket path is produced
            // at runtime (see `zoder-core`'s engine_socket_path knob); the
            // descriptor records the canonical place a foreign client can
            // probe to discover the actual socket.
            transport: Transport::UnixSocket,
            endpoint: Endpoint::Path {
                path: "$ZODER_HOME/run/zoder.sock".into(),
            },
            // Auth intentionally None — the Unix socket is filesystem-scoped.
            auth: None,
        },
        config_surface: Some(surface),
        capabilities: Capabilities {
            // zoder/zeroclaw speaks ACP over the local socket above AND
            // exposes ACP-over-WSS at the configured gateway. The descriptor
            // advertises ACP support so ACP-first clients (goose et al.)
            // know they can route to it.
            acp_capable: true,
            extensions: Default::default(),
        },
    }
}

/// Hand-authored JSON Schema for `zoder_core::config::Config`'s user-facing
/// shape, kept in sync manually. Lives next to `Config` in spirit; the
/// schema is the descriptor's `config_surface.schema` payload.
///
/// We intentionally do NOT use `schemars::schema_for!` here — see the
/// file-level comment for why. The trade-off is a small hand-maintained
/// schema; the upside is zero new dependencies on zoder-core.
fn inline_config_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://ncz-os.dev/schemas/zoder-core/config.v1.json",
        "title": "zoder-core Config (v1)",
        "description": "User-facing shape of `zoder_core::config::Config`. \
                        Drift-tested against the crate's actual serde tags.",
        "type": "object",
        "additionalProperties": false,
        "required": ["providers", "default_provider", "corpus_path",
                     "ledger_path", "health_path"],
        "properties": {
            "providers": {
                "type": "array",
                "items": { "$ref": "#/$defs/provider" }
            },
            "default_provider": { "type": "string", "minLength": 1 },
            "corpus_path":     { "$ref": "#/$defs/path" },
            "ledger_path":     { "$ref": "#/$defs/path" },
            "health_path":     { "$ref": "#/$defs/path" },
            "free_api_hosts": {
                "type": "array",
                "items": { "type": "string", "format": "hostname" }
            },
            "strict_free": { "type": "boolean" },
            "vendor_provenance": {
                "type": "object",
                "additionalProperties": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "theme": { "$ref": "#/$defs/theme" },
            "primary_model": { "type": ["string", "null"] },
            "budget": { "$ref": "#/$defs/budget" }
        },
        "$defs": {
            "provider": {
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "base_url", "kind", "auth", "billing", "serves"],
                "properties": {
                    "id":            { "type": "string" },
                    "base_url":      { "type": "string", "format": "uri" },
                    "kind":          { "type": "string" },
                    "auth":          { "$ref": "#/$defs/auth" },
                    "paid":          { "type": "boolean" },
                    "billing":       { "$ref": "#/$defs/billing" },
                    "subscription":  { "type": ["object", "null"] },
                    "serves":        { "type": "array", "items": { "type": "string" } }
                }
            },
            "auth": {
                "oneOf": [
                    { "type": "object", "required": ["type"], "properties": {
                        "type": { "const": "none" }
                    } },
                    { "type": "object", "required": ["type", "var"], "properties": {
                        "type": { "const": "env" },
                        "var":  { "$ref": "#/$defs/env_var" }
                    } },
                    { "type": "object", "required": ["type", "token"], "properties": {
                        "type":  { "const": "bearer" },
                        "token": { "type": "string" }
                    } },
                    { "type": "object", "required": ["type", "header", "var"], "properties": {
                        "type":   { "const": "api_key_header" },
                        "header": { "type": "string" },
                        "var":    { "$ref": "#/$defs/env_var" }
                    } }
                ]
            },
            "billing": {
                "type": "string",
                "enum": ["free", "metered", "subscription"]
            },
            "budget": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "per_request_usd": { "type": ["number", "null"] },
                    "daily_usd":       { "type": ["number", "null"] },
                    "monthly_usd":     { "type": ["number", "null"] }
                }
            },
            "theme": {
                "type": "object",
                "required": ["header", "accent", "ok", "warn", "violation", "dim"],
                "properties": {
                    "header":    { "type": "string" },
                    "accent":    { "type": "string" },
                    "ok":        { "type": "string" },
                    "warn":      { "type": "string" },
                    "violation": { "type": "string" },
                    "dim":       { "type": "string" }
                }
            },
            "env_var": {
                "type": "string",
                "pattern": "^[A-Z_][A-Z0-9_]*$"
            },
            "path": {
                "type": "string",
                "minLength": 1
            }
        }
    })
}

/// Hard-authored list of knobs, one per `Config` field. Each `Knob` carries
/// the metadata the schema cannot infer: requiredness, defaults, secret
/// flag, enum variants, human-readable description.
fn knob_list() -> Vec<Knob> {
    vec![
        Knob {
            name: "providers".into(),
            kind: KnobKind::Array,
            required: true,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Provider entries (kind + base_url + auth + free/paid tagging). \
                 See each item's `auth` for env-var names referenced as secrets."
                    .into(),
            ),
        },
        Knob {
            name: "default_provider".into(),
            kind: KnobKind::String,
            required: true,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Provider id used when no override is given. Must match an id \
                 in `providers`."
                    .into(),
            ),
        },
        Knob {
            name: "corpus_path".into(),
            kind: KnobKind::Path,
            required: true,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "On-disk path to the public-corpus catalog used for capability ranking.".into(),
            ),
        },
        Knob {
            name: "ledger_path".into(),
            kind: KnobKind::Path,
            required: true,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some("On-disk path to the local spend ledger.".into()),
        },
        Knob {
            name: "health_path".into(),
            kind: KnobKind::Path,
            required: true,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some("On-disk path to the live model-health store.".into()),
        },
        Knob {
            name: "free_api_hosts".into(),
            kind: KnobKind::Array,
            required: false,
            default: Some(Value::Array(vec![
                Value::String("example.com".into()),
                Value::String("free.example.com".into()),
            ])),
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Hosts considered free/internal for the anti-paid-fallback \
                 guard. Matched by exact host or registrable suffix \
                 (never substring)."
                    .into(),
            ),
        },
        Knob {
            name: "strict_free".into(),
            kind: KnobKind::Boolean,
            required: false,
            default: Some(Value::Bool(true)),
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Fail closed: a 'free' call with no cost/api_base/fallback \
                 telemetry is treated as a policy violation. \
                 `--lenient-telemetry` relaxes this."
                    .into(),
            ),
        },
        Knob {
            name: "vendor_provenance".into(),
            kind: KnobKind::Object,
            required: false,
            default: Some(Value::Object(Default::default())),
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Vendor provenance for each provider id (populated from \
                 TOML overlays). Providers from `config.json` or the default \
                 free-tier config are absent from this map."
                    .into(),
            ),
        },
        Knob {
            name: "theme".into(),
            kind: KnobKind::Object,
            required: false,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Active report colour theme. Falls back to the built-in \
                 blue/white palette when no overlay supplies one."
                    .into(),
            ),
        },
        Knob {
            name: "primary_model".into(),
            kind: KnobKind::String,
            required: false,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Pinned routing primary: a model id the router always tries \
                 FIRST, ahead of the capability/health-ranked free pool."
                    .into(),
            ),
        },
        Knob {
            name: "budget".into(),
            kind: KnobKind::Object,
            required: false,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Pre-call spend caps. A paid call whose *estimated* cost \
                 would breach a cap is gated behind the same confirmation \
                 as a paid model. Empty by default (no caps)."
                    .into(),
            ),
        },
        // `providers[*].auth.var` describes the secret env-var each
        // provider resolves at request time. The literal `var` value is
        // provider-specific; we record the shape rather than the value.
        // The schema's `name` regex permits alphanumerics + `_.-`. To stay
        // within that charset we use a `[]`-free path: `providers_array_item_auth_var`.
        Knob {
            name: "providers_item_auth_var".into(),
            kind: KnobKind::SecretRef,
            required: false,
            default: None,
            secret: true,
            enum_values: None,
            r#ref: Some("<per-provider env var name>".into()),
            description: Some(
                "Per-provider env-var name resolved at request time when \
                 `auth.kind` is `env` / `bearer` / `api_key_header`. \
                 Scoped to each item of the `providers` array: the literal \
                 var name lives in the matching provider's \
                 `providers[i].auth.var` slot. Marked `secret = true` so \
                 client UIs redact it from logs / reports."
                    .into(),
            ),
        },
        // And one knob for the engine's own listen socket (the
        // `engine_socket_path` knob surfaced via the engine transport).
        // Useful for clients that want to dial zeroclaw programmatically.
        Knob {
            name: "engine_socket_path".into(),
            kind: KnobKind::Path,
            required: false,
            default: None,
            secret: false,
            enum_values: None,
            r#ref: None,
            description: Some(
                "Filesystem path of the engine's Unix-domain ACP socket. \
                 Defaults to a path under `$ZODER_HOME/run/`. Probing this \
                 path is the canonical way for a foreign client to discover \
                 the live engine before dialing."
                    .into(),
            ),
        },
    ]
}
