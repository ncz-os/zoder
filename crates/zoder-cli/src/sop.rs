//! CLI-side facade for the `zoder sop` command family.
//!
//! The wire layer (`sops/graph` and `sops/run-overlay`) lives in
//! [`zoder_core::sop_graph`] so other engines and unit tests can reuse it
//! without taking a dependency on the CLI binary. This file is the
//! CLI-shaped view: command-family dispatch, the human / JSON renderers,
//! and the operator-facing exit-code decisions.
//!
//! Scope is deliberately narrow — this is a *monitor*, not a control plane.
//! The CLI does not start, stop, or mutate SOP runs; it asks the engine for
//! state and prints it. Anything that mutates runs goes through a different
//! command family so the read-only invariant is easy to audit.

use zoder_core::{
    fetch_sop_graph_report, render_sop_graph_human as render_sop_graph_human_core, SopGraphReport,
    SopOverlay, SopOverlayStep, SopStep,
};

/// Subcommands of `zoder sop …`. Today there is one — `graph <run-id>` —
/// but the family is structured so a future `status` / `decide` /
/// `cancel` set can extend it without breaking this command's parsing.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum SopCmd {
    /// Show the SOP step graph and current run overlay for a given run id.
    ///
    /// Calls `sops/graph` (canonical step graph) and, by default,
    /// `sops/run-overlay` (which step is active, which are done / pending /
    /// failed, routing hint) on the local zeroclaw engine. Print a clean
    /// human-readable summary, or pass `--json` for the raw wire-shape
    /// payload.
    Graph {
        /// The run id to query (e.g. `r-42`). Required.
        run_id: String,
        /// Emit the raw `SopGraphReport` as JSON instead of the human
        /// renderer. Stable wire shape for scripts.
        #[arg(long)]
        json: bool,
        /// Skip the `sops/run-overlay` round-trip — useful when the overlay
        /// RPC is failing or when the operator only wants the canonical
        /// graph. Default: include the overlay.
        #[arg(long)]
        no_overlay: bool,
    },
}

/// Drive one `zoder sop …` invocation. The dispatch in `main.rs` picks the
/// right variant; we keep the actual work in this module so `main.rs`
/// stays small and so the CLI logic is unit-testable through this fn.
///
/// Returns `Ok(())` on a clean answer (the exit code is 0); returns `Err`
/// on RPC / parse / arg failures so the binary's standard error handling
/// path applies. The CLI binary exits non-zero on `Err`.
pub async fn run_sop(cmd: SopCmd) -> anyhow::Result<()> {
    match cmd {
        SopCmd::Graph {
            run_id,
            json,
            no_overlay,
        } => cmd_sop_graph(&run_id, json, !no_overlay).await,
    }
}

/// `zoder sop graph <run-id>` implementation. Resolves the engine socket
/// the same way every other CLI command does (via
/// [`crate::engine_socket_path`]) so the operator's `$ZEROCLAW_SOCKET`
/// override is honored.
async fn cmd_sop_graph(
    run_id: &str,
    json_output: bool,
    include_overlay: bool,
) -> anyhow::Result<()> {
    if run_id.is_empty() {
        anyhow::bail!("zoder sop graph: <run-id> is required");
    }
    let socket = crate::engine_socket_path();
    if !socket.exists() {
        anyhow::bail!(
            "zoder sop graph: no zeroclaw engine is listening at {} \
             (start the engine or set $ZEROCLAW_SOCKET)",
            socket.display()
        );
    }

    let report = fetch_sop_graph_report(&socket, run_id, include_overlay).await?;

    if json_output {
        let payload = sop_graph_report_to_json(&report);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print!("{}", render_sop_graph_human_core(&report, run_id));
    }
    Ok(())
}

