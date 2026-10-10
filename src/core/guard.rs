//! Watching the vendor files that decide how a harness runs next time.
//!
//! A harness's own state directory has to be writable for it, and the files
//! that configure it (settings, hooks, MCP servers, sometimes its binary)
//! sit in that directory, so the sandbox cannot keep an agent from editing
//! them. What unharness can do is notice: a `Watch` fingerprints the files a
//! harness declares (`Harness::guarded`) before a session and compares after
//! each turn. It cannot tell the CLI's own change from the agent's; it makes
//! sure neither happens without the user knowing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Files larger than this are compared by metadata and not kept for restore.
const MAX_KEPT_BYTES: u64 = 1024 * 1024;
/// A tree larger than this is cut off; the rest goes unwatched.
const MAX_TREE_ENTRIES: usize = 50_000;
/// How much of a tree's bookkeeping is read; past it files are compared by
/// metadata.
const MAX_HASHED_BYTES: u64 = 16 * 1024 * 1024;

/// One thing to watch. Paths may start with `~`.
#[derive(Clone)]
pub enum Guarded {
    /// A file, by content.
    File(PathBuf),
    /// A file the CLI itself rewrites for bookkeeping: only what `project`
    /// makes of its content is compared. `None` (it could not be parsed)
    /// falls back to the whole content.
    Projected { path: PathBuf, project: Projection },
    /// A directory tree (skills, hooks, an installed binary), by the mode,
    /// length and change time of everything in it.
    Tree(PathBuf),
    /// A tree in which the CLI itself rewrites some files with the same
    /// bytes (Claude's sync of organisation skills): the files `rewritten`
    /// picks by their path in the tree are compared by content, read again
    /// at every check, when they are small enough to read. Everything else
    /// is as in `Tree`, so that a change undone before the check is still
    /// one there.
    TreeWithBookkeeping {
        path: PathBuf,
        rewritten: fn(&Path) -> bool,
    },
}

pub type Projection = Arc<dyn Fn(&[u8]) -> Option<String> + Send + Sync>;

impl Guarded {
    /// A JSON file of which only `project`'s extract matters.
    pub fn json(path: PathBuf, project: fn(&Value) -> Value) -> Self {
        Guarded::Projected {
            path,
            project: Arc::new(move |bytes| {
                let value: Value = serde_json::from_slice(bytes).ok()?;
                Some(project(&value).to_string())
            }),
        }
    }

    fn path(&self) -> &Path {
        match self {
            Guarded::File(p)
            | Guarded::Tree(p)
            | Guarded::TreeWithBookkeeping { path: p, .. }
            | Guarded::Projected { path: p, .. } => p,
        }
    }
}

impl std::fmt::Debug for Guarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Guarded::File(_) => "File",
            Guarded::Tree(_) => "Tree",
            Guarded::TreeWithBookkeeping { .. } => "TreeWithBookkeeping",
            Guarded::Projected { .. } => "Projected",
        };
        write!(f, "{kind}({})", self.path().display())
    }
}

/// Compared with `same`, not `==`: a bookkeeping file whose content was
/// read both times is not compared by its change time.
#[derive(Debug, Clone)]
enum Fingerprint {
    Missing,
    Content(Vec<u8>),
    /// Too large to keep: (length, change time).
    Large(u64, i128),
    Projection(String),
    Tree(BTreeMap<PathBuf, Stamp>),
}

impl Fingerprint {
    /// Whether nothing that matters changed.
    fn same(&self, other: &Fingerprint) -> bool {
        match (self, other) {
            (Fingerprint::Missing, Fingerprint::Missing) => true,
            (Fingerprint::Content(a), Fingerprint::Content(b)) => a == b,
            (Fingerprint::Large(la, ca), Fingerprint::Large(lb, cb)) => la == lb && ca == cb,
            (Fingerprint::Projection(a), Fingerprint::Projection(b)) => a == b,
            (Fingerprint::Tree(a), Fingerprint::Tree(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|((pa, sa), (pb, sb))| pa == pb && sa.same(sb))
            }
            _ => false,
        }
    }
}

