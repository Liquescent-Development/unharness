//! The conversation the controller runs: its transcript, the session each
//! harness has in it, where the bridge to each starts, the agent's plan,
//! where each harness can rewind its session to, the files as they were
//! before each prompt, and the usage of the active harness's session.
//! Installed as one ([`Controller::install`]), so that a new conversation,
//! or one resumed, takes nothing from the one before; saved
//! ([`Controller::persist`]); and the file checkpoints taken and restored
//! for it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use super::Controller;
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{
    CheckpointRecord, Conversation, ConversationStore, TurnAnchorRecord, now_rfc3339,
    truncate_title,
};
use crate::core::transcript::Transcript;
use crate::core::{ContextUsage, HarnessId, PlanEntry, Usage};

/// Where a run keeps its conversations, and checkpoints the workspace's
/// files.
pub struct Keeping {
    pub store: ConversationStore,
    /// The workspace's shadow repository; `None` outside a git repository
    /// or when switched off.
    pub checkpoints: Option<Checkpoints>,
}

/// The conversation a run is in. Everything here belongs to it: a new
/// conversation, or one resumed, replaces all of it.
pub struct ConversationState {
    /// What is saved: its id, title and dates, and, as of the last
    /// [`Controller::sync_conversation`], what is below.
    pub record: Conversation,
    pub transcript: Transcript,
    /// Session ids seen this run (or chosen via /resume), per harness.
    pub session_ids: HashMap<HarnessId, String>,
    /// Transcript length when each harness was last active (bridge start).
    pub last_active_index: HashMap<HarnessId, usize>,
    /// Harnesses whose stored session id must be branched, not reattached
    /// (set by `/fork` until the branch exists).
    pub fork_pending: HashSet<HarnessId>,
    pub plan: Vec<PlanEntry>,
    /// Where each harness can rewind its session to (see `TurnAnchor`).
    pub anchors: Vec<TurnAnchorRecord>,
    /// User blocks whose anchor has not arrived yet, oldest first. A harness
    /// that can rewind reports one anchor per turn, in order, but not
    /// always before the next turn starts (pi lists it after the turn).
    pub anchor_pending: VecDeque<usize>,
    /// The working tree as it was before each prompt.
    pub file_checkpoints: Vec<CheckpointRecord>,
    /// The tree as it was before the last file restore, for `/undo-restore`.
    pub restore_undo: Option<String>,
    /// What titles it once saved.
    pub first_prompt: Option<String>,
    pub turn_usage: Usage,
    /// The active harness's session's usage.
    pub session_usage: Usage,
    /// Context-window fill of the active harness's session.
    pub context: ContextUsage,
    /// How long the last turn took.
    pub generation_duration: Option<Duration>,
}

impl ConversationState {
    /// `record` as it was saved, run on `active`.
    pub fn new(record: Conversation, active: HarnessId) -> Self {
        ConversationState {
            transcript: Transcript::from_records(&record.blocks),
            session_ids: record.sessions.clone(),
            last_active_index: record.bookmarks.clone(),
            fork_pending: record.fork_pending.iter().copied().collect(),
            plan: record.plan.clone(),
            anchors: record.anchors.clone(),
            anchor_pending: VecDeque::new(),
            file_checkpoints: record.checkpoints.clone(),
            restore_undo: None,
            first_prompt: (!record.title.is_empty()).then(|| record.title.clone()),
            turn_usage: Usage::default(),
            session_usage: record.usage.get(&active).cloned().unwrap_or_default(),
            context: ContextUsage::default(),
            generation_duration: None,
            record,
        }
    }
}

/// The conversation: installing, saving and resuming it, and its sessions,
/// anchors and file checkpoints.
impl Controller {
    /// Run `record` from here on, on `active`, in place of the conversation
    /// before: a new one (`/clear`), or one saved (`--resume`, `/resume`).
    /// Nothing of the one before is kept, nor which harness planned in it.
    pub fn install(&mut self, record: Conversation, active: HarnessId) {
        self.conversation = ConversationState::new(record, active);
        self.policy.forget_plans();
    }

    /// A new conversation that keeps the models and efforts this one chose.
    pub fn successor(&self, active: HarnessId) -> Conversation {
        let mut next = Conversation::new(active);
        next.models = self.conversation.record.models.clone();
        next.efforts = self.conversation.record.efforts.clone();
        next
    }

    /// Usage and context start again with a session that knows nothing.
    pub fn reset_usage(&mut self) {
        let c = &mut self.conversation;
        c.anchor_pending.clear();
        c.session_usage = Usage::default();
        c.turn_usage = Usage::default();
        c.context = ContextUsage::default();
    }

