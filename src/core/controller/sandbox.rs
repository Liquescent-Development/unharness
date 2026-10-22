//! The sandbox level the active harness's processes run at, and the paths
//! the user grants it beyond its profile.

use std::path::{Component, Path, PathBuf};

use super::{Controller, Front};
use crate::config::Config;
use crate::core::rules::Scope;
use serde_json::Value;

use crate::core::sandbox::grants::{grant_target, tilde};
use crate::core::sandbox::{self, Access, Grant, Sandbox, SandboxLevel, denials};

/// Where a path granted from the TUI is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantScope {
    /// For this run only.
    Run,
    /// In the workspace's or the global settings, read again by every run.
    Saved(Scope),
}

/// A line the grant dialog shows beside the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantNote {
    pub text: String,
    pub kind: NoteKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Info,
    /// The grant will not go as it stands.
    Caution,
    /// It would let what runs in the sandbox act outside it.
    Danger,
}

impl GrantNote {
    fn info(text: impl Into<String>) -> Self {
        GrantNote {
            text: text.into(),
            kind: NoteKind::Info,
        }
    }

    fn caution(text: impl Into<String>) -> Self {
        GrantNote {
            text: text.into(),
            kind: NoteKind::Caution,
        }
    }

    fn danger(text: impl Into<String>) -> Self {
        GrantNote {
            text: text.into(),
            kind: NoteKind::Danger,
        }
    }
}

/// The offer to grant what the sandbox kept a call from (`Front::
/// offer_grant`).
#[derive(Debug, Clone)]
pub struct GrantOffer {
    pub blocked: denials::Blocked,
    /// What the agent tried, in a sentence.
    pub said: String,
    /// What would be granted (`grants::propose`), and whether it is a
    /// folder to be made.
    pub path: PathBuf,
    pub make: bool,
    pub notes: Vec<GrantNote>,
    /// There is a workspace, so it can be kept for it.
    pub has_workspace: bool,
}

impl GrantScope {
    /// Its row in the dialog.
    pub fn title(self) -> &'static str {
        match self {
            GrantScope::Run => "Until I quit",
            GrantScope::Saved(Scope::Workspace) => "In this project",
            GrantScope::Saved(Scope::Global) => "Everywhere",
        }
    }

    /// What its row says of it.
    pub fn detail(self) -> &'static str {
        match self {
            GrantScope::Run => "this session only",
            GrantScope::Saved(Scope::Workspace) => "saved in this project's settings",
            GrantScope::Saved(Scope::Global) => "saved in your global settings",
        }
    }

    /// How long it lasts, at the end of a sentence.
    pub fn phrase(self) -> &'static str {
        match self {
            GrantScope::Run => "until you quit",
            GrantScope::Saved(Scope::Workspace) => "in this project",
            GrantScope::Saved(Scope::Global) => "everywhere",
        }
    }

    /// Each scope, in the order the dialog lists them.
    pub fn all(has_workspace: bool) -> Vec<GrantScope> {
        let mut all = vec![GrantScope::Run];
        if has_workspace {
            all.push(GrantScope::Saved(Scope::Workspace));
        }
        all.push(GrantScope::Saved(Scope::Global));
        all
    }
}

impl Controller {
    /// The sandbox level the active harness runs at, and why it is off if
    /// that was not asked for.
    pub fn sandbox_level(&self) -> (SandboxLevel, Option<String>) {
        self.sandbox.level(self.harness().default_sandbox())
    }

    /// What confines the next process of the active harness.
    pub fn session_sandbox(&self) -> anyhow::Result<Sandbox> {
        crate::runner::session_sandbox(
            self.harness(),
            &self.sandbox,
            &self.config,
            self.workspace_root.as_deref().unwrap_or(&self.cwd),
        )
    }

    /// Choose the sandbox level for the run. Returns whether it could be
    /// set. The sandbox is applied when a process is spawned, so a live
    /// session is shut down and comes back under the new level, resumed
    /// where the harness can.
    pub fn set_sandbox(&mut self, level: SandboxLevel, front: &mut dyn Front) -> bool {
        if self.is_generating {
            self.conversation
                .transcript
                .push_error("finish or interrupt the current turn before changing the sandbox");
            return false;
        }
        if let Err(why) = &self.sandbox.backend
            && level != SandboxLevel::Off
        {
            self.conversation
                .transcript
                .push_error(format!("sandbox '{level}' is unavailable: {why}"));
            return false;
        }
        self.sandbox.explicit = Some(level);
        self.idle_start_paused = false;
        let restart = self.session_alive && self.session_sandbox_level != Some(level);
        self.conversation.transcript.push_system(if restart {
            format!(
                "Sandbox: {level} ({} restarts under it)",
                self.display_name()
            )
        } else {
            format!("Sandbox: {level}")
        });
        if restart {
            self.shutdown_session(front);
        }
        true
    }

