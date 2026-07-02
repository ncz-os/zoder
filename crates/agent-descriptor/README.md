# agent-descriptor

Vendor-neutral Agent Descriptor standard — slice 1 foundation.

See `docs/adr/0001-agent-descriptor-and-acp-posture.md`. The format
covers what ACP deliberately omits: a static, machine-readable
description of (a) **how to connect** to an agent (transport,
endpoint, auth, `acp_capable` flag) and (b) **how to configure** it
(knob list with names, types, defaults, required, secret flag).

The format is MIF-inspired: a checked-in JSON Schema with a stable
`$id` (`https://ncz-os.dev/schemas/agent-descriptor/v1.json`), serde
types that mirror it 1:1, and a checked-in artifact
(`schema/zoder.descriptor.json`) emitted for the local implementation.

## Layout

```
crates/agent-descriptor/
├── Cargo.toml
├── README.md
├── schema/
│   ├── agent-descriptor.v1.schema.json   # checked-in wire-format schema
│   └── zoder.descriptor.json             # emitted zoder-core descriptor
├── src/
│   ├── lib.rs                            # public API + version constants
│   ├── descriptor.rs                     # serde types (AgentDescriptor, Knob, …)
│   ├── schema.rs                         # embedded v1 schema accessor
│   ├── validate.rs                       # JSON-Schema validation helpers
│   └── bin/
│       └── codegen-zoder.rs              # build-time codegen (gated by
│                                         # `--features codegen`)
└── tests/
    └── integration.rs                    # round-trip + schema-validation
                                          # + checked-in artifact tests
```

## Public API at a glance

```rust
use agent_descriptor::{
    AgentDescriptor, Capabilities, ConformanceLevel, Connection, Endpoint, Knob,
    KnobKind, Transport, descriptor_id, validate::validate_v1,
};
```

Read a descriptor from disk:

```rust
let raw = std::fs::read_to_string("schema/zoder.descriptor.json")?;
let json: serde_json::Value = serde_json::from_str(&raw)?;
validate_v1(&json)?; // JSON-Schema check
let descriptor: AgentDescriptor = serde_json::from_value(json)?; // typed
```

## Build-time codegen

```bash
cargo run -p agent-descriptor --bin codegen-zoder --features codegen
```

This regenerates `schema/zoder.descriptor.json`. The artifact is
checked in; consumers do not run codegen at build time, they read
the static JSON (or the re-exported types) directly.

The codegen binary depends on `zoder-core` (gated behind the `codegen`
feature so the runtime library does not pull it). It constructs one
value of each imported type (`Auth`, `BillingMode`, `Budget`, `Config`,
`Provider`, `QuotaUnit`, `Theme`) to ensure the build breaks if the
upstream serde tags drift.

## Tests

```bash
cargo test -p agent-descriptor
# 17 unit + 7 integration tests; the integration suite re-validates
# `schema/zoder.descriptor.json` against the checked-in v1 schema.
```
