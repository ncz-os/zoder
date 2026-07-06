//! Engine-config MCP/extension server parsing.
//!
//! Reads the engine's `config.toml` and extracts the configured MCP servers
//! (a.k.a. goose extensions) into a typed [`McpServerSpec`] list. This is
//! the *first half* of MCP/session interoperability work — it surfaces
//! what the user has configured so a future slice can hand the same specs
//! to the goose ACP `session/new` call. The runtime/session path is NOT
//! changed here.
//!
//! ## Recognized section shapes
//!
//! The current `mcp list` heading-scanner (which this slice replaces) accepts:
//!
//!   * `[mcp_servers.<name>]`       — per-server table, fields directly inside
//!   * `[[mcp]]`                    — array-of-tables; each table is a server
//!   * `[extensions.<name>]`        — goose legacy "extension" form
//!
//! Each per-server table may declare a transport. The parser recognizes:
//!
//!   * stdio: has `command` (or `cmd`) — transports `command` + `args` + `env`
//!   * http:  has `url` (or `uri`)     — transports `url` + optional headers
//!
//! Anything else is captured as `Unknown` so the listing can still surface it
//! (the user gets to see what they wrote) without crashing.
//!
//! ## Tolerant parsing
//!
//! Missing optional fields stay `None` / empty. Malformed individual entries
//! are skipped with a warning rather than aborting the whole parse — the
//! engine config may legitimately contain other unrelated tables that the
//! MCP parser should ignore.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// How a server is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    /// Spawn a child process; talk JSON-RPC over its stdin/stdout.
    Stdio,
    /// Connect to a streamable HTTP / plain-HTTP MCP endpoint.
    Http,
    /// Section was found but didn't declare a recognizable transport
    /// (neither `command`/`cmd` nor `url`/`uri`). The raw fields are
    /// preserved so the listing can still display the entry.
    Unknown,
}

/// A single MCP/extension server entry parsed from the engine config.
///
/// Field set is deliberately minimal: name, transport, and the fields
/// that *actually* appear in the engine config today (`command`/`cmd`,
/// `args`, `env`, `url`/`uri`, `enabled`). We do not invent fields that
/// the schema doesn't carry; the follow-up slice that hands these to
/// goose `session/new` will extend this only if the wire format demands
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    /// Logical name of the server (the heading suffix for
    /// `[mcp_servers.foo]` / `[extensions.foo]`, or the `name =` field
    /// inside `[[mcp]]` tables).
    pub name: String,
    /// How to reach the server.
    pub transport: McpTransportKind,
    /// Command to spawn for stdio. `Some` iff `transport == Stdio`.
    pub command: Option<String>,
    /// Args to pass to `command` for stdio. Empty when absent.
    pub args: Vec<String>,
    /// Environment variables to set on the spawned child for stdio.
    /// Empty when absent.
    pub env: BTreeMap<String, String>,
    /// Endpoint URL for HTTP. `Some` iff `transport == Http`.
    pub url: Option<String>,
    /// Optional HTTP headers to send (e.g. `Authorization`). Empty when absent.
    pub headers: BTreeMap<String, String>,
    /// `enabled` flag if the config expresses one. `None` means "not
    /// declared" — callers should treat that as enabled by default, since
    /// presence under `[mcp_servers.<name>]` already implies intent.
    pub enabled: Option<bool>,
    /// Which heading form the server came from. Useful for the listing
    /// and for diagnostics; not used by the wire format itself.
    pub source: McpSource,
}

/// Which heading form a server was parsed from. The parser normalizes
/// all of them into [`McpServerSpec`]; this tag is kept so the listing
/// and follow-up diagnostics can surface "this was under
/// `[extensions.foo]`, which is the legacy form" without re-scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSource {
    /// `[mcp_servers.<name>]`
    McpServersTable,
    /// `[[mcp]]` array-of-tables
    McpArray,
    /// `[extensions.<name>]`
    ExtensionsTable,
}

/// Raw view of one per-server table. Captures every field the parser
/// actually reads, with `serde(default)` so missing ones stay empty.
#[derive(Debug, Default, Deserialize)]
struct RawServerEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Top-level TOML shape: a struct of named tables plus an optional
/// `[[mcp]]` array. `toml` allows extra unknown tables without
/// erroring, so unrelated sections in the engine config (provider
/// overlays, `[profile]`, …) parse cleanly.
#[derive(Debug, Default, Deserialize)]
struct EngineConfigFile {
    #[serde(default)]
    mcp_servers: toml::Table,
    #[serde(default)]
    extensions: toml::Table,
    #[serde(default)]
    mcp: Vec<toml::Table>,
}

