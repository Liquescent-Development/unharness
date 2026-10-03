use std::fs;
use std::path::Path;
use colored::*;
use anyhow::Result;

use crate::config::Config;
use crate::sync::run_full_sync;

pub fn init_workspace(cwd: &Path) -> Result<()> {
    println!("{}", "=== Initializing unharness in current repository ===".bold().cyan());
    println!();

    // 1. Create .agents/skills
    let ws_skills_dir = cwd.join(".agents").join("skills");
    if !ws_skills_dir.exists() {
        fs::create_dir_all(&ws_skills_dir)?;
        println!("{} Created directory {}", "[✓]".green().bold(), ".agents/skills".bold());
    } else {
        println!("{} Directory already exists: {}", "[✓]".green().bold(), ".agents/skills".dimmed());
    }

    // 2. Create AGENTS.md if none exists
    let agents_md = cwd.join("AGENTS.md");
    let claude_md = cwd.join("CLAUDE.md");
    if !agents_md.exists() && !claude_md.exists() {
        let starter_content = r#"# AGENTS.md

Project guidelines and procedures for AI coding agents.

## Development Principles

- **Test-Driven:** Verify functionality with automated tests before completing tasks.
- **Vendor-Neutral:** Procedures and skills are defined once in `.agents/skills` and shared across harnesses.
- **Auditability:** Document non-obvious design decisions.

## Common Workflows

- Run tests: see project test runner
- Check lints: see project linter
"#;
        fs::write(&agents_md, starter_content)?;
        println!("{} Created canonical instructions: {}", "[✓]".green().bold(), "AGENTS.md".bold());
    }

    // 3. Create default unharness.toml if none exists
    let config_path = cwd.join("unharness.toml");
    if !config_path.exists() {
        let mut cfg = Config::default();
        cfg.default_harness = Some("agy".to_string());
        cfg.auto_sync = true;
        cfg.save_to_dir(cwd)?;
        println!("{} Created configuration: {}", "[✓]".green().bold(), "unharness.toml".bold());
    }

    // 4. Run full sync to create symlinks (CLAUDE.md, GEMINI.md, skills projections)
    let report = run_full_sync(Some(cwd), true)?;
    if report.rules.claude_md_created {
        println!("{} Symlinked CLAUDE.md -> AGENTS.md", "[✓]".green().bold());
    }
    if report.rules.gemini_md_created {
        println!("{} Symlinked GEMINI.md -> AGENTS.md", "[✓]".green().bold());
    }

    println!();
    println!("{}", "unharness initialized successfully!".green().bold());
    println!("You can now run: 'unharness doctor' or 'unharness \"your task\"'");

    Ok(())
}