/// An entry in a tree (a directory has only its mode).
#[derive(Debug, Clone)]
struct Stamp {
    /// File type and permissions: a script made executable is a change.
    mode: u32,
    len: u64,
    changed: i128,
    /// SHA-256 of the content, for a bookkeeping file small enough to
    /// read.
    digest: Option<[u8; 32]>,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata, digest: Option<[u8; 32]>) -> Self {
        Stamp {
            mode: mode(meta),
            len: meta.len(),
            changed: changed_at(meta),
            digest,
        }
    }

    fn same(&self, other: &Stamp) -> bool {
        self.mode == other.mode
            && self.len == other.len
            && match (&self.digest, &other.digest) {
                (Some(a), Some(b)) => a == b,
                _ => self.changed == other.changed,
            }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    guarded: Guarded,
    path: PathBuf,
    fingerprint: Fingerprint,
    /// The file as it was when the watch began, for restoring it.
    original: Option<Vec<u8>>,
}

/// A watched file or tree that is no longer what it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    /// Where the content from before the session was saved, when it was
    /// small enough to keep.
    pub saved: Option<PathBuf>,
}

impl Change {
    /// The line shown to the user.
    pub fn describe(&self, harness: &str) -> String {
        let mut line = format!(
            "{harness} configuration changed while it ran: {}",
            self.path.display()
        );
        match &self.saved {
            Some(saved) => line.push_str(&format!(
                ". If you did not expect that, the version from before the session is at {}",
                saved.display()
            )),
            None => line.push_str(". Check it if you did not expect that"),
        }
        line
    }
}

#[derive(Debug, Clone, Default)]
pub struct Watch {
    entries: Vec<Entry>,
}

impl Watch {
    /// Fingerprint everything in `guarded` as it is now.
    pub fn begin(guarded: &[Guarded], home: Option<&Path>) -> Self {
        let entries = guarded
            .iter()
            .map(|g| {
                let path = expand(g.path(), home);
                let original = match g {
                    Guarded::Tree(_) | Guarded::TreeWithBookkeeping { .. } => None,
                    _ => read_small(&path),
                };
                Entry {
                    fingerprint: fingerprint(g, &path),
                    guarded: g.clone(),
                    path,
                    original,
                }
            })
            .collect();
        Watch { entries }
    }

    /// What changed since the last call (or since `begin`). The pre-session
    /// content of each changed file is written under `keep`.
    pub fn changes(&mut self, keep: &Path) -> Vec<Change> {
        let mut changes = Vec::new();
        for entry in &mut self.entries {
            let now = fingerprint(&entry.guarded, &entry.path);
            let same = now.same(&entry.fingerprint);
            // Kept also when the same: the change times move on.
            entry.fingerprint = now;
            if same {
                continue;
            }
            changes.push(Change {
                saved: entry
                    .original
                    .as_deref()
                    .and_then(|bytes| save(keep, &entry.path, bytes)),
                path: entry.path.clone(),
            });
        }
        changes
    }
}

/// Where pre-session copies of changed files go: `<state dir>/unharness/guard`.
pub fn default_keep_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("unharness")
        .join("guard")
}

fn expand(p: &Path, home: Option<&Path>) -> PathBuf {
    match (p.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => p.to_path_buf(),
    }
}

fn read_small(path: &Path) -> Option<Vec<u8>> {
    read_regular(path, true).map(|(_, bytes)| bytes)
}

/// The metadata and content of the regular file at `path`, if it is at
/// most `MAX_KEPT_BYTES` long, both of the file opened. The watched files
/// are writable by the agent: opened without blocking, so that a FIFO put
/// in a file's place cannot hang the guard, and read no further than the
/// limit.
fn read_regular(path: &Path, follow: bool) -> Option<(std::fs::Metadata, Vec<u8>)> {
    let file = crate::sync::open_regular(path, follow).ok()?;
    let meta = file.metadata().ok()?;
    let bytes = crate::sync::read_at_most(&file, MAX_KEPT_BYTES).ok()?;
    // Written meanwhile: what was read is not what was stamped.
    (bytes.len() as u64 == meta.len()).then_some((meta, bytes))
}

/// No file of a tree is bookkeeping.
fn no_bookkeeping(_: &Path) -> bool {
    false
}

