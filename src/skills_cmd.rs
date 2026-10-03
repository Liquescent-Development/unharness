use std::fs;
use std::path::Path;
use colored::*;
use anyhow::{bail, Result};

use crate::sync::{discover_skills_in_dir, find_workspace_root, parse_skill_md, sync_global_skills, sync_workspace_skills};

pub fn list_skills(cwd: &Path) -> Result<()> {
    println!("{}", "=== Available Agent Skills ===".bold().cyan());
    println!();

    // 1. Workspace Skills
    let ws_root = find_workspace_root(cwd);
    if let Some(ref root) = ws_root {
        let ws_skills_dir = root.join(".agents").join("skills");
        let ws_skills = discover_skills_in_dir(&ws_skills_dir, false);
        println!("{}:", format!("Workspace Skills ({})", ws_skills_dir.display()).bold());
        if ws_skills.is_empty() {
            println!("  (none found in .agents/skills)");
        } else {
            for skill in ws_skills {
                let desc = skill.description.as_deref().unwrap_or("No description provided");
                println!("  • {} - {}", skill.name.green().bold(), desc);
            }
        }
        println!();
    }

    // 2. Global Skills
    if let Some(home) = dirs::home_dir() {
        let global_skills_dir = home.join(".agents").join("skills");
        let global_skills = discover_skills_in_dir(&global_skills_dir, true);
        println!("{}:", format!("Global Skills ({})", global_skills_dir.display()).bold());
        if global_skills.is_empty() {
            println!("  (none found in ~/.agents/skills)");
        } else {
            for skill in global_skills {
                let desc = skill.description.as_deref().unwrap_or("No description provided");
                println!("  • {} - {}", skill.name.cyan().bold(), desc);
            }
        }
    }

    println!();
    Ok(())
}

pub fn create_skill(cwd: &Path, name: &str, is_global: bool) -> Result<()> {
    let target_parent = if is_global {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Home directory not found"))?;
        home.join(".agents").join("skills")
    } else {
        let root = find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
        root.join(".agents").join("skills")
    };

    let skill_dir = target_parent.join(name);
    if skill_dir.exists() {
        bail!("Skill directory already exists: {}", skill_dir.display());
    }

    fs::create_dir_all(&skill_dir)?;
    let skill_md = skill_dir.join("SKILL.md");

    let template = format!(
        r#"---
name: {name}
description: Concise one-sentence summary of what this skill does and when to invoke it.
---

# {name}

Describe the procedure, checklist, and criteria here.

## When to Invoke

- Condition 1
- Condition 2

## Steps

1. Step 1
2. Step 2
"#
    );

    fs::write(&skill_md, template)?;
    println!(
        "{} Created skill {} at {}",
        "[✓]".green().bold(),
        name.bold(),
        skill_md.display()
    );

    // Auto-sync after creation
    if is_global {
        let _ = sync_global_skills();
    } else if let Some(root) = find_workspace_root(cwd) {
        let _ = sync_workspace_skills(&root);
    }

    Ok(())
}

pub fn validate_skills(cwd: &Path) -> Result<()> {
    println!("{}", "=== Validating Skills ===".bold().cyan());
    println!();

    let mut total_checked = 0;
    let mut total_issues = 0;

    let mut check_dir = |dir: &std::path::PathBuf, label: &str| {
        if !dir.is_dir() {
            return;
        }
        println!("{}:", label.bold());
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    total_checked += 1;
                    let dir_name = p.file_name().unwrap().to_string_lossy();
                    let skill_file = p.join("SKILL.md");
                    if !skill_file.exists() {
                        println!("  {} {} - missing SKILL.md file", "[✗]".red().bold(), dir_name);
                        total_issues += 1;
                        continue;
                    }

                    let (name, desc) = parse_skill_md(&skill_file);
                    match (name, desc) {
                        (Some(n), Some(d)) => {
                            if n != dir_name {
                                println!(
                                    "  {} {} - frontmatter name '{}' does not match directory name",
                                    "[!]".yellow().bold(),
                                    dir_name,
                                    n
                                );
                            } else {
                                println!("  {} {} - valid (desc: {} chars)", "[✓]".green().bold(), n, d.len());
                            }
                        }
                        (Some(n), None) => {
                            println!("  {} {} - missing 'description' in YAML frontmatter", "[!]".yellow().bold(), n);
                            total_issues += 1;
                        }
                        (None, Some(_)) => {
                            println!("  {} {} - missing 'name' in YAML frontmatter", "[!]".yellow().bold(), dir_name);
                            total_issues += 1;
                        }
                        (None, None) => {
                            println!("  {} {} - invalid or missing YAML frontmatter (needs --- ... ---)", "[✗]".red().bold(), dir_name);
                            total_issues += 1;
                        }
                    }
                }
            }
        }
        println!();
    };

    if let Some(root) = find_workspace_root(cwd) {
        check_dir(&root.join(".agents").join("skills"), "Workspace Skills");
    }

    if let Some(home) = dirs::home_dir() {
        check_dir(&home.join(".agents").join("skills"), "Global Skills");
    }

    if total_issues == 0 {
        println!("{}", format!("All {} skills passed validation!", total_checked).green().bold());
    } else {
        println!("{}", format!("Validation finished with {} issue(s) across {} skills.", total_issues, total_checked).yellow().bold());
    }

    Ok(())
}
