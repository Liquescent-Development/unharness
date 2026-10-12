//! Whether vibe trusts a folder with its project configuration, read the
//! way vibe 2.26.1 reads it (`vibe/core/trusted_folders.py`,
//! `vibe/core/config/harness_files/_harness_manager.py`), and never
//! written: trust stays the user's act in vibe's own interface.
//!
//! In a folder vibe does not trust it reads none of the folder's project
//! configuration: no `AGENTS.md`, nothing under `.vibe/` (its
//! `config.toml`, `hooks.toml`, tools, skills, plugins, agents, prompts)
//! and no `.agents/skills/`. The user's own (`~/.vibe`, `~/.agents/skills`)
//! is read either way.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// vibe's record of the folders it trusts and the ones the user declined.
pub const TRUST_FILE: &str = "trusted_folders.toml";

/// The most read of [`TRUST_FILE`].
const TRUST_FILE_LIMIT: u64 = 1 << 20;

/// What vibe makes of a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Trusted by the entry for this folder or, failing one, the closest
    /// folder above it that has an entry.
    Trusted(PathBuf),
    /// Declined in vibe: an `untrusted` entry for this folder or the
    /// closest above it with an entry.
    Declined(PathBuf),
    /// No entry for it or any folder above it, which vibe takes as not
    /// trusted (its own interface asks when the folder has project
    /// configuration).
    Undecided,
    /// [`TRUST_FILE`] is there but could not be read as vibe reads it: vibe
    /// then trusts nothing and empties the file when it next starts.
    Unreadable(String),
    /// The folder's `.vibe` is vibe's home, so vibe reads no project
    /// configuration in it, whatever the file says (the home directory,
    /// with the default home).
    VibeHome,
}

impl Trust {
    /// Whether vibe reads the folder's project configuration.
    pub fn is_trusted(&self) -> bool {
        matches!(self, Trust::Trusted(_))
    }
}

/// How vibe with its home at `home` takes `folder`: the folder with its
/// links resolved, and then each folder above it, until one is listed,
/// `trusted` before `untrusted` (`TrustedFoldersManager.is_trusted`).
/// Entries are compared as the strings vibe stored, which are resolved
/// paths. Read only; a missing file is no entry at all.
pub fn folder_trust(home: &Path, folder: &Path) -> Trust {
    let folder = resolved(folder);
    if resolved(&folder.join(".vibe")) == resolved(home) {
        return Trust::VibeHome;
    }
    let file = home.join(TRUST_FILE);
    let text = match crate::sync::open_regular(&file, true)
        .and_then(|f| crate::sync::read_at_most(&f, TRUST_FILE_LIMIT))
    {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return Trust::Unreadable("not UTF-8".into()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Trust::Undecided,
        Err(e) => return Trust::Unreadable(e.to_string()),
    };
    let table = match text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(e) => return Trust::Unreadable(format!("not TOML: {}", e.message())),
    };
    let list = |key: &str| -> Vec<String> {
        table
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (trusted, untrusted) = (list("trusted"), list("untrusted"));
    let listed = |entries: &[String], dir: &Path| {
        entries
            .iter()
            .any(|e| OsStr::new(e.as_str()) == dir.as_os_str())
    };
    for dir in folder.ancestors() {
        if listed(&trusted, dir) {
            return Trust::Trusted(dir.to_path_buf());
        }
        if listed(&untrusted, dir) {
            return Trust::Declined(dir.to_path_buf());
        }
    }
    Trust::Undecided
}

/// The project configuration in `folder` that vibe reads only when it
/// trusts the folder, as named in vibe's trust prompt
/// (`find_trustable_files`, `find_local_config_dirs`): its `AGENTS.md`, a
/// `.vibe/` with something vibe reads in it, and `.agents/skills/`.
pub fn project_config(folder: &Path) -> Vec<&'static str> {
    let mut found = Vec::new();
    if folder.join("AGENTS.md").is_file() {
        found.push("AGENTS.md");
    }
    let vibe = folder.join(".vibe");
    if ["tools", "skills", "plugins", "agents", "prompts"]
        .iter()
        .any(|d| vibe.join(d).is_dir())
        || ["config.toml", "hooks.toml"]
            .iter()
            .any(|f| vibe.join(f).is_file())
    {
        found.push(".vibe/");
    }
    if folder.join(".agents").join("skills").is_dir() {
        found.push(".agents/skills/");
    }
    found
}

