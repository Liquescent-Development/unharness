//! macOS backend: the command is launched through `sandbox-exec` with a
//! generated Seatbelt profile. The profile is built on every platform so it
//! can be tested anywhere; only detection is macOS-specific.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use super::{SandboxBackend, SandboxProfile};

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

/// Everything is allowed except writes outside the writable paths and reads
/// of the denied ones. Later rules win, so the denies inside a writable path
/// come last.
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
    }
    out
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
            deny_read: vec![PathBuf::from("/Users/u/.ssh")],
            protected: vec![PathBuf::from("/Users/u/Library/unharness")],
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
