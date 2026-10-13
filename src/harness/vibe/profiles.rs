//! unharness's own vibe agent profiles, the modes behind `ask` and
//! `accept-edits` (#196).
//!
//! vibe 2.26.1 lets a tool run unasked in its built-in `ask` mode when its
//! configuration says so: `[tools.<name>] permission = "always"`, an
//! allowlist entry, or `bypass_tool_permissions = true`, in the user's
//! `config.toml`, a trusted project's `.vibe/config.toml` or a `VIBE_*`
//! variable; and a profile named `ask` in `~/.vibe/agents/` or a trusted
//! project's `.vibe/agents/` replaces the built-in one. None of that
//! reaches the client: the call runs and no `session/request_permission`
//! comes. vibe applies an agent profile above every one of those layers
//! (`default_orchestrator.py`: defaults, GrowthBook, user TOML, project
//! TOML, environment, runtime overrides, agent profile, admin), its
//! `tools` table merged key by key into theirs (`WithDeepMerge`) and a list
//! in it replacing theirs. So unharness writes two profiles of its own,
//! [`ASK`] and [`ACCEPT_EDITS`], under its state directory, which no
//! sandboxed process can write, names that directory in `VIBE_AGENT_PATHS`
//! and maps its policies to them: each sets `bypass_tool_permissions =
//! false` and makes every tool the configuration names ask (or keeps it
//! `never`), so that what the policy asks about is asked.
//!
//! vibe looks for profiles in its `agent_paths` first, then a trusted
//! project's `.vibe/agents/`, then `~/.vibe/agents/`, and keeps the first of
//! a name (`core/agents/registry.py`). `agent_paths` is concatenated across
//! the layers in their order (`WithConcatMerge`), so a directory listed in
//! `config.toml` or a trusted project's config comes before unharness's,
//! and a profile of the same name there would be the one used. Each
//! profile's description carries a nonce kept beside them; the session
//! must describe the mode with it ([`crate::harness::acp::OwnModes`]), or
//! the policy is refused for the session. The nonce is not secret (vibe,
//! inside the sandbox, has to read it), so it tells a profile that is not
//! unharness's from its own, not one copied from it.
//!
//! What vibe's admin configuration (fetched from the organisation, the
//! layer above the profile) sets still wins.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::harness::acp::OwnModes;

/// The profile behind `ask`: every write and command asked.
pub const ASK: &str = "unharness-ask";
/// The profile behind `accept-edits`: `write_file` and `edit` run in the
/// workspace, commands are asked.
pub const ACCEPT_EDITS: &str = "unharness-accept-edits";

/// The most read of a vibe `config.toml`.
const CONFIG_LIMIT: u64 = 1 << 20;

/// The file beside the profiles that holds their nonce.
const NONCE_FILE: &str = "nonce";

/// The shell commands that run unasked under both profiles: vibe 2.26.1's
/// default allowlist for a POSIX shell (`_get_default_allowlist`,
/// `vibe/core/tools/builtins/bash.py`) without `git status`, which
/// refreshes and rewrites `.git/index`. vibe matches an entry against each
/// part of a command (`cat` is `cat` or `cat …`), and a part runs unasked
/// only when every part does and its own checks pass: no redirect, `$()`
/// or `sudo`, no path outside the workspace, and none of the options that
/// write or run something (`sort -o`, `find -delete`/`-exec`, `tree -o`,
/// `uniq in out`, `git diff --output`, `date` setting the clock, `less`
/// commands; `_shell_command_policy.py`).
pub const BASH_READ_ONLY: [&str; 43] = [
    "cd",
    "echo",
    "git diff",
    "git log",
    "tree",
    "whoami",
    "basename",
    "cat",
    "comm",
    "cut",
    "date",
    "diff",
    "dirname",
    "du",
    "file",
    "find",
    "fmt",
    "fold",
    "grep",
    "head",
    "join",
    "less",
    "ls",
    "md5sum",
    "more",
    "nl",
    "od",
    "paste",
    "pwd",
    "readlink",
    "sha1sum",
    "sha256sum",
    "shasum",
    "sort",
    "stat",
    "sum",
    "tac",
    "tail",
    "tr",
    "uname",
    "uniq",
    "wc",
    "which",
];

/// vibe's shell tools: one call of its runtime's shell is checked against
/// each of them that is there (`_SHELL_TOOL_SOURCES`,
/// `vibe/app_server/_unified_permissions.py`).
const SHELLS: [&str; 3] = ["bash", "powershell", "git_bash"];

/// The tools that write files (`file_system.write_file` and
/// `.search_replace`).
const FILE_WRITERS: [&str; 2] = ["write_file", "edit"];

/// Which of the two profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Ask,
    AcceptEdits,
}

impl Profile {
    pub const ALL: [Profile; 2] = [Profile::Ask, Profile::AcceptEdits];

    /// The profile's name: its file is `<name>.toml`, and vibe's mode id.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Ask => ASK,
            Profile::AcceptEdits => ACCEPT_EDITS,
        }
    }

    /// What the profile says it is, with the nonce that tells it from a
    /// profile of the same name that is not unharness's.
    pub fn description(self, nonce: &str) -> String {
        let what = match self {
            Profile::Ask => "Asks before every write and command",
            Profile::AcceptEdits => "Edits run, commands are asked",
        };
        format!("{what} (unharness {nonce})")
    }
}

/// The tools vibe's configuration names, which a profile has to set, and
/// those of them it turns off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolKeys {
    pub names: BTreeSet<String>,
    /// Named with `permission = "never"` somewhere: kept off.
    pub never: BTreeSet<String>,
}

impl ToolKeys {
    fn add(&mut self, name: &str, settings: Option<&Value>) {
        self.names.insert(name.to_string());
        let never = settings
            .and_then(|s| s.get("permission"))
            .and_then(Value::as_str)
            .is_some_and(|p| p.eq_ignore_ascii_case("never"));
        if never {
            self.never.insert(name.to_string());
        }
    }

