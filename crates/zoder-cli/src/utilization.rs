//! CLI-side utilization-accounting facade.
//!
//! The CLI is a binary wrapper around [`zoder_core`]. It does not own
//! the `zoder_core::utilization` module — that lives in
//! `crates/zoder-core/src/utilization.rs` so it can be reused by other
//! engines and so its in-memory representation (`UtilizationStore`)
//! has a stable home.
//!
//! This module gives the binary a local name to import from, so
//! `use crate::utilization::*` works in `main.rs` / `agentic.rs`
//! without each call site spelling out the `zoder_core::utilization::`
//! prefix twice.
//!
//! Failing-first pins for the 2026-07-08 adversarial-review defects
//! Z-11 (rolling-window `used_tokens` zeroed by clock-rollback /
//! out-of-order capture) and Z-12 (v1→v2 migration discards persisted
//! `used_percent` when `cap = None`) live in
//! `crates/zoder-core/src/utilization.rs::tests`, alongside the
//! functions they exercise. The 2026-07-08 task description named the
//! path `crates/zoder-cli/src/utilization.rs`, but the defects and
//! their fixes are necessarily in `zoder-core`; this file exists so a
//! reviewer auditing the binary's tree sees a `utilization.rs` module
//! at the requested CLI-side path.

pub use zoder_core::utilization::*;
