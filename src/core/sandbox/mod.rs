//! The OS-level sandbox unharness puts around every harness process.
//!
//! A harness declares the directories its CLI writes to (`SandboxPaths`);
//! `resolve` turns that, the level and the user's extra paths into a
//! `SandboxProfile`; a `SandboxBackend` applies the profile to a command just
//! before it is spawned. The network is left open.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub mod linux;
pub mod seatbelt;

/// How far a harness process is confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLevel {
    /// No writes outside the harness's own state and temp directories.
    ReadOnly,
    /// Writes only inside the workspace, plus the harness's own state and
    /// temp directories.
    WorkspaceWrite,
    Off,
}

impl SandboxLevel {
    pub const ALL: [SandboxLevel; 3] = [
        SandboxLevel::ReadOnly,
        SandboxLevel::WorkspaceWrite,
        SandboxLevel::Off,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" | "ro" => Some(SandboxLevel::ReadOnly),
            "workspace-write" | "workspace" | "ws-write" => Some(SandboxLevel::WorkspaceWrite),
            "off" | "none" | "full-access" => Some(SandboxLevel::Off),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxLevel::ReadOnly => "read-only",
            SandboxLevel::WorkspaceWrite => "workspace-write",
            SandboxLevel::Off => "off",
        }
    }

    /// One line for the picker and the help.
    pub fn description(&self) -> &'static str {
        match self {
            SandboxLevel::ReadOnly => "No writes outside the harness's own state directories",
            SandboxLevel::WorkspaceWrite => "Writes only inside the workspace",
            SandboxLevel::Off => "No confinement",
        }
    }
}

impl fmt::Display for SandboxLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SandboxLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SandboxLevel::parse(s).ok_or_else(|| {
            format!("Unknown sandbox level '{s}'. Supported: read-only, workspace-write, off")
        })
    }
}

/// What a harness's CLI writes outside the workspace: sessions, credentials,
/// logs. Declared in `src/harness/<name>/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPaths {
    pub writable: Vec<PathBuf>,
}

/// Credential locations no harness process may read, relative to the home
/// directory. A harness's own writable paths and `[sandbox].readable` are
/// exempt. `~/.ssh` is not here: git over ssh and commit signing need it.
pub const DEFAULT_DENY_READ: &[&str] = &[
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".config/gh",
    ".config/gcloud",
    ".config/git/credentials",
    ".bash_history",
    ".zsh_history",
    ".local/share/fish/fish_history",
];

/// The machine as the sandbox sees it. Tests build their own.
#[derive(Debug, Clone, Default)]
pub struct SandboxEnv {
    pub home: Option<PathBuf>,
    /// What every harness may write: temp and device directories, and the
    /// runtime directory of herdr, which agents are commonly run under.
    pub scratch: Vec<PathBuf>,
    /// unharness's own state and configuration: never writable.
    pub protected: Vec<PathBuf>,
}

impl SandboxEnv {
    pub fn current() -> Self {
        let mut scratch = vec![
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/dev"),
            std::env::temp_dir(),
        ];
        if let Some(run) = dirs::runtime_dir() {
            scratch.push(run.join("herdr-a2a"));
        }
        let protected = [
            dirs::state_dir().or_else(dirs::data_local_dir),
            dirs::config_dir(),
        ]
        .into_iter()
        .flatten()
        .map(|d| d.join("unharness"))
        .collect();
        SandboxEnv {
            home: dirs::home_dir(),
            scratch,
            protected,
        }
    }
}

/// A fully resolved confinement: every path is canonical and exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    /// Never `Off`.
    pub level: SandboxLevel,
    pub workspace: PathBuf,
    /// Everything the process may write, the workspace included at
    /// `WorkspaceWrite`.
    pub writable: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
    /// Readable although inside a denied path: the workspace, the writable
    /// paths and `[sandbox].readable`.
    pub allow_read: Vec<PathBuf>,
    /// Kept unwritable even inside a writable path, where the backend can.
    pub protected: Vec<PathBuf>,
}

