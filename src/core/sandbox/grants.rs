//! Paths the user grants a harness beyond its profile from the TUI
//! (`/sandbox allow`): for the run, or saved as `[sandbox].writable` /
//! `readable`. The grant is the user's, made outside the sandbox, so it is
//! never something an agent decides; what it is offered on may be what an
//! agent printed, so the grant is the narrowest that exists, and one that
//! lets what runs in the sandbox reach beyond it later is said to.

use std::fmt;
use std::path::{Path, PathBuf};

use super::{DEFAULT_DENY_READ, SandboxEnv, canonical};

/// What a grant opens: writing (and with it reading), or reading only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    Write,
    Read,
}

impl Access {
    /// The `[sandbox]` key it is saved under.
    pub fn key(self) -> &'static str {
        match self {
            Access::Write => "writable",
            Access::Read => "readable",
        }
    }
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// A path granted for the run: the next process's profile has it as it
/// would have `[sandbox].writable` or `readable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Absolute and canonical: it existed when it was granted.
    pub path: PathBuf,
    pub access: Access,
}

/// What can be granted for `path`: the path itself if it exists, otherwise
/// the nearest directory above it that does, since the backends grant only
/// what exists (a file created later beneath it is covered).
pub fn grant_target(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(canonical)
}

/// What to grant so that `path`, blocked for `access`, can be reached,
/// and whether it has to be made first: the path itself if it exists;
/// a directory it lies beneath that does not exist yet, made (empty) when
/// it is granted, so that the grant reaches no further than asked; or,
/// for a new file in a directory that exists, that directory.
pub fn propose(path: &Path, access: Access) -> Option<(PathBuf, bool)> {
    if let Some(p) = canonical(path) {
        return Some((p, false));
    }
    let existing = grant_target(path)?;
    if access == Access::Read {
        return Some((existing, false));
    }
    let base = path.ancestors().find(|a| canonical(a).is_some())?;
    let rest = path.strip_prefix(base).ok()?;
    let mut parts = rest.components();
    let first = parts.next()?;
    if parts.next().is_some() {
        Some((existing.join(first), true))
    } else {
        Some((existing, false))
    }
}

/// Files a shell reads at startup, relative to the home directory: one an
/// agent may write runs what it put there outside the sandbox.
const STARTUP_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".config/fish",
    ".config/environment.d",
];

/// Directories whose contents are started at login, relative to home.
const LOGIN_DIRS: &[&str] = &[
    ".config/autostart",
    ".config/systemd",
    "Library/LaunchAgents",
];

/// Why granting `access` on `path` reaches beyond what the sandbox is for,
/// if it does: written, it would run outside the sandbox later (a shell's
/// startup file, a directory on `$PATH`, a git repository's hooks, a
/// harness's own configuration in `guarded`); read, it holds credentials.
/// Said to the user, not refused: the grant is theirs to make.
pub fn grant_risk(
    path: &Path,
    access: Access,
    env: &SandboxEnv,
    search_path: &[PathBuf],
    guarded: &[PathBuf],
) -> Option<String> {
    // Granting a directory grants everything beneath it; a path beneath
    // one of these is as much a part of it.
    let covers = |p: &Path| p.starts_with(path);
    let overlaps = |p: &Path| p.starts_with(path) || path.starts_with(p);
    let home = env.home.as_deref().and_then(canonical);
    if access == Access::Read {
        let home = home?;
        return DEFAULT_DENY_READ
            .iter()
            .filter_map(|rel| canonical(&home.join(rel)))
            .find(|denied| overlaps(denied))
            .map(|denied| format!("{} holds credentials", tilde(&denied)));
    }
    if path == Path::new("/") {
        return Some("That's the whole filesystem.".into());
    }
    if let Some(home) = &home {
        if covers(home) {
            return Some(
                "That's your whole home folder, including your shell's startup files.".into(),
            );
        }
        let under_home = |rels: &'static [&'static str]| {
            rels.iter()
                .map(move |rel| home.join(rel))
                .find(|p| overlaps(p))
        };
        if let Some(file) = under_home(STARTUP_FILES) {
            return Some(format!(
                "Your shell runs {} when it starts, outside the sandbox.",
                tilde(&file)
            ));
        }
        if let Some(dir) = under_home(LOGIN_DIRS) {
            return Some(format!(
                "What is put in {} runs when you log in, outside the sandbox.",
                tilde(&dir)
            ));
        }
    }
    if let Some(dir) = search_path
        .iter()
        .filter_map(|d| canonical(d))
        .find(|d| overlaps(d))
    {
        return Some(format!(
            "{} is on your $PATH: a program put there runs outside the sandbox.",
            tilde(&dir)
        ));
    }
    if let Some(file) = guarded.iter().find(|p| overlaps(p)) {
        return Some(format!(
            "{} is the agent's own configuration: it decides how the agent runs next time.",
            tilde(file)
        ));
    }
    let in_git = path.components().any(|c| c.as_os_str() == ".git");
    if in_git || path.join(".git").exists() {
        return Some("It holds a git repository, whose hooks git runs outside the sandbox.".into());
    }
    None
}

