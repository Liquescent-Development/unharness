use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use colored::*;

use crate::config::Config;
use crate::harness::{HarnessKind, get_adapter, which};
use crate::skills::{discover_skills_in_dir, global_skills_dir, workspace_skills_dir};
use crate::skills_cmd::SkillsCli;
use crate::sync::find_workspace_root;

pub fn run_doctor(cwd: &Path, config: &Config) -> Result<()> {
    println!("{}", "=== unharness doctor ===".bold().cyan());
    println!();

    // 1. Harnesses
    println!("{}", "AI Harnesses:".bold());
    for &kind in HarnessKind::default_priority() {
        let adapter = get_adapter(kind);
        let custom_bin = config.binary_override(kind.as_str());

        match adapter.resolve_binary(custom_bin) {
            Some(path) => {
                let version_str = adapter
                    .version(&path)
                    .unwrap_or_else(|| "unknown version".to_string());
                let auth = adapter.auth_status(&path);

                println!(
                    "  {} {} (v{}) at {}",
                    "[✓]".green().bold(),
                    adapter.display_name().bold(),
                    version_str,
                    path.display().to_string().dimmed()
                );

                let details = auth.details.unwrap_or_else(|| {
                    if auth.authenticated {
                        "logged in".to_string()
                    } else {
                        "not authenticated".to_string()
                    }
                });
                let details = if auth.authenticated {
                    details.green()
                } else {
                    details.yellow()
                };
                println!("      {} Auth: {}", "↳".dimmed(), details);
            }
            None => {
                println!(
                    "  {} {} {}",
                    "[-]".dimmed(),
                    adapter.display_name().dimmed(),
                    format!("(binary '{}' not found on PATH)", adapter.binary_name()).dimmed()
                );
            }
        }
    }

    if let Some(pi_bin) = which("pi") {
        println!(
            "  {} {} at {}",
            "[✓]".green().bold(),
            "Pi (pi)".bold(),
            pi_bin.display().to_string().dimmed()
        );
    }

    println!();

    match crate::harness::resolve_active_harness(
        None,
        config.default_harness.as_deref(),
        &HashMap::new(),
    ) {
        Ok((active_kind, active_bin)) => println!(
            "Active Default: {} ({})",
            active_kind.display_name().bold().green(),
            active_bin.display().to_string().dimmed()
        ),
        Err(e) => println!("Active Default: {}", format!("Error: {}", e).red()),
    }

    println!();

    // 2. Skills CLI
    println!("{}", "Skills CLI:".bold());
    let skills_cli = SkillsCli::detect();
    let marker = if skills_cli == SkillsCli::Missing {
        "[!]".yellow().bold()
    } else {
        "[✓]".green().bold()
    };
    println!("  {} {}", marker, skills_cli.describe());
    println!();

    // 3. Workspace Status
    println!("{}", "Workspace Context:".bold());
    match find_workspace_root(cwd) {
        Some(root) => {
            println!("  Root: {}", root.display());
            report_rules(&root);

            let ws_skills = discover_skills_in_dir(&workspace_skills_dir(&root));
            if ws_skills.is_empty() {
                println!(
                    "  {} Workspace Skills: none in .agents/skills",
                    "[-]".dimmed()
                );
            } else {
                println!(
                    "  {} Workspace Skills: {} in .agents/skills",
                    "[✓]".green().bold(),
                    ws_skills.len().to_string().bold()
                );
                for s in &ws_skills {
                    println!(
                        "      {} {} {}",
                        "•".dimmed(),
                        s.name,
                        s.description.as_deref().unwrap_or("").dimmed()
                    );
                }
            }
        }
        None => println!(
            "  Not inside a recognized workspace (no .git, .agents, AGENTS.md, or unharness.toml found)"
        ),
    }

    println!();

    // 4. Global Skills
    println!("{}", "Global Skills (~/.agents/skills):".bold());
    let global_skills = global_skills_dir()
        .map(|d| discover_skills_in_dir(&d))
        .unwrap_or_default();
    if global_skills.is_empty() {
        println!("  {} none", "[-]".dimmed());
    } else {
        println!(
            "  {} {} skills",
            "[✓]".green().bold(),
            global_skills.len().to_string().bold()
        );
        for s in &global_skills {
            println!(
                "      {} {} {}",
                "•".dimmed(),
                s.name,
                s.description.as_deref().unwrap_or("").dimmed()
            );
        }
    }

    println!();
    println!(
        "{}",
        "Run 'unharness \"prompt\"' to start, or 'unharness skills add <source>' to install skills.".green()
    );
    Ok(())
}

fn report_rules(root: &Path) {
    let agents_md = root.join("AGENTS.md");
    let claude_md = root.join("CLAUDE.md");
    let gemini_md = root.join("GEMINI.md");

    if !agents_md.exists() {
        if claude_md.exists() {
            println!(
                "  {} CLAUDE.md exists without AGENTS.md (run 'unharness sync' to promote it)",
                "[!]".yellow()
            );
        } else {
            println!(
                "  {} No AGENTS.md found (run 'unharness init' to create one)",
                "[-]".dimmed()
            );
        }
        return;
    }

    println!(
        "  {} AGENTS.md (canonical project instructions)",
        "[✓]".green().bold()
    );
    for (name, path, harness) in [
        ("CLAUDE.md", &claude_md, "Claude Code"),
        ("GEMINI.md", &gemini_md, "Antigravity"),
    ] {
        if path.is_symlink() {
            println!(
                "    {} {} -> AGENTS.md (symlinked for {})",
                "↳".dimmed(),
                name,
                harness
            );
        } else if path.exists() {
            println!(
                "    {} {} is a separate file (may diverge from AGENTS.md)",
                "[!]".yellow(),
                name
            );
        }
    }
}