/// Applies a profile to a command that is about to be spawned.
pub trait SandboxBackend: Send + Sync + fmt::Debug {
    fn name(&self) -> &'static str;
    /// One line for `doctor`, e.g. the kernel interface version.
    fn detail(&self) -> String;
    /// Return the command to spawn instead: the same one with a hook
    /// installed, or the sandbox launcher wrapped around it.
    fn wrap(&self, cmd: Command, profile: &SandboxProfile) -> Result<Command>;
}

/// The backend for this platform, or why there is none.
pub fn detect() -> std::result::Result<Arc<dyn SandboxBackend>, String> {
    #[cfg(target_os = "linux")]
    {
        linux::Landlock::detect().map(|b| Arc::new(b) as Arc<dyn SandboxBackend>)
    }
    #[cfg(target_os = "macos")]
    {
        seatbelt::Seatbelt::detect().map(|b| Arc::new(b) as Arc<dyn SandboxBackend>)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err("no sandbox backend for this platform".to_string())
    }
}

/// What travels with a session to every place it spawns its process.
#[derive(Debug, Clone)]
pub enum Sandbox {
    Off {
        /// Set when the sandbox was wanted but no backend could provide it.
        unavailable: Option<String>,
        /// The level that was wanted: `Off` when off by choice, otherwise
        /// what no backend could provide. A harness with a sandbox of its
        /// own holds to it instead.
        wanted: SandboxLevel,
    },
    Active {
        backend: Arc<dyn SandboxBackend>,
        profile: SandboxProfile,
    },
}

impl Default for Sandbox {
    fn default() -> Self {
        Sandbox::off()
    }
}

impl Sandbox {
    /// No confinement, by choice.
    pub fn off() -> Self {
        Sandbox::Off {
            unavailable: None,
            wanted: SandboxLevel::Off,
        }
    }

    /// The level unharness enforces: `Off` whenever the process runs
    /// unconfined, also when that was not what was wanted.
    pub fn level(&self) -> SandboxLevel {
        match self {
            Sandbox::Off { .. } => SandboxLevel::Off,
            Sandbox::Active { profile, .. } => profile.level,
        }
    }

    /// The level the user asked for, whether or not unharness enforces it.
    pub fn wanted(&self) -> SandboxLevel {
        match self {
            Sandbox::Off { wanted, .. } => *wanted,
            Sandbox::Active { profile, .. } => profile.level,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Sandbox::Active { .. })
    }

    /// Why the process runs unconfined although the sandbox was wanted.
    pub fn warning(&self) -> Option<String> {
        match self {
            Sandbox::Off {
                unavailable: Some(why),
                ..
            } => Some(format!("sandbox unavailable, running unconfined: {why}")),
            _ => None,
        }
    }

    pub fn wrap(&self, cmd: Command) -> Result<Command> {
        match self {
            Sandbox::Off { .. } => Ok(cmd),
            Sandbox::Active { backend, profile } => backend.wrap(cmd, profile),
        }
    }
}

/// What the user asked for and what the platform offers, fixed at launch.
#[derive(Debug, Clone)]
pub struct SandboxSetup {
    /// The level from the flag or the config, if any.
    pub explicit: Option<SandboxLevel>,
    pub backend: std::result::Result<Arc<dyn SandboxBackend>, String>,
}

impl SandboxSetup {
    pub fn detect(explicit: Option<SandboxLevel>) -> Self {
        SandboxSetup {
            explicit,
            backend: detect(),
        }
    }

    /// Sandbox off by choice; what tests use.
    pub fn off() -> Self {
        SandboxSetup {
            explicit: Some(SandboxLevel::Off),
            backend: Err("off".to_string()),
        }
    }

    /// The level a session gets when `default` is its harness's, and why it
    /// is off if that is not what was wanted.
    pub fn level(&self, default: SandboxLevel) -> (SandboxLevel, Option<String>) {
        let level = self.explicit.unwrap_or(default);
        match &self.backend {
            Err(why) if level != SandboxLevel::Off => (
                SandboxLevel::Off,
                Some(format!("sandbox unavailable, running unconfined: {why}")),
            ),
            _ => (level, None),
        }
    }
}