    /// The sandbox levels there is a backend for.
    pub fn available_sandbox_levels(&self) -> Vec<SandboxLevel> {
        SandboxLevel::ALL
            .into_iter()
            .filter(|l| self.sandbox.backend.is_ok() || *l == SandboxLevel::Off)
            .collect()
    }

    /// Grant `path` beyond the sandbox's profile, `scope` saying where it
    /// is kept. Returns whether it was. The user grants it, outside the
    /// sandbox. A directory to write that does not exist yet is made,
    /// empty, since the sandbox grants only what exists (and removed again
    /// when the grant fails); the profile the grant makes has to be one
    /// that can be set up, also on the next run when it is saved. Like a
    /// level, it reaches the next process: a live session restarts under
    /// it now, or when the turn running ends.
    pub fn grant_sandbox(
        &mut self,
        path: &Path,
        access: Access,
        scope: GrantScope,
        make: bool,
        front: &mut dyn Front,
    ) -> bool {
        if let Err(why) = &self.sandbox.backend {
            self.conversation.transcript.push_error(format!(
                "there is no sandbox here to grant a path in: {why}"
            ));
            return false;
        }
        // What it reaches is writable already: nothing is made.
        if let Some(reach) = grant_target(path)
            && access == Access::Write
            && self.sandbox_allows(&reach, access)
        {
            self.conversation.transcript.push_system(format!(
                "{} is {access} in the sandbox already",
                path.display()
            ));
            return true;
        }
        let made = match make_granted(path, access, make) {
            Ok(made) => made,
            Err(e) => {
                self.conversation
                    .transcript
                    .push_error(format!("could not grant {}: {e:#}", path.display()));
                return false;
            }
        };
        let granted = match path.canonicalize() {
            Ok(path) => self.grant_existing(path, access, scope, front),
            Err(_) => {
                self.conversation.transcript.push_error(format!(
                    "{} does not exist: grant the directory it would be made in",
                    path.display()
                ));
                false
            }
        };
        if !granted {
            unmake(&made);
        }
        granted
    }

    fn grant_existing(
        &mut self,
        path: PathBuf,
        access: Access,
        scope: GrantScope,
        front: &mut dyn Front,
    ) -> bool {
        if self.sandbox_allows(&path, access) {
            self.conversation.transcript.push_system(format!(
                "{} is {access} in the sandbox already",
                path.display()
            ));
            return true;
        }
        let mut setup = self.sandbox.clone();
        let mut config = self.config.clone();
        match scope {
            GrantScope::Run => setup.grants.push(Grant {
                path: path.clone(),
                access,
            }),
            GrantScope::Saved(s) => add_path(&mut config, &path, access, s == Scope::Workspace),
        }
        if let Err(e) = self.check_grant(&setup, &config) {
            self.conversation
                .transcript
                .push_error(format!("could not grant {}: {e:#}", path.display()));
            return false;
        }
        let mut kept = String::new();
        if let GrantScope::Saved(s) = scope {
            match self.save_grant(&path, access, s) {
                Ok(file) => kept = format!(" (saved in {})", tilde(&file)),
                Err(e) => {
                    self.conversation
                        .transcript
                        .push_error(format!("could not save {}: {e:#}", path.display()));
                    return false;
                }
            }
        }
        self.sandbox = setup;
        self.config = config;
        let name = self.short_name();
        let verb = match access {
            Access::Write => "write",
            Access::Read => "read",
        };
        let granted = format!(
            "Sandbox: {name} may now {verb} in {} {}{kept}",
            tilde(&path),
            scope.phrase()
        );
        let message = if self.sandbox_level().0 == SandboxLevel::Off {
            format!("{granted}. The sandbox is off right now.")
        } else if !self.session_alive {
            format!("{granted}.")
        } else if self.is_generating {
            self.sandbox_restart_due = true;
            format!("{granted}. {name} restarts when this turn ends.")
        } else {
            self.shutdown_session(front);
            format!("{granted}. {name} restarted and continues where it left off.")
        };
        self.conversation.transcript.push_system(message);
        true
    }

