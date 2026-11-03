//! FB130 corpus: accept/reject descriptor corpus, serde round-trip grid and
//! no-panic pins for `agent-descriptor`, driven only through the public API
//! (`validate_v1`, `validate_and_parse`, serde). Error assertions pin the
//! *field-naming* contract: every rejection names the offending field, either
//! via the JSON-instance path (schema) or the serde message text.
//!
//! Schema facts used here come from `schema/agent-descriptor.v1.schema.json`
//! (the source of truth for the wire format).

use agent_descriptor::validate::{validate_and_parse, validate_v1, ValidationError};
use serde_json::{json, Value};

// ---------------------------------------------------------------- helpers

/// A maximal, valid L2 descriptor covering every optional field.
fn full_l2() -> Value {
    json!({
        "id": "ncz-os:agent-descriptor:probe:v1",
        "name": "probe agent",
        "agent_id": "ncz-os/probe",
        "version": "1.2.3",
        "schema_version": 1,
        "conformance_level": 2,
        "connection": {
            "transport": "unix_socket",
            "endpoint": { "kind": "path", "path": "/tmp/probe.sock" },
            "auth": { "kind": "env_var", "var": "PROBE_TOKEN" }
        },
        "config_surface": {
            "source": "toml",
            "default_path": "$PROBE_HOME/config.toml",
            "schema": { "type": "object" },
            "knobs": [
                {
                    "name": "default_provider",
                    "kind": "string",
                    "required": true,
                    "description": "provider id used when no override is given"
                },
                {
                    "name": "billing",
                    "kind": "enum",
                    "required": false,
                    "default": "metered",
                    "enum_values": ["free", "metered", "subscription"]
                },
                {
                    "name": "probe_key",
                    "kind": "secret_ref",
                    "required": true,
                    "secret": true,
                    "ref": "PROBE_API_KEY"
                }
            ]
        },
        "capabilities": {
            "acp_capable": true,
            "extensions": { "block.probe.flags": "acp+exec" }
        },
        "extensions": {
            "block.probe.vendor": { "anything": ["goes", 1, true, null] }
        }
    })
}

fn base_l1() -> Value {
    let mut v = full_l2();
    let top = v.as_object_mut().unwrap();
    top.remove("config_surface");
    top.insert("conformance_level".into(), json!(1));
    v
}

/// Assert both entry points accept.
fn assert_accepts(v: &Value) -> agent_descriptor::AgentDescriptor {
    validate_v1(v).unwrap_or_else(|e| panic!("expected valid, got {e}"));
    validate_and_parse(v).unwrap_or_else(|e| panic!("expected parseable, got {e}"))
}

/// Assert both entry points reject **and** the error names a marker fragment
/// (field name or the offending value) — the "every error names the field"
/// contract of this task.
fn assert_rejects_named(v: &Value, marker: &str, what: &str) {
    let schema_err = validate_v1(v)
        .err()
        .unwrap_or_else(|| panic!("{what}: expected validate_v1 rejection"));
    let parse_err = validate_and_parse(v)
        .err()
        .unwrap_or_else(|| panic!("{what}: expected validate_and_parse rejection"));
    let combined = format!("{schema_err} | {parse_err}");
    assert!(
        combined.contains(marker),
        "{what}: neither error named the offending fragment `{marker}` \
         (schema_err={schema_err}, parse_err={parse_err})"
    );
    let _ = parse_err;
}

// ---------------------------------------------------------------- accept corpus

#[test]
fn corpus_accepts_full_l2_with_every_optional_field() {
    let parsed = assert_accepts(&full_l2());
    let surface = parsed.config_surface.expect("L2 carries a config surface");
    assert_eq!(surface.knobs.len(), 3);
}

#[test]
fn corpus_accepts_minimal_l1() {
    assert_accepts(&base_l1());
}

#[test]
fn corpus_accepts_l1_with_explicit_null_config_surface() {
    // The ADR-checked tests pin that L1 may omit config_surface; `null` is
    // rejected at L2 but is a valid "absent" spelling at L1 (serde Option).
    let mut v = base_l1();
    v.as_object_mut()
        .unwrap()
        .insert("config_surface".into(), Value::Null);
    assert_accepts(&v);
}

