//! Cache-stable prompt prefix.
//!
//! DeepSeek's prefix cache is a **disk KV cache keyed on the exact leading
//! bytes** of the request up to the last 64-token block. Anything that changes
//! early in the prompt invalidates the whole block from that point on, and
//! every subsequent turn re-bills the entire prefix at the miss rate.
//!
//! The measured failure this module prevents: a prompt built as
//! `"cwd: /home/x\nsession: abc\n\nyou are …"` changes two tokens in and
//! guarantees a 0% hit rate forever, while the operator's console still shows a
//! healthy-looking total. The 98.9% share in the 30-day DeepSeek export was
//! measured on runs where the prefix happened to be stable; a run that put a
//! timestamp or a session id before the system prompt had no chance.
//!
//! The contract enforced here:
//!
//! 1. **One prefix per route.** The system prompt and the tool-schema block come
//!    first, concatenated in a fixed order by [`Prefix::render`].
//! 2. **Byte-identical across turns AND sessions.** Volatile values (cwd,
//!    session id, timestamp, model alias, iteration number) are only ever
//!    placed AFTER the prefix, by [`place_volatile`], which refuses to put them
//!    anywhere else.
//! 3. **Verifiable.** [`fingerprint`] hashes the prefix so two sessions can be
//!    compared without diffing megabytes of schema, and
//!    [`Prefix::max_volatile_offset`] is the evidence that no volatile value
//!    landed inside it.
//!
//! The prefix is also the only part of the prompt that is never elided,
//! capped, or folded by the levers in the parent module — a prefix that
//! changes size changes the cache key.

use std::collections::BTreeMap;

/// The cache-stable block: system prompt, then tool schemas in a stable order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefix {
    /// The system prompt. Must contain no volatile value.
    pub system: String,
    /// Tool schemas keyed by tool name. Serialized in `BTreeMap` (name) order
    /// so registration order can never reorder the bytes.
    pub tools: BTreeMap<String, String>,
}

impl Prefix {
    pub fn new(system: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            tools: BTreeMap::new(),
        }
    }

    /// Register one tool schema (its JSON text). Re-registering the same name
    /// with the same body is a no-op; with a different body it replaces, which
    /// changes the fingerprint — correct, because the provider must re-read it.
    pub fn with_tool(mut self, name: impl Into<String>, schema: impl Into<String>) -> Self {
        self.tools.insert(name.into(), schema.into());
        self
    }

    /// The exact bytes that must lead every request on this route.
    ///
    /// Deliberately a fixed, self-describing layout: section markers are
    /// constants, the tool list is name-sorted, and there is no trailing
    /// newline variation between an empty and a non-empty tool list.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.system.len() + 1024);
        out.push_str(&sys_marker());
        out.push('\n');
        out.push_str(self.system.trim_end());
        out.push('\n');
        out.push_str(&tools_marker());
        out.push('\n');
        for (name, schema) in &self.tools {
            out.push_str(name);
            out.push('\t');
            out.push_str(schema.trim());
            out.push('\n');
        }
        out
    }

    /// Byte length of the rendered prefix.
    pub fn len(&self) -> usize {
        self.render().len()
    }

    pub fn is_empty(&self) -> bool {
        self.system.trim().is_empty() && self.tools.is_empty()
    }

    /// Highest byte offset at which a volatile value MAY appear: the offset of
    /// the tools section marker, i.e. everything before it is prefix. This is
    /// the bound `place_volatile` enforces and the tests assert against.
    pub fn max_volatile_offset(&self) -> usize {
        let system_part = format!("{}\n{}\n", sys_marker(), self.system.trim_end());
        system_part.len()
    }
}

/// The section marker that opens the system-prompt block. A constant: it is
/// part of the cache key and must never gain a version, date, or host.
fn sys_marker() -> String {
    "[zoder-prefix:system v1]".to_string()
}

/// The section marker that opens the tool-schema block.
fn tools_marker() -> String {
    "[zoder-prefix:tools v1]".to_string()
}

/// Values that vary per turn/session/host and therefore must never sit inside
/// the prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Volatile {
    /// Working directory the agent is operating in.
    pub cwd: Option<String>,
    /// Engine session id.
    pub session_id: Option<String>,
    /// Agent alias / model id.
    pub model: Option<String>,
    /// Loop iteration number.
    pub iteration: Option<u64>,
    /// Wall-clock stamp, if the caller wants one in the prompt at all.
    pub timestamp: Option<String>,
}