    /// What `/sandbox paths` shows: what the next process may write and
    /// may not read, what was granted for the run, and where the rest is
    /// kept.
    pub fn sandbox_report(&self) -> String {
        let (level, why) = self.sandbox_level();
        let mut out = format!("Sandbox: {level}");
        if let Some(why) = why {
            out.push_str(&format!(" ({why})"));
        }
        let list = |out: &mut String, title: &str, paths: &mut dyn Iterator<Item = String>| {
            let mut paths = paths.peekable();
            if paths.peek().is_some() {
                out.push_str(&format!("\n{title}:"));
                for p in paths {
                    out.push_str(&format!("\n• {p}"));
                }
            }
        };
        match self.session_sandbox() {
            Ok(Sandbox::Active { profile, .. }) => {
                let shown = |p: &PathBuf| p.display().to_string();
                list(
                    &mut out,
                    "Writable",
                    &mut profile.writable.iter().map(shown),
                );
                list(
                    &mut out,
                    "Not readable",
                    &mut profile.deny_read.iter().map(shown),
                );
                // What stays readable inside those.
                let inside = profile
                    .allow_read
                    .iter()
                    .filter(|a| profile.deny_read.iter().any(|d| a.starts_with(d)));
                list(&mut out, "Readable inside those", &mut inside.map(shown));
            }
            Ok(Sandbox::Off { .. }) => {}
            Err(e) => out.push_str(&format!("\ncannot be set up: {e:#}")),
        }
        list(
            &mut out,
            "Granted for this run",
            &mut self
                .sandbox
                .grants
                .iter()
                .map(|g| format!("{} ({})", g.path.display(), g.access)),
        );
        if let Some(dir) = self.tracking.rules.config_dir() {
            let root = self.workspace_root.as_deref();
            let files = [Scope::Workspace, Scope::Global]
                .into_iter()
                .filter_map(|s| Config::scoped_path(dir, s, root))
                .map(|p| p.display().to_string());
            list(
                &mut out,
                "Kept under [sandbox] writable and readable in",
                &mut files.into_iter(),
            );
        }
        out.push_str("\nGrant a path with /sandbox allow <path> or /sandbox allow-read <path>.");
        out
    }

    /// What the sandbox kept tool call `id` from, as its result tells
    /// (`sandbox::denials`), judged against what confines the process that
    /// ran it: each path is told once a run for each harness, and offered
    /// to the user once the turn has ended; one that cannot be allowed is
    /// only told.
    pub(super) fn notice_blocked(&mut self, id: &str, output: &str, is_error: bool) {
        let (name, input) = self
            .tool_inputs
            .remove(id)
            .unwrap_or_else(|| (String::new(), Value::Null));
        let candidates = denials::candidates(&input, output, is_error);
        if candidates.is_empty() {
            return;
        }
        let Some(profile) = &self.session_profile else {
            return;
        };
        let home = dirs::home_dir();
        let blocked = denials::deepest(denials::blocked(
            &candidates,
            profile,
            &self.cwd,
            home.as_deref(),
            denials::writes(&name),
            denials::may_access,
        ));
        for b in blocked {
            if !self.sandbox_told.insert((self.active, b.clone())) {
                continue;
            }
            let said = b.describe(self.short_name());
            if !b.grantable {
                self.conversation.transcript.push_notice(format!(
                    "Sandbox: {said} It can't be allowed: {} holds unharness's own settings. \
                     /sandbox off turns the sandbox off.",
                    tilde(&b.grant().0)
                ));
                continue;
            }
            self.conversation.transcript.push_notice(format!(
                "Sandbox: {said} You can allow it when the turn ends."
            ));
            self.sandbox_blocked.push(b);
        }
    }

    /// Offer the next path the sandbox kept a call from, while no turn
    /// runs and no other dialog is open. One a grant made meanwhile
    /// reaches is not offered.
    pub(super) fn offer_grants(&mut self, front: &mut dyn Front) {
        if self.is_generating || front.dialog_open() {
            return;
        }
        while let Some(b) = self.sandbox_offers.pop_front() {
            let reach = grant_target(&b.path).unwrap_or_else(|| b.path.clone());
            if self.sandbox_allows(&reach, b.access) {
                continue;
            }
            let (path, make) = b.grant();
            front.offer_grant(GrantOffer {
                said: b.describe(self.short_name()),
                notes: self.grant_notes(&path, b.access, make),
                path,
                make,
                has_workspace: self.workspace_root.is_some(),
                blocked: b,
            });
            return;
        }
    }