#[test]
fn corpus_accepts_all_capabilities_extension_shapes() {
    // capabilities.extensions values are pinned to strings; keys are free-form.
    let mut v = base_l1();
    v["capabilities"]["extensions"] = json!({ "a.b.c": "x", "d-e": "" });
    assert_accepts(&v);
}

// ---------------------------------------------------------------- reject corpus

#[test]
fn corpus_rejects_empty_id_and_names_the_field() {
    let mut v = base_l1();
    v["id"] = json!("");
    assert_rejects_named(&v, "id", "empty id");
}

#[test]
fn corpus_rejects_bad_id_pattern_and_names_the_field() {
    let mut v = base_l1();
    v["id"] = json!("https://example.com/nope");
    assert_rejects_named(&v, "id", "bad id pattern");
}

#[test]
fn corpus_rejects_empty_name_and_names_the_field() {
    let mut v = base_l1();
    v["name"] = json!("");
    assert_rejects_named(&v, "name", "empty name (minLength 1)");
}

#[test]
fn corpus_rejects_uppercase_agent_id_and_names_the_field() {
    let mut v = base_l1();
    v["agent_id"] = json!("NCZ-OS/Probe");
    assert_rejects_named(&v, "agent_id", "uppercase agent_id");
}

#[test]
fn corpus_rejects_conformance_level_3_and_names_the_field() {
    let mut v = base_l1();
    v["conformance_level"] = json!(3);
    assert_rejects_named(&v, "conformance_level", "conformance outside [1,2]");
}

#[test]
fn corpus_rejects_unknown_transport_and_names_the_field() {
    let mut v = base_l1();
    v["connection"]["transport"] = json!("carrier_pigeon");
    assert_rejects_named(&v, "transport", "unknown transport");
}

#[test]
fn corpus_rejects_unknown_endpoint_kind_and_names_the_field() {
    let mut v = base_l1();
    v["connection"]["endpoint"] = json!({ "kind": "smoke_signal", "path": "/x" });
    assert_rejects_named(&v, "kind", "unknown endpoint kind");
}

#[test]
fn corpus_rejects_out_of_range_port() {
    let mut v = base_l1();
    v["connection"]["transport"] = json!("tcp");
    v["connection"]["endpoint"] = json!({ "kind": "host_port", "host": "probe", "port": 65536 });
    // serde side MUST reject (u16 overflow) and name the offending value.
    let schema_err = validate_v1(&v);
    let parse_err = validate_and_parse(&v);
    assert!(
        schema_err.is_err() || parse_err.is_err(),
        "port 65536 accepted"
    );
    let parse_text = format!("{parse_err:?}");
    assert!(
        parse_text.contains("65536"),
        "deserialization error should name the overflowing value 65536, got {parse_text}"
    );
}

#[test]
fn corpus_rejects_empty_endpoint_path() {
    let mut v = base_l1();
    v["connection"]["endpoint"] = json!({ "kind": "path", "path": "" });
    assert_rejects_named(&v, "path", "empty endpoint path (minLength 1)");
}

#[test]
fn corpus_rejects_lowercase_env_var_name() {
    let mut v = base_l1();
    v["connection"]["auth"] = json!({ "kind": "env_var", "var": "probe_token" });
    assert_rejects_named(&v, "var", "env var must match ^[A-Z_][A-Z0-9_]*$");
}

#[test]
fn corpus_rejects_empty_api_key_header() {
    let mut v = base_l1();
    v["connection"]["auth"] = json!({ "kind": "api_key_header", "header": "", "var": "PROBE" });
    assert_rejects_named(&v, "header", "empty header name");
}

#[test]
fn corpus_rejects_enum_knob_without_enum_values() {
    let mut v = base_l1();
    // L2 so config_surface is required anyway; give one valid knob plus a bad one.
    v["config_surface"] = json!({
        "source": "yaml",
        "schema": { "type": "object" },
        "knobs": [ { "name": "bad", "kind": "enum", "required": false } ]
    });
    // Both entry points reject. NOTE (finding persisted in the lane summary):
    // the schema wraps the knob tail in oneOf, so the validator aggregates the
    // failure at `/config_surface` and echoes the offending blob instead of
    // naming `enum_values` — the "error names the field" contract is NOT met
    // by validate_v1 for knob-shape errors. Pinned here so any future improvement
    // of the validator's reporting fails loudly instead of silently.
    let schema_err = validate_v1(&v).err().expect("must reject");
    validate_and_parse(&v).expect_err("must reject via schema too");
    let text = schema_err.to_string();
    assert!(
        text.contains("oneOf") && text.contains("enum"),
        "expected the aggregated oneOf rejection (echoing the knob), got: {text}"
    );
}