    /// Bring what is saved up to date. `active` runs the conversation, and
    /// its live session was sent something (`prompted`).
    pub fn sync_conversation(&mut self, active: HarnessId, prompted: bool) {
        let c = &mut self.conversation;
        let r = &mut c.record;
        r.active_harness = active;
        r.sessions = c.session_ids.clone();
        r.bookmarks = c.last_active_index.clone();
        r.blocks = c.transcript.to_records();
        r.usage.insert(active, c.session_usage.clone());
        r.plan = c.plan.clone();
        r.anchors = c.anchors.clone();
        r.checkpoints = c.file_checkpoints.clone();
        r.fork_pending = c.fork_pending.iter().copied().collect();
        r.fork_pending.sort_by_key(|h| h.as_str());
        self.sync_choices(active, prompted);
        let c = &mut self.conversation;
        if c.record.title.is_empty()
            && let Some(p) = &c.first_prompt
        {
            c.record.title = truncate_title(p);
        }
        c.record.updated_at = now_rfc3339();
    }

    /// Write the conversation to disk. Failures are reported once per run.
    pub fn persist(&mut self, active: HarnessId, prompted: bool) {
        if let Err(e) = self.save_conversation(active, prompted)
            && !self.persist_failed
        {
            self.persist_failed = true;
            self.conversation
                .transcript
                .push_error(format!("could not save the conversation: {e:#}"));
        }
    }

    /// Write the conversation to disk, if it has anything to keep.
    pub fn save_conversation(&mut self, active: HarnessId, prompted: bool) -> anyhow::Result<()> {
        self.sync_conversation(active, prompted);
        if !self.conversation.record.has_content() {
            return Ok(());
        }
        self.store.save(&self.conversation.record)
    }

    /// Take what the resumed conversation was run with, its policy, models
    /// and efforts, except where this run named its own
    /// (`Controller::take_conversation`).
    pub fn take_conversation_choices(&mut self) {
        let record = self.conversation.record.clone();
        for notice in self.take_conversation(&record) {
            self.conversation.transcript.push_notice(notice);
        }
    }

    /// Say which conversation was resumed, and whether `active` runs under
    /// another policy than it last did. `at_start`: by `--resume`.
    pub fn announce_resume(&mut self, active: HarnessId, at_start: bool) {
        let summary = self.conversation.record.summary();
        let name = self.short_name(active);
        self.conversation.transcript.push_notice(format!(
            "resumed conversation {} ({}); continuing on {}",
            &summary.id[..8.min(summary.id.len())],
            summary
                .harnesses
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            name
        ));
        let Some(last) = self.conversation.record.last_policies.get(&active).copied() else {
            return;
        };
        if let Some(notice) = self.resumed_under(active, last, at_start) {
            self.conversation.transcript.push_notice(notice);
        }
    }

    /// Printed after the TUI closes, when a conversation was saved.
    pub fn exit_summary(&self) -> Option<String> {
        let c = &self.conversation;
        if !c.record.has_content() {
            return None;
        }
        let id = &c.record.id;
        let short = &id[..8.min(id.len())];
        let mut harnesses: Vec<&str> = c.session_ids.keys().map(|h| h.as_str()).collect();
        harnesses.sort_unstable();
        Some(format!(
            "Conversation {short} saved ({}). Resume it with:\n  unharness --resume {short}",
            if harnesses.is_empty() {
                "no sessions yet".to_string()
            } else {
                harnesses.join(", ")
            }
        ))
    }

    /// What points at `harness`'s vendor session: its id, where the bridge
    /// to it starts, a fork of it to make, rewind anchors into it.
    pub fn forget_ids(&mut self, harness: HarnessId) {
        let c = &mut self.conversation;
        c.session_ids.remove(&harness);
        c.last_active_index.remove(&harness);
        c.fork_pending.remove(&harness);
        c.anchors.retain(|a| a.harness != harness);
    }

    /// Whether `harness`'s next session must branch the stored session id
    /// (it belongs to the conversation this one was forked from).
    pub fn fork_pending(&self, harness: HarnessId) -> bool {
        self.conversation.fork_pending.contains(&harness)
    }

    /// The turn `harness`'s oldest prompt still owed one was is `id` to it;
    /// an empty id means the turn never reached the harness.
    pub fn turn_anchor(&mut self, harness: HarnessId, id: String) {
        let c = &mut self.conversation;
        if let Some(block) = c.anchor_pending.pop_front()
            && !id.is_empty()
        {
            c.anchors
                .retain(|a| !(a.block == block && a.harness == harness));
            c.anchors.push(TurnAnchorRecord { block, harness, id });
        }
    }

    /// Record the working tree as it is before the turn at `block` runs.
    pub fn checkpoint_files(&mut self, block: usize) {
        let Some(cp) = self.checkpoints.clone() else {
            return;
        };
        match cp.snapshot(&self.checkpoint_name(&block.to_string())) {
            Ok(commit) => {
                let c = &mut self.conversation;
                c.file_checkpoints.retain(|c| c.block != block);
                c.file_checkpoints.push(CheckpointRecord { block, commit });
            }
            Err(e) if !self.checkpoint_failed => {
                self.checkpoint_failed = true;
                self.conversation.transcript.push_notice(format!(
                    "could not checkpoint files (rewind will not be able to restore them): {e:#}"
                ));
            }
            Err(_) => {}
        }
    }