fn fingerprint(guarded: &Guarded, path: &Path) -> Fingerprint {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Fingerprint::Missing;
    };
    match guarded {
        Guarded::Tree(_) => tree(path, no_bookkeeping),
        Guarded::TreeWithBookkeeping { rewritten, .. } => tree(path, *rewritten),
        Guarded::Projected { project, .. } => match read_small(path) {
            Some(bytes) => match project(&bytes) {
                Some(projection) => Fingerprint::Projection(projection),
                // Unparseable is itself worth noticing, by content.
                None => Fingerprint::Content(bytes),
            },
            None => Fingerprint::Large(meta.len(), changed_at(&meta)),
        },
        Guarded::File(_) => match read_small(path) {
            Some(bytes) => Fingerprint::Content(bytes),
            None => Fingerprint::Large(meta.len(), changed_at(&meta)),
        },
    }
}

fn tree(root: &Path, rewritten: fn(&Path) -> bool) -> Fingerprint {
    let mut walk = Walk {
        root,
        rewritten,
        budget: MAX_HASHED_BYTES,
        out: BTreeMap::new(),
    };
    walk.dir(root);
    Fingerprint::Tree(walk.out)
}

/// Mode, length and change time of everything under a tree, and the
/// content digest of its bookkeeping files. The change time cannot be set
/// back by whoever wrote the file, unlike the modification time. A
/// bookkeeping file is read at every check, not only when its change time
/// moved: a write through a shared mapping that is already dirty does not
/// move it.
struct Walk<'a> {
    root: &'a Path,
    rewritten: fn(&Path) -> bool,
    /// Bytes left to read for digests.
    budget: u64,
    out: BTreeMap<PathBuf, Stamp>,
}

impl Walk<'_> {
    fn dir(&mut self, dir: &Path) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        // In order, so that the same files fit the budget each time.
        let mut paths: Vec<_> = read.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if self.out.len() >= MAX_TREE_ENTRIES {
                return;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let rel = path.strip_prefix(self.root).unwrap_or(&path).to_path_buf();
            if meta.is_dir() {
                let dir = Stamp {
                    mode: mode(&meta),
                    len: 0,
                    changed: 0,
                    digest: None,
                };
                self.out.insert(rel, dir);
                self.dir(&path);
            } else {
                let stamp = self.file(&rel, &path, &meta);
                self.out.insert(rel, stamp);
            }
        }
    }

    fn file(&mut self, rel: &Path, path: &Path, meta: &std::fs::Metadata) -> Stamp {
        let len = meta.len();
        let read =
            (self.rewritten)(rel) && meta.is_file() && len <= MAX_KEPT_BYTES && len <= self.budget;
        if !read {
            return Stamp::of(meta, None);
        }
        self.budget -= len;
        // Stamped from the file that was read, which is not the one looked
        // at above if it was swapped meanwhile.
        match read_regular(path, false) {
            Some((meta, bytes)) => Stamp::of(&meta, Some(Sha256::digest(&bytes).into())),
            None => Stamp::of(meta, None),
        }
    }
}

#[cfg(unix)]
fn mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn mode(meta: &std::fs::Metadata) -> u32 {
    u32::from(meta.permissions().readonly())
}

#[cfg(unix)]
fn changed_at(meta: &std::fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
}

