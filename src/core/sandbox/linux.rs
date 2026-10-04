//! Linux backend: Landlock, applied to the child between fork and exec, so
//! the harness keeps its pid and its pipes.
//!
//! Landlock only allows: a path cannot be denied inside an allowed one. Reads
//! are therefore granted on everything *around* the denied paths (see
//! `read_grants`), and unharness's own state is protected by never being
//! inside a writable path.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr,
};

use super::{SandboxBackend, SandboxProfile};

/// Truncation is only controlled from this version on (Linux 6.2); below it
/// a read-only file could still be emptied, so the sandbox is not offered.
const MIN_ABI: i32 = 3;
/// The rights this backend knows how to grant. Newer kernels' additions are
/// left unhandled, which means unrestricted.
const HANDLED: ABI = ABI::V5;

#[derive(Debug)]
pub struct Landlock {
    abi: i32,
}

impl Landlock {
    pub fn detect() -> std::result::Result<Self, String> {
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
        // SAFETY: with a null attribute and this flag the call only reports
        // the supported ABI version.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if abi < 0 {
            return Err(format!(
                "Landlock is not available ({})",
                std::io::Error::last_os_error()
            ));
        }
        if abi < MIN_ABI as libc::c_long {
            return Err(format!(
                "Landlock ABI {abi} is too old (need {MIN_ABI}, Linux 6.2)"
            ));
        }
        Ok(Landlock { abi: abi as i32 })
    }
}

impl SandboxBackend for Landlock {
    fn name(&self) -> &'static str {
        "landlock"
    }

    fn detail(&self) -> String {
        format!("ABI {}", self.abi)
    }

    fn wrap(&self, mut cmd: Command, profile: &SandboxProfile) -> Result<Command> {
        let ruleset = ruleset(profile)?;
        // SAFETY: the hook runs in the forked child and makes two raw
        // syscalls on a descriptor opened beforehand; it does not allocate.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(cmd)
    }
}

/// Build the ruleset in the parent; the child only has to enter it.
fn ruleset(profile: &SandboxProfile) -> Result<OwnedFd> {
    let all = AccessFs::from_all(HANDLED);
    let read = AccessFs::from_read(HANDLED);
    let mut created = Ruleset::default()
        .handle_access(all)
        .context("Landlock ruleset")?
        .create()
        .context("Landlock ruleset")?;

    let (around, listed) = read_grants(&profile.deny_read);
    for path in &around {
        created = allow(created, path, read)?;
    }
    for path in &listed {
        created = allow(created, path, AccessFs::ReadDir.into())?;
    }
    for path in &profile.writable {
        created = allow(created, path, all)?;
    }

    let fd: Option<OwnedFd> = created.into();
    fd.context("Landlock is not supported by this kernel")
}

