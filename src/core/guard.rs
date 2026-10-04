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

/// Files larger than this are compared by metadata and not kept for restore.
const MAX_KEPT_BYTES: u64 = 1024 * 1024;
/// A tree larger than this is cut off; the rest goes unwatched.
const MAX_TREE_ENTRIES: usize = 50_000;

/// One thing to watch. Paths may start with `~`.
#[derive(Clone)]
pub enum Guarded {
    /// A file, by content.
    File(PathBuf),
    /// A file the CLI itself rewrites for bookkeeping: only what `project`
    /// makes of its content is compared. `None` (it could not be parsed)
    /// falls back to the whole content.
    Projected { path: PathBuf, project: Projection },
    /// A directory tree (skills, hooks, an installed binary), by the
    /// metadata of everything in it.
    Tree(PathBuf),
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
            Guarded::File(p) | Guarded::Tree(p) | Guarded::Projected { path: p, .. } => p,
        }
    }
}

impl std::fmt::Debug for Guarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Guarded::File(_) => "File",
            Guarded::Tree(_) => "Tree",
            Guarded::Projected { .. } => "Projected",
        };
        write!(f, "{kind}({})", self.path().display())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Fingerprint {
    Missing,
    Content(Vec<u8>),
    /// Too large to keep: (length, change time).
    Large(u64, i128),
    Projection(String),
    Tree(BTreeMap<PathBuf, (u64, i128)>),
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
            "{harness} configuration changed during this turn: {}",
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
                    Guarded::Tree(_) => None,
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
            if now == entry.fingerprint {
                continue;
            }
            entry.fingerprint = now;
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
    let meta = std::fs::metadata(path).ok()?;
    (meta.is_file() && meta.len() <= MAX_KEPT_BYTES)
        .then(|| std::fs::read(path).ok())
        .flatten()
}

fn fingerprint(guarded: &Guarded, path: &Path) -> Fingerprint {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Fingerprint::Missing;
    };
    match guarded {
        Guarded::Tree(_) => {
            let mut entries = BTreeMap::new();
            walk(path, path, &mut entries);
            Fingerprint::Tree(entries)
        }
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

/// Length and change time of everything under `dir`. The change time cannot
/// be set back by whoever wrote the file, unlike the modification time.
fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (u64, i128)>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if out.len() >= MAX_TREE_ENTRIES {
            return;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        if meta.is_dir() {
            out.insert(rel, (0, 0));
            walk(root, &path, out);
        } else {
            out.insert(rel, (meta.len(), changed_at(&meta)));
        }
    }
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