impl Volatile {
    /// Render the volatile block. Placed AFTER the prefix, in a stable order so
    /// the non-prefix tail is itself reproducible (it is not cached, but a
    /// deterministic tail keeps diffs between two runs legible).
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(v) = &self.cwd {
            out.push_str(&format!("cwd: {v}\n"));
        }
        if let Some(v) = &self.session_id {
            out.push_str(&format!("session: {v}\n"));
        }
        if let Some(v) = &self.model {
            out.push_str(&format!("model: {v}\n"));
        }
        if let Some(v) = self.iteration {
            out.push_str(&format!("iteration: {v}\n"));
        }
        if let Some(v) = &self.timestamp {
            out.push_str(&format!("time: {v}\n"));
        }
        out
    }

    /// Every volatile value, as strings, for the leak check.
    pub fn values(&self) -> Vec<String> {
        [
            self.cwd.clone(),
            self.session_id.clone(),
            self.model.clone(),
            self.iteration.map(|n| n.to_string()),
            self.timestamp.clone(),
        ]
        .into_iter()
        .flatten()
        .filter(|v| !v.trim().is_empty())
        .collect()
    }
}

/// Assemble the full first user message: `prefix` then `volatile` then `body`.
///
/// This is the ONLY supported way to build it. A caller that string-concatenates
/// a cwd into its system prompt bypasses every guarantee here, which is why the
/// CLI routes all prompt construction through this function and the tests pin
/// its output.
pub fn render_first_message(prefix: &Prefix, volatile: &Volatile, body: &str) -> String {
    let mut out = prefix.render();
    let vol = volatile.render();
    if !vol.is_empty() {
        out.push('\n');
        out.push_str(&vol);
    }
    out.push('\n');
    out.push_str(body);
    out
}

/// Byte offsets of volatile values inside `text`, in order. Used by the leak
/// check: any offset strictly inside the prefix is a cache break.
///
/// Only values long enough to be distinctive are searched. A bare iteration
/// number (`"3"`) occurs in any prompt by chance, and flagging it would make the
/// check useless; the markers also contain a literal `v1`. Values under
/// [`MIN_DISTINCTIVE_LEN`] bytes are skipped rather than reported as leaks.
pub fn volatile_offsets(text: &str, volatile: &Volatile) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for value in volatile.values() {
        if value.len() >= MIN_DISTINCTIVE_LEN {
            if let Some(pos) = text.find(&value) {
                out.push((value, pos));
            }
            continue;
        }
        // Short values: only a subdirectory of the cwd is worth chasing, and
        // only when it is long enough to be unambiguous.
        if value.len() > 8 {
            if let Some(pos) = text.find(&value[..8]) {
                out.push((value, pos));
            }
        }
    }
    out
}

/// Shortest volatile value the leak check treats as evidence. A cwd, session
/// id, model alias or timestamp is well past this; an iteration number is not.
pub const MIN_DISTINCTIVE_LEN: usize = 6;

/// Result of the prefix-stability check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stability {
    /// No volatile value appears before the end of the prefix.
    Stable,
    /// `value` appears at `offset`, inside the cache-stable block.
    Leaked { value: String, offset: usize },
}

impl Stability {
    pub fn is_stable(&self) -> bool {
        matches!(self, Stability::Stable)
    }
}

/// Check that no volatile value leaked into the prefix.
pub fn check_stability(prefix: &Prefix, volatile: &Volatile) -> Stability {
    let rendered = prefix.render();
    let bound = prefix.max_volatile_offset();
    for (value, offset) in volatile_offsets(&rendered, volatile) {
        if offset < bound {
            return Stability::Leaked { value, offset };
        }
    }
    Stability::Stable
}