    fn extend(&mut self, other: ToolKeys) {
        self.names.extend(other.names);
        self.never.extend(other.never);
    }

    /// The `[tools.<name>]` tables of a vibe config file's text.
    pub fn from_toml(text: &str) -> Result<ToolKeys> {
        let config: toml::Table = text.parse().context("not TOML")?;
        let mut keys = ToolKeys::default();
        match config.get("tools") {
            None => {}
            Some(toml::Value::Table(tools)) => {
                for (name, settings) in tools {
                    let settings = serde_json::to_value(settings).ok();
                    keys.add(name, settings.as_ref());
                }
            }
            Some(_) => bail!("`tools` is not a table"),
        }
        Ok(keys)
    }

    /// The tools `VIBE_*` variables set: `VIBE_TOOLS` (a JSON object) and
    /// `VIBE_TOOLS__<NAME>__<FIELD>`, read by vibe without regard to case
    /// (`vibe/core/config/layers/environment.py`, pydantic-settings with
    /// `__` as the nesting delimiter).
    pub fn from_env<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<ToolKeys> {
        let mut keys = ToolKeys::default();
        for (name, value) in vars {
            let name = name.to_ascii_lowercase();
            if name == "vibe_tools" {
                if value.is_empty() {
                    continue;
                }
                let tools: Value = serde_json::from_str(value).context("VIBE_TOOLS is not JSON")?;
                let Some(tools) = tools.as_object() else {
                    bail!("VIBE_TOOLS is not a JSON object");
                };
                for (tool, settings) in tools {
                    keys.add(tool, Some(settings));
                }
            } else if let Some(rest) = name.strip_prefix("vibe_tools__") {
                let (tool, field) = rest.split_once("__").unwrap_or((rest, ""));
                if tool.is_empty() {
                    continue;
                }
                keys.names.insert(tool.to_string());
                if field == "permission" && value.eq_ignore_ascii_case("never") {
                    keys.never.insert(tool.to_string());
                }
            }
        }
        Ok(keys)
    }
}

