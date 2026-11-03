# PLAN-FB130

Task: zoder `crates/agent-descriptor` schema validation + round-trip tests.
Branch `freebuff/FB130-20261103` (base origin/master). Base crate state: unit tests in
each src module + `tests/integration.rs` (checked-in descriptors, C4-AD1/AD2, duplicate
knob names). Only tests added; no production change expected.

## New file
`crates/agent-descriptor/tests/descriptor_corpus_20261103.rs` (~400 lines):

1. **Corpus builder**: `full_l2()` returns a maximal valid descriptor Value; mutators
   build invalid variants for the reject corpus (schema facts pinned from
   `schema/agent-descriptor.v1.schema.json`: id pattern, name/agent_id minLength +
   agent_id lowercase pattern, conformance enum [1,2], connection required
   transport/endpoint/auth, endpoint oneOf, auth var pattern `^[A-Z_][A-Z0-9_]*$`,
   knob name pattern, if/then: kind enum -> enum_values, kind secret_ref -> ref +
   secret==true, config_surface required source/schema/knobs).
2. **Accept corpus**: minimal L1; full L2 with every optional field (default_path,
   knob default/description, enum knob, secret_ref knob, descriptor extensions map,
   capabilities.extensions non-empty). Each: validate_v1 ok + validate_and_parse ok.
3. **Reject corpus** (validate_v1 AND validate_and_parse both reject; error message
   or instance path names the offending field): empty/bad-pattern id, empty name,
   uppercase agent_id, conformance 3, unknown transport/source/knob kind, endpoint
   bad kind + port 65536 + empty path, auth lowercase var + empty header, knob enum
   missing enum_values, secret_ref missing ref / secret=false, unknown field at top
   / connection / knob level (deny_unknown_fields).
4. **Divergence pin**: serde alone maps conformance_level 7 -> L1 (documented From<u8>
   fallback); validate_and_parse rejects. Both directions asserted.
5. **Round-trip grid**: transports x matching endpoints x all 4 auth kinds x all knob
   kinds x optional-fields present/absent -> parse(serialize(x)) == x (Value equality).
6. **No-panic cases**: duplicate JSON key (serde last-wins pin), 20k-knob list
   (round-trip + validate), knob name unicode, capability extension key unicode.
7. Regression: none expected (types are pure serde); any panic found becomes an
   xfail-style pinned test reported in the summary.

## Risks
- jsonschema instance-path wording differs per assertion → assert on
  message-contains-field-name (serde) or path/message contains field name (schema),
  verified against actual output in the first run.
- Big workspace build on first `cargo test -p agent-descriptor` (deps incl. acp-client).

## Verification
`cargo test -p agent-descriptor` only (brief: crate-scoped; one full workspace run NOT
required here — FB130 names only `-p agent-descriptor`). Then zoder structured review,
push, ls-remote.