/// Stable JSON shape for `--json`. We intentionally do NOT just
/// `serde_json::to_string_pretty(&report)` because the `SopGraph` and
/// `SopOverlay` structs embed a forward-compat `extra` map that we'd rather
/// hoist into a structured `"extra"` field than have the top-level keys
/// leak into the root object (which would make the schema brittle to add
/// fields and would couple scripts to the engine's exact wire shape).
///
/// Pin this shape with a unit test — see `tests::json_payload_is_stable`.
pub fn sop_graph_report_to_json(report: &SopGraphReport) -> serde_json::Value {
    let graph = &report.graph;
    let steps: Vec<serde_json::Value> = graph.steps.iter().map(sop_step_to_json).collect();
    let overlay = report.overlay.as_ref().map(sop_overlay_to_json);
    serde_json::json!({
        "run_id": graph.run_id,
        "sop": graph.sop,
        "steps": steps,
        "overlay": overlay,
    })
}

fn sop_step_to_json(step: &SopStep) -> serde_json::Value {
    serde_json::json!({
        "id": step.id,
        "name": step.name,
        "kind": step.kind,
        "next": step.next,
        "extra": step.extra,
    })
}

fn sop_overlay_to_json(overlay: &SopOverlay) -> serde_json::Value {
    let steps: Vec<serde_json::Value> =
        overlay.steps.iter().map(sop_overlay_step_to_json).collect();
    serde_json::json!({
        "run_id": overlay.run_id,
        "active_step": overlay.active_step,
        "routing": overlay.routing,
        "outcome": overlay.outcome,
        "steps": steps,
        "extra": overlay.extra,
    })
}

fn sop_overlay_step_to_json(step: &SopOverlayStep) -> serde_json::Value {
    serde_json::json!({
        "id": step.id,
        "state": step.state,
        "notes": step.notes,
        "error": step.error,
        "extra": step.extra,
    })
}

#[cfg(test)]
mod tests {
    //! CLI-layer unit tests. The wire layer is exercised end-to-end in
    //! `crates/zoder-core/tests/sop_graph.rs`; these tests cover what the
    //! CLI binary actually does on top of that: argument validation, the
    //! JSON payload shape, and the rendering decision.
    use super::*;
    use clap::Parser;

