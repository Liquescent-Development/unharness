//! The conversation the controller runs: its transcript, the session each
//! harness has in it, where the bridge to each starts, the agent's plan,
//! where each harness can rewind its session to, the files as they were
//! before each prompt, and the usage of the active harness's session.
//! Installed as one ([`Controller::install`]), so that a new conversation,
//! or one resumed, takes nothing from the one before; saved
//! ([`Controller::persist`]); and the file checkpoints taken and restored
//! for it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use super::{Controller, Front};
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{
    CheckpointRecord, ContextRecord, Conversation, ConversationStore, TurnAnchorRecord,
    now_rfc3339, truncate_title,
};
use crate::core::transcript::{Block, KeptAt, Transcript, current_compaction};
use crate::core::{Compaction, ContextUsage, HarnessId, Kept, PlanEntry, Usage};

/// Where a run keeps its conversations, checkpoints the workspace's
/// files, and keeps the context windows harnesses reported.
pub struct Keeping {
    pub store: ConversationStore,
    /// The workspace's shadow repository; `None` outside a git repository
    /// or when switched off.
    pub checkpoints: Option<Checkpoints>,
    /// The file the context windows are kept in, under the state directory
    /// (`conversations::default_context_windows_path`).
    pub context_windows: PathBuf,
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
    /// User blocks whose anchor has not arrived yet, oldest first, each
    /// with the oldest block its message told of (`TurnAnchorRecord::
    /// told_from`). A harness that can rewind reports one anchor per turn,
    /// in order, but not always before the next turn starts (pi lists it
    /// after the turn).
    pub anchor_pending: VecDeque<(usize, usize)>,
    /// The working tree as it was before each prompt.
    pub file_checkpoints: Vec<CheckpointRecord>,
    /// The tree as it was before the last file restore, for `/undo-restore`.
    pub restore_undo: Option<String>,
    /// What titles it once saved.
    pub first_prompt: Option<String>,
    /// What the active harness's last turn used.
    pub turn_usage: Usage,
    /// Belongs to `turn_usage`, and is reset wherever it is set: the
    /// running turn, or the last one, has put its own numbers there. A
    /// report of one request adds to them where it is set; otherwise it
    /// starts them again.
    pub turn_reported: bool,
    /// The active harness's session's usage.
    pub session_usage: Usage,
    /// Context-window fill of the active harness's session.
    pub context: ContextUsage,
    /// `context.window` is one a session reported, not one filled from
    /// the context windows kept (`Controller::fill_context_window`): only
    /// such a one is saved, so that a filled one does not stand later in
    /// place of what the file knows by then.
    pub window_reported: bool,
    /// How long the last turn took.
    pub generation_duration: Option<Duration>,
}

impl ConversationState {
    /// `record` as it was saved, run on `active`. Its last turn's usage
    /// and context are taken by [`Controller::restore_context`], which
    /// knows the model it runs.
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
            turn_reported: false,
            session_usage: record.usage.get(&active).cloned().unwrap_or_default(),
            context: ContextUsage::default(),
            window_reported: false,
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
    /// Nothing of the one before is kept, nor which harness planned in it,
    /// nor a request of its waiting for an answer. Prompts held stay: they
    /// are the user's, not the conversation's.
    pub fn install(&mut self, record: Conversation, active: HarnessId) {
        self.conversation = ConversationState::new(record, active);
        self.policy.forget_plans();
        self.active = active;
        self.tracking.drop_waiting();
        self.forget_blocks();
        self.restore_context();
    }

    /// The active harness's last turn's usage and context as the
    /// conversation saved them. The fill was the session's: it is told
    /// again only for that session, still stored, on the model it was
    /// measured on; the window only for that model. What is not known is
    /// filled from the window the model last reported here.
    pub(super) fn restore_context(&mut self) {
        let active = self.active;
        let model = self.model_key(active);
        let c = &mut self.conversation;
        c.turn_usage = c
            .record
            .turn_usage
            .get(&active)
            .cloned()
            .unwrap_or_default();
        c.turn_reported = false;
        c.context = match c.record.context.get(&active) {
            Some(r) if r.model == model => ContextUsage {
                used: r.used.filter(|_| {
                    r.session.is_some() && r.session.as_ref() == c.session_ids.get(&active)
                }),
                window: r.window,
            },
            _ => ContextUsage::default(),
        };
        c.window_reported = c.context.window.is_some();
        self.fill_context_window();
    }

