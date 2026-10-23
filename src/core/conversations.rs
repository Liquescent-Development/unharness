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

use super::caps::PermissionPolicy;
use super::event::{CompactTrigger, HookOutcome, PlanEntry, SubagentStatus, Usage};
use super::ids::{HarnessId, ModelRef};

/// Conversations kept in the index; older ones are deleted on save.
pub const MAX_CONVERSATIONS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockRecord {
    User {
        text: String,
        /// Sent into a running turn (a steer), not a turn of its own.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        steer: bool,
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
    /// What a harness wrote for the next one when the user switched away
    /// from it, standing in for the conversation before it in a bridge.
    Handoff {
        text: String,
        sender: String,
        /// The harness it was written for, by name.
        to: String,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        output: String,
        is_error: bool,
        /// Only in older files, which kept a subagent's calls in the main
        /// list: the call that spawned that subagent. Such a call is moved
        /// into the subagent's own transcript when the file is loaded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        /// The subagent this call spawned.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentRecord>,
    },
    System {
        text: String,
    },
    Notice {
        text: String,
        /// What a compaction the notice tells of was (#359). An unharness
        /// before it reads the notice alone, so that the blocks after it
        /// keep their places (and the anchors and checkpoints on them).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compaction: Option<CompactionRecord>,
    },
    Error {
        text: String,
    },
    /// A hook the harness ran on its own events.
    Hook {
        name: String,
        /// `None`: it had not ended when the conversation was saved.
        outcome: Option<HookOutcome>,
        output: String,
    },
    /// A command the user ran from the prompt with `!`.
    Shell {
        command: String,
        /// Its output (stdout and stderr), the tail when it was long.
        output: String,
        /// Lines dropped from the front of `output` to keep it short.
        #[serde(default, skip_serializing_if = "is_zero")]
        dropped: usize,
        status: ShellStatus,
        /// It already went in front of a prompt to an agent.
        #[serde(default)]
        sent: bool,
        #[serde(default)]
        secs: Option<f32>,
    },
    /// A kind of block a later unharness wrote. Skipped on load, so that
    /// the rest of the conversation still opens; never written.
    #[serde(other)]
    Unknown,
}

/// A compaction, saved on the notice that tells of it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionRecord {
    /// The harness that compacted.
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CompactTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<u64>,
    /// The text the model had in place of what was compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The block where what the model kept as it was starts (this list's
    /// index); `None` when it was not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<usize>,
    /// The session that compacted was left: what it held is no one's now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retired: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// How a `!` command ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ShellStatus {
    Running,
    Exited {
        code: i32,
    },
    /// Ended by a signal it was not sent by the user.
    Signalled,
    /// Stopped by the user, or by unharness quitting.
    Killed,
    /// It could not be started.
    Failed {
        error: String,
    },
}

impl ShellStatus {
    /// "exit 0", "killed", …
    pub fn label(&self) -> String {
        match self {
            ShellStatus::Running => "running".into(),
            ShellStatus::Exited { code } => format!("exit {code}"),
            ShellStatus::Signalled => "ended by a signal".into(),
            ShellStatus::Killed => "stopped by the user".into(),
            ShellStatus::Failed { error } => format!("could not start: {error}"),
        }
    }
}

/// A subagent's work, as kept on the tool call that spawned it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRecord {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `None`: it was still running when this was saved.
    #[serde(default)]
    pub status: Option<SubagentStatus>,
    /// Its own transcript: its tool calls, its prose and, last, its report.
    #[serde(default)]
    pub blocks: Vec<BlockRecord>,
    #[serde(default)]
    pub secs: Option<f32>,
    /// The user took it off the list under the prompt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
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
    /// Harness → what its last turn used.
    #[serde(default)]
    pub turn_usage: HashMap<HarnessId, Usage>,
    /// Harness → how full its session's context window last was.
    #[serde(default)]
    pub context: HashMap<HarnessId, ContextRecord>,
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
    /// The policy named for every harness (`--policy`, and `/policy` in
    /// an older unharness); `None` when each followed its configured
    /// default.
    #[serde(default)]
    pub policy: Option<PermissionPolicy>,
    /// What was chosen in the TUI for each harness (in an older unharness
    /// only where the named policy was not available).
    #[serde(default)]
    pub policy_choices: HashMap<HarnessId, PermissionPolicy>,
    /// The policy each harness last ran under, to tell the user when a
    /// resumed one runs under another.
    #[serde(default)]
    pub last_policies: HashMap<HarnessId, PermissionPolicy>,
    /// The model chosen for each harness (`--model`, `/model`); a harness
    /// on its configured default has none.
    #[serde(default)]
    pub models: HashMap<HarnessId, ModelRef>,
    /// The effort chosen for each harness (`--effort`, `/effort`).
    #[serde(default)]
    pub efforts: HashMap<HarnessId, String>,
}

