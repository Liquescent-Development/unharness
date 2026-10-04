use std::fs;
use std::path::Path;

use anyhow::Result;
use colored::*;

use crate::config::Config;
use crate::sync::sync_workspace_rules;

pub const AGENTS_MD_STARTER: &str = r#"# AGENTS.md

Project guidelines and procedures for AI coding agents.

## Development Principles

- **Test-Driven:** Verify functionality with automated tests before completing tasks.
- **Vendor-Neutral:** Procedures and skills are defined once in `.agents/skills` and shared across harnesses.
- **Auditability:** Document non-obvious design decisions.

## Common Workflows

- Run tests: see project test runner
- Check lints: see project linter
"#;

pub fn init_workspace(cwd: &Path) -> Result<()> {
    let store = Config::workspace_store()
        .ok_or_else(|| anyhow::anyhow!("Could not determine user config directory"))?;
    init_workspace_in(cwd, &store)
}

/// Like [`init_workspace`], with the workspace's settings kept under `store`.
pub fn init_workspace_in(cwd: &Path, store: &Path) -> Result<()> {
    println!(
        "{}",
        "=== Initializing unharness in current repository ==="
            .bold()
            .cyan()
    );
    println!();

    // 1. Create .agents/skills (the canonical skills dir used by `npx skills`)
    let ws_skills_dir = cwd.join(".agents").join("skills");
    if !ws_skills_dir.exists() {
        fs::create_dir_all(&ws_skills_dir)?;
        println!(
            "{} Created directory {}",
            "[✓]".green().bold(),
            ".agents/skills".bold()
        );
    } else {
        println!(
            "{} Directory already exists: {}",
            "[✓]".green().bold(),
            ".agents/skills".dimmed()
        );
    }

    // 2. Create AGENTS.md if none exists
    let agents_md = cwd.join("AGENTS.md");
    let claude_md = cwd.join("CLAUDE.md");
    if !agents_md.exists() && !claude_md.exists() {
        fs::write(&agents_md, AGENTS_MD_STARTER)?;
        println!(
            "{} Created canonical instructions: {}",
            "[✓]".green().bold(),
            "AGENTS.md".bold()
        );
    }

    // 3. Workspace settings, kept outside the workspace so an agent working
    // in it cannot change them. An older in-tree unharness.toml is imported.
    let config_path = Config::workspace_path_in(store, cwd);
    if !config_path.exists() {
        match Config::legacy_workspace_file(cwd) {
            Some(legacy) => {
                Config::load_legacy(cwd)?.save_workspace_in(store, cwd)?;
                println!(
                    "{} Imported {} into {}",
                    "[✓]".green().bold(),
                    legacy.display(),
                    config_path.display().to_string().bold()
                );
                println!(
                    "    {} is no longer read and can be deleted",
                    legacy.display()
                );
            }
            None => {
                let cfg = Config {
                    default_harness: Some("agy".to_string()),
                    default_policy: Some("ask".to_string()),
                    auto_sync: true,
                    ..Default::default()
                };
                cfg.save_workspace_in(store, cwd)?;
                println!(
                    "{} Created workspace configuration: {}",
                    "[✓]".green().bold(),
                    config_path.display().to_string().bold()
                );
            }
        }
    }

    // 4. Ignore unharness session state
    ensure_gitignore_entry(cwd, ".unharness/")?;

    // 5. Link CLAUDE.md / GEMINI.md -> AGENTS.md
    let report = sync_workspace_rules(cwd)?;
    if report.claude_md_created {
        println!("{} Symlinked CLAUDE.md -> AGENTS.md", "[✓]".green().bold());
    }
    if report.gemini_md_created {
        println!("{} Symlinked GEMINI.md -> AGENTS.md", "[✓]".green().bold());
    }
    for w in &report.warnings {
        println!("{} {}", "[!]".yellow().bold(), w);
    }

    println!();
    println!("{}", "unharness initialized successfully!".green().bold());
    println!(
        "Next: 'unharness doctor', 'unharness skills add <owner/repo>', or 'unharness \"your task\"'"
    );

    Ok(())
}

/// Append `entry` to `.gitignore` in `dir` if it is not already present.
pub fn ensure_gitignore_entry(dir: &Path, entry: &str) -> Result<bool> {
    let path = dir.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == entry) {
        return Ok(false);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(entry);
    content.push('\n');
    fs::write(&path, content)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_gitignore_entry() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_gitignore_entry(dir.path(), ".unharness/").unwrap());
        assert!(!ensure_gitignore_entry(dir.path(), ".unharness/").unwrap());
        fs::write(dir.path().join(".gitignore"), "/target").unwrap();
        assert!(ensure_gitignore_entry(dir.path(), ".unharness/").unwrap());
        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content, "/target\n.unharness/\n");
    }

    #[test]
    fn test_init_workspace_creates_everything() {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        init_workspace_in(dir.path(), store.path()).unwrap();
        assert!(dir.path().join(".agents/skills").is_dir());
        assert!(dir.path().join("AGENTS.md").is_file());
        assert!(dir.path().join("CLAUDE.md").is_symlink());
        assert!(dir.path().join("GEMINI.md").is_symlink());
        // Settings go to the store, not into the workspace.
        assert!(!dir.path().join("unharness.toml").exists());
        assert!(Config::workspace_path_in(store.path(), dir.path()).is_file());
        let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.contains(".unharness/"));
        // Idempotent
        init_workspace_in(dir.path(), store.path()).unwrap();
    }

    #[test]
    fn init_imports_an_in_tree_config_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("unharness.toml"),
            "default_harness = \"codex\"\n",
        )
        .unwrap();
        init_workspace_in(dir.path(), store.path()).unwrap();
        let load = || Config::load_workspace_in(store.path(), dir.path()).unwrap();
        assert_eq!(load().default_harness.as_deref(), Some("codex"));

        // Later edits to the in-tree file change nothing.
        fs::write(
            dir.path().join("unharness.toml"),
            "default_harness = \"pi\"\n",
        )
        .unwrap();
        init_workspace_in(dir.path(), store.path()).unwrap();
        assert_eq!(load().default_harness.as_deref(), Some("codex"));
    }
}