/// Parse the raw TOML text of an engine config into a vector of
/// server specs.
///
/// Tolerant by design:
///   * Sections the parser doesn't recognize are ignored.
///   * Individual entries that fail to deserialize are skipped with
///     a warning logged via `tracing` (NOT propagated as an error).
///   * Missing optional fields stay `None` / empty.
///
/// Returns an empty vector for an empty file or a file with no MCP
/// sections.
pub fn parse_mcp_servers_config(raw: &str) -> anyhow::Result<Vec<McpServerSpec>> {
    let parsed: EngineConfigFile = match toml::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            // Whole-file parse failure: engine config may contain
            // unsupported tables or non-TOML comments. Surface the
            // error so the caller can decide (list command prints a
            // clear message; tests can assert the error path).
            return Err(anyhow::anyhow!("failed to parse engine config: {e}"));
        }
    };

    let mut out: Vec<McpServerSpec> = Vec::new();

    // [mcp_servers.<name>]
    for (name, table) in &parsed.mcp_servers {
        if let Some(entry) = table.as_table() {
            match build_spec(name, entry, McpSource::McpServersTable) {
                Ok(spec) => out.push(spec),
                Err(e) => tracing::warn!(
                    server = %name,
                    error = %e,
                    "skipping malformed [mcp_servers.{name}] entry",
                ),
            }
        }
    }

    // [extensions.<name>]
    for (name, table) in &parsed.extensions {
        if let Some(table) = table.as_table() {
            // Skip tables that look like unrelated extension metadata
            // rather than server entries (no `command`/`cmd`/`url`/`uri`
            // and no `type`). We only do this when the table is
            // completely empty of transport hints; if the user put
            // something there, surface it.
            match build_spec(name, table, McpSource::ExtensionsTable) {
                Ok(spec) => out.push(spec),
                Err(_) => {
                    // Table that isn't a server entry at all (e.g. a
                    // nested unrelated config). Silent skip — the old
                    // scanner also listed these as raw heading text,
                    // which was misleading; surfacing only real server
                    // entries is the documented improvement.
                }
            }
        }
    }

    // [[mcp]] (array of tables). Each table may carry its own `name`
    // field; if absent, fall back to the array index so the listing
    // still shows something distinguishable.
    for (idx, table) in parsed.mcp.iter().enumerate() {
        let inline_name = table
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let name = inline_name.unwrap_or_else(|| format!("mcp_{idx}"));
        match build_spec(&name, table, McpSource::McpArray) {
            Ok(spec) => out.push(spec),
            Err(e) => tracing::warn!(
                server = %name,
                error = %e,
                "skipping malformed [[mcp]] entry",
            ),
        }
    }

    Ok(out)
}

/// Read the engine config from `path` and return its parsed MCP
/// server specs. Returns an empty vector when the file is missing
/// — the caller (typically `mcp list`) treats "no config" as
/// "nothing configured".
pub fn parse_mcp_servers_file(path: &Path) -> anyhow::Result<Vec<McpServerSpec>> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    parse_mcp_servers_config(&raw)
}

/// Build a [`McpServerSpec`] from a raw per-server table. Returns
/// an error if the table can't be coerced into the recognized
/// shape (used to skip non-server entries silently for the
/// `[extensions.*]` case).
fn build_spec(
    name: &str,
    table: &toml::Table,
    source: McpSource,
) -> Result<McpServerSpec, anyhow::Error> {
    let raw: RawServerEntry = table
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("not a server-shaped table: {e}"))?;

    let transport = pick_transport(&raw);

    // Reject entries that have NO transport hint at all when they
    // aren't tagged with `type = "..."` either — these are clearly
    // not server entries (e.g. an unrelated nested table under
    // `[extensions]`).
    if transport == McpTransportKind::Unknown
        && raw.r#type.is_none()
        && raw.command.is_none()
        && raw.cmd.is_none()
        && raw.url.is_none()
        && raw.uri.is_none()
    {
        return Err(anyhow::anyhow!("entry has no transport hint"));
    }

    let command = raw.command.or(raw.cmd);
    let url = raw.url.or(raw.uri);
    let headers = if raw.headers.is_empty() {
        raw.http_headers
    } else {
        raw.headers
    };

    Ok(McpServerSpec {
        name: raw.name.unwrap_or_else(|| name.to_string()),
        transport,
        command,
        args: raw.args,
        env: raw.env,
        url,
        headers,
        enabled: raw.enabled,
        source,
    })
}

