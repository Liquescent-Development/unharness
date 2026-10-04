//! Persisted unharness conversations: the merged transcript, the vendor
//! session id of every harness that took part, the bridging bookmarks, and
//! the harness that was active. One JSON file per conversation under
//! `<workspace>/.unharness/conversations/`, plus a small index.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::event::{PlanEntry, Usage};
use super::ids::HarnessId;

/// Conversations kept in the index; older ones are deleted on save.
pub const MAX_CONVERSATIONS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockRecord {
    User {
        text: String,
    },
    Assistant {
        text: String,
        sender: String,
        secs: Option<f32>,
    },
    Thought {
        text: String,
        secs: Option<f32>,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        output: String,
        is_error: bool,
        /// The tool call that spawned the subagent this call ran in.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    System {
        text: String,
    },
    Notice {
        text: String,
    },
    Error {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub harnesses: Vec<HarnessId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub active_harness: HarnessId,
    /// Harness → vendor session id.
    #[serde(default)]
    pub sessions: HashMap<HarnessId, String>,
    /// Harness → transcript length when it was last active (bridge start).
    #[serde(default)]
    pub bookmarks: HashMap<HarnessId, usize>,
    #[serde(default)]
    pub blocks: Vec<BlockRecord>,
    /// Harness → that harness's session usage totals.
    #[serde(default)]
    pub usage: HashMap<HarnessId, Usage>,
    /// The agent's latest plan / todo list.
    #[serde(default)]
    pub plan: Vec<PlanEntry>,
    /// Where each harness can rewind its own session to.
    #[serde(default)]
    pub anchors: Vec<TurnAnchorRecord>,
    /// The working tree as it was before each prompt.
    #[serde(default)]
    pub checkpoints: Vec<CheckpointRecord>,
    /// Harnesses whose session id here belongs to the conversation this one
    /// was forked from: their next session must branch it, not reattach.
    #[serde(default)]
    pub fork_pending: Vec<HarnessId>,
}

/// A file checkpoint taken just before the user block at `block` was sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRecord {
    pub block: usize,
    /// Commit id (see `core::checkpoints`).
    pub commit: String,
}

/// A user turn as one harness's session knows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnAnchorRecord {
    /// Index of the user block in the transcript.
    pub block: usize,
    pub harness: HarnessId,
    /// The harness's id for that turn (`AgentEvent::TurnAnchor`).
    pub id: String,
}

impl Conversation {
    pub fn new(active_harness: HarnessId) -> Self {
        let now = now_rfc3339();
        Conversation {
            id: uuid::Uuid::new_v4().to_string(),
            title: String::new(),
            created_at: now.clone(),
            updated_at: now,
            active_harness,
            sessions: HashMap::new(),
            bookmarks: HashMap::new(),
            blocks: Vec::new(),
            usage: HashMap::new(),
            plan: Vec::new(),
            anchors: Vec::new(),
            checkpoints: Vec::new(),
            fork_pending: Vec::new(),
        }
    }

