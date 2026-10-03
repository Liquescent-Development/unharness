mod cli;
mod config;
mod doctor;
mod harness;
mod init;
mod runner;
mod skills_cmd;
mod switch;
mod sync;
mod tui;

use std::env;
use anyhow::Result;
use clap::Parser;
use colored::*;

use cli::{Cli, Commands, CommonRunArgs, SkillsSubcommand};
use config::Config;
use harness::RunOptions;
use sync::find_workspace_root;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir()?;
    let ws_root = find_workspace_root(&cwd);
    let config = Config::load_effective(ws_root.as_deref());

    match cli.command {
        Some(Commands::Init) => {
            init::init_workspace(&cwd)?;
        }
        Some(Commands::Doctor) => {
            doctor::run_doctor(&cwd, &config)?;
        }
        Some(Commands::Sync { workspace, global }) => {
            handle_sync(&cwd, workspace, global)?;
        }
        Some(Commands::Skills { cmd }) => match cmd {
            SkillsSubcommand::List => {
                skills_cmd::list_skills(&cwd)?;
            }
            SkillsSubcommand::Create { name, global } => {
                skills_cmd::create_skill(&cwd, &name, global)?;
            }
            SkillsSubcommand::Validate => {
                skills_cmd::validate_skills(&cwd)?;
            }
            SkillsSubcommand::Sync => {
                handle_sync(&cwd, false, false)?;
            }
        },
        Some(Commands::Switch { harness, global }) => {
            switch::switch_default_harness(&cwd, &harness, global)?;
        }
        Some(Commands::Run(run_args)) => {
            execute_run(run_args, &config, &cwd).await?;
        }
        None => {
            execute_run(cli.run_args, &config, &cwd).await?;
        }
    }

    Ok(())
}

fn handle_sync(cwd: &std::path::Path, workspace_only: bool, global_only: bool) -> Result<()> {
    println!("{}", "=== Synchronizing Skills and Rules ===".bold().cyan());
    println!();

    let ws_root = find_workspace_root(cwd);
    let do_ws = !global_only;
    let do_global = !workspace_only;

    if do_ws {
        match ws_root {
            Some(ref root) => {
                println!("Workspace ({}):", root.display());
                let ws_skills = sync::sync_workspace_skills(root)?;
                println!(
                    "  {} Discovered {} workspace skill(s)",
                    "[✓]".green().bold(),
                    ws_skills.total_discovered
                );
                if ws_skills.symlinks_created > 0 {
                    println!(
                        "  {} Created {} projection symlink(s)",
                        "↳".dimmed(),
                        ws_skills.symlinks_created
                    );
                }
                if ws_skills.symlinks_pruned > 0 {
                    println!(
                        "  {} Pruned {} stale symlink(s)",
                        "↳".dimmed(),
                        ws_skills.symlinks_pruned
                    );
                }
                for w in &ws_skills.warnings {
                    println!("  {} {}", "[!]".yellow().bold(), w);
                }

                let rules = sync::sync_workspace_rules(root)?;
                if rules.claude_md_created {
                    println!("  {} Linked CLAUDE.md -> AGENTS.md", "[✓]".green().bold());
                }
                if rules.gemini_md_created {
                    println!("  {} Linked GEMINI.md -> AGENTS.md", "[✓]".green().bold());
                }
                for w in &rules.warnings {
                    println!("  {} {}", "[!]".yellow().bold(), w);
                }
            }
            None => {
                println!("No workspace detected (not inside a git or unharness repo)");
            }
        }
        println!();
    }

    if do_global {
        println!("Global (~/.agents/skills):");
        let global_skills = sync::sync_global_skills()?;
        println!(
            "  {} Discovered {} global skill(s)",
            "[✓]".green().bold(),
            global_skills.total_discovered
        );
        if global_skills.symlinks_created > 0 {
            println!(
                "  {} Created/updated {} global projection symlink(s)",
                "↳".dimmed(),
                global_skills.symlinks_created
            );
        }
        if global_skills.symlinks_pruned > 0 {
            println!(
                "  {} Pruned {} stale global symlink(s)",
                "↳".dimmed(),
                global_skills.symlinks_pruned
            );
        }
        for w in &global_skills.warnings {
            println!("  {} {}", "[!]".yellow().bold(), w);
        }
        println!();
    }

    println!("{}", "Synchronization complete!".green().bold());
    Ok(())
}

async fn execute_run(args: CommonRunArgs, config: &Config, cwd: &std::path::Path) -> Result<()> {
    let prompt = if !args.prompt.is_empty() {
        Some(args.prompt.join(" "))
    } else {
        None
    };

    let opts = RunOptions {
        prompt,
        print_mode: args.print,
        auto_approve: args.auto,
        model: args.model,
        effort: args.effort,
        format: args.format,
        cwd: Some(cwd.to_path_buf()),
        extra_args: Vec::new(),
    };

    runner::run_harness(args.harness, opts, config, args.no_sync, args.no_tui, cwd).await
}
