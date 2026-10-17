//! macOS backend: the command is launched through `sandbox-exec` with a
//! generated Seatbelt profile. The profile is built on every platform so it
//! can be tested anywhere; only detection is macOS-specific.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use super::{FileName, SandboxBackend, SandboxProfile, WritableFiles};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Debug)]
pub struct Seatbelt;

impl Seatbelt {
    pub fn detect() -> std::result::Result<Self, String> {
        if Path::new(SANDBOX_EXEC).exists() {
            Ok(Seatbelt)
        } else {
            Err(format!("{SANDBOX_EXEC} not found"))
        }
    }
}

impl SandboxBackend for Seatbelt {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn detail(&self) -> String {
        SANDBOX_EXEC.to_string()
    }

    fn wrap(&self, cmd: Command, profile: &SandboxProfile) -> Result<Command> {
        let mut wrapped = Command::new(SANDBOX_EXEC);
        wrapped
            .arg("-p")
            .arg(seatbelt_profile(profile))
            .arg(cmd.get_program())
            .args(cmd.get_args());
        for (key, value) in cmd.get_envs() {
            match value {
                Some(v) => wrapped.env(key, v),
                None => wrapped.env_remove(key),
            };
        }
        if let Some(dir) = cmd.get_current_dir() {
            wrapped.current_dir(dir);
        }
        Ok(wrapped)
    }
}

/// Everything is allowed except writes outside the writable paths and files
/// and reads of the denied ones. Later rules win, so the denies inside a
/// writable path come last.
pub fn seatbelt_profile(profile: &SandboxProfile) -> String {
    let subpaths = |paths: &[std::path::PathBuf]| -> String {
        paths
            .iter()
            .map(|p| format!(" (subpath {})", quote(p)))
            .collect()
    };
    let mut out = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    if !profile.writable.is_empty() {
        out.push_str(&format!(
            "(allow file-write*{})\n",
            subpaths(&profile.writable)
        ));
    }
    let files: String = profile
        .writable_files
        .iter()
        .filter_map(files_regex)
        .map(|re| format!(" (regex #\"{re}\")"))
        .collect();
    if !files.is_empty() {
        out.push_str(&format!("(allow file-write*{files})\n"));
    }
    if !profile.protected.is_empty() {
        out.push_str(&format!(
            "(deny file-write*{})\n",
            subpaths(&profile.protected)
        ));
    }
    if !profile.deny_read.is_empty() {
        out.push_str(&format!(
            "(deny file-read*{})\n",
            subpaths(&profile.deny_read)
        ));
        let inside: Vec<_> = profile
            .allow_read
            .iter()
            .filter(|a| profile.deny_read.iter().any(|d| a.starts_with(d)))
            .cloned()
            .collect();
        if !inside.is_empty() {
            out.push_str(&format!("(allow file-read*{})\n", subpaths(&inside)));
        }
    }
    out
}