/// `path` for showing, with the home directory as `~`.
pub fn tilde(path: &Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The directories on `$PATH`.
pub fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_at(home: &Path) -> SandboxEnv {
        SandboxEnv {
            home: Some(home.to_path_buf()),
            ..SandboxEnv::default()
        }
    }

    #[test]
    fn a_missing_path_is_granted_by_the_directory_above_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("data")).unwrap();
        assert_eq!(grant_target(&root.join("data")), Some(root.join("data")));
        assert_eq!(
            grant_target(&root.join("data/new/file.txt")),
            Some(root.join("data"))
        );
    }

    /// A file beneath a directory not made yet is granted by that
    /// directory, made when it is granted; a new file in one that exists,
    /// by the directory.
    #[test]
    fn a_grant_reaches_no_further_than_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("data")).unwrap();
        std::fs::write(root.join("data/f"), "").unwrap();
        let propose = |p: &str, a| propose(&root.join(p), a);
        assert_eq!(
            propose("data/f", Access::Write),
            Some((root.join("data/f"), false))
        );
        assert_eq!(
            propose("data/new.txt", Access::Write),
            Some((root.join("data"), false))
        );
        assert_eq!(
            propose("data/a/b/c.txt", Access::Write),
            Some((root.join("data/a"), true))
        );
        assert_eq!(
            propose("data/a/b/c.txt", Access::Read),
            Some((root.join("data"), false))
        );
    }

    #[test]
    fn what_would_run_outside_the_sandbox_is_said() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let env = env_at(&home);
        for dir in ["bin", "project/.git", "plain", ".config/systemd/user"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::write(home.join(".zshrc"), "").unwrap();
        std::fs::write(home.join("settings.json"), "").unwrap();
        let bin = vec![home.join("bin")];
        let guarded = vec![home.join("settings.json")];
        let risk = |p: &str| grant_risk(&home.join(p), Access::Write, &env, &bin, &guarded);

        assert!(risk("").unwrap().contains("home folder"));
        assert!(risk(".zshrc").unwrap().contains("when it starts"));
        assert!(risk(".config").unwrap().contains("when it starts"));
        assert!(risk(".config/systemd/user").unwrap().contains("log in"));
        assert!(risk("bin").unwrap().contains("$PATH"));
        assert!(risk("settings.json").unwrap().contains("own configuration"));
        assert!(risk("project").unwrap().contains("hooks"));
        assert!(risk("project/.git").unwrap().contains("hooks"));
        assert_eq!(risk("plain"), None);
        assert_eq!(
            grant_risk(Path::new("/"), Access::Write, &env, &[], &[]).as_deref(),
            Some("That's the whole filesystem.")
        );
    }

    #[test]
    fn reading_credentials_is_said() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let env = env_at(&home);
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();
        let risk = |p: &str| grant_risk(&home.join(p), Access::Read, &env, &[], &[]);
        assert!(risk(".aws").unwrap().contains("credentials"));
        // Around it and inside it alike.
        assert!(risk("").unwrap().contains("credentials"));
        std::fs::write(home.join(".aws/config"), "").unwrap();
        assert!(risk(".aws/config").unwrap().contains("credentials"));
        assert_eq!(risk("notes"), None);
    }
}
