//! Workspace file checkpoints, so a rewind can put files back whichever
//! harness changed them.
//!
//! Checkpoints live in a *shadow repository*: a git directory of unharness's
//! own, outside the project (under the user's state directory), that treats
//! the project as its working tree. A checkpoint is a commit there of the
//! whole tree (tracked and untracked files, minus what the project's ignore
//! rules exclude). Nothing is ever written to the project's own `.git`: no
//! objects, no refs, no index. Only git projects are checkpointed, so the
//! ignore rules bound what gets copied.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

/// Ref namespace (inside the shadow repository) holding the checkpoints.
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
    /// The project (the shadow repository's working tree).
    root: PathBuf,
    /// The shadow repository's git directory.
    shadow: PathBuf,
}

impl Checkpoints {
    /// Where shadow repositories are kept: `<state dir>/unharness/checkpoints`.
    pub fn default_store() -> PathBuf {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(std::env::temp_dir)
            .join("unharness")
            .join("checkpoints")
    }

    /// `Some` when `root` is the top of a git work tree.
    pub fn open(root: &Path) -> Option<Self> {
        Self::open_in(root, &Self::default_store())
    }

    /// Like [`open`](Self::open), with the shadow repository kept under `store`.
    pub fn open_in(root: &Path, store: &Path) -> Option<Self> {
        if !root.join(".git").exists() {
            return None;
        }
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        // One shadow per project path: a readable name plus a hash of the
        // full path (FNV-1a, so the name is the same in every build).
        let hash = root
            .to_string_lossy()
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".into());
        Some(Checkpoints {
            shadow: store.join(format!("{name}-{hash:016x}.git")),
            root,
        })
    }

    /// The shadow repository's git directory.
    pub fn shadow_dir(&self) -> &Path {
        &self.shadow
    }

