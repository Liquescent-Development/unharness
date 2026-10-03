//! Workspace file checkpoints, so a rewind can put files back whichever
//! harness changed them.
//!
//! A checkpoint is a commit of the whole working tree (tracked and untracked
//! files, minus what `.gitignore` excludes) built through a private index
//! file. The user's index, branch, HEAD and stash are never touched; the
//! commits are only reachable from `refs/unharness/…`, which keeps them from
//! being garbage collected. Outside a git repository there are no checkpoints.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

/// Ref namespace holding every checkpoint commit.
pub const REF_PREFIX: &str = "refs/unharness/checkpoints";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Checkpoint of the tree as it was just before the restore.
    pub undo: String,
    /// Files that differ between that tree and the restored one.
    pub changed: usize,
}

#[derive(Debug, Clone)]
pub struct Checkpoints {
    root: PathBuf,
    git_dir: PathBuf,
}

impl Checkpoints {
    /// `Some` when `root` is the top of a git work tree.
    pub fn open(root: &Path) -> Option<Self> {
        if !root.join(".git").exists() {
            return None;
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let git_dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        Some(Checkpoints {
            root: root.to_path_buf(),
            git_dir,
        })
    }

    fn git(&self, index: Option<&Path>) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd
    }

    /// A private index seeded from the user's (so unchanged files are not re-hashed).
    fn scratch_index(&self) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = self.git_dir.join(format!(
            "unharness-index.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::copy(self.git_dir.join("index"), &path);
        path
    }

    /// Commit the working tree as it is now and keep it under `name`
    /// (a path below [`REF_PREFIX`]). Returns the commit id.
    pub fn snapshot(&self, name: &str) -> Result<String> {
        let index = self.scratch_index();
        let result = self.snapshot_with(&index, name);
        let _ = std::fs::remove_file(&index);
        result
    }

    fn snapshot_with(&self, index: &Path, name: &str) -> Result<String> {
        run(self.git(Some(index)).args(["add", "-A", "--", "."]))?;
        // unharness's own state directory is never part of a checkpoint, even
        // when it is not ignored. (Excluding it in the `add` pathspec makes
        // git fail when the directory exists and *is* ignored.)
        run(self.git(Some(index)).args([
            "rm",
            "-r",
            "-q",
            "--cached",
            "--ignore-unmatch",
            "--",
            ".unharness",
        ]))?;
        let tree = run(self.git(Some(index)).arg("write-tree"))?;
        let mut commit = self.git(None);
        commit
            .args(["commit-tree", &tree, "--no-gpg-sign", "-m"])
            .arg(format!("unharness checkpoint {name}"))
            // Works without a configured identity and never looks like the user's commit.
            .env("GIT_AUTHOR_NAME", "unharness")
            .env("GIT_AUTHOR_EMAIL", "unharness@localhost")
            .env("GIT_COMMITTER_NAME", "unharness")
            .env("GIT_COMMITTER_EMAIL", "unharness@localhost");
        if let Ok(head) = run(self.git(None).args(["rev-parse", "-q", "--verify", "HEAD"])) {
            commit.args(["-p", &head]);
        }
        let id = run(&mut commit)?;
        run(self
            .git(None)
            .args(["update-ref", &format!("{REF_PREFIX}/{name}"), &id]))?;
        Ok(id)
    }

    /// Make the working tree match `checkpoint`: files it has are written
    /// back, files created since are removed. Ignored files and the index
    /// are left alone. The tree as it was is checkpointed first under
    /// `undo_name`, so the restore itself can be undone.
    pub fn restore(&self, checkpoint: &str, undo_name: &str) -> Result<RestoreReport> {
        let undo = self.snapshot(undo_name)?;
        let diff = |filter: &[&str]| -> Result<Vec<String>> {
            let out = run(self
                .git(None)
                .args(["diff", "--name-only", "-z", "--no-renames"])
                .args(filter)
                .args([checkpoint, &undo]))?;
            Ok(out
                .split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect())
        };
        let changed = diff(&[])?.len();
        for created in diff(&["--diff-filter=A"])? {
            let path = self.root.join(&created);
            std::fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
        let index = self.scratch_index();
        let result = run(self.git(Some(&index)).args(["read-tree", checkpoint])).and_then(|_| {
            run(self
                .git(Some(&index))
                .args(["checkout-index", "--all", "--force"]))
        });
        let _ = std::fs::remove_file(&index);
        result?;
        Ok(RestoreReport { undo, changed })
    }

    /// Drop every checkpoint ref below `prefix` (e.g. one conversation's).
    /// The commits become unreachable and git collects them in time.
    pub fn forget(&self, prefix: &str) -> Result<usize> {
        let refs = run(self.git(None).args([
            "for-each-ref",
            "--format=%(refname)",
            &format!("{REF_PREFIX}/{prefix}"),
        ]))?;
        let mut n = 0;
        for r in refs.lines().filter(|l| !l.is_empty()) {
            run(self.git(None).args(["update-ref", "-d", r]))?;
            n += 1;
        }
        Ok(n)
    }
}

/// Run a git command; trimmed stdout on success, stderr in the error otherwise.
fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().context("could not run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            cmd.get_args()
                .skip(2)
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join(".gitignore"), "ignored.log\n.unharness/\n").unwrap();
        fs::write(dir.path().join("kept.txt"), "v1\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "--no-gpg-sign", "-m", "init"]);
        dir
    }

    #[test]
    fn no_checkpoints_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Checkpoints::open(dir.path()).is_none());
    }

    #[test]
    fn restore_puts_files_back_and_can_be_undone() {
        let dir = repo();
        let root = dir.path();
        let cp = Checkpoints::open(root).unwrap();
        fs::write(root.join("untracked.txt"), "mine\n").unwrap();
        fs::write(root.join("ignored.log"), "noise 1\n").unwrap();
        fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(root, &["add", "staged.txt"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        let status = git(root, &["status", "--porcelain"]);

        let before = cp.snapshot("c1/0").unwrap();
        // Taking a checkpoint is invisible to the user's repository.
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(root, &["status", "--porcelain"]), status);
        assert_eq!(git(root, &["stash", "list"]), "");

        // An agent edits, creates, deletes.
        fs::write(root.join("kept.txt"), "v2\n").unwrap();
        fs::write(root.join("new.txt"), "agent made this\n").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/deep.txt"), "x\n").unwrap();
        fs::remove_file(root.join("untracked.txt")).unwrap();
        fs::write(root.join("ignored.log"), "noise 2\n").unwrap();

        let report = cp.restore(&before, "c1/undo").unwrap();
        assert_eq!(report.changed, 4);
        assert_eq!(fs::read_to_string(root.join("kept.txt")).unwrap(), "v1\n");
        assert_eq!(
            fs::read_to_string(root.join("untracked.txt")).unwrap(),
            "mine\n"
        );
        assert!(!root.join("new.txt").exists() && !root.join("sub/deep.txt").exists());
        // Ignored files are outside checkpoints and are left as they are.
        assert_eq!(
            fs::read_to_string(root.join("ignored.log")).unwrap(),
            "noise 2\n"
        );
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(root, &["status", "--porcelain"]), status);

        // Undo brings the agent's version back.
        cp.restore(&report.undo, "c1/undo2").unwrap();
        assert_eq!(fs::read_to_string(root.join("kept.txt")).unwrap(), "v2\n");
        assert!(root.join("new.txt").exists() && root.join("sub/deep.txt").exists());
        assert!(!root.join("untracked.txt").exists());

        assert_eq!(cp.forget("c1/").unwrap(), 3);
        assert_eq!(
            git(root, &["for-each-ref", REF_PREFIX]),
            "",
            "no checkpoint refs left"
        );
    }

    #[test]
    fn state_directory_is_left_out_whether_or_not_it_is_ignored() {
        for ignored in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            git(root, &["init", "-q"]);
            if ignored {
                fs::write(root.join(".gitignore"), ".unharness/\n").unwrap();
            }
            fs::create_dir_all(root.join(".unharness/conversations")).unwrap();
            fs::write(root.join(".unharness/conversations/c.json"), "old").unwrap();
            fs::write(root.join("a.txt"), "1\n").unwrap();
            let cp = Checkpoints::open(root).unwrap();
            let first = cp.snapshot("c/0").unwrap();
            let files = git(root, &["ls-tree", "-r", "--name-only", &first]);
            assert!(
                files.contains("a.txt") && !files.contains(".unharness"),
                "{files}"
            );

            // A restore never rolls back unharness's own records.
            fs::write(root.join(".unharness/conversations/c.json"), "new").unwrap();
            fs::write(root.join("a.txt"), "2\n").unwrap();
            cp.restore(&first, "c/undo").unwrap();
            assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "1\n");
            assert_eq!(
                fs::read_to_string(root.join(".unharness/conversations/c.json")).unwrap(),
                "new"
            );
        }
    }

    #[test]
    fn works_before_the_first_commit() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("a.txt"), "1\n").unwrap();
        let cp = Checkpoints::open(dir.path()).unwrap();
        let first = cp.snapshot("c/0").unwrap();
        fs::write(dir.path().join("a.txt"), "2\n").unwrap();
        cp.restore(&first, "c/undo").unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "1\n");
    }
}