/// A regex for `files`: their names directly inside the directory, whose
/// path is matched literally. Nothing when the path cannot be put in a
/// `#"…"` literal (a `"` or a control character in it, or not UTF-8), so
/// the files stay unwritable; `profile` has refused such names already.
fn files_regex(files: &WritableFiles) -> Option<String> {
    let literal = |s: &str| -> Option<String> {
        if s.contains('"') || s.chars().any(char::is_control) {
            return None;
        }
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if "\\.^$|?*+()[]{}".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        Some(out)
    };
    let dir = literal(files.dir.to_str()?)?;
    let names = files
        .names
        .iter()
        .map(|name| {
            if name.refusal().is_some() {
                return None;
            }
            let text = literal(name.literal())?;
            Some(match name {
                FileName::Prefix(_) => format!("{text}[^/]*"),
                FileName::HexSuffix(_) => format!("{text}[0-9A-Fa-f]+"),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if names.is_empty() {
        return None;
    }
    Some(format!("^{dir}/({})$", names.join("|")))
}

fn quote(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::super::SandboxLevel;
    use super::*;
    use std::path::PathBuf;

    fn profile() -> SandboxProfile {
        SandboxProfile {
            level: SandboxLevel::WorkspaceWrite,
            workspace: PathBuf::from("/Users/u/code"),
            writable: vec![
                PathBuf::from("/Users/u/code"),
                PathBuf::from("/private/tmp"),
            ],
            writable_files: vec![],
            deny_read: vec![PathBuf::from("/Users/u/.ssh")],
            allow_read: vec![PathBuf::from("/Users/u/code")],
            protected: vec![PathBuf::from("/Users/u/Library/unharness")],
        }
    }

    fn keychain(dir: &str) -> WritableFiles {
        WritableFiles {
            dir: PathBuf::from(dir),
            names: vec![
                FileName::Prefix("login.keychain-db"),
                FileName::HexSuffix(".fl"),
            ],
        }
    }

    #[test]
    fn profile_text() {
        assert_eq!(
            seatbelt_profile(&profile()),
            "(version 1)\n\
             (allow default)\n\
             (deny file-write*)\n\
             (allow file-write* (subpath \"/Users/u/code\") (subpath \"/private/tmp\"))\n\
             (deny file-write* (subpath \"/Users/u/Library/unharness\"))\n\
             (deny file-read* (subpath \"/Users/u/.ssh\"))\n"
        );
    }

    #[test]
    fn writable_files_come_after_the_writable_paths_and_before_the_protected_ones() {
        let mut p = profile();
        p.writable_files = vec![keychain("/Users/u/Library/Keychains")];
        assert_eq!(
            seatbelt_profile(&p),
            "(version 1)\n\
             (allow default)\n\
             (deny file-write*)\n\
             (allow file-write* (subpath \"/Users/u/code\") (subpath \"/private/tmp\"))\n\
             (allow file-write* (regex #\"^/Users/u/Library/Keychains/\
             (login\\.keychain-db[^/]*|\\.fl[0-9A-Fa-f]+)$\"))\n\
             (deny file-write* (subpath \"/Users/u/Library/unharness\"))\n\
             (deny file-read* (subpath \"/Users/u/.ssh\"))\n"
        );
    }

    #[test]
    fn the_directory_of_writable_files_is_matched_literally() {
        assert_eq!(
            files_regex(&keychain(
                "/Users/a.b (c)+[d]{e}|f^g$h?i*j\\k/Library/Keychains"
            ))
            .unwrap(),
            "^/Users/a\\.b \\(c\\)\\+\\[d\\]\\{e\\}\\|f\\^g\\$h\\?i\\*j\\\\k/Library/Keychains/\
             (login\\.keychain-db[^/]*|\\.fl[0-9A-Fa-f]+)$"
        );
    }

    #[test]
    fn writable_files_that_cannot_be_written_down_are_left_out() {
        let mut p = profile();
        p.writable_files = vec![
            keychain("/Users/u\"/Library/Keychains"),
            keychain("/Users/u\n/Library/Keychains"),
            WritableFiles {
                dir: PathBuf::from("/Users/u/Library/Keychains"),
                names: vec![FileName::Prefix("ok"), FileName::Prefix("X/y")],
            },
            WritableFiles {
                dir: PathBuf::from("/Users/u/Library/Keychains"),
                names: vec![],
            },
        ];
        assert!(!seatbelt_profile(&p).contains("regex"));
    }

    #[test]
    fn readable_paths_inside_a_denied_one_are_reopened_after_it() {
        let mut p = profile();
        p.deny_read = vec![PathBuf::from("/Users/u")];
        let text = seatbelt_profile(&p);
        assert!(text.ends_with(
            "(deny file-read* (subpath \"/Users/u\"))\n\
             (allow file-read* (subpath \"/Users/u/code\"))\n"
        ));
    }

    #[test]
    fn paths_are_quoted() {
        assert_eq!(quote(Path::new("/a \"b\"\\c")), "\"/a \\\"b\\\"\\\\c\"");
    }

    #[test]
    fn wrap_keeps_program_args_env_and_cwd() {
        let mut cmd = Command::new("agent");
        cmd.args(["--flag", "x"])
            .env("KEEP", "1")
            .env_remove("DROP")
            .current_dir("/Users/u/code");
        let wrapped = Seatbelt.wrap(cmd, &profile()).unwrap();
        assert_eq!(wrapped.get_program(), SANDBOX_EXEC);
        let args: Vec<_> = wrapped.get_args().map(|a| a.to_os_string()).collect();
        assert_eq!(args[0], "-p");
        assert_eq!(&args[2..], ["agent", "--flag", "x"]);
        let envs: Vec<_> = wrapped.get_envs().collect();
        assert!(envs.contains(&("KEEP".as_ref(), Some("1".as_ref()))));
        assert!(envs.contains(&("DROP".as_ref(), None)));
        assert_eq!(wrapped.get_current_dir(), Some(Path::new("/Users/u/code")));
    }
}
