//! `zoder mcp-server` — Model Context Protocol server exposing the
//! `zoder_route` router tool and zeroclaw-backed SOP lifecycle tools over stdio.
//!
//! Usage: `zoder mcp-server` (the `mcp-server` subcommand of the zoder CLI)
//! or `zoder-mcp-server` (standalone binary). The binary speaks the MCP
//! protocol over stdio: initialize → tools/list → tools/call.

use zoder_mcp_server::{run_server, RoutingContext};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("zoder_mcp_server=info".parse()?),
        )
        .init();

    run_server(RoutingContext::new())?;
    Ok(())
}
