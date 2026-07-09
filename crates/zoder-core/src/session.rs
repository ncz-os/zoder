//! Multi-turn session persistence.
//!
//! A session is an append-only transcript of chat messages stored as a single
//! JSON file under `$ZODER_HOME/sessions/<id>.json`. `exec` can attach to a
//! session (`--session <id>`) or continue the most-recently-updated one
//! (`--continue`), so follow-up prompts carry prior context.

use crate::provider::Message;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process nonce for save() temp-file names.
///
/// Combined with the process id it makes each writer's temp file
/// (`<stem>.json.tmp.<pid>.<nonce>`) unique, so two overlapping
/// `zoder exec --session <shared-id>` fan-out writers can never compute
/// the same temp path and clobber each other into a torn transcript.
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds; bumped on every save.
    pub updated: i64,
    pub messages: Vec<Message>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn safe_id(id: &str) -> String {
    // Keep session ids filesystem-safe; never allow path traversal.
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Session {
    pub fn new(id: &str) -> Self {
        let t = now();
        Session {
            id: safe_id(id),
            created: t,
            updated: t,
            messages: Vec::new(),
        }
    }

    fn path_in(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{}.json", safe_id(id)))
    }

    /// Load a session by id, or create a fresh empty one if it does not exist.
    pub fn load_or_new(dir: &Path, id: &str) -> anyhow::Result<Self> {
        let path = Self::path_in(dir, id);
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let s: Session = serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("session {}: corrupt transcript: {e}", path.display())
            })?;
            Ok(s)
        } else {
            Ok(Session::new(id))
        }
    }

    /// Find the most-recently-updated session in `dir`, if any.
    pub fn latest(dir: &Path) -> anyhow::Result<Option<Self>> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(None);
        };
        let mut best: Option<Session> = None;
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(s) = serde_json::from_str::<Session>(&raw) else {
                continue;
            };
            if best.as_ref().map(|b| s.updated > b.updated).unwrap_or(true) {
                best = Some(s);
            }
        }
        Ok(best)
    }

    pub fn push(&mut self, role: &str, content: &str) {
        self.messages.push(Message::new(role, content));
    }

    /// Persist atomically (unique temp file + rename) under `dir`.
    ///
    /// C7-S1 (= S19 for sessions): the temp filename carries the process id AND
    /// a monotonic nonce (`<stem>.json.tmp.<pid>.<nonce>`) so two concurrent
    /// `zoder exec --session <shared-id>` fan-out writers can never share a temp
    /// path -- otherwise an interleaved write+rename could promote a torn or
    /// foreign temp file over the live transcript, losing messages. The temp is
    /// removed on any error so a failed write never litters the sessions dir with
    /// a half-written file a later reader could pick up. Mirrors the unique-temp
    /// pattern in `corpus.rs` (C5-4) and `pricing.rs` (C6-P1).
    pub fn save(&mut self, dir: &Path) -> anyhow::Result<()> {
        self.updated = now();
        std::fs::create_dir_all(dir)?;
        let path = Self::path_in(dir, &self.id);
        let data = serde_json::to_vec_pretty(self)?;
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));
        // Write to the unique temp; on any failure remove it so it can never be
        // renamed over the live transcript or left behind torn.
        let f = match std::fs::File::create(&tmp) {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        };
        {
            use std::io::Write as _;
            let mut w = std::io::BufWriter::new(&f);
            if let Err(e) = w.write_all(&data).and_then(|_| w.flush()) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
        // Durability: flush the file contents to disk before the rename so a
        // crash after rename cannot expose a zero-length transcript (matches
        // utilization.rs).
        if let Err(e) = f.sync_all() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// List sessions (id, updated, message-count), newest first.
    pub fn list(dir: &Path) -> anyhow::Result<Vec<(String, i64, usize)>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(out);
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(s) = serde_json::from_str::<Session>(&raw) else {
                continue;
            };
            out.push((s.id, s.updated, s.messages.len()));
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.1));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut d = std::env::temp_dir();
        let uniq = format!(
            "zoder-session-test-{}-{}",
            std::process::id(),
            SAVE_NONCE.fetch_add(1, Ordering::Relaxed)
        );
        d.push(uniq);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // C7-S1: save() uses a unique temp filename and leaves no stray `.json.tmp`
    // (deterministic or per-writer) behind, and the transcript reloads intact.
    #[test]
    fn save_uses_unique_temp_and_leaves_no_stray_tmp() {
        let dir = tmpdir();
        let mut sess = Session {
            id: "shared-id".into(),
            created: now(),
            updated: 0,
            messages: Vec::new(),
        };
        sess.push("user", "hello");
        sess.push("assistant", "hi there");
        sess.save(&dir).unwrap();

        // The transcript is present and reloads with content intact.
        let path = Session::path_in(&dir, "shared-id");
        assert!(path.exists());
        let raw = std::fs::read_to_string(&path).unwrap();
        let reloaded: Session = serde_json::from_str(&raw).unwrap();
        assert_eq!(reloaded.id, "shared-id");
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[0].content, "hello");
        assert_eq!(reloaded.messages[1].content, "hi there");

        // No file matching `.json.tmp` (deterministic or per-writer) survives:
        // the temp must be uniquely named AND renamed/removed, so only
        // `shared-id.json` is left.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".json.tmp"),
                "stray temp file left behind: {name}"
            );
        }
        // Belt-and-suspenders: the legacy deterministic temp path never exists.
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