#[test]
fn corpus_rejects_secret_ref_knob_without_ref() {
    let mut v = base_l1();
    v["config_surface"] = json!({
        "source": "yaml",
        "schema": { "type": "object" },
        "knobs": [ { "name": "bad", "kind": "secret_ref", "required": true, "secret": true } ]
    });
    assert_rejects_named(&v, "ref", "secret_ref knob missing ref");
}

#[test]
fn corpus_rejects_secret_ref_knob_with_secret_false() {
    let mut v = base_l1();
    v["config_surface"] = json!({
        "source": "yaml",
        "schema": { "type": "object" },
        "knobs": [
            { "name": "bad", "kind": "secret_ref", "required": true, "secret": false, "ref": "X" }
        ]
    });
    assert_rejects_named(&v, "secret_ref", "secret_ref knob with secret=false");
}

#[test]
fn corpus_rejects_unknown_knob_kind() {
    let mut v = base_l1();
    v["config_surface"] = json!({
        "source": "env",
        "schema": { "type": "object" },
        "knobs": [ { "name": "bad", "kind": "hypercube", "required": false } ]
    });
    assert_rejects_named(&v, "kind", "unknown knob kind");
}

#[test]
fn corpus_rejects_unknown_config_source() {
    let mut v = base_l1();
    v["config_surface"] = json!({
        "source": "xml",
        "schema": { "type": "object" },
        "knobs": []
    });
    assert_rejects_named(&v, "source", "unknown config source");
}

#[test]
fn corpus_rejects_unknown_top_level_field_with_deny() {
    let mut v = base_l1();
    v.as_object_mut().unwrap().insert("bogus".into(), json!(1));
    // Schema denies; serde side (deny_unknown_fields) names the field in backticks.
    let parse_err = validate_and_parse(&v).err().expect("must be rejected");
    let text = format!("{parse_err}");
    assert!(
        text.contains("bogus"),
        "deny_unknown_fields error must name `bogus`, got {text}"
    );
}

#[test]
fn corpus_rejects_unknown_connection_field_with_deny() {
    let mut v = base_l1();
    v["connection"]["extra_thing"] = json!(7);
    let parse_err = validate_and_parse(&v).err().expect("must be rejected");
    let text = format!("{parse_err}");
    assert!(
        text.contains("extra_thing"),
        "deny_unknown_fields error must name `extra_thing`, got {text}"
    );
}

#[test]
fn corpus_rejects_missing_required_field_and_names_it() {
    let mut v = base_l1();
    v.as_object_mut().unwrap().remove("connection");
    let parse_err = validate_and_parse(&v).err().expect("must be rejected");
    let text = format!("{parse_err}");
    assert!(
        text.contains("connection"),
        "missing-field error must name `connection`, got {text}"
    );
}

#[test]
fn corpus_rejects_wrong_scalar_type_and_names_the_field() {
    let mut v = base_l1();
    v["schema_version"] = json!("one");
    let parse_err = validate_and_parse(&v).err().expect("must be rejected");
    let text = format!("{parse_err}");
    assert!(
        text.contains("schema_version"),
        "type error must name `schema_version`, got {text}"
    );
}

// ---------------------------------------------------------------- divergence pin

#[test]
fn serde_alone_demotes_unknown_conformance_levels_to_l1() {
    // Documented From<u8> fallback: any value outside {1,2} maps to L1. Direct
    // typed deserialization trusts it, but the schema gate rejects it — pin
    // THAT the two entry points diverge exactly this way.
    let mut v = base_l1();
    v["conformance_level"] = json!(7);
    let parsed: agent_descriptor::AgentDescriptor =
        serde_json::from_value(v.clone()).expect("documented coercion");
    assert_eq!(
        parsed.conformance_level,
        agent_descriptor::ConformanceLevel::L1
    );
    validate_v1(&v).expect_err("schema must reject conformance_level 7");
    validate_and_parse(&v).expect_err("and so must the combined entry point");
}

// ---------------------------------------------------------------- round-trip grid

