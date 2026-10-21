//! Noticing that the sandbox refused a tool call, so that the user can be
//! offered the path (`grants`). No CLI says "the sandbox": each says
//! "permission denied" in its own words (`fixtures/sandbox_denied.jsonl`
//! of Claude Code, Codex, pi and agy). So a result only suggests paths
//! ([`candidates`]), and a path counts as blocked by the sandbox only when
//! the profile leaves it out and unharness, outside the sandbox, could
//! reach it ([`blocked`]). What an agent prints cannot make a path blocked
//! that is not: at most it is offered one the sandbox does keep it from.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::grants::{propose, tilde};
use super::{Access, SandboxProfile, canonical, expand_home};

/// A path the sandbox kept a tool call from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Blocked {
    /// Absolute: its existing part canonical, the rest as it was asked
    /// for (a file or directory not made yet).
    pub path: PathBuf,
    pub access: Access,
    /// The path in `path` the call removed or renamed, when that is what
    /// the sandbox refused: Landlock checks such a change against the
    /// directory it is made in, so a granted path cannot be removed
    /// unless its parent is writable too.
    pub entry: Option<PathBuf>,
    /// It can be allowed: what would be granted for it
    /// (`grants::propose`) is not in or around unharness's own
    /// directories. Removing a directory made in the home directory, or
    /// writing a file there, cannot be: the home directory holds them.
    pub grantable: bool,
}

impl Blocked {
    /// What would be granted for it, which unharness's own directories
    /// are in when it cannot be.
    pub fn grant(&self) -> PathBuf {
        propose(&self.path, self.access)
            .map(|(p, _)| p)
            .unwrap_or_else(|| self.path.clone())
    }

    /// What `name` tried, in a sentence for the user.
    pub fn describe(&self, name: &str) -> String {
        let path = tilde(&self.path);
        match (&self.entry, self.access) {
            (Some(entry), _) => format!(
                "{name} tried to remove or rename {}. That changes {path}, which is outside \
                 the sandbox.",
                tilde(entry)
            ),
            (None, Access::Write) => {
                format!("{name} tried to write {path}, which is outside the sandbox.")
            }
            (None, Access::Read) => {
                format!("{name} tried to read {path}, which the sandbox hides.")
            }
        }
    }
}

/// How each CLI's tools say a path could not be reached, in lowercase.
/// Landlock refuses with `EACCES`; a read-only bind (Codex's own sandbox)
/// with `EROFS`.
const DENIED: &[&str] = &[
    "permission denied",
    "eacces",
    "read-only file system",
    "operation not permitted",
];

/// Input keys whose value is a path (Claude's `file_path`, pi's `path`,
/// agy's `Path`, Codex's `changes[].path`) or a command line.
fn is_path_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.ends_with("path") || key == "paths"
}

fn is_command_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "command" | "commandline" | "cmd"
    )
}

/// The paths a tool call's result suggests it could not reach, as
/// written: those its error lines name, and with an error, the paths in
/// its input. A command whose error names none (zsh's `mkdir: Permission
/// denied`) suggests the paths in its command line. Nothing is checked
/// here.
pub fn candidates(input: &Value, output: &str, is_error: bool) -> Vec<PathBuf> {
    let lines: Vec<&str> = output
        .lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            DENIED.iter().any(|d| l.contains(d))
        })
        .collect();
    if lines.is_empty() && !is_error {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let mut add = |p: &str| {
        let p = PathBuf::from(p);
        if !out.contains(&p) {
            out.push(p);
        }
    };
    let mut named = false;
    for line in &lines {
        for p in paths_in_line(line) {
            named = true;
            add(p);
        }
    }
    let mut values = Vec::new();
    collect(input, &mut values);
    for (key, value) in &values {
        if is_path_key(key) {
            add(value);
        }
    }
    if !lines.is_empty() && !named {
        for (key, value) in &values {
            if is_command_key(key) {
                for word in shell_paths(value) {
                    add(word);
                }
            }
        }
    }
    out
}