/// Pick the transport kind for one entry.
///
/// Precedence:
///   * `type = "stdio"` (explicit)        → stdio
///   * `type = "http"|"streamable_http"`  → http
///   * `command`/`cmd` present            → stdio
///   * `url`/`uri` present                → http
///   * anything else                      → unknown
fn pick_transport(raw: &RawServerEntry) -> McpTransportKind {
    if let Some(t) = raw.r#type.as_deref() {
        match t.to_ascii_lowercase().as_str() {
            "stdio" => return McpTransportKind::Stdio,
            "http" | "streamable_http" | "streamable-http" => return McpTransportKind::Http,
            _ => {} // fall through to field-based detection
        }
    }
    if raw.command.is_some() || raw.cmd.is_some() {
        return McpTransportKind::Stdio;
    }
    if raw.url.is_some() || raw.uri.is_some() {
        return McpTransportKind::Http;
    }
    McpTransportKind::Unknown
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Both flavors of a stdio server: `[mcp_servers.foo]` and
    /// `[[mcp]]` with a `name` field. One http server. One
    /// `[extensions.bar]` legacy form. One `enabled = false`
    /// server. Tolerant of all three heading shapes.
    #[test]
    fn parses_stdio_http_and_extensions_table_forms() {
        let raw = r#"
# [mcp_servers.<name>] — per-server table.
[mcp_servers.lookup]
command = "node"
args = ["server.js"]
env = { API_KEY = "secret" }

# [[mcp]] — array of tables with inline name.
[[mcp]]
name = "kiwi"
url = "https://mcp.kiwi.com"

# [extensions.<name>] — legacy goose form. Same fields.
[extensions.bitbucket]
type = "stdio"
cmd = "/usr/local/bin/bitbucket-mcp"
args = ["--stdio"]

# http transport, tagged via `type`.
[mcp_servers.github]
type = "streamable_http"
url = "https://api.githubcopilot.com/mcp/"
headers = { Authorization = "Bearer TOKEN" }

# explicitly disabled.
[mcp_servers.disabled_one]
command = "noop"
enabled = false
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert_eq!(specs.len(), 5, "got specs: {specs:#?}");

        // lookup — stdio, command + args + env.
        let lookup = specs
            .iter()
            .find(|s| s.name == "lookup")
            .expect("lookup present");
        assert_eq!(lookup.transport, McpTransportKind::Stdio);
        assert_eq!(lookup.command.as_deref(), Some("node"));
        assert_eq!(lookup.args, vec!["server.js".to_string()]);
        assert_eq!(
            lookup.env.get("API_KEY").map(String::as_str),
            Some("secret")
        );
        assert!(lookup.url.is_none());
        assert_eq!(lookup.source, McpSource::McpServersTable);
        assert_eq!(lookup.enabled, None);

        // kiwi — http, from [[mcp]].
        let kiwi = specs
            .iter()
            .find(|s| s.name == "kiwi")
            .expect("kiwi present");
        assert_eq!(kiwi.transport, McpTransportKind::Http);
        assert_eq!(kiwi.url.as_deref(), Some("https://mcp.kiwi.com"));
        assert!(kiwi.command.is_none());
        assert_eq!(kiwi.source, McpSource::McpArray);

        // bitbucket — stdio, from [extensions.*], uses `cmd`.
        let bitbucket = specs
            .iter()
            .find(|s| s.name == "bitbucket")
            .expect("bitbucket present");
        assert_eq!(bitbucket.transport, McpTransportKind::Stdio);
        assert_eq!(
            bitbucket.command.as_deref(),
            Some("/usr/local/bin/bitbucket-mcp")
        );
        assert_eq!(bitbucket.args, vec!["--stdio".to_string()]);
        assert_eq!(bitbucket.source, McpSource::ExtensionsTable);

        // github — http, explicit `type = "streamable_http"`.
        let github = specs
            .iter()
            .find(|s| s.name == "github")
            .expect("github present");
        assert_eq!(github.transport, McpTransportKind::Http);
        assert_eq!(
            github.url.as_deref(),
            Some("https://api.githubcopilot.com/mcp/")
        );
        assert_eq!(
            github.headers.get("Authorization").map(String::as_str),
            Some("Bearer TOKEN")
        );

        // disabled_one — enabled flag captured.
        let disabled = specs
            .iter()
            .find(|s| s.name == "disabled_one")
            .expect("disabled_one present");
        assert_eq!(disabled.enabled, Some(false));
    }

    /// The exact test required by the slice spec: one stdio + one
    /// http, yielding the correct typed specs.
    #[test]
    fn parses_one_stdio_one_http() {
        let raw = r#"
[mcp_servers.lookup]
command = "node"
args = ["server.js"]

[mcp_servers.mcp_kiwi_com]
url = "https://mcp.kiwi.com"
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert_eq!(specs.len(), 2);

        let lookup = &specs[0];
        assert_eq!(lookup.name, "lookup");
        assert_eq!(lookup.transport, McpTransportKind::Stdio);
        assert_eq!(lookup.command.as_deref(), Some("node"));
        assert_eq!(lookup.args, vec!["server.js".to_string()]);
        assert!(lookup.url.is_none());

        let kiwi = &specs[1];
        assert_eq!(kiwi.name, "mcp_kiwi_com");
        assert_eq!(kiwi.transport, McpTransportKind::Http);
        assert_eq!(kiwi.url.as_deref(), Some("https://mcp.kiwi.com"));
        assert!(kiwi.command.is_none());
    }

    /// No MCP tables at all → empty vector. The `mcp list`
    /// command treats this as "none configured" and shows the
    /// hint about adding under `[mcp_servers.<name>]`.
    #[test]
    fn parses_empty_config() {
        let raw = r#"
# Engine config with provider overlays etc., but no MCP tables.
[profile]
primary_model = "openai/gpt-4o"
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert!(specs.is_empty());
    }

    /// A truly empty config string also yields empty.
    #[test]
    fn parses_zero_byte_config() {
        let specs = parse_mcp_servers_config("").expect("parse");
        assert!(specs.is_empty());
    }

    /// `[[mcp]]` array-of-tables: each entry carries its own
    /// `name`, no heading provides one.
    #[test]
    fn parses_mcp_array_with_inline_names() {
        let raw = r#"
[[mcp]]
name = "alpha"
command = "a-bin"
args = ["--x"]

[[mcp]]
name = "beta"
url = "https://beta.example/mcp"
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "alpha");
        assert_eq!(specs[0].transport, McpTransportKind::Stdio);
        assert_eq!(specs[1].name, "beta");
        assert_eq!(specs[1].transport, McpTransportKind::Http);
    }

    /// `[[mcp]]` without a `name` falls back to the array index
    /// so the listing still distinguishes them.
    #[test]
    fn parses_mcp_array_without_names_uses_indices() {
        let raw = r#"
[[mcp]]
command = "a"

[[mcp]]
url = "https://b.example"
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "mcp_0");
        assert_eq!(specs[0].transport, McpTransportKind::Stdio);
        assert_eq!(specs[1].name, "mcp_1");
        assert_eq!(specs[1].transport, McpTransportKind::Http);
    }

    /// A non-MCP table under `[extensions]` (e.g. `[extensions.theme]`)
    /// is silently skipped — it has no transport hint and no `type`.
    #[test]
    fn skips_unrelated_tables_under_extensions() {
        let raw = r#"
[extensions.theme]
color = "blue"

[mcp_servers.real_one]
command = "real"
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "real_one");
    }

    /// Whole-file parse failure (genuinely broken TOML) surfaces
    /// as an error so callers can decide — `mcp list` prints a
    /// clear message; tests can assert.
    #[test]
    fn broken_toml_returns_error() {
        let raw = "this is not [ valid toml";
        let err = parse_mcp_servers_config(raw).expect_err("broken toml");
        assert!(err.to_string().contains("failed to parse engine config"));
    }

    /// `uri` is accepted as the HTTP endpoint, same as `url`.
    #[test]
    fn uri_alias_for_url() {
        let raw = r#"
[mcp_servers.x]
uri = "https://x.example/mcp"
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].transport, McpTransportKind::Http);
        assert_eq!(specs[0].url.as_deref(), Some("https://x.example/mcp"));
    }

    /// `http_headers` is accepted as the headers map, same as
    /// `headers`. Mirrors the field name used in the codex
    /// override generator in upstream goose.
    #[test]
    fn http_headers_alias_for_headers() {
        let raw = r#"
[mcp_servers.x]
url = "https://x.example/mcp"
http_headers = { Authorization = "Bearer T" }
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer T")
        );
    }

    /// `parse_mcp_servers_file` returns an empty vector for a
    /// missing file — the listing treats "no config" as "nothing
    /// configured" rather than as an error.
    #[test]
    fn missing_file_yields_empty_vector() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("does-not-exist.toml");
        let specs = parse_mcp_servers_file(&p).expect("missing file is not an error");
        assert!(specs.is_empty());
    }

    /// `parse_mcp_servers_file` reads from disk and returns the
    /// same thing as `parse_mcp_servers_config`.
    #[test]
    fn file_path_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[mcp_servers.lookup]
command = "node"
args = ["server.js"]
"#,
        )
        .unwrap();
        let specs = parse_mcp_servers_file(&p).expect("parse");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "lookup");
        assert_eq!(specs[0].transport, McpTransportKind::Stdio);
    }

    /// The serialized JSON form is the contract for `--json`
    /// output and for the future slice that hands these to
    /// `session/new`. Round-trip parse → JSON → re-parse must
    /// produce the same logical spec.
    #[test]
    fn serde_json_round_trip_preserves_fields() {
        let raw = r#"
[mcp_servers.lookup]
command = "node"
args = ["server.js"]
env = { K = "v" }
"#;
        let specs = parse_mcp_servers_config(raw).expect("parse");
        let json = serde_json::to_string(&specs).expect("serialize");
        let back: Vec<McpServerSpec> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(specs, back);
    }
}