    /// The live session's process started: what confines it is what its
    /// refusals are told against.
    pub(crate) fn sandbox_started(&mut self) {
        self.session_profile = match self.session_sandbox() {
            Ok(Sandbox::Active { profile, .. }) => Some(profile),
            _ => None,
        };
    }

    /// The turn ended, also when its process was ended for it: what the
    /// sandbox kept it from is offered.
    pub(super) fn blocks_turn_ended(&mut self) {
        self.tool_inputs.clear();
        self.sandbox_offers.extend(self.sandbox_blocked.drain(..));
    }

    /// Another harness or conversation takes over: what the sandbox kept
    /// the one before from is not offered to it.
    pub(super) fn forget_blocks(&mut self) {
        self.tool_inputs.clear();
        self.sandbox_blocked.clear();
        self.sandbox_offers.clear();
    }

    /// What the dialog says beside `path` before it is granted for
    /// `access`: whether it exists (or is made, `make`), whether the
    /// sandbox reaches it already, and what granting it would open
    /// (`grant_risk`). Worked out once for a path, not each time the
    /// dialog is drawn.
    pub fn grant_notes(&self, path: &Path, access: Access, make: bool) -> Vec<GrantNote> {
        let name = self.short_name();
        let verb = match access {
            Access::Write => "write",
            Access::Read => "read",
        };
        let reach = grant_target(path);
        let mut notes = Vec::new();
        match path.canonicalize() {
            Err(_) if access == Access::Read || reach.is_none() => {
                notes.push(GrantNote::caution("That path doesn't exist."))
            }
            Err(_) if make => notes.push(GrantNote::info(
                "This folder doesn't exist yet. Allowing creates it.",
            )),
            Err(_) => notes.push(GrantNote::caution(
                "That path doesn't exist. End it with / to create it as a folder.",
            )),
            Ok(p) if self.sandbox_allows(&p, access) => {
                notes.push(GrantNote::info(format!("{name} can already {verb} there.")))
            }
            Ok(_) => {}
        }
        let checked = path.canonicalize().ok().or_else(|| {
            let base = path.ancestors().find(|a| a.exists())?;
            Some(reach?.join(path.strip_prefix(base).ok()?))
        });
        if let Some(risk) = checked.and_then(|p| self.grant_risk(&p, access)) {
            notes.push(GrantNote::danger(format!("⚠ {risk}")));
        }
        notes
    }

    /// The session restarts under paths granted during the turn that ended.
    pub(super) fn restart_for_grants(&mut self, front: &mut dyn Front) {
        if std::mem::take(&mut self.sandbox_restart_due) && self.session_alive {
            self.shutdown_session(front);
        }
    }

    /// Whether the next process could already reach `path` as `access`
    /// says: write it, or read it.
    /// When the sandbox is off, as it would be at the harness's default.
    pub fn sandbox_allows(&self, path: &Path, access: Access) -> bool {
        let Ok(Sandbox::Active { profile, .. }) =
            self.grant_sandbox_of(&self.sandbox, &self.config)
        else {
            return true;
        };
        match access {
            Access::Write => profile.writable.iter().any(|w| path.starts_with(w)),
            Access::Read => {
                profile.allow_read.iter().any(|a| path.starts_with(a))
                    || !profile
                        .deny_read
                        .iter()
                        .any(|d| path.starts_with(d) || d.starts_with(path))
            }
        }
    }

    /// Why granting `path` reaches beyond what the sandbox is for, if it
    /// does (`sandbox::grants::grant_risk`).
    pub fn grant_risk(&self, path: &Path, access: Access) -> Option<String> {
        let env = sandbox::SandboxEnv::current();
        if env
            .protected
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .any(|p| p.starts_with(path) || path.starts_with(&p))
        {
            return Some("It holds unharness's own settings, so it can't be allowed.".into());
        }
        let workspace = self.workspace_root.as_deref().unwrap_or(&self.cwd);
        let guarded: Vec<PathBuf> = self
            .harness()
            .guarded(workspace)
            .iter()
            .map(|g| g.path().to_path_buf())
            .collect();
        sandbox::grants::grant_risk(
            path,
            access,
            &env,
            &sandbox::grants::search_path(),
            &guarded,
        )
    }