    /// git, acting on the shadow repository with the project as work tree.
    fn git(&self, index: Option<&Path>) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(&self.shadow)
            .arg("--work-tree")
            .arg(&self.root)
            .current_dir(&self.root);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd
    }

    /// Create the shadow repository on first use (private to the user: it
    /// holds copies of the project's files) and refresh its ignore rules.
    fn prepare(&self) -> Result<()> {
        if !self.shadow.join("HEAD").exists() {
            std::fs::create_dir_all(&self.shadow)
                .with_context(|| format!("could not create {}", self.shadow.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&self.shadow, std::fs::Permissions::from_mode(0o700));
            }
            run(Command::new("git")
                .arg("init")
                .arg("-q")
                .env("GIT_DIR", &self.shadow)
                .current_dir(&self.root))?;
        }
        // `.gitignore` files are read from the work tree as usual; the
        // project's private excludes are not, so mirror them. unharness's own
        // state directory is never part of a checkpoint.
        let mut exclude =
            std::fs::read_to_string(self.root.join(".git/info/exclude")).unwrap_or_default();
        exclude.push_str("\n.unharness/\n");
        let info = self.shadow.join("info");
        std::fs::create_dir_all(&info)?;
        std::fs::write(info.join("exclude"), exclude)?;
        Ok(())
    }

    /// A throwaway index inside the shadow repository.
    fn scratch_index(&self) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        self.shadow.join(format!(
            "restore-index.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Commit the working tree as it is now and keep it under `name`
    /// (a path below [`REF_PREFIX`]). Returns the commit id.
    pub fn snapshot(&self, name: &str) -> Result<String> {
        self.prepare()?;
        // The shadow's own index persists between checkpoints, so unchanged
        // files are not read again.
        run(self.git(None).args(["add", "-A", "--", "."]))?;
        let tree = run(self.git(None).arg("write-tree"))?;
        let id = run(self
            .git(None)
            .args(["commit-tree", &tree, "--no-gpg-sign", "-m"])
            .arg(format!("unharness checkpoint {name}"))
            // Works without a configured identity.
            .env("GIT_AUTHOR_NAME", "unharness")
            .env("GIT_AUTHOR_EMAIL", "unharness@localhost")
            .env("GIT_COMMITTER_NAME", "unharness")
            .env("GIT_COMMITTER_EMAIL", "unharness@localhost"))?;
        run(self
            .git(None)
            .args(["update-ref", &format!("{REF_PREFIX}/{name}"), &id]))?;
        Ok(id)
    }

    /// Make the working tree match `checkpoint`: files it has are written
    /// back, files created since are removed. Ignored files and the
    /// project's own git state are left alone. The tree as it was is
    /// checkpointed first under `undo_name`, so the restore can be undone.
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

    /// Drop every checkpoint below `prefix` (e.g. one conversation's).
    pub fn forget(&self, prefix: &str) -> Result<usize> {
        if !self.shadow.join("HEAD").exists() {
            return Ok(0);
        }
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

    /// Delete the shadow repository, and with it every checkpoint of this project.
    pub fn destroy(&self) -> Result<bool> {
        if !self.shadow.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&self.shadow)
            .with_context(|| format!("could not remove {}", self.shadow.display()))?;
        Ok(true)
    }
}

/// Run a git command; trimmed stdout on success, stderr in the error otherwise.
fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().context("could not run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            cmd.get_args()
                .map(|a| a.to_string_lossy())
                .filter(|a| !a.starts_with('/')
                    && !a.starts_with("--git-dir")
                    && !a.starts_with("--work-tree"))
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

    /// Everything git knows about the project's own repository.
    fn project_git_state(root: &Path) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            git(root, &["rev-parse", "HEAD"]),
            git(root, &["status", "--porcelain"]),
            git(root, &["stash", "list"]),
            git(root, &["for-each-ref"]),
            git(root, &["count-objects", "-v"])
        )
    }

    /// A project repository and a separate place for its shadow.
    fn project() -> (tempfile::TempDir, tempfile::TempDir, Checkpoints) {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join(".gitignore"), "ignored.log\n").unwrap();
        fs::write(dir.path().join("kept.txt"), "v1\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "--no-gpg-sign", "-m", "init"]);
        let cp = Checkpoints::open_in(dir.path(), store.path()).unwrap();
        (dir, store, cp)
    }

    #[test]
    fn no_checkpoints_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        assert!(Checkpoints::open_in(dir.path(), store.path()).is_none());
    }

    #[test]
    fn restore_puts_files_back_and_can_be_undone() {
        let (dir, store, cp) = project();
        let root = dir.path();
        fs::write(root.join("untracked.txt"), "mine\n").unwrap();
        fs::write(root.join("ignored.log"), "noise 1\n").unwrap();
        fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(root, &["add", "staged.txt"]);
        let state = project_git_state(root);

        let before = cp.snapshot("c1/0").unwrap();
        // The project's repository is untouched: same HEAD, status, stash,
        // refs, and not one object more. The checkpoint is in the shadow.
        assert_eq!(project_git_state(root), state);
        assert!(cp.shadow_dir().starts_with(store.path()));
        assert!(cp.shadow_dir().join("HEAD").exists());

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
        assert_eq!(project_git_state(root), state);

        // Undo brings the agent's version back.
        cp.restore(&report.undo, "c1/undo2").unwrap();
        assert_eq!(fs::read_to_string(root.join("kept.txt")).unwrap(), "v2\n");
        assert!(root.join("new.txt").exists() && root.join("sub/deep.txt").exists());
        assert!(!root.join("untracked.txt").exists());

        assert_eq!(cp.forget("c1/").unwrap(), 3);
        assert_eq!(cp.forget("c1/").unwrap(), 0);
        assert!(cp.destroy().unwrap());
        assert!(!cp.shadow_dir().exists());
    }

    #[test]
    fn private_excludes_and_the_state_directory_are_left_out() {
        for ignored in [true, false] {
            let (dir, _store, cp) = project();
            let root = dir.path();
            if ignored {
                fs::write(root.join(".gitignore"), "ignored.log\n.unharness/\n").unwrap();
            }
            // Excluded only in the project's private `.git/info/exclude`.
            fs::write(root.join(".git/info/exclude"), "secret.env\n").unwrap();
            fs::write(root.join("secret.env"), "TOKEN=1\n").unwrap();
            fs::create_dir_all(root.join(".unharness/conversations")).unwrap();
            fs::write(root.join(".unharness/conversations/c.json"), "old").unwrap();
            fs::write(root.join("a.txt"), "1\n").unwrap();

            let first = cp.snapshot("c/0").unwrap();
            let files = run(cp.git(None).args(["ls-tree", "-r", "--name-only", &first])).unwrap();
            assert!(files.contains("a.txt"), "{files}");
            assert!(
                !files.contains(".unharness") && !files.contains("secret.env"),
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
    fn works_before_the_first_commit_and_keeps_projects_apart() {
        let store = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for dir in [&a, &b] {
            git(dir.path(), &["init", "-q"]);
            fs::write(dir.path().join("a.txt"), "1\n").unwrap();
        }
        let cp_a = Checkpoints::open_in(a.path(), store.path()).unwrap();
        let cp_b = Checkpoints::open_in(b.path(), store.path()).unwrap();
        assert_ne!(cp_a.shadow_dir(), cp_b.shadow_dir());
        let first = cp_a.snapshot("c/0").unwrap();
        cp_b.snapshot("c/0").unwrap();
        fs::write(a.path().join("a.txt"), "2\n").unwrap();
        cp_a.restore(&first, "c/undo").unwrap();
        assert_eq!(fs::read_to_string(a.path().join("a.txt")).unwrap(), "1\n");
        // Same project path, same shadow, on every run.
        assert_eq!(
            Checkpoints::open_in(a.path(), store.path())
                .unwrap()
                .shadow_dir(),
            cp_a.shadow_dir()
        );
    }
}