/// Everything `resolve` needs to know about one session.
#[derive(Debug, Clone)]
pub struct SandboxRequest<'a> {
    /// The level the user set (flag, config or the TUI), if any.
    pub explicit: Option<SandboxLevel>,
    /// The harness's default, used otherwise.
    pub default: SandboxLevel,
    pub workspace: &'a Path,
    pub harness: &'a SandboxPaths,
    /// `[sandbox].writable`.
    pub extra_writable: &'a [PathBuf],
    /// `[sandbox].readable`: exemptions from `DEFAULT_DENY_READ`, and paths
    /// that stay readable inside a `deny_read` one.
    pub extra_readable: &'a [PathBuf],
    /// `[sandbox].deny_read`: more paths no harness process may read.
    pub extra_deny_read: &'a [PathBuf],
}

/// Decide what confines a session.
///
/// A level the user asked for fails when no backend exists; the default
/// degrades to `Off` with the reason, which callers show to the user.
pub fn resolve(
    req: &SandboxRequest,
    backend: &std::result::Result<Arc<dyn SandboxBackend>, String>,
    env: &SandboxEnv,
) -> Result<Sandbox> {
    let level = req.explicit.unwrap_or(req.default);
    if level == SandboxLevel::Off {
        return Ok(Sandbox::off());
    }
    let backend = match backend {
        Ok(b) => b.clone(),
        Err(why) if req.explicit.is_some() => {
            bail!("sandbox '{level}' was requested but is unavailable: {why}")
        }
        Err(why) => {
            return Ok(Sandbox::Off {
                unavailable: Some(why.clone()),
                wanted: level,
            });
        }
    };
    Ok(Sandbox::Active {
        backend,
        profile: profile(level, req, env)?,
    })
}

fn profile(level: SandboxLevel, req: &SandboxRequest, env: &SandboxEnv) -> Result<SandboxProfile> {
    let workspace = canonical(req.workspace).unwrap_or_else(|| req.workspace.to_path_buf());
    let expand = |p: &Path| workspace.join(expand_home(p, env.home.as_deref()));
    let protected: Vec<PathBuf> = env.protected.iter().filter_map(|p| canonical(p)).collect();

    for extra in req.extra_writable {
        let Some(path) = canonical(&expand(extra)) else {
            continue;
        };
        if let Some(hit) = protected.iter().find(|p| p.starts_with(&path)) {
            bail!(
                "[sandbox].writable path {} contains unharness's own state ({})",
                path.display(),
                hit.display()
            );
        }
    }

    if level == SandboxLevel::WorkspaceWrite
        && let Some(hit) = protected.iter().find(|p| p.starts_with(&workspace))
    {
        bail!(
            "the workspace {} contains unharness's own state ({}); run from a project \
             directory, or with --sandbox read-only or --sandbox off",
            workspace.display(),
            hit.display()
        );
    }
    let mut writable: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if let Some(c) = canonical(&p)
            && !writable.contains(&c)
        {
            writable.push(c);
        }
    };
    if level == SandboxLevel::WorkspaceWrite {
        add(workspace.clone());
    }
    for p in &req.harness.writable {
        add(expand(p));
    }
    for p in &env.scratch {
        add(p.clone());
    }
    for p in req.extra_writable {
        add(expand(p));
    }

    let readable: Vec<PathBuf> = req
        .extra_readable
        .iter()
        .filter_map(|p| canonical(&expand(p)))
        .collect();
    let mut deny_read: Vec<PathBuf> = Vec::new();
    if let Some(home) = &env.home {
        for rel in DEFAULT_DENY_READ {
            let Some(path) = canonical(&home.join(rel)) else {
                continue;
            };
            // A writable path grants reading beneath it, and one beneath a
            // denied directory would reopen it on Linux: both lift the deny.
            let exempt = readable
                .iter()
                .chain(&writable)
                .any(|r| path.starts_with(r) || r.starts_with(&path));
            if !exempt && !deny_read.contains(&path) {
                deny_read.push(path);
            }
        }
    }

    let mut allow_read = vec![workspace.clone()];
    for p in writable.iter().chain(&readable) {
        if !allow_read.contains(p) {
            allow_read.push(p.clone());
        }
    }
    for extra in req.extra_deny_read {
        let Some(path) = canonical(&expand(extra)) else {
            continue;
        };
        // Reading is granted per subtree, so a path inside a readable one
        // cannot be taken out of it again.
        if let Some(around) = allow_read.iter().find(|a| path.starts_with(a)) {
            bail!(
                "[sandbox].deny_read path {} cannot be enforced: it is inside {}, which the \
                 harness may read{}",
                path.display(),
                around.display(),
                if writable.contains(around) {
                    " and write"
                } else {
                    ""
                }
            );
        }
        if !deny_read.contains(&path) {
            deny_read.push(path);
        }
    }

    Ok(SandboxProfile {
        level,
        workspace,
        writable,
        deny_read,
        allow_read,
        protected,
    })
}