    /// Whether a sandbox can be set up with `setup` and `config`: at the
    /// level chosen, or the harness's default when it is off now, so that
    /// a grant made then is checked too.
    fn check_grant(&self, setup: &sandbox::SandboxSetup, config: &Config) -> anyhow::Result<()> {
        self.grant_sandbox_of(setup, config).map(drop)
    }

    fn grant_sandbox_of(
        &self,
        setup: &sandbox::SandboxSetup,
        config: &Config,
    ) -> anyhow::Result<Sandbox> {
        let mut setup = setup.clone();
        if setup.explicit == Some(SandboxLevel::Off) {
            setup.explicit = None;
        }
        let workspace = self.workspace_root.as_deref().unwrap_or(&self.cwd);
        crate::runner::session_sandbox(self.harness(), &setup, config, workspace)
    }

    /// Add `path` to the settings of `scope`, once the next run is known
    /// to take it: the global settings and the workspace's merged as a
    /// launch merges them, with the path, have to make a sandbox.
    fn save_grant(&self, path: &Path, access: Access, scope: Scope) -> anyhow::Result<PathBuf> {
        let dir = self
            .tracking
            .rules
            .config_dir()
            .ok_or_else(|| anyhow::anyhow!("there is no config directory"))?;
        let root = self.workspace_root.as_deref();
        let file = |s: Scope| Config::scoped_path(dir, s, root);
        let target = file(scope).ok_or_else(|| anyhow::anyhow!("there is no workspace"))?;
        let mut global = Config::load_file(&dir.join("config.toml"))?.unwrap_or_default();
        let mut local = match file(Scope::Workspace) {
            Some(f) => Config::load_file(&f)?.unwrap_or_default(),
            None => Config::default(),
        };
        // The merge lifts a global `deny_read` the workspace's names.
        add_path(
            match scope {
                Scope::Global => &mut global,
                Scope::Workspace => &mut local,
            },
            path,
            access,
            false,
        );
        let mut setup = self.sandbox.clone();
        setup.grants.clear();
        self.check_grant(&setup, &Config::merge(global, local))?;
        Config::add_sandbox_path(&target, access, path)?;
        Ok(target)
    }
}

/// `path` in `config`'s `[sandbox]` list for `access`. With `lift`, one
/// made readable is no longer denied, as a workspace's `readable` lifts a
/// global `deny_read` of the same path when the two are merged.
fn add_path(config: &mut Config, path: &Path, access: Access, lift: bool) {
    match access {
        Access::Write => config.sandbox.writable.push(path.to_path_buf()),
        Access::Read => {
            config.sandbox.readable.push(path.to_path_buf());
            if !lift {
                return;
            }
            let home = dirs::home_dir();
            config.sandbox.deny_read.retain(|d| {
                let denied = sandbox::expand_home(d, home.as_deref());
                denied.canonicalize().unwrap_or(denied) != path
            });
        }
    }
}

/// Make the directories `path` names that do not exist yet, to be
/// granted to write, when the user chose to (`make`: the folder offered,
/// or a path ending in `/`), and return them innermost first. Never in or around
/// unharness's own directories.
fn make_granted(path: &Path, access: Access, make: bool) -> anyhow::Result<Vec<PathBuf>> {
    if !make || access == Access::Read || !path.is_absolute() || path.exists() {
        return Ok(Vec::new());
    }
    let missing: Vec<&Path> = path.ancestors().take_while(|a| !a.exists()).collect();
    if missing.iter().any(|a| {
        a.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::RootDir))
    }) {
        anyhow::bail!("it is not a plain path");
    }
    let Some(existing) = grant_target(path) else {
        anyhow::bail!("nothing of it exists");
    };
    let rest = path.strip_prefix(path.ancestors().find(|a| a.exists()).unwrap_or(path))?;
    let target = existing.join(rest);
    let protected = sandbox::SandboxEnv::current().protected;
    if let Some(p) = protected
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .find(|p| target.starts_with(p) || p.starts_with(&target))
    {
        anyhow::bail!("it is in or around unharness's own state ({})", p.display());
    }
    let mut made = Vec::new();
    for dir in missing.iter().rev() {
        if let Err(e) = std::fs::create_dir(dir) {
            unmake(&made);
            anyhow::bail!("could not make {}: {e}", dir.display());
        }
        made.insert(0, dir.to_path_buf());
    }
    Ok(made)
}

/// Remove the directories `make_granted` made, innermost first: nothing
/// is in them.
fn unmake(made: &[PathBuf]) {
    for dir in made {
        let _ = std::fs::remove_dir(dir);
    }
}
