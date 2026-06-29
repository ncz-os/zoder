//! Multi-turn session persistence.
//!
//! A session is an append-only transcript of chat messages stored as a single
//! JSON file under `$ZODER_HOME/sessions/<id>.json`. `exec` can attach to a
//! session (`--session <id>`) or continue the most-recently-updated one
//! (`--continue`), so follow-up prompts carry prior context.

use crate::provider::Message;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

    /// Persist atomically (temp file + rename) under `dir`.
    pub fn save(&mut self, dir: &Path) -> anyhow::Result<()> {
        self.updated = now();
        std::fs::create_dir_all(dir)?;
        let path = Self::path_in(dir, &self.id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
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