/// Every string in `value` with the key it is under (an array's items
/// under the array's key).
fn collect<'a>(value: &'a Value, out: &mut Vec<(&'a str, &'a str)>) {
    fn walk<'a>(key: &'a str, value: &'a Value, out: &mut Vec<(&'a str, &'a str)>) {
        match value {
            Value::String(s) => out.push((key, s)),
            Value::Array(items) => items.iter().for_each(|v| walk(key, v, out)),
            Value::Object(map) => map.iter().for_each(|(k, v)| walk(k, v, out)),
            _ => {}
        }
    }
    walk("", value, out)
}

/// The paths an error line names: what it quotes that looks like a path,
/// and its words that are absolute or under `~`. A word that names the
/// program reporting (`/bin/bash: line 2: …`) is not one.
fn paths_in_line(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find(['\'', '"', '`']) {
        let quote = rest[start..].chars().next().unwrap_or('\'');
        let after = &rest[start + 1..];
        let Some(end) = after.find(quote) else {
            break;
        };
        let quoted = &after[..end];
        if looks_like_path(quoted) {
            out.push(quoted);
        }
        rest = &after[end + 1..];
    }
    if !out.is_empty() {
        return out;
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        let trimmed = word.trim_matches(|c: char| matches!(c, ':' | ',' | ';' | '(' | ')'));
        let reporter = word.ends_with(':') && words.get(i + 1) == Some(&"line");
        if !reporter && (trimmed.starts_with('/') || trimmed.starts_with("~/")) {
            out.push(trimmed);
        }
    }
    out
}

fn looks_like_path(s: &str) -> bool {
    !s.is_empty()
        && !s.contains(char::is_whitespace)
        && (s.starts_with('/') || s.starts_with('~') || s.starts_with('.') || s.contains('/'))
}

/// The words of a command line that look like paths, quotes and
/// redirections taken off.
fn shell_paths(command: &str) -> Vec<&str> {
    command
        .split_whitespace()
        .map(|w| {
            w.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '>' | '<' | '&'))
                .trim_matches(|c: char| matches!(c, '\'' | '"' | ';' | '(' | ')'))
        })
        .filter(|w| looks_like_path(w) && !w.starts_with('-'))
        .collect()
}