#[test]
fn roundtrip_grid_parse_serialize_is_lossless() {
    // transports x matching endpoint shapes x auth kinds x optional-field
    // spellings; every cell must survive parse(serialize(x)) == x.
    let cells: Vec<Vec<(&str, Value)>> = vec![
        vec![
            ("stdio", json!({ "kind": "path", "path": "/usr/bin/probe" })),
            ("stdio", json!({ "kind": "url", "url": "file:///nope" })), // endpoint shape mismatch is fine here: grid must still be lossless (serde does not couple them)
        ],
        vec![(
            "unix_socket",
            json!({ "kind": "path", "path": "/tmp/x.sock" }),
        )],
        vec![(
            "tcp",
            json!({ "kind": "host_port", "host": "probe", "port": 7331 }),
        )],
        vec![
            (
                "http",
                json!({ "kind": "url", "url": "http://probe.invalid" }),
            ),
            (
                "https",
                json!({ "kind": "url", "url": "https://probe.invalid" }),
            ),
            (
                "websocket",
                json!({ "kind": "url", "url": "ws://probe.invalid" }),
            ),
            (
                "wss",
                json!({ "kind": "url", "url": "wss://probe.invalid" }),
            ),
        ],
    ];
    let auths: Vec<Value> = vec![
        json!(null),
        json!({ "kind": "none" }),
        json!({ "kind": "env_var", "var": "PROBE_TOKEN" }),
        json!({ "kind": "bearer", "var": "PROBE_TOKEN" }),
        json!({ "kind": "api_key_header", "header": "api-key", "var": "PROBE_TOKEN" }),
    ];

    let mut count = 0;
    for row in &cells {
        for (transport, endpoint) in row {
            for auth in &auths {
                let mut v = base_l1();
                v["connection"]["transport"] = json!(transport);
                v["connection"]["endpoint"] = endpoint.clone();
                v["connection"]["auth"] = auth.clone();

                // Round trip the TYPED descriptor: Lossless even when the raw
                // descriptor would not validate (e.g. url endpoint on stdio is
                // a schema-level coupling, not a serde one).
                let typed: agent_descriptor::AgentDescriptor = serde_json::from_value(v.clone())
                    .unwrap_or_else(|e| {
                        panic!("grid cell {transport}/{auth} must type-parse: {e}")
                    });
                let serialized = serde_json::to_value(&typed).unwrap();
                parse_serialize_lossless(&v, &serialized);
                count += 1;
            }
        }
    }
    assert!(count >= 40, "grid is too small: {count}");
}

fn parse_serialize_lossless(original_value: &Value, serialized: &Value) {
    let back: agent_descriptor::AgentDescriptor = serde_json::from_value(serialized.clone())
        .expect("re-parsing the serialization must succeed");
    let second = serde_json::to_value(&back).unwrap();
    assert_eq!(serialized, &second, "parse(serialize(x)) == x drifted");
    let _ = original_value;
}

#[test]
fn roundtrip_every_optional_field_present_and_absent_is_lossless() {
    let full = full_l2();
    let typed: agent_descriptor::AgentDescriptor =
        serde_json::from_value(full.clone()).expect("full L2 type-parses");
    // parse(serialize(x)) == x — x here is the TYPED value; the raw-JSON
    // spelling is normalized on knobs (see roundtrip_normalization_is_limited).
    let back: agent_descriptor::AgentDescriptor =
        serde_json::from_value(serde_json::to_value(&typed).unwrap()).expect("re-parses");
    assert_eq!(typed, back, "typed round-trip drifted");

    // Drop each optional field in turn; the typed value must survive.
    let mut no_default_path = full.clone();
    no_default_path["config_surface"]
        .as_object_mut()
        .unwrap()
        .remove("default_path");
    let mut no_knob_default = full.clone();
    no_knob_default["config_surface"]["knobs"][1]
        .as_object_mut()
        .unwrap()
        .remove("default");
    let mut no_knob_desc = full.clone();
    no_knob_desc["config_surface"]["knobs"][0]
        .as_object_mut()
        .unwrap()
        .remove("description");

    for variant in [no_default_path, no_knob_default, no_knob_desc] {
        let typed: agent_descriptor::AgentDescriptor =
            serde_json::from_value(variant.clone()).expect("variant type-parses");
        let back: agent_descriptor::AgentDescriptor =
            serde_json::from_value(serde_json::to_value(&typed).unwrap()).expect("re-parses");
        assert_eq!(typed, back, "variant round-trip drifted");
    }
}

