//! Persisted session ids per harness so conversations can be resumed.
//!
//! Stored at `<workspace>/.unharness/sessions.json`, or under the user's state
//! directory keyed by a hash of the cwd when there is no workspace root.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::ids::HarnessId;

pub const MAX_RECENT: usize = 20;
const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub model: Option<String>,
    /// RFC 3339 timestamps.
    pub started_at: String,
    pub last_used: String,
    /// First user prompt, truncated.
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HarnessSessions {
    pub last: Option<String>,
    #[serde(default)]
    pub recent: Vec<SessionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionsStore {
    pub version: u32,
    #[serde(default)]
    pub harnesses: HashMap<String, HarnessSessions>,
    #[serde(skip)]
    path: PathBuf,
}

impl SessionsStore {
    /// Where the store lives for a given workspace root (or cwd fallback).
    pub fn path_for(workspace_root: Option<&Path>, cwd: &Path) -> PathBuf {
        match workspace_root {
            Some(root) => root.join(".unharness").join("sessions.json"),
            None => {
                let mut h = DefaultHasher::new();
                cwd.hash(&mut h);
                let base = dirs::state_dir()
                    .or_else(dirs::data_local_dir)
                    .unwrap_or_else(std::env::temp_dir);
                base.join("unharness")
                    .join(format!("{:016x}.json", h.finish()))
            }
        }
    }

    pub fn load(path: PathBuf) -> Self {
        let mut store = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<SessionsStore>(&c).ok())
            .unwrap_or(SessionsStore {
                version: STORE_VERSION,
                harnesses: HashMap::new(),
                path: PathBuf::new(),
            });
        store.path = path;
        store
    }

    pub fn open(workspace_root: Option<&Path>, cwd: &Path) -> Self {
        Self::load(Self::path_for(workspace_root, cwd))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, json).with_context(|| format!("write {}", self.path.display()))
    }

    pub fn harness(&self, id: HarnessId) -> Option<&HarnessSessions> {
        self.harnesses.get(id.as_str())
    }

    pub fn last(&self, id: HarnessId) -> Option<&str> {
        self.harness(id).and_then(|h| h.last.as_deref())
    }

    pub fn recent(&self, id: HarnessId) -> &[SessionRecord] {
        self.harness(id).map(|h| h.recent.as_slice()).unwrap_or(&[])
    }

    /// Record a session start (or touch an existing one) and make it `last`.
    pub fn record_started(
        &mut self,
        id: HarnessId,
        session_id: &str,
        model: Option<&str>,
        title: &str,
    ) {
        let now = now_rfc3339();
        let entry = self.harnesses.entry(id.as_str().to_string()).or_default();
        entry.last = Some(session_id.to_string());
        if let Some(existing) = entry.recent.iter_mut().find(|r| r.id == session_id) {
            existing.last_used = now;
            if existing.title.is_empty() {
                existing.title = truncate_title(title);
            }
            if model.is_some() {
                existing.model = model.map(str::to_string);
            }
        } else {
            entry.recent.insert(
                0,
                SessionRecord {
                    id: session_id.to_string(),
                    model: model.map(str::to_string),
                    started_at: now.clone(),
                    last_used: now,
                    title: truncate_title(title),
                },
            );
            entry.recent.truncate(MAX_RECENT);
        }
    }

    /// Update `last_used` (and the title if still empty) after a turn.
    pub fn touch(&mut self, id: HarnessId, session_id: &str, title: &str) {
        let now = now_rfc3339();
        if let Some(entry) = self.harnesses.get_mut(id.as_str())
            && let Some(pos) = entry.recent.iter().position(|r| r.id == session_id)
        {
            let mut rec = entry.recent.remove(pos);
            rec.last_used = now;
            if rec.title.is_empty() {
                rec.title = truncate_title(title);
            }
            entry.recent.insert(0, rec);
            entry.last = Some(session_id.to_string());
        }
    }
}

fn truncate_title(s: &str) -> String {
    let one_line = s.lines().next().unwrap_or("").trim();
    if one_line.chars().count() > 80 {
        let cut: String = one_line.chars().take(77).collect();
        format!("{}...", cut)
    } else {
        one_line.to_string()
    }
}

fn now_rfc3339() -> String {
    // Avoid a chrono dependency: seconds since epoch rendered as a UTC timestamp.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    epoch_to_rfc3339(secs)
}

fn epoch_to_rfc3339(secs: u64) -> String {
    // Civil-from-days algorithm (Howard Hinnant), good for the Gregorian range we care about.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_rendering() {
        assert_eq!(epoch_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn roundtrip_and_recent_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let path = SessionsStore::path_for(Some(dir.path()), dir.path());
        assert!(path.ends_with(".unharness/sessions.json"));

        let mut store = SessionsStore::load(path.clone());
        store.record_started(HarnessId::Claude, "s1", Some("opus"), "first prompt");
        store.record_started(HarnessId::Claude, "s2", None, "second\nprompt");
        store.record_started(HarnessId::Pi, "p1", None, "");
        store.save().unwrap();

        let loaded = SessionsStore::load(path);
        assert_eq!(loaded.last(HarnessId::Claude), Some("s2"));
        let recent = loaded.recent(HarnessId::Claude);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, "s2");
        assert_eq!(recent[0].title, "second");
        assert_eq!(recent[1].model.as_deref(), Some("opus"));
        assert_eq!(loaded.last(HarnessId::Pi), Some("p1"));
        assert!(loaded.recent(HarnessId::Codex).is_empty());

        let mut loaded = loaded;
        loaded.touch(HarnessId::Claude, "s1", "ignored");
        assert_eq!(loaded.last(HarnessId::Claude), Some("s1"));
        assert_eq!(loaded.recent(HarnessId::Claude)[0].id, "s1");
    }

    #[test]
    fn recent_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionsStore::open(Some(dir.path()), dir.path());
        for i in 0..(MAX_RECENT + 5) {
            store.record_started(HarnessId::Agy, &format!("s{i}"), None, "t");
        }
        assert_eq!(store.recent(HarnessId::Agy).len(), MAX_RECENT);
        assert_eq!(
            store.recent(HarnessId::Agy)[0].id,
            format!("s{}", MAX_RECENT + 4)
        );
    }

    #[test]
    fn fallback_path_without_workspace() {
        let p = SessionsStore::path_for(None, Path::new("/some/where"));
        assert!(p.to_string_lossy().contains("unharness"));
        assert!(p.extension().is_some_and(|e| e == "json"));
    }
}
