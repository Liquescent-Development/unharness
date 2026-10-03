use std::collections::HashMap;
use std::path::Path;
use colored::*;
use anyhow::Result;

use crate::config::Config;
use crate::harness::{get_adapter, which, HarnessKind};
use crate::sync::{discover_skills_in_dir, find_workspace_root};

pub fn run_doctor(cwd: &Path, config: &Config) -> Result<()> {
    println!("{}", "=== unharness doctor ===".bold().cyan());
    println!();

    // 1. Harnesses
    println!("{}", "AI Harnesses:".bold());
    let mut available_kinds = Vec::new();

    let kinds = [HarnessKind::Agy, HarnessKind::Claude, HarnessKind::Codex];
    for &kind in &kinds {
        let adapter = get_adapter(kind);
        let custom_bin = match kind {
            HarnessKind::Agy => config.harnesses.agy.binary.as_deref(),
            HarnessKind::Claude => config.harnesses.claude.binary.as_deref(),
            HarnessKind::Codex => config.harnesses.codex.binary.as_deref(),
        };

        let binary_path = adapter.resolve_binary(custom_bin);
        match binary_path {
            Some(path) => {
                available_kinds.push(kind);
                let version_str = adapter.version(&path).unwrap_or_else(|| "unknown version".to_string());
                let auth = adapter.auth_status(&path);

                print!("  {} {} (v{})", "[✓]".green().bold(), adapter.display_name().bold(), version_str);
                println!(" at {}", path.display().to_string().dimmed());

                if auth.authenticated {
                    let details = auth.details.unwrap_or_else(|| "logged in".to_string());
                    println!("      {} Auth: {}", "↳".dimmed(), details.green());
                } else {
                    let details = auth.details.unwrap_or_else(|| "not authenticated".to_string());
                    println!("      {} Auth: {}", "↳".dimmed(), details.yellow());
                }
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

    // Check pi as bonus detection
    if let Some(pi_bin) = which("pi") {
        println!("  {} {} at {}", "[✓]".green().bold(), "Pi (pi)".bold(), pi_bin.display().to_string().dimmed());
    }

    println!();

    // Priority resolution
    let resolved_harness = crate::harness::resolve_active_harness(
        None,
        config.default_harness.as_deref(),
        &HashMap::new(),
    );
    match resolved_harness {
        Ok((active_kind, active_bin)) => {
            println!(
                "Active Default: {} ({})",
                active_kind.display_name().bold().green(),
                active_bin.display().to_string().dimmed()
            );
        }
        Err(e) => {
            println!("Active Default: {}", format!("Error: {}", e).red());
        }
    }

    println!();

    // 2. Workspace Status
    println!("{}", "Workspace Context:".bold());
    let ws_root = find_workspace_root(cwd);
    match ws_root {
        Some(root) => {
            println!("  Root: {}", root.display());

            // Rules check
            let agents_md = root.join("AGENTS.md");
            let claude_md = root.join("CLAUDE.md");
            let gemini_md = root.join("GEMINI.md");

            if agents_md.exists() {
                println!("  {} AGENTS.md (canonical project instructions)", "[✓]".green().bold());
                if claude_md.is_symlink() {
                    println!("    {} CLAUDE.md -> AGENTS.md (symlinked for Claude Code)", "↳".dimmed());
                } else if claude_md.exists() {
                    println!("    {} CLAUDE.md is a separate file (may diverge from AGENTS.md)", "[!]".yellow());
                }

                if gemini_md.is_symlink() {
                    println!("    {} GEMINI.md -> AGENTS.md (symlinked for Antigravity)", "↳".dimmed());
                } else if gemini_md.exists() {
                    println!("    {} GEMINI.md is a separate file (may diverge from AGENTS.md)", "[!]".yellow());
                }
            } else if claude_md.exists() {
                println!("  {} CLAUDE.md exists (run 'unharness init' or 'sync' to link AGENTS.md)", "[!]".yellow());
            } else {
                println!("  {} No AGENTS.md found (run 'unharness init' to create one)", "[-]".dimmed());
            }

            // Workspace Skills
            let ws_skills_dir = root.join(".agents").join("skills");
            let ws_skills = discover_skills_in_dir(&ws_skills_dir, false);
            if !ws_skills.is_empty() {
                println!(
                    "  {} Workspace Skills: {} loaded from .agents/skills",
                    "[✓]".green().bold(),
                    ws_skills.len().to_string().bold()
                );
                let claude_skills_dir = root.join(".claude").join("skills");
                let claude_skills = discover_skills_in_dir(&claude_skills_dir, false);
                println!(
                    "    {} Claude projection: {}/{} skills linked in .claude/skills",
                    "↳".dimmed(),
                    claude_skills.len(),
                    ws_skills.len()
                );
            } else {
                println!("  {} Workspace Skills: none in .agents/skills", "[-]".dimmed());
            }
        }
        None => {
            println!("  Not inside a recognized workspace (no .git, .agents, or unharness.toml found)");
        }
    }

    println!();

    // 3. Global Skills Status
    println!("{}", "Global Skills Context:".bold());
    if let Some(home) = dirs::home_dir() {
        let global_skills_dir = home.join(".agents").join("skills");
        let global_skills = discover_skills_in_dir(&global_skills_dir, true);
        if !global_skills.is_empty() {
            println!(
                "  {} Canonical: {} skills found in ~/.agents/skills",
                "[✓]".green().bold(),
                global_skills.len().to_string().bold()
            );

            let claude_global = home.join(".claude").join("skills");
            let claude_skills = discover_skills_in_dir(&claude_global, true);
            println!(
                "    {} ~/.claude/skills: {} skills linked",
                "↳".dimmed(),
                claude_skills.len()
            );

            let agy_agents = home.join(".gemini").join("antigravity-cli").join(".agents").join("skills");
            if agy_agents.exists() {
                let agy_skills = discover_skills_in_dir(&agy_agents, true);
                println!(
                    "    {} ~/.gemini/antigravity-cli/.agents/skills: {} skills linked",
                    "↳".dimmed(),
                    agy_skills.len()
                );
            }
        } else {
            println!("  {} No global skills in ~/.agents/skills", "[-]".dimmed());
        }
    }

    println!();
    println!("{}", "Everything is configured. Run 'unharness \"prompt\"' to start.".green());
    Ok(())
}