#[cfg(not(unix))]
fn changed_at(meta: &std::fs::Metadata) -> i128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Write `bytes` under `keep` as `<unix seconds>-<file name>`.
fn save(keep: &Path, path: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    std::fs::create_dir_all(keep).ok()?;
    let target = keep.join(format!("{now}-{name}"));
    std::fs::write(&target, bytes).ok()?;
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn servers(v: &Value) -> Value {
        v.get("mcpServers").cloned().unwrap_or(Value::Null)
    }

    #[test]
    fn notices_changed_created_and_tree_edits_once() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let keep = tmp.path().join("keep");
        std::fs::create_dir_all(home.join(".vendor/skills/a")).unwrap();
        std::fs::write(home.join(".vendor/config.toml"), "a = 1\n").unwrap();
        std::fs::write(home.join(".vendor/skills/a/SKILL.md"), "x").unwrap();
        let guarded = [
            Guarded::File("~/.vendor/config.toml".into()),
            Guarded::File("~/.vendor/hooks.json".into()),
            Guarded::Tree("~/.vendor/skills".into()),
        ];
        let mut watch = Watch::begin(&guarded, Some(&home));
        assert!(watch.changes(&keep).is_empty());

        std::fs::write(home.join(".vendor/config.toml"), "a = 2\n").unwrap();
        std::fs::write(home.join(".vendor/hooks.json"), "{}").unwrap();
        std::fs::write(home.join(".vendor/skills/a/run.sh"), "rm -rf").unwrap();
        let changes = watch.changes(&keep);
        let paths: Vec<_> = changes.iter().map(|c| c.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                home.join(".vendor/config.toml"),
                home.join(".vendor/hooks.json"),
                home.join(".vendor/skills"),
            ]
        );
        // The old content of the edited file is kept; a new file and a tree
        // have none.
        let saved = changes[0].saved.as_ref().unwrap();
        assert_eq!(std::fs::read_to_string(saved).unwrap(), "a = 1\n");
        assert!(changes[1].saved.is_none() && changes[2].saved.is_none());
        assert!(changes[0].describe("Codex").contains("config.toml"));

        // Reported once.
        assert!(watch.changes(&keep).is_empty());
    }

    fn manifests(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "manifest.json")
    }

    fn every_file(_: &Path) -> bool {
        true
    }

    /// The change time of a file written after this has moved, also where
    /// it is kept coarsely.
    fn tick() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    /// Claude's sync of organisation skills rewrites its manifest with the
    /// same bytes on a timer (#114). Any other file rewritten so is still a
    /// change.
    #[test]
    fn bookkeeping_rewritten_with_the_same_content_is_not_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        let org = skills.join("synced/org");
        std::fs::create_dir_all(&org).unwrap();
        let manifest = org.join("manifest.json");
        std::fs::write(&manifest, r#"{"skills":[]}"#).unwrap();
        std::fs::write(org.join("SKILL.md"), "x").unwrap();
        let guarded = Guarded::TreeWithBookkeeping {
            path: skills.clone(),
            rewritten: manifests,
        };
        let mut watch = Watch::begin(&[guarded], None);

        // In place, and through a new file renamed over it.
        tick();
        std::fs::write(&manifest, r#"{"skills":[]}"#).unwrap();
        assert!(watch.changes(tmp.path()).is_empty());
        let fresh = org.join(".manifest.tmp");
        std::fs::write(&fresh, r#"{"skills":[]}"#).unwrap();
        std::fs::rename(&fresh, &manifest).unwrap();
        assert!(watch.changes(tmp.path()).is_empty());

        // Other bytes of the same length are a change, reported once.
        std::fs::write(&manifest, r#"{"skills":[1]"#).unwrap();
        let changes = watch.changes(tmp.path());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, skills);
        assert!(watch.changes(tmp.path()).is_empty());

        tick();
        std::fs::write(org.join("SKILL.md"), "x").unwrap();
        assert_eq!(watch.changes(tmp.path()).len(), 1);
    }

    /// Outside bookkeeping a file changed and changed back before the
    /// check is a change.
    #[test]
    fn a_change_undone_before_the_check_is_one() {
        let (tmp, hooks, mut watch) = hooks(false);
        tick();
        std::fs::write(hooks.join("h.sh"), "evil\n").unwrap();
        std::fs::write(hooks.join("h.sh"), "true\n").unwrap();
        assert_eq!(watch.changes(tmp.path()).len(), 1);
    }

    /// Bookkeeping too large to read is compared by its metadata.
    #[test]
    fn large_bookkeeping_rewritten_is_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("skills");
        std::fs::create_dir_all(&tree).unwrap();
        let big = tree.join("manifest.json");
        let bytes = vec![0u8; MAX_KEPT_BYTES as usize + 1];
        std::fs::write(&big, &bytes).unwrap();
        let guarded = Guarded::TreeWithBookkeeping {
            path: tree,
            rewritten: manifests,
        };
        let mut watch = Watch::begin(&[guarded], None);

        tick();
        std::fs::write(&big, &bytes).unwrap();
        assert_eq!(watch.changes(tmp.path()).len(), 1);
    }

    /// A tree with one file, `hooks/h.sh`, watched; `bookkept`, that file
    /// is bookkeeping.
    fn hooks(bookkept: bool) -> (tempfile::TempDir, PathBuf, Watch) {
        let tmp = tempfile::tempdir().unwrap();
        let hooks = tmp.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("h.sh"), "true\n").unwrap();
        let guarded = if bookkept {
            Guarded::TreeWithBookkeeping {
                path: hooks.clone(),
                rewritten: every_file,
            }
        } else {
            Guarded::Tree(hooks.clone())
        };
        let watch = Watch::begin(&[guarded], None);
        (tmp, hooks, watch)
    }

    #[cfg(unix)]
    #[test]
    fn a_script_made_executable_is_a_change() {
        use std::os::unix::fs::PermissionsExt;
        for bookkept in [false, true] {
            let (tmp, hooks, mut watch) = hooks(bookkept);
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(hooks.join("h.sh"), perms).unwrap();
            assert_eq!(watch.changes(tmp.path()).len(), 1, "{bookkept}");
        }
    }

    /// The same content behind a link, or a FIFO, is a change, and the FIFO
    /// is not waited on.
    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_link_or_a_fifo_is_a_change() {
        let (tmp, hooks, mut watch) = hooks(true);
        let file = hooks.join("h.sh");
        let elsewhere = tmp.path().join("h.sh");
        std::fs::write(&elsewhere, "true\n").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
        assert_eq!(watch.changes(tmp.path()).len(), 1);

        std::fs::remove_file(&file).unwrap();
        let c = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert_eq!(watch.changes(tmp.path()).len(), 1);
        // A watched file that is a FIFO does not hang the watch either.
        let mut fifo = Watch::begin(&[Guarded::File(file)], None);
        assert!(fifo.changes(tmp.path()).is_empty());
    }

    /// A write through a shared mapping that is already dirty does not move
    /// the change time; bookkeeping is read anyway.
    #[cfg(unix)]
    #[test]
    fn a_write_through_a_mapping_to_bookkeeping_is_a_change() {
        use std::os::fd::AsRawFd;
        let (tmp, hooks, mut watch) = hooks(true);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(hooks.join("h.sh"))
            .unwrap();
        let len = "true\n".len();
        // SAFETY: a fresh mapping of a file this test owns, unmapped below.
        let map = unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len)
        };
        map.copy_from_slice(b"true\n");
        assert!(watch.changes(tmp.path()).is_empty());
        map.copy_from_slice(b"evil\n");
        assert_eq!(watch.changes(tmp.path()).len(), 1);
        unsafe { libc::munmap(map.as_mut_ptr().cast(), len) };
    }

    /// Past the budget bookkeeping is compared by its change time, and the
    /// same files fit each time.
    #[test]
    fn bookkeeping_past_the_budget_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["b", "a", "c"] {
            std::fs::write(tmp.path().join(name), "1234").unwrap();
        }
        let mut walk = Walk {
            root: tmp.path(),
            rewritten: every_file,
            budget: 4,
            out: BTreeMap::new(),
        };
        walk.dir(tmp.path());
        let read: Vec<_> = walk.out.values().map(|s| s.digest.is_some()).collect();
        assert_eq!(read, [true, false, false]);
    }

    #[test]
    fn json_bookkeeping_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("state.json");
        let write = |v: Value| std::fs::write(&file, v.to_string()).unwrap();
        write(json!({"numStartups": 1, "mcpServers": {}}));
        let guarded = [Guarded::json(file.clone(), servers)];
        let mut watch = Watch::begin(&guarded, None);

        write(json!({"numStartups": 2, "mcpServers": {}}));
        assert!(watch.changes(tmp.path()).is_empty());

        write(json!({"numStartups": 2, "mcpServers": {"x": {"command": "sh"}}}));
        let changes = watch.changes(tmp.path());
        assert_eq!(changes.len(), 1);
        // The copy is the whole file from before the session.
        let saved = std::fs::read_to_string(changes[0].saved.as_ref().unwrap()).unwrap();
        assert!(saved.contains("\"numStartups\":1"));
    }
}
