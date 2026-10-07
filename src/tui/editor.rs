//! Editing the prompt in the user's own editor (`$VISUAL`, else `$EDITOR`).

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

/// The editor command line from the environment, if one is set.
pub fn command() -> Option<String> {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|v| !v.trim().is_empty())
}

/// How an edit ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Edited {
    Saved(String),
    /// The editor exited with an error.
    Failed,
    /// `stop` came first, and the editor was ended.
    Stopped,
}

/// How long an editor asked to end (SIGTERM) has before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Run `editor` on a temporary file in `dir` holding `text` and return what
/// it saved. `editor` goes through the shell, as git does, so it may carry
/// arguments (`code -w`). The file is private to the user and removed
/// afterwards. Waits until the editor exits, or until `stop`, when the
/// editor and what it started are ended; the caller hands it the terminal
/// first.
pub async fn edit(
    editor: &str,
    text: &str,
    dir: &Path,
    stop: impl Future<Output = ()>,
) -> Result<Edited> {
    let path = dir.join(format!("unharness-prompt-{}.md", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    let result = async {
        file.write_all(text.as_bytes())?;
        drop(file);
        // In unharness's process group, which the terminal reads for.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&path)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("run {editor}"))?;
        let status = tokio::select! {
            status = child.wait() => status.with_context(|| format!("run {editor}"))?,
            _ = stop => {
                end(&mut child).await;
                return Ok(Edited::Stopped);
            }
        };
        if !status.success() {
            return Ok(Edited::Failed);
        }
        let edited =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        // Editors end the file with a newline the prompt does not want.
        Ok(Edited::Saved(
            edited.trim_end_matches(['\n', '\r']).to_string(),
        ))
    }
    .await;
    let _ = std::fs::remove_file(&path);
    result
}

/// SIGTERM to the editor and what it started (the shell may have forked
/// it, and an editor may outlive that shell), and SIGKILL to what of them
/// is left after [`STOP_GRACE`].
async fn end(child: &mut tokio::process::Child) {
    #[cfg(target_os = "linux")]
    if let Some(pid) = child.id() {
        let tree = crate::core::process::HeldTree::of(pid);
        tree.signal(libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        while tree.any_running() && tokio::time::Instant::now() < deadline {
            // Reaped as it goes, so that it no longer counts.
            let _ = child.try_wait();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tree.signal(libc::SIGKILL);
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    if let Some(pid) = child.id() {
        // SAFETY: a signal to the editor's shell, a child not yet reaped.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        let _ = tokio::time::timeout(STOP_GRACE, child.wait()).await;
    }
    let _ = child.kill().await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn leftovers(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    async fn edit_to_end(editor: &str, text: &str, dir: &Path) -> Result<Edited> {
        edit(editor, text, dir, std::future::pending()).await
    }

    #[tokio::test]
    async fn returns_what_the_editor_saved_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        // An "editor" with arguments of its own that appends two lines.
        let editor = r#"sh -c 'printf "second\nthird\n" >> "$0"'"#;
        let edited = edit_to_end(editor, "first\n", tmp.path()).await.unwrap();
        assert_eq!(edited, Edited::Saved("first\nsecond\nthird".into()));
        assert_eq!(leftovers(tmp.path()), 0);
    }

    #[tokio::test]
    async fn a_failing_editor_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            edit_to_end("false", "keep me", tmp.path()).await.unwrap(),
            Edited::Failed
        );
        assert_eq!(
            edit_to_end("unharness-no-such-editor", "keep me", tmp.path())
                .await
                .unwrap(),
            Edited::Failed
        );
        assert_eq!(leftovers(tmp.path()), 0);
        assert!(
            edit_to_end("true", "x", &tmp.path().join("missing"))
                .await
                .is_err()
        );
    }

    /// An editor that writes the pid of the shell it runs in to `pid`
    /// and waits, in a process of its own below that shell.
    fn waiting_editor(pid: &Path, deaf: bool) -> String {
        let trap = if deaf { "trap \"\" TERM; " } else { "" };
        format!(
            "sh -c '{trap}echo $$ > {}; while :; do sleep 0.1; done'",
            pid.display()
        )
    }

    async fn stopped(deaf: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("files");
        std::fs::create_dir(&files).unwrap();
        let pid_file = tmp.path().join("pid");
        let editor = waiting_editor(&pid_file, deaf);
        let pid = async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|p| p.trim().parse::<u32>().ok())
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let stop = async move {
            let _ = pid_tx.send(pid.await);
        };
        let edited = tokio::time::timeout(
            STOP_GRACE + Duration::from_secs(5),
            edit(&editor, "draft", &files, stop),
        )
        .await
        .expect("the editor was not ended")
        .unwrap();
        assert_eq!(edited, Edited::Stopped);
        let pid = pid_rx.await.unwrap();
        // Left running, it would hold the test's output open.
        struct Reap(u32);
        impl Drop for Reap {
            fn drop(&mut self) {
                // SAFETY: the shell this test's editor ran in.
                unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
            }
        }
        let _reap = Reap(pid);
        // Killed, it may take a moment to be gone.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while crate::core::process::running(pid) {
            assert!(tokio::time::Instant::now() < deadline, "the editor runs on");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(leftovers(&files), 0);
    }

    #[tokio::test]
    async fn a_stop_ends_the_editor_and_what_it_started() {
        stopped(false).await;
    }

    #[tokio::test]
    async fn a_stop_kills_an_editor_deaf_to_sigterm() {
        stopped(true).await;
    }
}