    /// A new conversation that keeps the models and efforts this one chose.
    pub fn successor(&self, active: HarnessId) -> Conversation {
        let mut next = Conversation::new(active);
        next.models = self.conversation.record.models.clone();
        next.efforts = self.conversation.record.efforts.clone();
        next
    }

    /// Usage starts again in a fork ([`Controller::fork`]): what it spends
    /// is its own. Its context is the session it goes on in: a branch
    /// knows what the original knew, so the fill stays, a fresh session's
    /// is not known until it reports. The window is the model's, and stays.
    pub fn reset_usage(&mut self) {
        let branched = self.fork_pending(self.active);
        let c = &mut self.conversation;
        c.anchor_pending.clear();
        c.session_usage = Usage::default();
        c.turn_usage = Usage::default();
        c.turn_reported = false;
        if !branched {
            c.context.used = None;
        }
    }

    /// Bring what is saved up to date. `active` runs the conversation, and
    /// its live session was sent something (`prompted`).
    pub fn sync_conversation(&mut self, active: HarnessId, prompted: bool) {
        let model = self.model_key(active);
        let c = &mut self.conversation;
        let r = &mut c.record;
        r.active_harness = active;
        r.sessions = c.session_ids.clone();
        r.bookmarks = c.last_active_index.clone();
        r.blocks = c.transcript.to_records();
        r.usage.insert(active, c.session_usage.clone());
        r.turn_usage.insert(active, c.turn_usage.clone());
        r.context.insert(
            active,
            ContextRecord {
                used: c.context.used,
                window: c.context.window.filter(|_| c.window_reported),
                model,
                session: c.session_ids.get(&active).cloned(),
            },
        );
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
    pub fn persist(&mut self) {
        let prompted = self.session_seen();
        if let Err(e) = self.save_conversation(self.active, prompted)
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
    /// (`Controller::take_conversation`), and its context as of the model
    /// that runs it now.
    pub fn take_conversation_choices(&mut self) {
        let record = self.conversation.record.clone();
        for notice in self.take_conversation(&record) {
            self.conversation.transcript.push_notice(notice);
        }
        self.restore_context();
    }

    /// Say which conversation was resumed, and whether `active` runs under
    /// another policy than it last did. `at_start`: by `--resume`.
    pub fn announce_resume(&mut self, active: HarnessId, at_start: bool) {
        let summary = self.conversation.record.summary();
        let name = self.short_name_of(active);
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
    /// to it starts, a fork of it to make, rewind anchors into it, how full
    /// its context was.
    pub fn forget_ids(&mut self, harness: HarnessId) {
        let active = self.active;
        let c = &mut self.conversation;
        c.session_ids.remove(&harness);
        c.last_active_index.remove(&harness);
        c.fork_pending.remove(&harness);
        c.anchors.retain(|a| a.harness != harness);
        // What its session compacted, it no longer holds.
        c.transcript.retire_compactions(harness);
        if harness == active {
            c.context.used = None;
        }
        if let Some(saved) = c.record.context.get_mut(&harness) {
            saved.used = None;
        }
    }

    /// Whether `harness`'s next session must branch the stored session id
    /// (it belongs to the conversation this one was forked from).
    pub fn fork_pending(&self, harness: HarnessId) -> bool {
        self.conversation.fork_pending.contains(&harness)
    }

    /// The turn `harness`'s oldest prompt still owed one was is `id` to it;
    /// an empty id means the turn never reached the harness. A compaction
    /// that waited for that anchor is put on the transcript now.
    pub fn turn_anchor(&mut self, harness: HarnessId, id: String) {
        let c = &mut self.conversation;
        if let Some((block, told_from)) = c.anchor_pending.pop_front()
            && !id.is_empty()
        {
            c.anchors
                .retain(|a| !(a.block == block && a.harness == harness));
            c.anchors.push(TurnAnchorRecord {
                block,
                harness,
                id,
                told_from: Some(told_from),
            });
        }
        let waiting: Vec<(usize, Vec<String>)> = self
            .conversation
            .transcript
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(at, b)| match b {
                Block::Compaction {
                    harness: h,
                    kept: KeptAt::Pending(ids),
                    ..
                } if *h == harness => Some((at, ids.clone())),
                _ => None,
            })
            .collect();
        for (at, ids) in waiting {
            let kept = self.turn_kept(&ids);
            self.set_kept(at, kept);
        }
    }

    /// The active harness compacted its context: a block for it, which
    /// folds the blocks before what it kept as it was, where that can be
    /// told for certain ([`Self::kept_at`]).
    pub fn compacted(&mut self, c: Compaction) {
        let kept = self.kept_at(&c.kept, self.conversation.transcript.blocks.len());
        let active = self.active;
        self.conversation
            .transcript
            .push_compaction(active, c, kept);
    }

    /// The harness said after its last compaction what that kept
    /// (`AgentEvent::CompactionKept`, pi from its entries). Only the
    /// active session's events come, and only while it runs; an answer
    /// that would come after another harness's compaction is not taken
    /// for it.
    pub fn compaction_kept(&mut self, kept: Kept) {
        let active = self.active;
        let blocks = &self.conversation.transcript.blocks;
        let Some(at) = current_compaction(blocks, active) else {
            return;
        };
        if blocks
            .iter()
            .rposition(|b| matches!(b, Block::Compaction { .. }))
            != Some(at)
        {
            return;
        }
        let kept = self.kept_at(&kept, at);
        self.set_kept(at, kept);
    }

    /// What the compaction at block `at` kept starts at `to`: never after
    /// it (a prompt pi took after a compaction that kept nothing, whose
    /// block is after the compaction's).
    fn set_kept(&mut self, at: usize, to: KeptAt) {
        if let Some(Block::Compaction { kept, .. }) =
            self.conversation.transcript.blocks.get_mut(at)
        {
            *kept = match to {
                KeptAt::Block(b) => KeptAt::Block(b.min(at)),
                to => to,
            };
        }
    }

    /// Where in the transcript what a compaction at block `at` of the
    /// active harness's `kept` starts. Certain only where the harness
    /// names a user turn by the id its anchor gave (`TurnAnchorRecord`),
    /// or keeps nothing: from that turn on, or from the oldest block its
    /// message told of besides it (what the harness had not seen, `!`
    /// commands), which the model has in that message. A turn whose
    /// anchor is still owed is waited for (pi lists it after the turn).
    /// Anything else (a message inside a turn the harness does not place,
    /// a turn of a session unharness did not see, an anchor saved before
    /// #359 that did not say what its message told) is not known. Nothing
    /// kept is all before it, but for what was sent into the turn running,
    /// which the harness may not have taken yet.
    pub fn kept_at(&self, kept: &Kept, at: usize) -> KeptAt {
        let blocks = &self.conversation.transcript.blocks[..at];
        match kept {
            Kept::Unknown => KeptAt::Unknown,
            Kept::Nothing if self.is_generating => {
                let prompt = blocks
                    .iter()
                    .rposition(|b| matches!(b, Block::User { steer: false, .. }))
                    .map_or(0, |p| p + 1);
                KeptAt::Block(
                    blocks[prompt..]
                        .iter()
                        .position(|b| matches!(b, Block::User { steer: true, .. }))
                        .map_or(at, |s| prompt + s),
                )
            }
            Kept::Nothing => KeptAt::Block(at),
            Kept::From(id) => self.turn_kept(std::slice::from_ref(id)),
            Kept::InTurnOf(ids) => self.turn_kept(ids),
        }
    }

    /// The turn of the latest of `ids` (latest first) that is one of the
    /// active harness's anchors: from the oldest block its message told
    /// of. One not known while an anchor is owed may be that one's: it is
    /// waited for.
    fn turn_kept(&self, ids: &[String]) -> KeptAt {
        let c = &self.conversation;
        for (n, id) in ids.iter().enumerate() {
            match c
                .anchors
                .iter()
                .find(|a| a.harness == self.active && a.id == *id)
            {
                Some(a) => return a.told_from.map_or(KeptAt::Unknown, KeptAt::Block),
                None if !c.anchor_pending.is_empty() => {
                    return KeptAt::Pending(ids[n..].to_vec());
                }
                None => {}
            }
        }
        KeptAt::Unknown
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
        if self.is_generating {
            self.conversation
                .transcript
                .push_error("finish or interrupt the current turn first");
            return;
        }
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
        // Each fill is told again only for a session still stored, the
        // branch to be made of it (`Controller::restore_context`).
        fork.context = self.conversation.record.context.clone();
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
                    c.transcript.retire_compactions(h);
                    fresh.push(h.as_str());
                }
            }
        }
        branched.sort_unstable();
        fresh.sort_unstable();
        (branched, fresh)
    }
}