/// How full a harness's context window was (`ContextUsage`), and what
/// that was measured on: a fill is only good for the session that
/// reported it, and a window only for its model.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRecord {
    #[serde(default)]
    pub used: Option<u64>,
    #[serde(default)]
    pub window: Option<u64>,
    /// The model it ran, as the context windows are kept by (`ModelRef::
    /// label`, or `default`).
    #[serde(default)]
    pub model: String,
    /// The vendor session `used` is of; `None` when it had none stored.
    #[serde(default)]
    pub session: Option<String>,
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
    /// The oldest block the turn's message told the harness of: what it
    /// had not seen of the conversation, `!` commands, else `block`
    /// itself. A compaction that keeps the turn keeps those too. `None`
    /// in a file of an unharness before #359, which did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub told_from: Option<usize>,
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
            turn_usage: HashMap::new(),
            context: HashMap::new(),
            plan: Vec::new(),
            anchors: Vec::new(),
            checkpoints: Vec::new(),
            fork_pending: Vec::new(),
            policy: None,
            policy_choices: HashMap::new(),
            last_policies: HashMap::new(),
            models: HashMap::new(),
            efforts: HashMap::new(),
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

/// Harness → model (as `ModelRef::label` has it, or `default`) → context
/// window in tokens.
pub type ContextWindows = HashMap<HarnessId, HashMap<String, u64>>;

/// The windows believed, in tokens: a value outside them, from the file or
/// from a harness, is taken as the nearest end, so that it neither empties
/// the bridge nor lifts its cap.
pub const CONTEXT_WINDOW_MIN: u64 = 8_000;
pub const CONTEXT_WINDOW_MAX: u64 = 2_000_000;

/// Where the context windows are kept: `<state dir>/unharness/
/// context_windows.json`. Not in the workspace, where an agent could write
/// what decides how much of the conversation it is sent; the sandbox keeps
/// the state directory read-only. One file, since a window belongs to a
/// model and not to a workspace.
pub fn default_context_windows_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("unharness")
        .join("context_windows.json")
}

/// The context windows the harnesses reported, by model, kept across
/// conversations: what the bridge to a harness is measured against before
/// its session has said. Empty when unreadable.
pub fn load_context_windows(path: &Path) -> ContextWindows {
    let mut windows: ContextWindows = std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();
    for models in windows.values_mut() {
        for window in models.values_mut() {
            *window = (*window).clamp(CONTEXT_WINDOW_MIN, CONTEXT_WINDOW_MAX);
        }
    }
    windows
}