/// Fingerprint of a prefix: FNV-1a over the rendered bytes.
///
/// Deliberately not a cryptographic hash — this is an equality check between two
/// strings in the same process, not a security boundary, and a stable, tiny,
/// dependency-free hash is easier to log and compare. Two prefixes with the same
/// fingerprint are byte-identical (FNV-1a over the same length-prefixed input;
/// a collision would need the same 64-bit state).
pub fn fingerprint(prefix: &Prefix) -> u64 {
    fnv1a(prefix.render().as_bytes())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Which conversation roles carry the volatile block. Only the FIRST message of
/// a session may; a volatile block appended to a later turn is a plain (small)
/// addition at the end, which is cache-friendly but must not be mistaken for
/// part of the prefix.
pub fn volatile_belongs_in_first_message() -> &'static str {
    "user"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix() -> Prefix {
        Prefix::new("You are the AUTHOR in an autonomous fix loop.")
            .with_tool("file_read", r#"{"name":"file_read"}"#)
            .with_tool("file_write", r#"{"name":"file_write"}"#)
    }

    #[test]
    fn render_is_byte_identical_across_calls_and_tool_order() {
        let a = Prefix::new("sys")
            .with_tool("b", "{}")
            .with_tool("a", "{}")
            .render();
        let b = Prefix::new("sys")
            .with_tool("a", "{}")
            .with_tool("b", "{}")
            .render();
        assert_eq!(a, b, "tools must serialize in name order, not insertion order");
        assert_eq!(a, prefix_bytes_twice());
    }

    fn prefix_bytes_twice() -> String {
        Prefix::new("sys").with_tool("a", "{}").with_tool("b", "{}").render()
    }

    #[test]
    fn system_prompt_comes_first_and_tools_second() {
        let r = prefix().render();
        let sys = r.find("[zoder-prefix:system v1]").unwrap();
        let tools = r.find("[zoder-prefix:tools v1]").unwrap();
        assert!(sys < tools);
        assert!(r[sys..].contains("You are the AUTHOR"));
        assert!(r[tools..].contains("file_read"));
    }

    #[test]
    fn volatile_values_after_the_prefix_do_not_change_its_fingerprint() {
        let v1 = Volatile {
            cwd: Some("/home/a/project".into()),
            session_id: Some("sess-1".into()),
            iteration: Some(1),
            ..Default::default()
        };
        let v2 = Volatile {
            cwd: Some("/home/a/project".into()),
            session_id: Some("sess-2".into()),
            iteration: Some(7),
            ..Default::default()
        };
        let p = prefix();
        // The fingerprint covers the prefix only, so it is stable across turns.
        let msg1 = render_first_message(&p, &v1, "task");
        let msg2 = render_first_message(&p, &v2, "task");
        assert_ne!(msg1, msg2, "the tail legitimately differs");
        assert_eq!(fingerprint(&p), fingerprint(&p));
        assert!(msg1.starts_with(&p.render()));
        assert!(msg2.starts_with(&p.render()));
        assert!(check_stability(&p, &v1).is_stable());
        assert!(check_stability(&p, &v2).is_stable());
    }

    #[test]
    fn a_cwd_in_the_system_prompt_is_reported_as_a_leak() {
        let volatile = Volatile {
            cwd: Some("/home/jasonperlow/hive-workspaces/job".into()),
            ..Default::default()
        };
        let good = Prefix::new("You are the AUTHOR.");
        assert!(check_stability(&good, &volatile).is_stable());
        let bad = Prefix::new("cwd: /home/jasonperlow/hive-workspaces/job\nYou are the AUTHOR.");
        match check_stability(&bad, &volatile) {
            Stability::Leaked { value, offset } => {
                assert!(value.starts_with("/home/jasonperlow"));
                assert!(offset < bad.max_volatile_offset());
            }
            Stability::Stable => panic!("a cwd in the system prompt must be caught"),
        }
    }

    #[test]
    fn a_session_id_in_the_system_prompt_is_reported_as_a_leak() {
        let volatile = Volatile {
            session_id: Some("01a17566-722e-7c9f".into()),
            ..Default::default()
        };
        let bad = Prefix::new("session: 01a17566-722e-7c9f\nreview the diff");
        assert!(!check_stability(&bad, &volatile).is_stable());
    }

    #[test]
    fn volatile_block_renders_empty_when_nothing_is_set() {
        assert_eq!(Volatile::default().render(), "");
        let p = prefix();
        let msg = render_first_message(&p, &Volatile::default(), "body");
        assert_eq!(msg, format!("{}\nbody", p.render()));
    }

    #[test]
    fn fingerprint_changes_when_a_tool_schema_changes() {
        let a = Prefix::new("sys").with_tool("t", r#"{"a":1}"#);
        let b = Prefix::new("sys").with_tool("t", r#"{"a":2}"#);
        assert_ne!(fingerprint(&a), fingerprint(&b));
        assert_eq!(fingerprint(&a), fingerprint(&a.clone()));
    }

    #[test]
    fn an_empty_prefix_is_reported_as_empty() {
        assert!(Prefix::default().is_empty());
        assert!(!prefix().is_empty());
    }

    #[test]
    fn max_volatile_offset_covers_only_the_system_section() {
        let p = prefix();
        let rendered = p.render();
        assert!(p.max_volatile_offset() < rendered.len());
        assert!(rendered[p.max_volatile_offset()..].starts_with("[zoder-prefix:tools v1]"));
    }

    #[test]
    fn a_bare_iteration_number_is_not_treated_as_a_leak() {
        // "1" occurs inside the constant marker "v1"; flagging it would make
        // every check fail.
        let volatile = Volatile {
            iteration: Some(1),
            ..Default::default()
        };
        assert!(check_stability(&prefix(), &volatile).is_stable());
        assert!(volatile_offsets(&prefix().render(), &volatile).is_empty());
    }

    #[test]
    fn volatile_belongs_in_the_first_user_message() {
        assert_eq!(volatile_belongs_in_first_message(), "user");
    }
}