/// What `doctor` says of `folder` for a vibe with its home at `home`, and
/// whether that is something to look at: project configuration vibe
/// ignores, or a trust file it cannot read.
pub fn describe(home: &Path, folder: &Path) -> (String, bool) {
    let trust = folder_trust(home, folder);
    let found = project_config(folder);
    let what = if found.is_empty() {
        "no AGENTS.md, .vibe/ or .agents/skills/ here".to_string()
    } else {
        found.join(", ")
    };
    match trust {
        Trust::Trusted(by) => (
            if found.is_empty() {
                format!("trusted in vibe ({}); {what}", by.display())
            } else {
                format!(
                    "trusted in vibe ({}): vibe reads this folder's {what}",
                    by.display()
                )
            },
            false,
        ),
        Trust::VibeHome => (
            "this folder's .vibe is vibe's home: vibe reads no project configuration here"
                .to_string(),
            false,
        ),
        Trust::Unreadable(why) => (
            format!(
                "{} could not be read ({why}): vibe trusts no folder and empties the file when it next starts",
                home.join(TRUST_FILE).display()
            ),
            true,
        ),
        Trust::Declined(_) | Trust::Undecided => {
            let why = match &trust {
                Trust::Declined(by) => format!("declined for {}", by.display()),
                _ => "no decision".to_string(),
            };
            if found.is_empty() {
                (format!("not trusted in vibe ({why}); {what}"), false)
            } else {
                (
                    format!(
                        "not trusted in vibe ({why}): vibe ignores this folder's {what}; to use them, trust the folder in vibe itself (`vibe` started here asks), which unharness never does for you"
                    ),
                    true,
                )
            }
        }
    }
}

