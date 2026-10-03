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

    // 3. Create default unharness.toml if none exists
    let config_path = cwd.join("unharness.toml");
    if !config_path.exists() {
        let cfg = Config {
            default_harness: Some("agy".to_string()),
            default_policy: Some("ask".to_string()),
            auto_sync: true,
            ..Default::default()
        };
        cfg.save_to_dir(cwd)?;
        println!(
            "{} Created configuration: {}",
            "[✓]".green().bold(),
            "unharness.toml".bold()
        );
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
        init_workspace(dir.path()).unwrap();
        assert!(dir.path().join(".agents/skills").is_dir());
        assert!(dir.path().join("AGENTS.md").is_file());
        assert!(dir.path().join("CLAUDE.md").is_symlink());
        assert!(dir.path().join("GEMINI.md").is_symlink());
        assert!(dir.path().join("unharness.toml").is_file());
        let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.contains(".unharness/"));
        // Idempotent
        init_workspace(dir.path()).unwrap();
    }
}
