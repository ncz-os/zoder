//! Static JSON Schema for the descriptor format. MIF-style: a single
//! checked-in file with a stable `$id`, embedded via `include_str!` so
//! the runtime library is self-contained.
//!
//! The schema is the source of truth for the wire format. If you change
//! [`crate::descriptor`], change `schema/agent-descriptor.v1.schema.json`
//! in the same commit (and vice versa) and run the unit tests, which
//! parse both and assert consistency.

use serde_json::Value;

/// The checked-in v1 schema, parsed to a `Value` (lazy: parsed on first
/// use; the `OnceLock` is `Send + Sync`).
pub static V1: std::sync::OnceLock<Value> = std::sync::OnceLock::new();

/// Raw JSON string of the v1 schema. Embedded at compile time.
pub const V1_RAW: &str = include_str!("../schema/agent-descriptor.v1.schema.json");

/// Return a handle to the parsed v1 schema, parsing on first call.
pub fn v1() -> &'static Value {
    V1.get_or_init(|| serde_json::from_str(V1_RAW).expect("v1 schema file must be valid JSON"))
}

/// The canonical `$id` of the v1 schema (duplicated as
/// [`crate::DESCRIPTOR_SCHEMA_V1_ID`]; the `pub const` is the authority —
/// the file's `$id` is asserted-equal in the lib-level test).
pub const V1_ID: &str = crate::DESCRIPTOR_SCHEMA_V1_ID;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_schema_parses() {
        let s = v1();
        assert_eq!(s["$id"].as_str(), Some(V1_ID));
    }

    #[test]
    fn v1_raw_is_valid_json() {
        let _: Value = serde_json::from_str(V1_RAW).expect("V1_RAW parses");
    }
}