    pub fn summary(&self) -> ConversationSummary {
        let mut harnesses: Vec<HarnessId> = self.sessions.keys().copied().collect();
        harnesses.sort_by_key(|h| h.as_str());
        ConversationSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            harnesses,
        }
    }

    /// True once there is something worth keeping.
    pub fn has_content(&self) -> bool {
        !self.sessions.is_empty()
            || self
                .blocks
                .iter()
                .any(|b| matches!(b, BlockRecord::User { .. } | BlockRecord::Assistant { .. }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Index {
    #[serde(default)]
    conversations: Vec<ConversationSummary>,
}

pub struct ConversationStore {
    root: PathBuf,
}

impl ConversationStore {
    /// `<workspace>/.unharness` or a per-cwd directory under the state dir.
    pub fn root_for(workspace_root: Option<&Path>, cwd: &Path) -> PathBuf {
        match workspace_root {
            Some(root) => root.join(".unharness"),
            None => {
                let mut h = DefaultHasher::new();
                cwd.hash(&mut h);
                let base = dirs::state_dir()
                    .or_else(dirs::data_local_dir)
                    .unwrap_or_else(std::env::temp_dir);
                base.join("unharness").join(format!("{:016x}", h.finish()))
            }
        }
    }

    pub fn open(workspace_root: Option<&Path>, cwd: &Path) -> Self {
        ConversationStore {
            root: Self::root_for(workspace_root, cwd),
        }
    }

    pub fn at(root: PathBuf) -> Self {
        ConversationStore { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self) -> PathBuf {
        self.root.join("conversations")
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("conversations.json")
    }

    fn file_for(&self, id: &str) -> PathBuf {
        self.dir().join(format!("{id}.json"))
    }

    /// Prompts sent from this workspace (the TUI's Up/Down recall).
    pub fn history_path(&self) -> PathBuf {
        self.root.join("prompt_history.jsonl")
    }

    fn read_index(&self) -> Index {
        std::fs::read_to_string(self.index_path())
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| self.rebuild_index())
    }

    /// Scan the directory when the index is missing or unreadable.
    fn rebuild_index(&self) -> Index {
        let mut rows = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.dir()) {
            for e in entries.flatten() {
                if let Ok(c) = std::fs::read_to_string(e.path())
                    && let Ok(conv) = serde_json::from_str::<Conversation>(&c)
                {
                    rows.push(conv.summary());
                }
            }
        }
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Index {
            conversations: rows,
        }
    }

    fn write_index(&self, index: &Index) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        write_atomic(&self.index_path(), &serde_json::to_vec_pretty(index)?)
    }

    /// Newest first.
    pub fn list(&self) -> Vec<ConversationSummary> {
        self.read_index().conversations
    }

    pub fn last(&self) -> Option<ConversationSummary> {
        self.list().into_iter().next()
    }

    /// Exact id or unique prefix.
    pub fn resolve(&self, id_or_prefix: &str) -> Result<ConversationSummary> {
        let rows = self.list();
        if let Some(r) = rows.iter().find(|r| r.id == id_or_prefix) {
            return Ok(r.clone());
        }
        let matches: Vec<&ConversationSummary> = rows
            .iter()
            .filter(|r| r.id.starts_with(id_or_prefix))
            .collect();
        match matches.len() {
            0 => bail!("no conversation matches '{id_or_prefix}'"),
            1 => Ok(matches[0].clone()),
            n => bail!("'{id_or_prefix}' matches {n} conversations; give more of the id"),
        }
    }

    pub fn load(&self, id_or_prefix: &str) -> Result<Conversation> {
        let row = self.resolve(id_or_prefix)?;
        let path = self.file_for(&row.id);
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, conv: &Conversation) -> Result<()> {
        std::fs::create_dir_all(self.dir())?;
        write_atomic(&self.file_for(&conv.id), &serde_json::to_vec_pretty(conv)?)?;
        let mut index = self.read_index();
        index.conversations.retain(|r| r.id != conv.id);
        index.conversations.insert(0, conv.summary());
        while index.conversations.len() > MAX_CONVERSATIONS {
            if let Some(old) = index.conversations.pop() {
                let _ = std::fs::remove_file(self.file_for(&old.id));
            }
        }
        self.write_index(&index)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let _ = std::fs::remove_file(self.file_for(id));
        let mut index = self.read_index();
        index.conversations.retain(|r| r.id != id);
        self.write_index(&index)
    }

    pub fn clear(&self) -> Result<()> {
        let _ = std::fs::remove_dir_all(self.dir());
        let _ = std::fs::remove_file(self.index_path());
        let _ = std::fs::remove_file(self.history_path());
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

pub fn truncate_title(s: &str) -> String {
    let one_line = s.lines().next().unwrap_or("").trim();
    if one_line.chars().count() > 80 {
        let cut: String = one_line.chars().take(77).collect();
        format!("{}...", cut)
    } else {
        one_line.to_string()
    }
}

pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    epoch_to_rfc3339(secs)
}

/// Civil-from-days (Howard Hinnant); fine for the Gregorian range we need.
pub fn epoch_to_rfc3339(secs: u64) -> String {
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

    fn conv(title: &str) -> Conversation {
        let mut c = Conversation::new(HarnessId::CLAUDE);
        c.title = title.into();
        c.sessions.insert(HarnessId::CLAUDE, "claude-sess".into());
        c.sessions.insert(HarnessId::CODEX, "codex-thread".into());
        c.bookmarks.insert(HarnessId::CLAUDE, 2);
        c.blocks.push(BlockRecord::User { text: "hi".into() });
        c.blocks.push(BlockRecord::Assistant {
            text: "hello".into(),
            sender: "Claude".into(),
            secs: Some(1.5),
        });
        c.usage.insert(
            HarnessId::CLAUDE,
            Usage {
                input: 10,
                ..Default::default()
            },
        );
        c
    }

    #[test]
    fn rfc3339_rendering() {
        assert_eq!(epoch_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn roundtrip_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(Some(dir.path()), dir.path());
        assert!(store.root().ends_with(".unharness"));
        assert!(store.list().is_empty());
        assert!(store.last().is_none());

        let c = conv("first prompt");
        store.save(&c).unwrap();
        let loaded = store.load(&c.id).unwrap();
        assert_eq!(loaded, c);
        let row = store.last().unwrap();
        assert_eq!(row.harnesses, vec![HarnessId::CLAUDE, HarnessId::CODEX]);
        assert_eq!(row.title, "first prompt");

        // Prefix lookup
        assert_eq!(store.resolve(&c.id[..8]).unwrap().id, c.id);
        assert!(store.resolve("zzzz").is_err());

        // Saving again moves it to the top and keeps one row.
        let mut c2 = conv("second");
        c2.updated_at = "2030-01-01T00:00:00Z".into();
        store.save(&c2).unwrap();
        store.save(&c).unwrap();
        let rows = store.list();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, c.id);

        // Ambiguous prefix (both ids share "" prefix)
        assert!(store.resolve("").is_err());

        store.delete(&c2.id).unwrap();
        assert_eq!(store.list().len(), 1);
        store.clear().unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn index_rebuilds_from_files_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(Some(dir.path()), dir.path());
        for i in 0..(MAX_CONVERSATIONS + 3) {
            let mut c = conv(&format!("c{i}"));
            c.updated_at = format!("2026-01-01T00:00:{:02}Z", i % 60);
            store.save(&c).unwrap();
        }
        assert_eq!(store.list().len(), MAX_CONVERSATIONS);
        assert_eq!(
            std::fs::read_dir(store.dir()).unwrap().count(),
            MAX_CONVERSATIONS
        );
        std::fs::remove_file(store.index_path()).unwrap();
        assert_eq!(store.list().len(), MAX_CONVERSATIONS);
    }

    #[test]
    fn fallback_root_without_workspace() {
        let p = ConversationStore::root_for(None, Path::new("/some/where"));
        assert!(p.to_string_lossy().contains("unharness"));
        assert_eq!(truncate_title("a\nb"), "a");
    }
}