/// The config file vibe's project layer reads for a session in `cwd`: the
/// first `.vibe/config.toml` from `cwd` up, stopping at the directory
/// that holds vibe's home (`_discover_config_file`,
/// `vibe/core/config/layers/project.py`). Taken whether or not vibe trusts
/// it: a tool it names asks under the profile either way, which can only
/// ask more. Both with links resolved ([`resolved`]): vibe walks up from
/// its working directory, which the system gives it resolved, and is
/// given its home in `VIBE_HOME`, which it resolves (`get_vibe_home`), so
/// with `~/.vibe` a link to `~/dotfiles/vibe` it stops at `~/dotfiles`
/// and a session in `~/proj` reads `~/.vibe/config.toml` as the project's.
pub fn project_config(home: &Path, cwd: &Path) -> Option<PathBuf> {
    let home = resolved(home);
    let stop = home.parent();
    for dir in resolved(cwd).ancestors() {
        if Some(dir) == stop {
            break;
        }
        let candidate = dir.join(".vibe").join("config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `path` with its links resolved as far as it exists, the rest joined as
/// given, as Python's `Path.resolve()` does.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// A vibe config file's text, read only: a regular file of at most 1 MiB
/// in UTF-8, opened without blocking; `None` when there is none.
fn read_config(path: &Path) -> Result<Option<String>> {
    let file = match crate::sync::open_regular(path, true) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let bytes = crate::sync::read_at_most(&file, CONFIG_LIMIT)
        .with_context(|| format!("reading {}", path.display()))?;
    String::from_utf8(bytes)
        .map(Some)
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

/// The tools named by vibe's configuration for a session in `cwd`, with
/// vibe's home at `home`: its `config.toml`, the project's
/// ([`project_config`]), the `VIBE_*` variables of the environment vibe
/// inherits from unharness (`inherited`; a test passes none) and the
/// session's additions (`env`), and those `<home>/.env` adds to them
/// ([`dotenv_tool_vars`]). A file that is there and cannot be read is an
/// error: what it allows could not be overridden.
pub fn read_tool_keys(
    home: &Path,
    cwd: &Path,
    inherited: &[(String, String)],
    env: &[(String, String)],
) -> Result<ToolKeys> {
    let mut keys = ToolKeys::default();
    let files = std::iter::once(home.join("config.toml")).chain(project_config(home, cwd));
    for path in files {
        if let Some(text) = read_config(&path)? {
            keys.extend(
                ToolKeys::from_toml(&text)
                    .with_context(|| format!("reading {}", path.display()))?,
            );
        }
    }
    keys.extend(ToolKeys::from_env(
        inherited
            .iter()
            .chain(env)
            .map(|(k, v)| (k.as_str(), v.as_str())),
    )?);
    let from_dotenv = dotenv_tool_vars(home, inherited, env)?;
    let path = home.join(".env");
    // Only the outermost message: what serde_json says of a value that
    // does not parse is not shown for a file that holds the API key.
    let dotenv_keys = ToolKeys::from_env(from_dotenv.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    keys.extend(dotenv_keys);
    Ok(keys)
}

/// The `VIBE_TOOLS` and `VIBE_TOOLS__…` variables vibe takes from
/// `<home>/.env`: vibe-acp loads the file into its environment before any
/// configuration layer is read (`load_dotenv_values` in
/// `vibe/core/config/vibe_schema.py`, from `acp/entrypoint.py`), parsed as
/// python-dotenv does ([`super::dotenv`]), each value that is not empty
/// for a key the process's environment (`inherited` with `env`, what
/// unharness and the session set, over it) does not give a value that is
/// not empty. The file holds the API key: only those variables are taken
/// from it, and no other value is kept or shown.
///
/// Looked at before it is opened, as vibe does: what is neither a file
/// nor a FIFO, or cannot be looked at, vibe skips, and so does this. A
/// FIFO (1Password's local env files are one) is refused without being
/// opened: opening it would have it serve its content, which vibe then
/// would not get. A file is read as a config file is: one that is not
/// UTF-8 or is over 1 MiB is an error. So is a `VIBE_HOME` the file would
/// set, which would move vibe to a configuration unharness did not read
/// (the processes unharness starts are always given one, so this is only
/// reached when the session's environment sets it empty).
fn dotenv_tool_vars(
    home: &Path,
    inherited: &[(String, String)],
    env: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    let path = home.join(".env");
    let Ok(meta) = std::fs::metadata(&path) else {
        return Ok(Vec::new());
    };
    #[cfg(unix)]
    if std::os::unix::fs::FileTypeExt::is_fifo(&meta.file_type()) {
        bail!(
            "{} is a FIFO (as a 1Password local env file is); unharness cannot read the \
             VIBE_TOOLS settings vibe takes from it without taking its content, so ask and \
             accept-edits are refused; use auto or bypass, or make it a regular file",
            path.display()
        );
    }
    if !meta.is_file() {
        return Ok(Vec::new());
    }
    let Some(text) = read_config(&path)? else {
        return Ok(Vec::new());
    };
    let environ: std::collections::HashMap<String, String> =
        inherited.iter().chain(env).cloned().collect();
    let loaded = super::dotenv::loaded(&text, &environ)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if loaded.iter().any(|(k, _)| k == "VIBE_HOME") {
        bail!(
            "{} sets VIBE_HOME, which would move vibe to a configuration unharness has not read",
            path.display()
        );
    }
    Ok(loaded
        .into_iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            k == "vibe_tools" || k.starts_with("vibe_tools__")
        })
        .collect())
}

/// The `agent_paths` vibe's config files name for a session in `cwd`:
/// relative to `cwd` (vibe-acp runs there), each also with `~` expanded
/// (vibe 2.26.1 does not expand it, `vibe_schema.py`; both are looked at).
/// The project's file is taken trusted or not. A profile in one of them is
/// found before unharness's: `agent_paths` concatenates across vibe's
/// layers in order, and both TOML layers are below the environment.
pub fn configured_agent_paths(home: &Path, cwd: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let files = std::iter::once(home.join("config.toml")).chain(project_config(home, cwd));
    for path in files {
        let Some(text) = read_config(&path)? else {
            continue;
        };
        let config: toml::Table = text
            .parse()
            .with_context(|| format!("{} is not TOML", path.display()))?;
        let Some(list) = config.get("agent_paths").and_then(toml::Value::as_array) else {
            continue;
        };
        for entry in list.iter().filter_map(toml::Value::as_str) {
            let literal = cwd.join(entry);
            let expanded = cwd.join(crate::core::sandbox::expand_home(
                Path::new(entry),
                dirs::home_dir().as_deref(),
            ));
            if expanded != literal {
                out.push(expanded);
            }
            out.push(literal);
        }
    }
    Ok(out)
}

/// The file vibe would take as `profile` before unharness's own: one of that
/// name in an `agent_paths` directory of the user's or the project's
/// config ([`configured_agent_paths`]). A `VIBE_AGENT_PATHS` the user set
/// is not looked at: unharness puts its own directory before those.
pub fn shadowing(home: &Path, cwd: &Path, profile: Profile) -> Result<Option<PathBuf>> {
    let file = format!("{}.toml", profile.name());
    Ok(configured_agent_paths(home, cwd)
        .context("reading the agent_paths of vibe's configuration")?
        .into_iter()
        .map(|dir| dir.join(&file))
        .find(|path| path.exists()))
}

/// A profile's text: the header, then the TOML vibe reads.
pub fn profile_text(profile: Profile, nonce: &str, keys: &ToolKeys) -> String {
    use toml::Value as V;
    let table = |entries: Vec<(&str, V)>| -> V {
        V::Table(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    };
    let list = |items: &[&str]| V::Array(items.iter().map(|s| V::String(s.to_string())).collect());
    let tool = |name: &str, permission: &str, allowlist: V| {
        let permission = if keys.never.contains(name) {
            "never"
        } else {
            permission
        };
        table(vec![
            ("permission", V::String(permission.to_string())),
            ("allowlist", allowlist),
        ])
    };
    let mut tools = toml::Table::new();
    for name in &keys.names {
        tools.insert(name.clone(), tool(name, "ask", list(&[])));
    }
    let writes = match profile {
        Profile::Ask => "ask",
        Profile::AcceptEdits => "always",
    };
    for name in FILE_WRITERS {
        tools.insert(name.to_string(), tool(name, writes, list(&[])));
    }
    for name in SHELLS {
        let allowlist = if name == "bash" {
            list(&BASH_READ_ONLY)
        } else {
            list(&[])
        };
        tools.insert(name.to_string(), tool(name, "ask", allowlist));
    }
    let mut body = toml::Table::new();
    body.insert(
        "display_name".into(),
        V::String(profile.name().replace('-', " ")),
    );
    body.insert("description".into(), V::String(profile.description(nonce)));
    body.insert(
        "safety".into(),
        V::String(
            match profile {
                Profile::Ask => "neutral",
                Profile::AcceptEdits => "destructive",
            }
            .into(),
        ),
    );
    body.insert("bypass_tool_permissions".into(), V::Boolean(false));
    body.insert("disabled_tools".into(), list(&["exit_plan_mode"]));
    body.insert("tools".into(), V::Table(tools));
    let header = match profile {
        Profile::Ask => "the mode behind unharness's `ask`: every write and command asked",
        Profile::AcceptEdits => {
            "the mode behind unharness's `accept-edits`: `write_file` and `edit` run\n\
             # in the workspace (vibe still asks about a sensitive file, which unharness\n\
             # allows in the workspace, and a path outside it, which goes to the user),\n\
             # commands are asked"
        }
    };
    format!(
        "# Written by unharness, {header}.\n\
         # It is written again whenever it differs; edits here are lost.\n\
         # Every tool vibe's configuration names asks here (or stays `never`).\n\
         # bash's allowlist is vibe 2.26.1's default (vibe/core/tools/builtins/bash.py)\n\
         # without `git status`, which rewrites .git/index: read-only commands, run\n\
         # unasked only when vibe's own checks of the command pass.\n\
         {}",
        toml::to_string(&body).expect("a profile serializes")
    )
}

/// The nonce in `root`, made when there is none. Written beside its final
/// name and linked into place, so that two unharness starting at once
/// agree on one.
fn nonce(root: &Path) -> Result<String> {
    let path = root.join(NONCE_FILE);
    let valid = |s: &str| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let read = || -> Option<String> {
        let file = crate::sync::open_regular(&path, false).ok()?;
        let bytes = crate::sync::read_at_most(&file, 1 << 20).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        valid(text.trim()).then(|| text.trim().to_string())
    };
    if let Some(n) = read() {
        return Ok(n);
    }
    let fresh = uuid::Uuid::new_v4().simple().to_string();
    let partial = root.join(format!(".nonce-{fresh}.partial"));
    std::fs::write(&partial, format!("{fresh}\n"))
        .with_context(|| format!("writing {}", partial.display()))?;
    let linked = std::fs::hard_link(&partial, &path);
    let _ = std::fs::remove_file(&partial);
    match linked {
        Ok(()) => Ok(fresh),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read() {
            Some(n) => Ok(n),
            // A file there that is not a nonce: replaced.
            None => {
                let partial = root.join(format!(".nonce-{fresh}.partial"));
                std::fs::write(&partial, format!("{fresh}\n"))
                    .and_then(|()| std::fs::rename(&partial, &path))
                    .with_context(|| format!("writing {}", path.display()))?;
                Ok(fresh)
            }
        },
        Err(e) => Err(e).with_context(|| format!("writing {}", path.display())),
    }
}

/// Write `text` to `path` unless it holds exactly that already: beside it
/// first and renamed into place, so that vibe never reads half a profile.
pub fn write_if_differs(path: &Path, text: &str) -> Result<()> {
    let same = crate::sync::open_regular(path, false)
        .and_then(|f| crate::sync::read_at_most(&f, 1 << 20))
        .is_ok_and(|bytes| bytes == text.as_bytes());
    if same {
        return Ok(());
    }
    let dir = path.parent().context("a profile path has a directory")?;
    let partial = dir.join(format!(
        ".{}-{}.partial",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let written = std::fs::File::create(&partial)
        .and_then(|mut f| {
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&partial, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// A directory created with mode 0700 where it is missing.
fn private_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// The profiles for `keys`, written under `root` (unharness's
/// `<state dir>/unharness/vibe-agents`): in a directory named after their
/// content, so that two unharness whose vibe configurations name different
/// tools never rewrite each other's while a vibe of the other reads them.
/// Returns that directory and the description of each profile.
pub fn install(root: &Path, keys: &ToolKeys) -> Result<(PathBuf, OwnModes)> {
    private_dir(root)?;
    let nonce = nonce(root)?;
    let texts: Vec<(Profile, String)> = Profile::ALL
        .iter()
        .map(|p| (*p, profile_text(*p, &nonce, keys)))
        .collect();
    let mut digest = Sha256::new();
    for (_, text) in &texts {
        digest.update(text.as_bytes());
        digest.update([0]);
    }
    let digest = digest.finalize();
    let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = root.join(name);
    private_dir(&dir)?;
    for (profile, text) in &texts {
        write_if_differs(&dir.join(format!("{}.toml", profile.name())), text)?;
    }
    let own = Profile::ALL
        .iter()
        .map(|p| (p.name().to_string(), Ok(p.description(&nonce))))
        .collect();
    Ok((dir, own))
}

/// Where unharness keeps the profiles: under its state directory, which
/// no sandboxed process can write. There is no fallback to one that could.
pub fn default_root() -> Result<PathBuf> {
    let state = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("no state directory to keep vibe's profiles in")?;
    Ok(state.join("unharness").join("vibe-agents"))
}

/// What a vibe process is given for its profiles: the directory for
/// `VIBE_AGENT_PATHS`, and what each profile's mode has to be described as;
/// when they cannot be written, no directory and why for each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared {
    pub dir: Option<PathBuf>,
    pub own: OwnModes,
}

/// [`read_tool_keys`] and [`install`] under `root` (else
/// [`default_root`]), for a session in `cwd` with vibe's home at `home`,
/// the environment it inherits `inherited` and the session's additions
/// `env`;
/// a profile vibe would take from elsewhere first ([`shadowing`]) is
/// refused with the file named. Read at spawn: a config or profile
/// written between this and vibe's session open is not seen here (the
/// nonce check of the session's answer still is).
pub fn prepare(
    root: Option<&Path>,
    home: &Path,
    cwd: &Path,
    inherited: &[(String, String)],
    env: &[(String, String)],
) -> Prepared {
    let made = (|| {
        let root = match root {
            Some(r) => r.to_path_buf(),
            None => default_root()?,
        };
        let keys = read_tool_keys(home, cwd, inherited, env)?;
        install(&root, &keys)
    })();
    match made {
        Ok((dir, mut own)) => {
            for profile in Profile::ALL {
                let why = match shadowing(home, cwd, profile) {
                    Ok(None) => continue,
                    Ok(Some(file)) => format!(
                        "vibe would take its mode '{}' from {}, in an agent_paths directory of \
                         its configuration, which it reads before unharness's own",
                        profile.name(),
                        file.display()
                    ),
                    Err(e) => format!("unharness could not check vibe's agent_paths: {e:#}"),
                };
                own.insert(profile.name().to_string(), Err(why));
            }
            Prepared {
                dir: Some(dir),
                own,
            }
        }
        Err(e) => {
            let why = format!("unharness could not put its vibe profile in place: {e:#}");
            Prepared {
                dir: None,
                own: Profile::ALL
                    .iter()
                    .map(|p| (p.name().to_string(), Err(why.clone())))
                    .collect(),
            }
        }
    }
}

/// `VIBE_AGENT_PATHS` for `dir`: it first, then those the variable already
/// names in `inherited` (unharness's environment or the session's), as
/// vibe would have read them. A value vibe could not read is left out.
pub fn agent_paths_env(dir: &Path, inherited: Option<&str>) -> String {
    let mut paths = vec![Value::String(dir.to_string_lossy().into_owned())];
    if let Some(Value::Array(more)) = inherited.and_then(|v| serde_json::from_str(v).ok()) {
        paths.extend(more.into_iter().filter(Value::is_string));
    }
    Value::Array(paths).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str], never: &[&str]) -> ToolKeys {
        ToolKeys {
            names: names.iter().map(|s| s.to_string()).collect(),
            never: never.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The profile vibe reads: valid TOML, the description with the nonce,
    /// bypass off, and the tools.
    #[test]
    fn the_ask_profile_asks_for_every_tool_named() {
        let text = profile_text(
            Profile::Ask,
            "0123456789abcdef0123456789abcdef",
            &keys(
                &["mcp_tiny.shout", "read_file", "web_fetch"],
                &["web_fetch"],
            ),
        );
        assert!(text.starts_with("# Written by unharness"), "{text}");
        assert!(text.contains("vibe 2.26.1"), "{text}");
        let t: toml::Table = text.parse().unwrap();
        assert_eq!(t["display_name"].as_str(), Some("unharness ask"));
        assert_eq!(
            t["description"].as_str(),
            Some(
                "Asks before every write and command (unharness 0123456789abcdef0123456789abcdef)"
            )
        );
        assert_eq!(t["bypass_tool_permissions"].as_bool(), Some(false));
        assert_eq!(t["safety"].as_str(), Some("neutral"));
        let tools = t["tools"].as_table().unwrap();
        let permission = |name: &str| tools[name]["permission"].as_str().unwrap().to_string();
        let allowlist = |name: &str| -> Vec<String> {
            tools[name]["allowlist"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        for name in ["write_file", "edit", "bash", "powershell", "git_bash"] {
            assert_eq!(permission(name), "ask", "{name}");
        }
        assert_eq!(permission("mcp_tiny.shout"), "ask");
        assert_eq!(permission("read_file"), "ask");
        // Off where the configuration turns it off.
        assert_eq!(permission("web_fetch"), "never");
        // No allowlist lets a write through.
        for name in ["write_file", "edit", "powershell", "git_bash", "read_file"] {
            assert!(allowlist(name).is_empty(), "{name}");
        }
        assert_eq!(allowlist("bash"), BASH_READ_ONLY);
        assert_eq!(tools.len(), 8);
    }

    #[test]
    fn the_accept_edits_profile_runs_edits_and_asks_for_commands() {
        let text = profile_text(
            Profile::AcceptEdits,
            "0123456789abcdef0123456789abcdef",
            &keys(&["bash"], &[]),
        );
        let t: toml::Table = text.parse().unwrap();
        assert_eq!(t["display_name"].as_str(), Some("unharness accept edits"));
        assert!(
            t["description"]
                .as_str()
                .unwrap()
                .starts_with("Edits run, commands are asked (unharness ")
        );
        assert_eq!(t["bypass_tool_permissions"].as_bool(), Some(false));
        let tools = t["tools"].as_table().unwrap();
        assert_eq!(tools["write_file"]["permission"].as_str(), Some("always"));
        assert_eq!(tools["edit"]["permission"].as_str(), Some("always"));
        assert_eq!(tools["bash"]["permission"].as_str(), Some("ask"));
        assert!(
            tools["write_file"]["allowlist"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // Turned off in vibe's configuration: stays off.
        let off = profile_text(
            Profile::AcceptEdits,
            "0123456789abcdef0123456789abcdef",
            &keys(&["edit"], &["edit"]),
        );
        let t: toml::Table = off.parse().unwrap();
        assert_eq!(t["tools"]["edit"]["permission"].as_str(), Some("never"));
    }

    /// The read-only list is vibe 2.26.1's default (`bash.py`) without
    /// `git status`, and nothing in it writes by itself.
    #[test]
    fn the_bash_allowlist_is_vibes_read_only_default() {
        let common = [
            "cd",
            "echo",
            "git diff",
            "git log",
            "git status",
            "tree",
            "whoami",
        ];
        let read_only = [
            "basename",
            "cat",
            "comm",
            "cut",
            "date",
            "diff",
            "dirname",
            "du",
            "file",
            "find",
            "fmt",
            "fold",
            "grep",
            "head",
            "join",
            "less",
            "ls",
            "md5sum",
            "more",
            "nl",
            "od",
            "paste",
            "pwd",
            "readlink",
            "sha1sum",
            "sha256sum",
            "shasum",
            "sort",
            "stat",
            "sum",
            "tac",
            "tail",
            "tr",
            "uname",
            "uniq",
            "wc",
            "which",
        ];
        let default: Vec<&str> = common.iter().chain(&read_only).copied().collect();
        assert_eq!(default.len(), 44);
        let ours: Vec<&str> = default.into_iter().filter(|c| *c != "git status").collect();
        assert_eq!(ours, BASH_READ_ONLY);
        for writes in [
            "rm", "mv", "cp", "touch", "mkdir", "tee", "sed", "git", "chmod",
        ] {
            assert!(!BASH_READ_ONLY.contains(&writes), "{writes}");
        }
    }

    #[test]
    fn tool_keys_are_read_from_config_and_environment() {
        let text = r#"
            active_model = "x"
            [tools.write_file]
            permission = "always"
            [tools.bash]
            allowlist = ["touch"]
            [tools."mcp_srv.tool"]
            permission = "NEVER"
            [tools.odd]
        "#;
        assert_eq!(
            ToolKeys::from_toml(text).unwrap(),
            keys(
                &["bash", "mcp_srv.tool", "odd", "write_file"],
                &["mcp_srv.tool"]
            )
        );
        assert_eq!(ToolKeys::from_toml("a = 1").unwrap(), ToolKeys::default());
        assert!(ToolKeys::from_toml("tools = 3").is_err());
        assert!(ToolKeys::from_toml("[tools").is_err());
        let env = [
            ("PATH", "/bin"),
            (
                "VIBE_TOOLS",
                r#"{"write_file":{"permission":"always"},"grep":{"permission":"never"}}"#,
            ),
            ("vibe_tools__Bash__permission", "always"),
            ("VIBE_TOOLS__web_fetch__PERMISSION", "never"),
            ("VIBE_ACTIVE_MODEL", "x"),
        ];
        assert_eq!(
            ToolKeys::from_env(env).unwrap(),
            keys(
                &["bash", "grep", "web_fetch", "write_file"],
                &["grep", "web_fetch"]
            )
        );
        assert!(ToolKeys::from_env([("VIBE_TOOLS", "[1]")]).is_err());
        assert!(ToolKeys::from_env([("VIBE_TOOLS", "{")]).is_err());
        assert_eq!(
            ToolKeys::from_env([("VIBE_TOOLS", "")]).unwrap(),
            ToolKeys::default()
        );
    }

    /// The user's config and the project's, found from the session's
    /// directory up to the one that holds vibe's home; one that cannot be
    /// read is an error, not an empty set.
    #[test]
    fn tool_keys_come_from_the_user_and_project_files() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vibe");
        let project = root.path().join("code/proj");
        let cwd = project.join("src/deep");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".vibe")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[tools.write_file]\npermission = \"always\"\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".vibe/config.toml"),
            "[tools.bash]\npermission = \"always\"\n",
        )
        .unwrap();
        // Found with links resolved, as vibe finds it (the temporary
        // directory is behind one on macOS).
        assert_eq!(
            project_config(&home, &cwd),
            Some(project.canonicalize().unwrap().join(".vibe/config.toml"))
        );
        let found = read_tool_keys(&home, &cwd, &[], &[]).unwrap();
        assert!(found.names.contains("write_file") && found.names.contains("bash"));
        // A `.vibe` in the directory that holds vibe's home is its home.
        assert_eq!(project_config(&home, root.path()), None);
        // The session's additions count too, and the environment vibe
        // inherits, which is only what the caller passes.
        let env = [(
            "VIBE_TOOLS__edit__permission".to_string(),
            "always".to_string(),
        )];
        let inherited = [(
            "VIBE_TOOLS__grep__permission".to_string(),
            "never".to_string(),
        )];
        let found = read_tool_keys(&home, &cwd, &inherited, &env).unwrap();
        assert!(found.names.contains("edit") && found.never.contains("grep"));
        assert!(
            !read_tool_keys(&home, &cwd, &[], &env)
                .unwrap()
                .names
                .contains("grep")
        );
        // Unreadable: not TOML, too large, not a regular file.
        std::fs::write(home.join("config.toml"), "[tools").unwrap();
        assert!(read_tool_keys(&home, &cwd, &[], &[]).is_err());
        std::fs::write(
            home.join("config.toml"),
            vec![b'#'; (CONFIG_LIMIT + 1) as usize],
        )
        .unwrap();
        assert!(read_tool_keys(&home, &cwd, &[], &[]).is_err());
        std::fs::remove_file(home.join("config.toml")).unwrap();
        std::fs::create_dir(home.join("config.toml")).unwrap();
        assert!(read_tool_keys(&home, &cwd, &[], &[]).is_err());
        std::fs::remove_dir(home.join("config.toml")).unwrap();
        // None at all is no key.
        let keys = read_tool_keys(&home, &cwd, &[], &[]).unwrap();
        assert!(keys.names.contains("bash") && !keys.names.contains("write_file"));
    }

    /// vibe resolves the home it is given, so with `~/.vibe` a link to a
    /// directory elsewhere it stops looking for project config at that
    /// directory's parent, not at `~`, and a session in `~/proj` reads
    /// `~/.vibe/config.toml` as the project's; so does this (#212).
    #[cfg(unix)]
    #[test]
    fn project_config_stops_where_vibe_stops_with_a_linked_home() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("u");
        let real = user.join("dotfiles/vibe");
        let proj = user.join("proj");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::os::unix::fs::symlink(&real, user.join(".vibe")).unwrap();
        std::fs::write(real.join("config.toml"), "[tools.grep]\n").unwrap();
        let home = user.join(".vibe");
        assert_eq!(
            project_config(&home, &proj),
            Some(user.canonicalize().unwrap().join(".vibe/config.toml"))
        );
        // Given resolved, the same; below the stop nothing is read.
        assert_eq!(project_config(&real, &proj), project_config(&home, &proj));
        assert_eq!(project_config(&home, &real.join("x")), None);
        // A home that is not there is resolved as far as it goes.
        let link = user.join("linked");
        std::os::unix::fs::symlink(user.join("dotfiles"), &link).unwrap();
        assert_eq!(
            project_config(&link.join("missing"), &proj),
            Some(user.canonicalize().unwrap().join(".vibe/config.toml"))
        );
    }

    /// `<home>/.env` names tools as vibe loads it into its environment
    /// (#212): its `VIBE_TOOLS*` variables count unless the environment
    /// gives the key a value already, nothing else in it is taken, and one
    /// that cannot be read is an error that shows none of its values.
    #[test]
    fn tool_keys_come_from_vibes_dotenv() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vibe");
        let cwd = root.path().join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let dotenv = home.join(".env");
        let var = |k: &str, v: &str| (k.to_string(), v.to_string());
        std::fs::write(
            &dotenv,
            "MISTRAL_API_KEY=sk-secret-value\n\
             export VIBE_TOOLS__WEB_FETCH__PERMISSION=always\n\
             VIBE_TOOLS='{\"grep\": {\"permission\": \"never\"}}'\n\
             vibe_tools__Search__allowlist=\"${UNSET:-x}\"\n\
             VIBE_TOOLS__EMPTY__PERMISSION=\n\
             VIBE_TOOLS__TAKEN__PERMISSION=always\n\
             VIBE_ACTIVE_MODEL=x\n",
        )
        .unwrap();
        let found = read_tool_keys(&home, &cwd, &[], &[]).unwrap();
        assert_eq!(
            found,
            keys(&["grep", "search", "taken", "web_fetch"], &["grep"])
        );
        let text = profile_text(Profile::Ask, "n", &found);
        let profile: toml::Table = text.parse().unwrap();
        assert_eq!(
            profile["tools"]["web_fetch"]["permission"].as_str(),
            Some("ask")
        );
        // A key the environment gives a value is the environment's, the
        // inherited one or the session's; an empty one is not a value.
        let found = read_tool_keys(
            &home,
            &cwd,
            &[
                var("VIBE_TOOLS", r#"{"edit": {}}"#),
                var("VIBE_TOOLS__TAKEN__PERMISSION", ""),
            ],
            &[var("VIBE_TOOLS__WEB_FETCH__PERMISSION", "ask")],
        )
        .unwrap();
        assert_eq!(found, keys(&["edit", "search", "taken", "web_fetch"], &[]));
        // A VIBE_TOOLS in it that vibe could not read: an error naming the
        // file, without the key or what JSON says of the value.
        std::fs::write(
            &dotenv,
            "MISTRAL_API_KEY=sk-secret-value\nVIBE_TOOLS={sk-secret-value\n",
        )
        .unwrap();
        let err = format!("{:#}", read_tool_keys(&home, &cwd, &[], &[]).unwrap_err());
        assert_eq!(
            err,
            format!("reading {}: VIBE_TOOLS is not JSON", dotenv.display())
        );
        // Values that expand past the limit: refused, with no value shown.
        let mut chain = "V0=sk-secret-value\n".to_string();
        for n in 1..40 {
            chain.push_str(&format!("V{n}=${{V{}}}${{V{}}}\n", n - 1, n - 1));
        }
        std::fs::write(&dotenv, &chain).unwrap();
        let err = format!("{:#}", read_tool_keys(&home, &cwd, &[], &[]).unwrap_err());
        assert_eq!(
            err,
            format!("{}: a value expands past 1 MiB", dotenv.display())
        );
        std::fs::write(&dotenv, "VIBE_TOOLS={sk-secret-value\n").unwrap();
        // Set in the environment, the file's is not read.
        assert!(read_tool_keys(&home, &cwd, &[], &[var("VIBE_TOOLS", "{}")]).is_ok());
        // Not a regular file, or not UTF-8: an error.
        std::fs::write(&dotenv, b"MISTRAL_API_KEY=\xff\n").unwrap();
        let err = format!("{:#}", read_tool_keys(&home, &cwd, &[], &[]).unwrap_err());
        assert!(
            err.contains("is not UTF-8") && !err.contains("MISTRAL"),
            "{err}"
        );
        // A VIBE_HOME it would set (none in the environment) would move
        // vibe to a configuration that was not read: refused, unless the
        // environment sets one, as every process unharness starts has.
        std::fs::write(&dotenv, "VIBE_HOME=/elsewhere\n").unwrap();
        let err = format!("{:#}", read_tool_keys(&home, &cwd, &[], &[]).unwrap_err());
        assert!(
            err.contains("sets VIBE_HOME") && !err.contains("/elsewhere"),
            "{err}"
        );
        assert!(read_tool_keys(&home, &cwd, &[var("VIBE_HOME", "/h")], &[]).is_ok());
        std::fs::remove_file(&dotenv).unwrap();
        // A FIFO is refused, saying what to do, and never opened.
        #[cfg(unix)]
        super::super::dotenv::assert_fifo_unopened(&dotenv, || {
            let err = format!("{:#}", read_tool_keys(&home, &cwd, &[], &[]).unwrap_err());
            assert!(
                err.contains("is a FIFO") && err.contains("use auto or bypass"),
                "{err}"
            );
        });
        // What is neither a file nor a FIFO vibe skips, and so is it here.
        std::fs::create_dir(&dotenv).unwrap();
        assert_eq!(
            read_tool_keys(&home, &cwd, &[], &[]).unwrap(),
            ToolKeys::default()
        );
        std::fs::remove_dir(&dotenv).unwrap();
        #[cfg(unix)]
        for target in [Path::new("/dev/null"), &home.join(".env")] {
            std::os::unix::fs::symlink(target, &dotenv).unwrap();
            assert_eq!(
                read_tool_keys(&home, &cwd, &[], &[]).unwrap(),
                ToolKeys::default()
            );
            std::fs::remove_file(&dotenv).unwrap();
        }
        assert_eq!(
            read_tool_keys(&home, &cwd, &[], &[]).unwrap(),
            ToolKeys::default()
        );
    }

    /// A profile of unharness's name in an `agent_paths` directory of the
    /// user's or the project's config refuses that mode at spawn, the file
    /// named; the other mode stays.
    #[test]
    fn a_profile_vibe_would_find_first_refuses_its_mode() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vibe");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".vibe/theirs")).unwrap();
        std::fs::write(cwd.join(".vibe/theirs/unharness-ask.toml"), "").unwrap();
        let store = root.path().join("store");
        let clean = prepare(Some(&store), &home, &cwd, &[], &[]);
        assert!(clean.own[ASK].is_ok() && clean.own[ACCEPT_EDITS].is_ok());
        std::fs::write(
            cwd.join(".vibe/config.toml"),
            "agent_paths = [\".vibe/theirs\"]\n",
        )
        .unwrap();
        let shadowed = prepare(Some(&store), &home, &cwd, &[], &[]);
        assert!(shadowed.dir.is_some());
        let why = shadowed.own[ASK].as_ref().unwrap_err();
        assert!(
            why.contains("would take its mode 'unharness-ask' from"),
            "{why}"
        );
        assert!(why.contains(".vibe/theirs/unharness-ask.toml"), "{why}");
        assert!(shadowed.own[ACCEPT_EDITS].is_ok());
        // From the user's config too, by an absolute path.
        let theirs = root.path().join("mine");
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(theirs.join("unharness-accept-edits.toml"), "").unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!("agent_paths = [{:?}]\n", theirs.to_string_lossy()),
        )
        .unwrap();
        let both = prepare(Some(&store), &home, &cwd, &[], &[]);
        assert!(
            both.own[ACCEPT_EDITS]
                .as_ref()
                .unwrap_err()
                .contains("mine/unharness-accept-edits.toml")
        );
        assert!(both.own[ASK].is_err());
    }

    #[test]
    fn agent_paths_are_read_relative_to_the_session() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vibe");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".vibe")).unwrap();
        std::fs::write(home.join("config.toml"), "agent_paths = [\"/abs/a\"]\n").unwrap();
        std::fs::write(
            cwd.join(".vibe/config.toml"),
            "agent_paths = [\"rel\", \"~/t\"]\n",
        )
        .unwrap();
        let home_dir = dirs::home_dir().unwrap();
        assert_eq!(
            configured_agent_paths(&home, &cwd).unwrap(),
            [
                PathBuf::from("/abs/a"),
                cwd.join("rel"),
                home_dir.join("t"),
                cwd.join("~/t"),
            ]
        );
    }

    /// Written once under a directory named after the content, renamed
    /// into place, rewritten when it differs, the nonce kept.
    #[test]
    fn profiles_are_written_atomically_and_rewritten_when_they_differ() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("vibe-agents");
        let (dir, own) = install(&store, &keys(&["bash"], &[])).unwrap();
        assert_eq!(dir.parent(), Some(store.as_path()));
        let nonce = std::fs::read_to_string(store.join(NONCE_FILE)).unwrap();
        let nonce = nonce.trim();
        assert_eq!(nonce.len(), 32);
        assert_eq!(
            own[ASK],
            Ok(format!(
                "Asks before every write and command (unharness {nonce})"
            ))
        );
        let ask = dir.join("unharness-ask.toml");
        let text = std::fs::read_to_string(&ask).unwrap();
        assert!(text.contains(nonce));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // The same keys: the same directory, the file left alone.
        let before = std::fs::metadata(&ask).unwrap().modified().unwrap();
        let (again, _) = install(&store, &keys(&["bash"], &[])).unwrap();
        assert_eq!(again, dir);
        assert_eq!(std::fs::metadata(&ask).unwrap().modified().unwrap(), before);
        // Changed under it: written again.
        std::fs::write(&ask, "tampered").unwrap();
        install(&store, &keys(&["bash"], &[])).unwrap();
        assert_eq!(std::fs::read_to_string(&ask).unwrap(), text);
        // Other keys: another directory, the first one untouched.
        let (other, _) = install(&store, &keys(&["bash", "web_fetch"], &[])).unwrap();
        assert_ne!(other, dir);
        assert_eq!(std::fs::read_to_string(&ask).unwrap(), text);
        // No partial file left anywhere.
        for d in [&store, &dir, &other] {
            for e in std::fs::read_dir(d).unwrap() {
                let name = e.unwrap().file_name();
                assert!(!name.to_string_lossy().ends_with(".partial"), "{name:?}");
            }
        }
        // A nonce file that is not one is replaced.
        std::fs::write(store.join(NONCE_FILE), "junk").unwrap();
        let (_, own) = install(&store, &keys(&[], &[])).unwrap();
        let fresh = std::fs::read_to_string(store.join(NONCE_FILE)).unwrap();
        assert_ne!(fresh.trim(), nonce);
        assert!(own[ASK].as_ref().unwrap().contains(fresh.trim()));
    }

    #[test]
    fn write_if_differs_replaces_by_rename() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("p.toml");
        write_if_differs(&path, "a = 1\n").unwrap();
        // A link in its place is replaced, not written through.
        let target = root.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        std::fs::remove_file(&path).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &path).unwrap();
            write_if_differs(&path, "a = 2\n").unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
            assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 2\n");
        }
    }

    #[test]
    fn a_profile_that_cannot_be_written_refuses_its_mode() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".vibe");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "[tools").unwrap();
        let prepared = prepare(
            Some(&root.path().join("store")),
            &home,
            root.path(),
            &[],
            &[],
        );
        assert_eq!(prepared.dir, None);
        for name in [ASK, ACCEPT_EDITS] {
            let why = prepared.own[name].as_ref().unwrap_err();
            assert!(
                why.contains("could not put its vibe profile in place"),
                "{why}"
            );
            assert!(why.contains("config.toml"), "{why}");
        }
    }

    #[test]
    fn the_agent_paths_variable_puts_unharness_first() {
        let dir = Path::new("/state/unharness/vibe-agents/ab");
        assert_eq!(
            agent_paths_env(dir, None),
            r#"["/state/unharness/vibe-agents/ab"]"#
        );
        assert_eq!(
            agent_paths_env(dir, Some(r#"["/mine", 3]"#)),
            r#"["/state/unharness/vibe-agents/ab","/mine"]"#
        );
        assert_eq!(
            agent_paths_env(dir, Some("not json")),
            r#"["/state/unharness/vibe-agents/ab"]"#
        );
    }
}