/// Starting a new conversation and resuming a saved one.
impl Controller {
    /// Start a new conversation in this workspace, as a fresh launch would,
    /// keeping the harness and what the user chose to run it with. The one
    /// left is saved first and stays in `/resume`; its sessions stay with
    /// it, so every harness starts afresh in the new one.
    pub fn new_conversation(&mut self, front: &mut dyn Front) {
        if self.is_generating {
            self.conversation
                .transcript
                .push_error("finish or interrupt the current turn before clearing");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        // They would end with the session, and report into the new one.
        if self.refused_for_subagents("clearing") {
            return;
        }
        if self.shell.is_some() {
            self.conversation.transcript.push_error(
                "stop the ! command first (Esc); its output belongs to this conversation",
            );
            return;
        }
        // Nothing of it is dropped: one that cannot be saved stays.
        let prompted = self.session_seen();
        if let Err(e) = self.save_conversation(self.active, prompted) {
            self.conversation.transcript.push_error(format!(
                "could not save the conversation, so /clear keeps it: {e:#}"
            ));
            return;
        }
        let had_content = self.conversation.record.has_content();
        // A session started with nothing to resume and sent nothing knows
        // nothing of this conversation, and serves the next. Any other ends,
        // a blank one resumed on a session of this conversation too, and
        // one that refused a policy, so that the next is asked for it again
        // (its process may have been started with what refused it).
        let keep = self.session_alive
            && self.session_blank
            && !self.conversation.session_ids.contains_key(&self.active)
            && !self.session_refused();
        self.end_remote_control();
        let said = self.conversation.transcript.blocks.len();
        if self.session_alive && !keep {
            self.shutdown_session(front);
        }
        // What ending it told (a change to the harness's configuration) is
        // told in the new conversation.
        let told = self.conversation.transcript.blocks.split_off(said);
        self.idle_start_paused = false;
        let next = self.successor(self.active);
        self.install(next, self.active);
        front.conversation_installed();
        self.conversation.transcript.push_system(if had_content {
            "New conversation; the previous one is in /resume."
        } else {
            "New conversation."
        });
        self.conversation.transcript.blocks.extend(told);
        // Prompts held after a turn that did not finish are the user's, not
        // the conversation's.
        if !self.queued.is_empty() {
            self.conversation.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// Replace the current state with a saved conversation.
    pub fn resume_conversation(&mut self, id_or_prefix: String, front: &mut dyn Front) {
        if self.is_generating {
            self.conversation
                .transcript
                .push_error("finish or interrupt the current turn before resuming");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        let conv = match self.store.load(&id_or_prefix) {
            Ok(c) => c,
            Err(e) => {
                self.conversation
                    .transcript
                    .push_error(format!("could not resume: {e}"));
                return;
            }
        };
        // Another conversation, perhaps on another harness.
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session(front);
        }
        self.idle_start_paused = false;
        self.persist();
        let active = if self.registry().get(conv.active_harness).is_some() {
            conv.active_harness
        } else {
            self.active
        };
        self.install(conv, active);
        front.conversation_installed();
        self.take_conversation_choices();
        self.announce_resume(self.active, false);
        if let Some(w) = self.policy_warning() {
            self.conversation.transcript.push_notice(w);
        }
        self.require_policy(front);
    }
}
