//! Workspace rules synchronization.
//!
//! `AGENTS.md` is the source of truth for instructions and `CLAUDE.md` /
//! `GEMINI.md` are symlinks to it. Custom commands and subagents under
//! `.agents/` are projected into each vendor's directory (`projections`).
//! Skills are installed and projected by the external `skills` CLI.

pub mod projections;
pub mod rules;

use std::fs;
use std::path::{Path, PathBuf};

pub use projections::sync_projections;
pub use rules::sync_workspace_rules;

/// Opens `path` without following a link in its last component. Sync runs
/// outside the sandbox, so a link an agent put there must not carry a
/// write out of the workspace.
fn open_nofollow(options: &mut fs::OpenOptions, path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(options, libc::O_NOFOLLOW);
    options.open(path)
}

/// Creates `path` new (`O_EXCL`, which no link passes) and fills it with
/// `fill`. A file that could not be filled is removed, so a write that
/// fails part-way leaves nothing behind to be warned about later.
fn create_new_with(
    path: &Path,
    fill: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = open_nofollow(&mut options, path)?;
    fill(&mut file).inspect_err(|_| {
        let _ = fs::remove_file(path);
    })
}

/// The most sync reads of one file. Instructions, commands and agent
/// definitions are far smaller; a file an agent made endless is not.
const READ_LIMIT: u64 = 4 << 20;

/// Opens `path` for reading only if it is a regular file, following a
/// link in its last component only if `follow`. Sync runs outside the
/// sandbox and before every launch, so a FIFO (whose read would wait for
/// a writer) or a device an agent left in the workspace must not hang it:
/// the open does not block (`O_NONBLOCK`) and the handle is checked.
pub(crate) fn open_regular(path: &Path, follow: bool) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        let mut flags = libc::O_NONBLOCK | libc::O_NOCTTY;
        if !follow {
            flags |= libc::O_NOFOLLOW;
        }
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, flags);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// All of `file`, if it is at most [`READ_LIMIT`] long.
fn read_capped(file: &fs::File) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut content = Vec::new();
    file.take(READ_LIMIT + 1).read_to_end(&mut content)?;
    if content.len() as u64 > READ_LIMIT {
        return Err(std::io::Error::other(format!(
            "larger than {} MiB",
            READ_LIMIT >> 20
        )));
    }
    Ok(content)
}

/// [`open_regular`] and [`read_capped`].
fn read_regular(path: &Path, follow: bool) -> std::io::Result<Vec<u8>> {
    read_capped(&open_regular(path, follow)?)
}

/// Whether `open_nofollow` failed because `path` is a link.
#[cfg(unix)]
fn is_link_error(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_link_error(_: &std::io::Error) -> bool {
    false
}

#[cfg(all(test, unix))]
pub(crate) mod testing {
    use std::path::Path;
    use std::time::Duration;

    pub fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path that lives through the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
    }

    /// `f`'s value, or a failed test if it takes more than five seconds
    /// (a read of a FIFO with no writer never ends).
    pub fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(f()));
        rx.recv_timeout(Duration::from_secs(5))
            .expect("panicked or did not finish in five seconds")
    }

    #[test]
    fn a_write_that_fails_part_way_leaves_nothing() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let e = super::create_new_with(&path, |file| {
            file.write_all(b"# half")?;
            Err(std::io::Error::other("disk full"))
        })
        .unwrap_err();
        assert_eq!(e.to_string(), "disk full");
        assert!(std::fs::symlink_metadata(&path).is_err());
        super::create_new_with(&path, |file| file.write_all(b"# rules")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# rules");
    }
}

pub fn find_workspace_root(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists()
            || current.join(".agents").exists()
            || current.join("AGENTS.md").exists()
        {
            return Some(current);
        }
        if !current.pop() {
            break;
        }
    }
    None
}