pub fn save_context_windows(path: &Path, windows: &ContextWindows) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(path, &serde_json::to_vec_pretty(windows)?)
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

    #[test]
    fn context_windows_out_of_range_or_unreadable_are_not_believed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/context_windows.json");
        assert!(load_context_windows(&path).is_empty());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"codex\": not json").unwrap();
        assert!(load_context_windows(&path).is_empty());

        std::fs::write(
            &path,
            r#"{"codex": {"default": 1, "big": 18446744073709551615, "gpt": 258400}}"#,
        )
        .unwrap();
        let windows = load_context_windows(&path);
        let codex = &windows[&HarnessId::CODEX];
        assert_eq!(codex["default"], CONTEXT_WINDOW_MIN);
        assert_eq!(codex["big"], CONTEXT_WINDOW_MAX);
        assert_eq!(codex["gpt"], 258_400);

        save_context_windows(&path, &windows).unwrap();
        assert_eq!(load_context_windows(&path), windows);
    }

    fn conv(title: &str) -> Conversation {
        let mut c = Conversation::new(HarnessId::CLAUDE);
        c.title = title.into();
        c.sessions.insert(HarnessId::CLAUDE, "claude-sess".into());
        c.sessions.insert(HarnessId::CODEX, "codex-thread".into());
        c.bookmarks.insert(HarnessId::CLAUDE, 2);
        c.policy = Some(PermissionPolicy::Auto);
        c.policy_choices
            .insert(HarnessId::AGY, PermissionPolicy::AcceptEdits);
        c.last_policies
            .insert(HarnessId::CLAUDE, PermissionPolicy::Auto);
        c.models.insert(
            HarnessId::CODEX,
            ModelRef::new(HarnessId::CODEX, "openai", "gpt-x"),
        );
        c.efforts.insert(HarnessId::CODEX, "high".into());
        c.blocks.push(BlockRecord::User {
            text: "hi".into(),
            steer: false,
        });
        c.blocks.push(BlockRecord::Assistant {
            text: "hello".into(),
            sender: "Claude".into(),
            secs: Some(1.5),
        });
        c.blocks.push(BlockRecord::Shell {
            command: "ls".into(),
            output: "a\nb\n".into(),
            dropped: 3,
            status: ShellStatus::Exited { code: 1 },
            sent: true,
            secs: Some(0.5),
        });
        c.usage.insert(
            HarnessId::CLAUDE,
            Usage {
                input: 10,
                ..Default::default()
            },
        );
        c.turn_usage.insert(
            HarnessId::CLAUDE,
            Usage {
                input: 4,
                ..Default::default()
            },
        );
        c.context.insert(
            HarnessId::CLAUDE,
            ContextRecord {
                used: Some(40_000),
                window: Some(200_000),
                model: "default".into(),
                session: Some("claude-sess".into()),
            },
        );
        c
    }

    /// A conversation an unharness before #309 saved has no last turn nor
    /// context: it opens without them. One a later unharness saved, with
    /// fields this one does not know, opens too.
    #[test]
    fn a_record_without_context_or_with_more_opens() {
        let old = serde_json::json!({
            "id": "abc", "title": "t", "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z", "active_harness": "claude",
            "sessions": {"claude": "s1"},
            "usage": {"claude": {"input": 1, "output": 2, "cache_read": 0,
                "cache_write": 0, "cost_usd": null, "cumulative": false}},
        });
        let c: Conversation = serde_json::from_value(old.clone()).unwrap();
        assert!(c.turn_usage.is_empty() && c.context.is_empty());
        assert_eq!(c.usage[&HarnessId::CLAUDE].output, 2);

        let mut newer = old;
        newer["context"] = serde_json::json!({"claude": {"used": 5, "from_the_future": 1}});
        newer["from_the_future"] = serde_json::json!([1, 2]);
        let c: Conversation = serde_json::from_value(newer).unwrap();
        assert_eq!(
            c.context[&HarnessId::CLAUDE],
            ContextRecord {
                used: Some(5),
                ..Default::default()
            }
        );
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

    /// A block a later unharness wrote does not keep the rest from opening.
    #[test]
    fn a_block_of_an_unknown_kind_is_skipped() {
        let blocks: Vec<BlockRecord> = serde_json::from_value(serde_json::json!([
            {"kind": "user", "text": "hi"},
            {"kind": "from_the_future", "whatever": [1, 2]},
            {"kind": "hook", "name": "Stop", "outcome": "blocked", "output": "no"},
        ]))
        .unwrap();
        assert_eq!(
            blocks,
            [
                BlockRecord::User {
                    text: "hi".into(),
                    steer: false
                },
                BlockRecord::Unknown,
                BlockRecord::Hook {
                    name: "Stop".into(),
                    outcome: Some(HookOutcome::Blocked),
                    output: "no".into(),
                },
            ]
        );
        let back = crate::core::transcript::Transcript::from_records(&blocks);
        assert_eq!(back.blocks.len(), 2);
    }

    /// A compaction is saved on the notice that tells of it, which an
    /// unharness before #359 reads as a notice: every block keeps its
    /// place, and the anchors and checkpoints on the blocks after it
    /// still point at them.
    #[test]
    fn a_compaction_is_a_notice_an_older_unharness_reads_in_its_place() {
        let r = BlockRecord::Notice {
            text: "context compacted · 9 tokens before · overflow".into(),
            compaction: Some(CompactionRecord {
                harness: HarnessId::PI,
                trigger: Some(CompactTrigger::Overflow),
                tokens_before: Some(9),
                tokens_after: None,
                summary: Some("s".into()),
                kept: Some(0),
                retired: false,
            }),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "notice", "text": "context compacted · 9 tokens before · overflow",
                "compaction": {"harness": "pi", "trigger": "overflow", "tokens_before": 9,
                               "summary": "s", "kept": 0}})
        );
        assert_eq!(
            serde_json::from_value::<BlockRecord>(json.clone()).unwrap(),
            r
        );
        // A plain notice is saved as before.
        let plain = BlockRecord::Notice {
            text: "n".into(),
            compaction: None,
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"kind": "notice", "text": "n"})
        );

        // The records as an unharness before #359 had them, with or
        // without `serde(other)`: the compaction is a notice, in its place.
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Older {
            User { text: String },
            Notice { text: String },
        }
        let user = serde_json::json!({"kind": "user", "text": "after"});
        let read: Vec<Older> = serde_json::from_value(serde_json::json!([
            {"kind": "user", "text": "hi"},
            json,
            user
        ]))
        .unwrap();
        assert_eq!(
            read,
            [
                Older::User { text: "hi".into() },
                Older::Notice {
                    text: "context compacted · 9 tokens before · overflow".into()
                },
                Older::User {
                    text: "after".into()
                },
            ]
        );
        let back = crate::core::transcript::Transcript::from_records(
            &serde_json::from_value::<Vec<BlockRecord>>(serde_json::json!([
                {"kind": "notice", "text": "context compacted"}
            ]))
            .unwrap(),
        );
        assert!(
            matches!(&back.blocks[0], crate::core::transcript::Block::Notice(t) if t == "context compacted")
        );
    }

    #[test]
    fn a_shell_record_is_tagged_and_its_extras_are_optional() {
        let r = BlockRecord::Shell {
            command: "make".into(),
            output: String::new(),
            dropped: 0,
            status: ShellStatus::Failed { error: "x".into() },
            sent: false,
            secs: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["kind"], "shell");
        assert_eq!(json["status"]["state"], "failed");
        assert!(json.get("dropped").is_none());
        let minimal = serde_json::json!({
            "kind": "shell", "command": "ls", "output": "x\n",
            "status": {"state": "exited", "code": 0}
        });
        assert_eq!(
            serde_json::from_value::<BlockRecord>(minimal).unwrap(),
            BlockRecord::Shell {
                command: "ls".into(),
                output: "x\n".into(),
                dropped: 0,
                status: ShellStatus::Exited { code: 0 },
                sent: false,
                secs: None,
            }
        );
    }

    /// A conversation saved before the policy was kept follows the config.
    #[test]
    fn a_conversation_without_a_policy_loads() {
        let c: Conversation = serde_json::from_value(serde_json::json!({
            "id": "x", "title": "", "created_at": "", "updated_at": "",
            "active_harness": "claude",
        }))
        .unwrap();
        assert_eq!(c.policy, None);
        assert!(c.policy_choices.is_empty());
        assert!(c.last_policies.is_empty());
        assert!(c.models.is_empty() && c.efforts.is_empty());
    }

    #[test]
    fn fallback_root_without_workspace() {
        let p = ConversationStore::root_for(None, Path::new("/some/where"));
        assert!(p.to_string_lossy().contains("unharness"));
        assert_eq!(truncate_title("a\nb"), "a");
    }
}