/// Grant `access` beneath `path`. A path that vanished meanwhile is skipped.
fn allow(
    created: RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated> {
    let Ok(fd) = PathFd::new(path) else {
        return Ok(created);
    };
    let access = if path.is_dir() {
        access
    } else {
        access & AccessFs::from_file(HANDLED)
    };
    if access.is_empty() {
        return Ok(created);
    }
    created
        .add_rule(PathBeneath::new(fd, access))
        .with_context(|| format!("Landlock rule for {}", path.display()))
}

/// What to grant reading on so that everything but `deny` is readable.
///
/// Returns the paths readable in full, and the directories on the way to a
/// denied path, which are only listable: their entries are granted one by
/// one. (The listing right reaches into the denied directories too, so names
/// in them are visible; their contents are not.) An entry that appears in
/// such a directory later is not readable until the next spawn.
fn read_grants(deny: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut around = Vec::new();
    let mut listed = Vec::new();
    walk(Path::new("/"), deny, &mut around, &mut listed);
    (around, listed)
}

fn walk(dir: &Path, deny: &[PathBuf], around: &mut Vec<PathBuf>, listed: &mut Vec<PathBuf>) {
    let leads_to_denied = |p: &Path| deny.iter().any(|d| d != p && d.starts_with(p));
    if !leads_to_denied(dir) {
        around.push(dir.to_path_buf());
        return;
    }
    listed.push(dir.to_path_buf());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if deny.contains(&path) {
            continue;
        }
        match entry.file_type() {
            // Access is checked on what a link points to, which the walk
            // reaches by its real path.
            Ok(t) if t.is_symlink() => {}
            Ok(t) if t.is_dir() => walk(&path, deny, around, listed),
            _ => around.push(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::SandboxLevel;
    use super::*;

    #[test]
    fn read_grants_go_around_the_denied_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in [
            "home/.ssh",
            "home/.config/gh",
            "home/.config/other",
            "home/code",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("home/.profile"), "").unwrap();
        std::os::unix::fs::symlink(root.join("home/.ssh"), root.join("home/link")).unwrap();

        let deny = vec![root.join("home/.ssh"), root.join("home/.config/gh")];
        let (around, listed) = read_grants(&deny);

        let home = root.join("home");
        for p in [".profile", "code", ".config/other"] {
            assert!(around.contains(&home.join(p)), "{p} missing");
        }
        for p in [".ssh", ".config/gh", ".config", "link", ""] {
            assert!(!around.contains(&home.join(p)), "{p} granted");
        }
        assert!(listed.contains(&home) && listed.contains(&home.join(".config")));
        assert!(listed.contains(&PathBuf::from("/")));

        assert_eq!(read_grants(&[]), (vec![PathBuf::from("/")], vec![]));
    }

    /// Run `script` under `sh` inside the profile and return its exit code.
    fn run(profile: &SandboxProfile, script: &str) -> Option<i32> {
        let backend = Landlock::detect().ok()?;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let mut cmd = backend.wrap(cmd, profile).unwrap();
        cmd.status().unwrap().code()
    }

    #[test]
    fn confines_writes_and_denied_reads() {
        if let Err(why) = Landlock::detect() {
            eprintln!("skipping: {why}");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["ws", "outside", "secret"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("secret/key"), "k").unwrap();
        std::fs::write(root.join("outside/file"), "o").unwrap();
        let profile = SandboxProfile {
            level: SandboxLevel::WorkspaceWrite,
            workspace: root.join("ws"),
            writable: vec![root.join("ws"), PathBuf::from("/dev")],
            deny_read: vec![root.join("secret")],
            protected: vec![],
        };
        let at = |p: &str| root.join(p).display().to_string();

        assert_eq!(
            run(&profile, &format!("echo x > {}", at("ws/new"))),
            Some(0)
        );
        assert_eq!(std::fs::read_to_string(root.join("ws/new")).unwrap(), "x\n");
        assert_eq!(
            run(&profile, &format!("cat {} > /dev/null", at("outside/file"))),
            Some(0)
        );

        assert_ne!(
            run(
                &profile,
                &format!("echo x > {} 2>/dev/null", at("outside/new"))
            ),
            Some(0)
        );
        assert!(!root.join("outside/new").exists());
        assert_ne!(
            run(&profile, &format!(": > {} 2>/dev/null", at("outside/file"))),
            Some(0)
        );
        assert_eq!(
            std::fs::read_to_string(root.join("outside/file")).unwrap(),
            "o"
        );
        assert_ne!(
            run(&profile, &format!("rm {} 2>/dev/null", at("outside/file"))),
            Some(0)
        );
        assert_ne!(
            run(&profile, &format!("cat {} 2>/dev/null", at("secret/key"))),
            Some(0)
        );

        // A grandchild inherits the confinement.
        assert_ne!(
            run(
                &profile,
                &format!("sh -c 'echo x > {}' 2>/dev/null", at("outside/new"))
            ),
            Some(0)
        );
    }
}
