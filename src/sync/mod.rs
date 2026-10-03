//! Workspace rules synchronization.
//!
//! Only the canonical instructions file is managed here: `AGENTS.md` is the
//! source of truth and `CLAUDE.md` / `GEMINI.md` are symlinks to it. Skills are
//! installed and projected by the external `skills` CLI.

pub mod rules;

use std::path::{Path, PathBuf};

pub use rules::sync_workspace_rules;

pub fn find_workspace_root(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists()
            || current.join(".agents").exists()
            || current.join("unharness.toml").exists()
            || current.join(".unharness.toml").exists()
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