    /// `harness`'s own id for the user turn at `block`, when its session
    /// (running or resumable) can be rewound to it.
    pub fn native_anchor(&self, harness: HarnessId, block: usize) -> Option<&str> {
        let rewinds = self.caps_of(harness).is_some_and(|c| c.rewind.conversation);
        if !rewinds || !self.conversation.session_ids.contains_key(&harness) {
            return None;
        }
        // Going back to before the session's first turn leaves nothing of
        // it: a fresh session is the same thing, and Claude refuses to
        // rewind to its first message ("stale target").
        let mut mine = self
            .conversation
            .anchors
            .iter()
            .filter(|a| a.harness == harness);
        if !mine.clone().any(|a| a.block < block) {
            return None;
        }
        mine.find(|a| a.block == block).map(|a| a.id.as_str())
    }

    /// The file checkpoint taken just before the user block at `block`.
    pub fn checkpoint_at(&self, block: usize) -> Option<&str> {
        self.conversation
            .file_checkpoints
            .iter()
            .find(|c| c.block == block)
            .map(|c| c.commit.as_str())
    }

    /// Name of a checkpoint ref for this conversation (see `core::checkpoints`).
    fn checkpoint_name(&self, suffix: &str) -> String {
        let id = &self.conversation.record.id;
        format!("{}/{suffix}", &id[..8.min(id.len())])
    }

    /// Put the working tree back to how it was before the user block at
    /// `block`. Returns what is said of it, or why it could not be done
    /// (then nothing changed).
    pub fn restore_files(&mut self, block: usize) -> Result<String, String> {
        let (Some(cp), Some(commit)) = (
            self.checkpoints.clone(),
            self.checkpoint_at(block).map(str::to_string),
        ) else {
            return Err("there is no file checkpoint for that prompt".to_string());
        };
        self.restore_seq += 1;
        let undo_name = self.checkpoint_name(&format!("undo-{}", self.restore_seq));
        match cp.restore(&commit, &undo_name) {
            Ok(report) => {
                self.conversation.restore_undo = Some(report.undo);
                Ok(format!(
                    "{} file(s) put back to how they were before that prompt (/undo-restore reverses this)",
                    report.changed
                ))
            }
            Err(e) => Err(format!(
                "could not restore files, nothing was rewound: {e:#}"
            )),
        }
    }

    /// Reverse the last file restore (itself reversible the same way).
    pub fn undo_restore(&mut self) {
        let (Some(cp), Some(undo)) = (
            self.checkpoints.clone(),
            self.conversation.restore_undo.clone(),
        ) else {
            self.conversation
                .transcript
                .push_notice("no file restore to undo");
            return;
        };
        self.restore_seq += 1;
        let name = self.checkpoint_name(&format!("undo-{}", self.restore_seq));
        match cp.restore(&undo, &name) {
            Ok(report) => {
                self.conversation.restore_undo = Some(report.undo);
                self.conversation.transcript.push_system(format!(
                    "{} file(s) put back to how they were before the restore.",
                    report.changed
                ));
            }
            Err(e) => self
                .conversation
                .transcript
                .push_error(format!("could not undo the restore: {e:#}")),
        }
    }

    /// Drop the user turn at `block` and everything after it from the
    /// conversation: its blocks, and the anchors and checkpoints of the
    /// turns dropped.
    pub fn rewind_conversation(&mut self, block: usize) {
        let c = &mut self.conversation;
        c.transcript.blocks.truncate(block);
        c.anchors.retain(|a| a.block < block);
        c.file_checkpoints.retain(|c| c.block < block);
        c.anchor_pending.clear();
    }

    /// Make the conversation a copy of itself, which ran where it ran and
    /// keeps its title, models and efforts. Each harness's session is to
    /// be branched where its last session in this run said it can fork;
    /// the others start fresh with the transcript. Returns the harnesses
    /// branched and those left to start fresh, by name.
    pub fn fork(&mut self, active: HarnessId) -> (Vec<&'static str>, Vec<&'static str>) {
        let mut fork = self.successor(active);
        fork.title = self.conversation.record.title.clone();
        fork.last_policies = self.conversation.record.last_policies.clone();
        self.conversation.record = fork;

        let mut branched = Vec::new();
        let mut fresh = Vec::new();
        let harnesses: Vec<HarnessId> = self.conversation.session_ids.keys().copied().collect();
        for h in harnesses {
            // What its last session in this run reported counts: an ACP
            // agent declares no fork until it says it can. One not run here
            // since launch starts fresh.
            let caps = self.caps_of(h);
            let c = &mut self.conversation;
            match caps {
                Some(caps) if caps.fork && caps.resume_by_id => {
                    c.fork_pending.insert(h);
                    if !caps.rewind.anchors_survive_fork {
                        c.anchors.retain(|a| a.harness != h);
                    }
                    branched.push(h.as_str());
                }
                _ => {
                    c.session_ids.remove(&h);
                    c.last_active_index.remove(&h);
                    c.anchors.retain(|a| a.harness != h);
                    fresh.push(h.as_str());
                }
            }
        }
        branched.sort_unstable();
        fresh.sort_unstable();
        (branched, fresh)
    }
}
