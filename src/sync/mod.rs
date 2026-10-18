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
    read_at_most(file, READ_LIMIT)
}

/// All of `file`, if it is at most `limit` (whole MiB) long; never more
/// is read.
pub(crate) fn read_at_most(file: &fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut content = Vec::new();
    file.take(limit + 1).read_to_end(&mut content)?;
    if content.len() as u64 > limit {
        return Err(std::io::Error::other(format!(
            "larger than {} MiB",
            limit >> 20
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

/// The workspace a run started in `start_dir` belongs to: the nearest
/// directory, `start_dir` or one above it, that holds `.git`, `.agents`
/// or `AGENTS.md`. `None` when there is none, and callers take
/// `start_dir` as the workspace; nothing is synced then.
///
/// The home directory is never one, and the walk stops when it reaches
/// it. Other tools keep global skills in `~/.agents`, and dotfiles live
/// in a `~/.git`: either made the home directory the workspace of every
/// folder below it that had no marker of its own, so that auto-sync
/// wrote the vendors' global settings (`~/.claude/commands`) and the
/// sandbox refused the workspace for holding unharness's own state
/// (#343). Started in the home directory itself there is no root either:
/// the workspace is then the home directory, nothing is synced, and the
/// sandbox refuses it as it did. A directory outside the home directory
/// is walked up to `/` as before.
pub fn find_workspace_root(start_dir: &Path) -> Option<PathBuf> {
    workspace_root_below(start_dir, dirs::home_dir().as_deref())
}

/// [`find_workspace_root`] with the home directory given.
fn workspace_root_below(start_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let home = resolved_home(home);
    let mut current = start_dir.to_path_buf();
    loop {
        if is_resolved_home(&current, home.as_deref()) {
            return None;
        }
        if current.join(".git").exists()
            || current.join(".agents").exists()
            || current.join("AGENTS.md").exists()
        {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Whether `dir` is the home directory, which is never a workspace
/// ([`find_workspace_root`]). The two are compared as they resolve, so a
/// home named through a link, or reached through one, is still the home
/// directory.
pub fn is_home_dir(dir: &Path) -> bool {
    is_home_in(dir, dirs::home_dir().as_deref())
}

/// [`is_home_dir`] with the home directory given.
pub(crate) fn is_home_in(dir: &Path, home: Option<&Path>) -> bool {
    is_resolved_home(dir, resolved_home(home).as_deref())
}

/// Why [`find_workspace_root`] found no root for `dir`, and what to do,
/// for the user.
pub fn no_workspace_reason(dir: &Path) -> String {
    no_workspace_reason_in(dir, dirs::home_dir().as_deref())
}

/// [`no_workspace_reason`] with the home directory given.
fn no_workspace_reason_in(dir: &Path, home: Option<&Path>) -> String {
    let home = resolved_home(home);
    if is_resolved_home(dir, home.as_deref()) {
        return "the home directory is never a workspace; 'unharness init' makes a project \
                directory one"
            .to_string();
    }
    let below_home = dir
        .ancestors()
        .skip(1)
        .any(|above| is_resolved_home(above, home.as_deref()));
    format!(
        "no .git, .agents or AGENTS.md in this directory or above it{}; 'unharness init' \
         makes this directory one",
        if below_home {
            " up to the home directory, which is never a workspace"
        } else {
            ""
        }
    )
}

fn resolved_home(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|h| h.canonicalize().unwrap_or_else(|_| h.to_path_buf()))
}

fn is_resolved_home(dir: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|home| {
        dir == home || dir.canonicalize().is_ok_and(|resolved| resolved == home)
    })
}

#[cfg(test)]
mod tests {
    use super::{no_workspace_reason_in, workspace_root_below};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn dir(path: PathBuf) -> PathBuf {
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn home_in(tmp: &Path) -> PathBuf {
        dir(tmp.join("home"))
    }

    #[test]
    fn a_marker_in_the_home_directory_is_not_a_root_for_a_folder_below_it() {
        for marker in [".agents", ".git", "AGENTS.md"] {
            let tmp = tempfile::tempdir().unwrap();
            let home = home_in(tmp.path());
            if marker == "AGENTS.md" {
                fs::write(home.join(marker), "# rules").unwrap();
            } else {
                dir(home.join(marker));
            }
            let folder = dir(home.join("code/apex/apex-data"));
            assert_eq!(workspace_root_below(&folder, Some(&home)), None, "{marker}");
        }
    }

    #[test]
    fn a_marker_below_the_home_directory_is_still_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_in(tmp.path());
        dir(home.join(".agents"));
        let repo = dir(home.join("code/repo"));
        dir(repo.join(".git"));
        let below = dir(repo.join("src/sync"));
        assert_eq!(
            workspace_root_below(&below, Some(&home)),
            Some(repo.clone())
        );
        assert_eq!(workspace_root_below(&repo, Some(&home)), Some(repo));
    }

    #[test]
    fn started_in_the_home_directory_there_is_no_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_in(tmp.path());
        dir(home.join(".agents"));
        dir(home.join(".git"));
        assert_eq!(workspace_root_below(&home, Some(&home)), None);
    }

    #[test]
    fn a_folder_outside_the_home_directory_is_walked_up_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_in(tmp.path());
        let project = dir(tmp.path().join("srv/project"));
        fs::write(project.join("AGENTS.md"), "# rules").unwrap();
        let below = dir(project.join("a/b"));
        assert_eq!(
            workspace_root_below(&below, Some(&home)),
            Some(project.clone())
        );
        assert_eq!(workspace_root_below(&below, None), Some(project));
    }

    #[test]
    fn a_marker_above_the_home_directory_is_not_a_root_for_a_folder_below_it() {
        let tmp = tempfile::tempdir().unwrap();
        dir(tmp.path().join(".git"));
        let home = home_in(tmp.path());
        let code = dir(home.join("code"));
        assert_eq!(workspace_root_below(&code, Some(&home)), None);
        assert_eq!(workspace_root_below(&home, Some(&home)), None);
        // From outside the home directory it still is.
        let other = dir(tmp.path().join("srv/other"));
        assert_eq!(
            workspace_root_below(&other, Some(&home)),
            Some(tmp.path().to_path_buf())
        );
    }

    #[test]
    fn why_there_is_no_root_says_where_the_search_went() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_in(tmp.path());
        let folder = dir(home.join("code/folder"));
        let outside = dir(tmp.path().join("srv/folder"));
        let in_home = no_workspace_reason_in(&home, Some(&home));
        assert!(in_home.starts_with("the home directory is never a workspace"));
        let below = no_workspace_reason_in(&folder, Some(&home));
        assert!(
            below.contains("or above it up to the home directory, which is never"),
            "{below}"
        );
        let elsewhere = no_workspace_reason_in(&outside, Some(&home));
        assert!(
            elsewhere.contains("or above it; 'unharness init' makes this directory one"),
            "{elsewhere}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_home_directory_named_through_a_link_still_ends_the_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let real = dir(tmp.path().join("private/home"));
        dir(real.join(".agents"));
        let link = tmp.path().join("home");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let folder = dir(real.join("code/folder"));
        // The home directory named by the link, the folder by its own path.
        assert_eq!(workspace_root_below(&folder, Some(&link)), None);
        assert_eq!(workspace_root_below(&real, Some(&link)), None);
        // The home directory named as it is, the folder through the link.
        assert_eq!(
            workspace_root_below(&link.join("code/folder"), Some(&real)),
            None
        );
        assert_eq!(workspace_root_below(&link, Some(&real)), None);
        // A home named with a trailing component that comes back to it.
        assert_eq!(
            workspace_root_below(&folder, Some(&link.join("code/.."))),
            None
        );
    }
}
