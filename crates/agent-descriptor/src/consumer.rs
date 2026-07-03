//! Consumer: take a parsed [`AgentDescriptor`] and derive what is needed
//! to *drive* the engine it describes.
//!
//! This is slice 2 (Phase-1 part 3) of ADR-0001 — the second of the two
//! conformant implementers:
//!
//!   * `goose` (hand-authored `schema/goose.descriptor.json`) -> spawn
//!     `goose acp` over stdio, mirror `EngineTransport::Stdio`.
//!   * `zoder / zeroclaw` (codegen-emitted `schema/zoder.descriptor.json`)
//!     -> connect to a Unix-domain socket, mirror `EngineTransport::UnixSocket`.
//!
//! Today only those two transports exist on the engine side; a descriptor
//! that names any other transport (`wss`, `http`, `tcp`, ...) returns
//! [`Error::UnsupportedTransport`] so a foreign client can refuse early
//! rather than silently misdriving the engine.
//!
//! The mapping is deliberately trivial: the descriptor's `Connection`
//! half **already** carries the same shape the engine transport wants
//! (`transport` + `endpoint` + `auth`); [`derive_transport`] is just a
//! type-checked projection from one into the other. The value of the
//! consumer is having a single, testable seam that the CLI / other
//! callers use, instead of open-coding the projection per descriptor.

use thiserror::Error;

use crate::descriptor::{AgentDescriptor, Endpoint, Transport};
use acp_client::EngineTransport;

/// All errors that can occur while projecting a descriptor onto a driver
/// transport.
#[derive(Debug, Error)]
pub enum Error {
    /// The descriptor names a transport that this consumer cannot drive.
    /// Today: `wss`, `https`, `http`, `websocket`, `tcp`. A future
    /// engine surface that handles one of those will widen this enum;
    /// refusing the unsupported case is what keeps a static descriptor
    /// honest.
    #[error("descriptor transport `{0:?}` is not driveable by this consumer")]
    UnsupportedTransport(Transport),
    /// The descriptor names a transport that is mappable, but with an
    /// endpoint shape the consumer does not know how to project. Today
    /// only `Endpoint::Path` is supported (covers `stdio` (binary path)
    /// and `unix_socket`).
    #[error("descriptor endpoint `{0:?}` is not driveable for transport `{1:?}`")]
    UnsupportedEndpoint(Endpoint, Transport),
}

/// Project a descriptor's connection half onto a concrete [`EngineTransport`]
/// the engine side can drive.
///
/// Mapping:
/// * `transport = stdio` + `endpoint = path` (binary path) -> [`EngineTransport::Stdio`]
///   with `args = ["acp"]` and an empty `env`. The descriptor's
///   capability (`acp_capable = true`) is what makes this hardcoding
///   correct: the conventional ACP-over-stdio entrypoint is
///   `<binary> acp`. The credential/endpoint bridge (e.g. zoder's
///   `GooseProviderEnv`) is composed on top by callers; this function
///   only yields the transport *shape*.
/// * `transport = unix_socket` + `endpoint = path` -> [`EngineTransport::UnixSocket`].
/// * Anything else -> [`Error::UnsupportedTransport`] /
///   [`Error::UnsupportedEndpoint`].
pub fn derive_transport(desc: &AgentDescriptor) -> Result<EngineTransport, Error> {
    match desc.connection.transport {
        Transport::Stdio => match &desc.connection.endpoint {
            Endpoint::Path { path } => Ok(EngineTransport::Stdio {
                command: path.clone(),
                args: vec!["acp".to_string()],
                env: Vec::new(),
            }),
            other => Err(Error::UnsupportedEndpoint(other.clone(), Transport::Stdio)),
        },
        Transport::UnixSocket => match &desc.connection.endpoint {
            Endpoint::Path { path } => {
                Ok(EngineTransport::UnixSocket(std::path::PathBuf::from(path)))
            }
            other => Err(Error::UnsupportedEndpoint(
                other.clone(),
                Transport::UnixSocket,
            )),
        },
        other => Err(Error::UnsupportedTransport(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_transport_surfaces_a_clear_error() {
        // Build a minimal descriptor by hand that names `wss` — the
        // consumer must reject it (no engine transport variant maps onto it).
        let desc = AgentDescriptor {
            id: "ncz-os:agent-descriptor:probe:v1".into(),
            name: "probe".into(),
            agent_id: "ncz-os/probe".into(),
            version: "0.0.1".into(),
            schema_version: 1,
            conformance_level: crate::ConformanceLevel::L1,
            connection: crate::Connection {
                transport: Transport::Wss,
                endpoint: Endpoint::Url {
                    url: "wss://example.invalid/acp".into(),
                },
                auth: None,
            },
            config_surface: None,
            capabilities: crate::Capabilities {
                acp_capable: true,
                extensions: Default::default(),
            },
            extensions: Default::default(),
        };
        let err = derive_transport(&desc).expect_err("wss must be rejected");
        assert!(matches!(err, Error::UnsupportedTransport(Transport::Wss)));
    }
}