    /// Mirror the `Cli` struct's `sop` field, just enough to exercise the
    /// `SopCmd` parser in isolation. We can't import the full `Cli` here
    /// without dragging every other subcommand through the test build, so
    /// we wrap `SopCmd` behind a tiny shim that uses `clap::Parser`.
    ///
    /// Note: the args shape is `["test-bin", "graph", "r-42"]` — the FIRST
    /// arg is the program name, the SECOND is the `SopCmd` variant name
    /// (`graph`), and the rest are flags / values passed to that variant.
    /// We can't model the outer `sop` shim because clap would then look up
    /// `sop` against `SopCmd`'s variants and fail.
    #[derive(Parser, Debug)]
    #[command(
        name = "zoder-sop-test",
        bin_name = "test-bin",
        disable_help_flag = true
    )]
    struct SopOnlyCli {
        #[command(subcommand)]
        cmd: SopCmd,
    }

    #[test]
    fn parse_sop_graph_minimal() {
        // We pass `["test-bin", "graph", "r-42"]` — index 0 is the program
        // name, index 1 is the `SopCmd` variant name (`graph`), and the
        // rest are flags / values passed to that variant. Using
        // `try_parse_from` lets us assert the actual clap error shape on
        // bad inputs instead of catching panics.
        match SopOnlyCli::try_parse_from(["test-bin", "graph", "r-42"])
            .expect("parse must succeed")
            .cmd
        {
            SopCmd::Graph {
                run_id,
                json,
                no_overlay,
            } => {
                assert_eq!(run_id, "r-42");
                assert!(!json);
                assert!(!no_overlay);
            }
        }
    }

    #[test]
    fn parse_sop_graph_with_flags() {
        match SopOnlyCli::try_parse_from(["test-bin", "graph", "r-9", "--json", "--no-overlay"])
            .expect("parse must succeed")
            .cmd
        {
            SopCmd::Graph {
                run_id,
                json,
                no_overlay,
            } => {
                assert_eq!(run_id, "r-9");
                assert!(json);
                assert!(no_overlay);
            }
        }
    }

    #[test]
    fn parse_sop_graph_requires_run_id() {
        // Missing required arg surfaces as a clap error — `try_parse_from`
        // returns `Err` instead of panicking, so we can assert the error
        // shape precisely.
        let err = SopOnlyCli::try_parse_from(["test-bin", "graph"])
            .expect_err("missing run_id must error");
        let msg = err.to_string();
        assert!(
            msg.contains("run_id") || msg.contains("<run_id>") || msg.contains("required"),
            "error should mention the missing arg, got: {msg}"
        );
    }

    #[test]
    fn parse_sop_unknown_subcommand_errors() {
        let err = SopOnlyCli::try_parse_from(["test-bin", "wat", "r-1"])
            .expect_err("unknown subcommand must error");
        let msg = err.to_string();
        assert!(
            msg.contains("wat") || msg.contains("unknown"),
            "error should mention the unknown subcommand, got: {msg}"
        );
    }

    #[test]
    fn json_payload_is_stable_for_minimal_report() {
        // The `--json` contract is the load-bearing schema scripts depend
        // on. Pin it here so a future refactor of the `SopGraph` struct
        // (or the `extra` flattening) can't accidentally break consumers.
        let report = SopGraphReport {
            graph: zoder_core::SopGraph {
                run_id: "r-1".into(),
                sop: "incident-response".into(),
                steps: vec![SopStep {
                    id: "triage".into(),
                    name: "Triage".into(),
                    kind: "decision".into(),
                    next: vec!["ack".into()],
                    extra: Default::default(),
                }],
                extra: Default::default(),
            },
            overlay: Some(SopOverlay {
                run_id: "r-1".into(),
                active_step: "triage".into(),
                routing: "branch:escalate".into(),
                outcome: "running".into(),
                steps: vec![SopOverlayStep {
                    id: "triage".into(),
                    state: "active".into(),
                    notes: "".into(),
                    error: "".into(),
                    extra: Default::default(),
                }],
                extra: Default::default(),
            }),
        };

        let v = sop_graph_report_to_json(&report);
        let obj = v.as_object().expect("payload must be a JSON object");
        // Top-level keys: the contract.
        let keys: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        assert!(keys.contains("run_id"));
        assert!(keys.contains("sop"));
        assert!(keys.contains("steps"));
        assert!(keys.contains("overlay"));

        // Step shape.
        let steps = obj["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        let step = steps[0].as_object().unwrap();
        let step_keys: std::collections::BTreeSet<&str> = step.keys().map(String::as_str).collect();
        assert!(step_keys.contains("id"));
        assert!(step_keys.contains("name"));
        assert!(step_keys.contains("kind"));
        assert!(step_keys.contains("next"));
        assert!(step_keys.contains("extra"));
        assert_eq!(step["id"], "triage");

        // Overlay shape.
        let overlay = obj["overlay"].as_object().unwrap();
        let overlay_keys: std::collections::BTreeSet<&str> =
            overlay.keys().map(String::as_str).collect();
        assert!(overlay_keys.contains("run_id"));
        assert!(overlay_keys.contains("active_step"));
        assert!(overlay_keys.contains("routing"));
        assert!(overlay_keys.contains("outcome"));
        assert!(overlay_keys.contains("steps"));
        assert!(overlay_keys.contains("extra"));
        assert_eq!(overlay["active_step"], "triage");
        assert_eq!(overlay["outcome"], "running");
    }

    #[test]
    fn json_payload_handles_missing_overlay() {
        let report = SopGraphReport {
            graph: zoder_core::SopGraph {
                run_id: "r-2".into(),
                sop: "".into(),
                steps: vec![],
                extra: Default::default(),
            },
            overlay: None,
        };
        let v = sop_graph_report_to_json(&report);
        // `overlay` is null (not missing) so scripts can branch on it
        // uniformly. The cost of a JSON null is negligible.
        assert!(v["overlay"].is_null(), "overlay must be null when absent");
        assert_eq!(v["run_id"], "r-2");
        assert!(v["steps"].as_array().unwrap().is_empty());
    }
}