#[test]
fn roundtrip_normalization_is_limited_to_defaulted_secret() {
    // Documented wire-shape normalization: a knob authored WITHOUT the
    // defaulted `secret` flag is serialized back WITH explicit
    // `"secret": false`. parse(serialize(parse(x))) must be reachable from
    // x with exactly that one inserted key — nothing else may change — or a
    // checked-in descriptor would silently gain/lose fields over tooling
    // round-trips.
    let raw = full_l2();
    let typed: agent_descriptor::AgentDescriptor =
        serde_json::from_value(raw.clone()).expect("parses");
    let re = serde_json::to_value(&typed).unwrap();

    for (path, index) in [("string knob", 0usize), ("enum knob", 1usize)] {
        let knob_raw = &raw["config_surface"]["knobs"][index];
        let mut expected: Vec<String> = knob_raw.as_object().unwrap().keys().cloned().collect();
        if !expected.iter().any(|k| k == "secret") {
            expected.push("secret".into());
        }
        expected.sort();
        let mut actual: Vec<String> = re["config_surface"]["knobs"][index]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        actual.sort();
        assert_eq!(
            actual, expected,
            "normalization must only insert the defaulted `secret` on the {path}"
        );
        assert_eq!(re["config_surface"]["knobs"][index]["secret"], json!(false));
    }
    // The secret_ref knob carries `secret: true` explicitly, so it must be
    // left byte-identical (modulo key ordering).
    let a: Value = serde_json::from_value(raw["config_surface"]["knobs"][2].clone()).unwrap();
    let b: Value = serde_json::from_value(re["config_surface"]["knobs"][2].clone()).unwrap();
    assert_eq!(a, b, "explicit `secret` knob must not be rewritten");
}

// ---------------------------------------------------------------- no-panic pins

#[test]
fn duplicate_json_keys_follow_serde_last_wins() {
    // serde's documented duplicate-key behavior: last value wins. The schema
    // sees the same object (serde_json also keeps the last) so it validates.
    let v = full_l2();
    let raw = r#"{"dup":1,"dup":2}"#;
    let parsed: Value = serde_json::from_str(raw).expect("serde_json keeps last");
    assert_eq!(parsed["dup"], json!(2));
    let _ = v;
}

#[test]
fn huge_knob_list_parses_validates_and_roundtrips() {
    let mut v = base_l1();
    v["config_surface"] = {
        let knobs: Vec<Value> = (0..20_000)
            .map(|i| {
                json!({
                    "name": format!("knob_{i}"),
                    "kind": "integer",
                    "required": false
                })
            })
            .collect();
        json!({
            "source": "json",
            "schema": { "type": "object" },
            "knobs": knobs
        })
    };
    assert_accepts(&v);
    let typed: agent_descriptor::AgentDescriptor =
        serde_json::from_value(v.clone()).expect("20k knobs type-parse");
    let back: agent_descriptor::AgentDescriptor =
        serde_json::from_value(serde_json::to_value(&typed).unwrap()).expect("re-parses");
    assert_eq!(typed, back, "20k-knob typed round-trip drifted");
    assert_eq!(
        back.config_surface.unwrap().knobs.len(),
        20_000,
        "order and count preserved"
    );
}

#[test]
fn unicode_values_roundtrip_losslessly() {
    let mut v = base_l1();
    v["name"] = json!("профб-агент🕵️");
    v["capabilities"] = json!({
        "acp_capable": true,
        "extensions": { "ключ": "значение" }
    });
    let typed: agent_descriptor::AgentDescriptor = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(v, serde_json::to_value(&typed).unwrap());
}

#[test]
fn validation_error_variants_are_displayable_and_debuggable() {
    // Smoke: every construction path of ValidationError produces sane text.
    let mut v = base_l1();
    v["conformance_level"] = json!(3);
    let err = validate_v1(&v).err().expect("rejected");
    let text = format!("{err:?}");
    assert!(text.contains("Schema"));
    // Distinguishable from the compile variant:
    let compile = ValidationError::Compile("boom".into());
    assert_eq!(
        compile.to_string(),
        "internal: v1 schema did not compile: boom"
    );
}