/// The candidates the sandbox kept the call from: a path the profile
/// does not let it write (or one it does not let it read) which
/// unharness, outside the sandbox, could (`may`, the `access(2)` check
/// in production). A path in or around one of `protected` is never one.
/// Relative paths are under `cwd`, `~` is `home`.
pub fn blocked(
    candidates: &[PathBuf],
    profile: &SandboxProfile,
    cwd: &Path,
    home: Option<&Path>,
    may: impl Fn(&Path, Access) -> bool,
) -> Vec<Blocked> {
    let mut out: Vec<Blocked> = Vec::new();
    for candidate in candidates {
        let Some((existing, path)) = resolve(&cwd.join(expand_home(candidate, home))) else {
            continue;
        };
        let around = |p: &PathBuf| path.starts_with(p) || p.starts_with(&path);
        if profile.protected.iter().any(around) {
            continue;
        }
        let readable = profile.allow_read.iter().any(|a| path.starts_with(a));
        let writable = |p: &Path| profile.writable.iter().any(|w| p.starts_with(w));
        let parent = path.parent().map(Path::to_path_buf);
        let found = if !readable && profile.deny_read.iter().any(|d| path.starts_with(d)) {
            (existing == path && may(&path, Access::Read)).then_some(Blocked {
                path,
                access: Access::Read,
                entry: None,
                grantable: true,
            })
        } else if !writable(&path) {
            may(&existing, Access::Write).then_some(Blocked {
                path,
                access: Access::Write,
                entry: None,
                grantable: true,
            })
        } else if let Some(parent) = parent
            && existing == path
            && !writable(&parent)
        {
            // Writable itself, and there: what was refused is removing or
            // renaming it, a change to the directory it is in.
            may(&parent, Access::Write).then_some(Blocked {
                path: parent,
                access: Access::Write,
                entry: Some(path),
                grantable: true,
            })
        } else {
            None
        };
        if let Some(mut b) = found {
            let grant = b.grant();
            b.grantable = !profile
                .protected
                .iter()
                .any(|p| p.starts_with(&grant) || grant.starts_with(p));
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out
}

/// `path` with its existing part canonical, and that part: `None` when
/// nothing of it exists. `..` past what does not exist yet is not
/// followed.
fn resolve(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let (existing, base) = path
        .ancestors()
        .find_map(|a| canonical(a).map(|c| (c, a)))?;
    let rest = path.strip_prefix(base).ok()?;
    if rest
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let full = existing.join(rest);
    Some((existing, full))
}

/// Whether unharness itself may write (or read) `path`: what the user
/// could grant.
pub fn may_access(path: &Path, access: Access) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mode = match access {
        Access::Write => libc::W_OK,
        Access::Read => libc::R_OK,
    };
    // SAFETY: `c` is a valid NUL-terminated string that outlives the call.
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SandboxLevel;
    use crate::core::testing::fixtures_dir;
    use serde_json::json;

    /// The tool results of a recording, with each call's input.
    fn results(harness: &str, case: &str) -> Vec<(Value, String, bool)> {
        use crate::core::event::AgentEvent;
        let dir = fixtures_dir(&format!("src/harness/{harness}/parse.rs"));
        let text = std::fs::read_to_string(dir.join(format!("{case}.jsonl"))).unwrap();
        let events = match harness {
            "claude" => crate::core::testing::replay(
                &mut crate::harness::claude::parse::ClaudeParser::new(),
                &text,
            ),
            "codex" => crate::core::testing::replay(
                &mut crate::harness::codex::app_server_parse::CodexAppServerParser::new(),
                &text,
            ),
            "pi" => crate::core::testing::replay(
                &mut crate::harness::pi::parse::PiParser::new(None),
                &text,
            ),
            "agy" => crate::core::testing::replay(
                &mut crate::harness::agy::parse::AgyParser::new(None),
                &text,
            ),
            _ => unreachable!(),
        };
        let mut inputs = std::collections::HashMap::new();
        let mut out = Vec::new();
        for e in events {
            match e {
                AgentEvent::ToolCallStarted { id, input, .. } => {
                    inputs.insert(id, input);
                }
                AgentEvent::ToolCallResult {
                    id,
                    output,
                    is_error,
                } => out.push((inputs.remove(&id).unwrap_or(Value::Null), output, is_error)),
                _ => {}
            }
        }
        out
    }

    /// Each CLI's four refused writes suggest the path the call was for:
    /// a file tool's, `touch`, `mkdir` (whose message names none) and a
    /// redirection.
    #[test]
    fn every_recorded_denial_suggests_its_path() {
        let wanted = [
            "/HOME/unharness-denied/out.txt",
            "/HOME/unharness-denied.txt",
            "/HOME/unharness-denied-dir",
            "/HOME/unharness-denied-redirect.txt",
        ];
        for (harness, case) in [
            ("claude", "sandbox_denied"),
            ("codex", "app_server_sandbox_denied"),
            ("pi", "sandbox_denied"),
            ("agy", "sandbox_denied"),
        ] {
            let results = results(harness, case);
            assert_eq!(results.len(), 4, "{harness}");
            for ((input, output, is_error), path) in results.iter().zip(wanted) {
                let found = candidates(input, output, *is_error);
                assert!(
                    found.contains(&PathBuf::from(path)),
                    "{harness}: {path} not in {found:?} from {output:?}"
                );
            }
        }
    }

    #[test]
    fn what_is_not_a_denial_suggests_nothing() {
        let input = json!({"command": "ls /etc"});
        assert!(candidates(&input, "a\nb\n", false).is_empty());
        // A failed call names what it was for, though.
        let input = json!({"file_path": "/x/y"});
        assert_eq!(candidates(&input, "", true), [PathBuf::from("/x/y")]);
    }

    #[test]
    fn the_program_reporting_is_not_a_path_it_was_denied() {
        let found = paths_in_line("/bin/bash: line 2: /h/out.txt: Permission denied");
        assert_eq!(found, ["/h/out.txt"]);
        let found = paths_in_line("mkdir /h/new: permission denied");
        assert_eq!(found, ["/h/new"]);
        let found = paths_in_line("zsh:1: permission denied: /h/out.txt");
        assert_eq!(found, ["/h/out.txt"]);
        let input = json!({"command": "/usr/bin/zsh -lc 'echo hi 2>/h/err > \"/h/a b\"'"});
        assert_eq!(
            candidates(&input, "zsh: Permission denied", false),
            [
                PathBuf::from("/usr/bin/zsh"),
                PathBuf::from("/h/err"),
                PathBuf::from("/h/a")
            ]
        );
    }

    /// `rmdir ~/testing` after `~/testing` was granted: removing it is a
    /// change to `~`, which the grant does not reach (conversation
    /// 161a2ad9, Claude Code 2.1.306).
    #[test]
    fn removing_a_granted_path_is_blocked_at_its_parent() {
        let mut w = world();
        let home = w.root.join("home");
        w.profile.writable.push(home.join("data"));
        let input = json!({"command": format!("rmdir {}", home.join("data").display())});
        let output = format!(
            "rmdir: failed to remove '{}': Permission denied",
            home.join("data").display()
        );
        let found = blocked(
            &candidates(&input, &output, false),
            &w.profile,
            &w.root.join("ws"),
            Some(&home),
            |_: &Path, _| true,
        );
        assert_eq!(
            found,
            vec![Blocked {
                path: home.clone(),
                access: Access::Write,
                entry: Some(home.join("data")),
                grantable: true,
            }]
        );
        // Inside it, a refused write is not the sandbox's.
        let output = format!(
            "touch: cannot touch '{}': Permission denied",
            home.join("data/x").display()
        );
        assert!(
            blocked(
                &candidates(&Value::Null, &output, true),
                &w.profile,
                &w.root.join("ws"),
                Some(&home),
                |_: &Path, _| true,
            )
            .is_empty()
        );
    }

    struct World {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        profile: SandboxProfile,
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["ws", "home/.aws", "home/data", "state", "system"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("home/.aws/config"), "").unwrap();
        let profile = SandboxProfile {
            level: SandboxLevel::WorkspaceWrite,
            workspace: root.join("ws"),
            writable: vec![root.join("ws")],
            writable_files: Vec::new(),
            deny_read: vec![root.join("home/.aws")],
            allow_read: vec![root.join("ws")],
            protected: vec![root.join("state")],
        };
        World {
            _tmp: tmp,
            root,
            profile,
        }
    }

    /// Only what the sandbox keeps from the call and the user could
    /// grant is blocked; a path not there yet keeps what it was asked as.
    #[test]
    fn a_path_is_blocked_only_where_the_sandbox_is_why() {
        let w = world();
        let system = w.root.join("system");
        // unharness may reach everything but `system` (a root-owned
        // directory, in the world).
        let may = |p: &Path, _: Access| !p.starts_with(&system);
        let check = |c: &str| {
            blocked(
                &[PathBuf::from(c)],
                &w.profile,
                &w.root.join("ws"),
                Some(&w.root.join("home")),
                may,
            )
        };
        let write = |p: PathBuf| {
            vec![Blocked {
                path: p,
                access: Access::Write,
                entry: None,
                grantable: true,
            }]
        };
        assert_eq!(
            check("~/data/new/out.txt"),
            write(w.root.join("home/data/new/out.txt"))
        );
        assert_eq!(check("../home/data"), write(w.root.join("home/data")));
        assert_eq!(
            check("~/.aws/config"),
            vec![Blocked {
                path: w.root.join("home/.aws/config"),
                access: Access::Read,
                entry: None,
                grantable: true,
            }]
        );
        // The workspace cannot be removed either, and the directory it is
        // in holds unharness's state: it cannot be allowed.
        assert_eq!(
            check("../ws"),
            vec![Blocked {
                path: w.root.clone(),
                access: Access::Write,
                entry: Some(w.root.join("ws")),
                grantable: false,
            }]
        );
        // Writable already, not the user's to write, unharness's own,
        // nowhere at all.
        assert!(check("out.txt").is_empty());
        assert!(check("../system/x").is_empty());
        assert!(check("../state/conversations/c.json").is_empty());
        assert!(check("..").is_empty());
        assert!(check("~/new/../../x").is_empty());
    }
}