/// `path` with its links resolved, as vibe's `Path.resolve()` gives it;
/// as it is when that fails (vibe's is not strict either).
fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Setup {
        _dir: tempfile::TempDir,
        home: PathBuf,
        ws: PathBuf,
    }

    /// A vibe home and a workspace under one temporary directory, both
    /// resolved (the temporary directory may sit behind a link).
    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("vibehome");
        let ws = root.join("projects").join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        Setup {
            _dir: dir,
            home,
            ws,
        }
    }

    fn write_trust(home: &Path, trusted: &[&Path], untrusted: &[&Path]) {
        // As vibe's `tomli_w` writes it.
        let list = |l: &[&Path]| {
            l.iter()
                .map(|p| format!("{:?}", p.display().to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        std::fs::write(
            home.join(TRUST_FILE),
            format!(
                "trusted = [{}]\nuntrusted = [{}]\n",
                list(trusted),
                list(untrusted)
            ),
        )
        .unwrap();
    }

    #[test]
    fn no_file_or_no_entry_is_undecided() {
        let s = setup();
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Undecided);
        let other = s.ws.parent().unwrap().join("other");
        write_trust(&s.home, &[&other], &[]);
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Undecided);
        // vibe 2.26.1 writes this when it creates the file.
        std::fs::write(s.home.join(TRUST_FILE), "trusted = []\nuntrusted = []\n").unwrap();
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Undecided);
        assert!(!Trust::Undecided.is_trusted());
    }

    #[test]
    fn the_closest_entry_decides() {
        let s = setup();
        let projects = s.ws.parent().unwrap().to_path_buf();
        write_trust(&s.home, &[&s.ws], &[]);
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Trusted(s.ws.clone()));
        assert!(folder_trust(&s.home, &s.ws).is_trusted());

        // A folder above trusts everything below it.
        write_trust(&s.home, &[&projects], &[]);
        assert_eq!(
            folder_trust(&s.home, &s.ws),
            Trust::Trusted(projects.clone())
        );
        let deeper = s.ws.join("src");
        std::fs::create_dir(&deeper).unwrap();
        assert_eq!(
            folder_trust(&s.home, &deeper),
            Trust::Trusted(projects.clone())
        );

        // A closer decline wins over a trusted folder above.
        write_trust(&s.home, &[&projects], &[&s.ws]);
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Declined(s.ws.clone()));
        assert!(!folder_trust(&s.home, &s.ws).is_trusted());
        // And a closer trust over a decline above.
        write_trust(&s.home, &[&s.ws], &[&projects]);
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Trusted(s.ws.clone()));
        // Listed in both: trusted, as vibe checks that list first.
        write_trust(&s.home, &[&s.ws], &[&s.ws]);
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Trusted(s.ws.clone()));
    }

    /// vibe stores resolved paths and resolves the folder before looking.
    #[cfg(unix)]
    #[test]
    fn a_folder_reached_through_a_link_is_the_folder() {
        let s = setup();
        let link = s.ws.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&s.ws, &link).unwrap();
        write_trust(&s.home, &[&s.ws], &[]);
        assert_eq!(folder_trust(&s.home, &link), Trust::Trusted(s.ws.clone()));
        // An entry naming the link is not the folder's.
        write_trust(&s.home, &[&link], &[]);
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Undecided);
        assert_eq!(folder_trust(&s.home, &link), Trust::Undecided);
    }

    /// Entries are compared as stored: one with a trailing slash names no
    /// folder vibe resolves.
    #[test]
    fn entries_are_compared_as_strings() {
        let s = setup();
        std::fs::write(
            s.home.join(TRUST_FILE),
            format!("trusted = [{:?}]\n", format!("{}/", s.ws.display())),
        )
        .unwrap();
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Undecided);
    }

    #[test]
    fn a_file_vibe_cannot_read_is_said_so() {
        let s = setup();
        std::fs::write(s.home.join(TRUST_FILE), "trusted = [").unwrap();
        assert!(matches!(
            folder_trust(&s.home, &s.ws),
            Trust::Unreadable(why) if why.starts_with("not TOML")
        ));
        std::fs::remove_file(s.home.join(TRUST_FILE)).unwrap();
        std::fs::create_dir(s.home.join(TRUST_FILE)).unwrap();
        assert!(matches!(folder_trust(&s.home, &s.ws), Trust::Unreadable(_)));
    }

    /// A FIFO in the file's place does not hang the read.
    #[cfg(unix)]
    #[test]
    fn a_fifo_for_the_file_is_not_waited_on() {
        let s = setup();
        crate::sync::testing::mkfifo(&s.home.join(TRUST_FILE));
        assert!(matches!(folder_trust(&s.home, &s.ws), Trust::Unreadable(_)));
    }

    #[test]
    fn the_folder_holding_vibes_home_reads_no_project_configuration() {
        let s = setup();
        let home = s.ws.join(".vibe");
        std::fs::create_dir(&home).unwrap();
        write_trust(&home, &[&s.ws], &[]);
        assert_eq!(folder_trust(&home, &s.ws), Trust::VibeHome);
        // Also when the home is not there yet.
        let missing = s.ws.parent().unwrap().join("nohome");
        assert_eq!(
            folder_trust(&missing.join(".vibe"), &missing),
            Trust::VibeHome
        );
    }

    #[test]
    fn nothing_is_written() {
        let s = setup();
        assert_eq!(folder_trust(&s.home, &s.ws), Trust::Undecided);
        assert!(!s.home.join(TRUST_FILE).exists());
        write_trust(&s.home, &[&s.ws], &[]);
        let before = std::fs::read(s.home.join(TRUST_FILE)).unwrap();
        let _ = describe(&s.home, &s.ws);
        assert_eq!(std::fs::read(s.home.join(TRUST_FILE)).unwrap(), before);
        // A missing home stays missing.
        let missing = s.ws.parent().unwrap().join("missing-home");
        assert_eq!(folder_trust(&missing, &s.ws), Trust::Undecided);
        let _ = describe(&missing, &s.ws);
        assert!(!missing.exists());
    }

    #[test]
    fn project_configuration_as_vibe_finds_it() {
        let s = setup();
        assert!(project_config(&s.ws).is_empty());
        // An empty `.vibe/` or `.agents/` holds nothing vibe reads.
        std::fs::create_dir(s.ws.join(".vibe")).unwrap();
        std::fs::create_dir(s.ws.join(".agents")).unwrap();
        assert!(project_config(&s.ws).is_empty());
        std::fs::write(s.ws.join("AGENTS.md"), "x").unwrap();
        std::fs::create_dir(s.ws.join(".agents/skills")).unwrap();
        assert_eq!(project_config(&s.ws), ["AGENTS.md", ".agents/skills/"]);
        std::fs::write(s.ws.join(".vibe/config.toml"), "").unwrap();
        assert_eq!(
            project_config(&s.ws),
            ["AGENTS.md", ".vibe/", ".agents/skills/"]
        );
        std::fs::remove_file(s.ws.join(".vibe/config.toml")).unwrap();
        std::fs::create_dir(s.ws.join(".vibe/skills")).unwrap();
        assert!(project_config(&s.ws).contains(&".vibe/"));
    }

    #[test]
    fn what_doctor_says() {
        let s = setup();
        // Nothing to ignore: not worth a warning.
        let (text, warn) = describe(&s.home, &s.ws);
        assert!(
            text.starts_with("not trusted in vibe (no decision)"),
            "{text}"
        );
        assert!(!warn);

        std::fs::create_dir_all(s.ws.join(".agents/skills/hello")).unwrap();
        let (text, warn) = describe(&s.home, &s.ws);
        assert!(
            text.contains("vibe ignores this folder's .agents/skills/"),
            "{text}"
        );
        assert!(text.contains("`vibe` started here asks"), "{text}");
        assert!(warn);

        write_trust(&s.home, &[], &[&s.ws]);
        let (text, warn) = describe(&s.home, &s.ws);
        assert!(
            text.contains(&format!("declined for {}", s.ws.display())),
            "{text}"
        );
        assert!(warn);

        write_trust(&s.home, &[&s.ws], &[]);
        let (text, warn) = describe(&s.home, &s.ws);
        assert_eq!(
            text,
            format!(
                "trusted in vibe ({}): vibe reads this folder's .agents/skills/",
                s.ws.display()
            )
        );
        assert!(!warn);

        std::fs::write(s.home.join(TRUST_FILE), "nope = ").unwrap();
        let (text, warn) = describe(&s.home, &s.ws);
        assert!(text.contains("could not be read"), "{text}");
        assert!(warn);
    }
}