fn canonical(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

fn expand_home(p: &Path, home: Option<&Path>) -> PathBuf {
    match (p.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Null;
    impl SandboxBackend for Null {
        fn name(&self) -> &'static str {
            "null"
        }
        fn detail(&self) -> String {
            String::new()
        }
        fn wrap(&self, cmd: Command, _profile: &SandboxProfile) -> Result<Command> {
            Ok(cmd)
        }
    }

    fn available() -> std::result::Result<Arc<dyn SandboxBackend>, String> {
        Ok(Arc::new(Null))
    }

    /// A home with a workspace, a harness state dir, credentials and
    /// unharness's own state.
    struct World {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        workspace: PathBuf,
        env: SandboxEnv,
        harness: SandboxPaths,
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        let workspace = home.join("code/project");
        for d in [
            "code/project",
            ".vendor",
            ".ssh",
            ".gnupg",
            ".config/gh",
            ".local/state/unharness",
            "scratch",
        ] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let env = SandboxEnv {
            home: Some(home.clone()),
            scratch: vec![home.join("scratch")],
            protected: vec![home.join(".local/state/unharness")],
        };
        let harness = SandboxPaths {
            writable: vec![PathBuf::from("~/.vendor"), PathBuf::from("~/.missing")],
        };
        World {
            _tmp: tmp,
            home,
            workspace,
            env,
            harness,
        }
    }

    fn request<'a>(w: &'a World, explicit: Option<SandboxLevel>) -> SandboxRequest<'a> {
        SandboxRequest {
            explicit,
            default: SandboxLevel::WorkspaceWrite,
            workspace: &w.workspace,
            harness: &w.harness,
            extra_writable: &[],
            extra_readable: &[],
            extra_deny_read: &[],
        }
    }

    fn active(s: Sandbox) -> SandboxProfile {
        match s {
            Sandbox::Active { profile, .. } => profile,
            other => panic!("not active: {other:?}"),
        }
    }

    #[test]
    fn parse_level() {
        assert_eq!(
            SandboxLevel::parse("Workspace_Write"),
            Some(SandboxLevel::WorkspaceWrite)
        );
        assert_eq!(
            SandboxLevel::parse("read-only"),
            Some(SandboxLevel::ReadOnly)
        );
        assert_eq!(SandboxLevel::parse("off"), Some(SandboxLevel::Off));
        assert_eq!(SandboxLevel::parse("nope"), None);
        for l in SandboxLevel::ALL {
            assert_eq!(SandboxLevel::parse(l.as_str()), Some(l));
        }
    }

    #[test]
    fn workspace_write_profile() {
        let w = world();
        let p = active(resolve(&request(&w, None), &available(), &w.env).unwrap());
        assert_eq!(p.level, SandboxLevel::WorkspaceWrite);
        // Missing harness directories are left out.
        assert_eq!(
            p.writable,
            vec![
                w.workspace.clone(),
                w.home.join(".vendor"),
                w.home.join("scratch")
            ]
        );
        // ~/.ssh stays readable.
        assert_eq!(
            p.deny_read,
            vec![w.home.join(".gnupg"), w.home.join(".config/gh")]
        );
        assert_eq!(p.protected, vec![w.home.join(".local/state/unharness")]);
    }

    #[test]
    fn read_only_leaves_the_workspace_out() {
        let w = world();
        let p = active(
            resolve(
                &request(&w, Some(SandboxLevel::ReadOnly)),
                &available(),
                &w.env,
            )
            .unwrap(),
        );
        assert!(!p.writable.contains(&w.workspace));
        assert!(p.writable.contains(&w.home.join(".vendor")));
    }

    #[test]
    fn readable_and_writable_paths_lift_the_read_deny() {
        let w = world();
        let readable = [PathBuf::from("~/.gnupg")];
        let writable = [w.home.join(".config/gh")];
        let mut req = request(&w, None);
        req.extra_readable = &readable;
        req.extra_writable = &writable;
        let p = active(resolve(&req, &available(), &w.env).unwrap());
        assert!(p.deny_read.is_empty(), "{:?}", p.deny_read);
        assert!(p.writable.contains(&w.home.join(".config/gh")));
    }

    #[test]
    fn extra_denied_paths_keep_the_workspace_and_readable_paths_open() {
        let w = world();
        std::fs::create_dir_all(w.home.join("code/other")).unwrap();
        let deny = [PathBuf::from("~/code"), PathBuf::from("~/missing")];
        let readable = [PathBuf::from("~/code/other")];
        let mut req = request(&w, Some(SandboxLevel::ReadOnly));
        req.extra_deny_read = &deny;
        req.extra_readable = &readable;
        let p = active(resolve(&req, &available(), &w.env).unwrap());
        assert!(p.deny_read.contains(&w.home.join("code")));
        assert!(!p.deny_read.contains(&w.home.join("missing")));
        // Read-only: the workspace is not writable but stays readable.
        assert!(!p.writable.contains(&w.workspace));
        assert_eq!(p.allow_read[0], w.workspace);
        assert!(p.allow_read.contains(&w.home.join("code/other")));
        assert!(p.allow_read.contains(&w.home.join(".vendor")));
    }

    #[test]
    fn a_deny_inside_a_readable_path_is_refused() {
        let w = world();
        std::fs::write(w.workspace.join(".env"), "S=1").unwrap();
        let deny = [PathBuf::from(".env")];
        let mut req = request(&w, None);
        req.extra_deny_read = &deny;
        let err = resolve(&req, &available(), &w.env).unwrap_err().to_string();
        assert!(
            err.contains(".env") && err.contains("cannot be enforced"),
            "{err}"
        );
    }

    #[test]
    fn writable_path_over_own_state_is_refused() {
        let w = world();
        let writable = [w.home.clone()];
        let mut req = request(&w, None);
        req.extra_writable = &writable;
        let err = resolve(&req, &available(), &w.env).unwrap_err();
        assert!(err.to_string().contains("unharness's own state"), "{err}");
    }

    #[test]
    fn off_and_unavailable() {
        let w = world();
        let none: std::result::Result<Arc<dyn SandboxBackend>, String> = Err("no kernel".into());

        let off = resolve(&request(&w, Some(SandboxLevel::Off)), &none, &w.env).unwrap();
        assert_eq!(off.level(), SandboxLevel::Off);
        assert_eq!(off.wanted(), SandboxLevel::Off);
        assert!(off.warning().is_none());

        // The default degrades and says so; an explicit level does not.
        // What was wanted stays known, for a harness's own sandbox.
        let degraded = resolve(&request(&w, None), &none, &w.env).unwrap();
        assert!(!degraded.is_active());
        assert_eq!(degraded.level(), SandboxLevel::Off);
        assert_eq!(degraded.wanted(), SandboxLevel::WorkspaceWrite);
        assert!(degraded.warning().unwrap().contains("no kernel"));

        let active = resolve(&request(&w, None), &available(), &w.env).unwrap();
        assert_eq!(active.wanted(), SandboxLevel::WorkspaceWrite);
        let err = resolve(
            &request(&w, Some(SandboxLevel::WorkspaceWrite)),
            &none,
            &w.env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no kernel"), "{err}");
    }
}
