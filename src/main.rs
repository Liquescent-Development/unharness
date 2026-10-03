use std::env;

use anyhow::Result;
use clap::Parser;
use colored::*;

use unharness::cli::{Cli, Commands, CommonRunArgs};
use unharness::config::Config;
use unharness::harness::RunOptions;
use unharness::sync::find_workspace_root;
use unharness::{doctor, init, models_cmd, runner, skills_cmd, switch, sync};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir()?;
    let ws_root = find_workspace_root(&cwd);
    let config = Config::load_effective(ws_root.as_deref());

    match cli.command {
        Some(Commands::Init) => init::init_workspace(&cwd)?,
        Some(Commands::Doctor) => doctor::run_doctor(&cwd, &config)?,
        Some(Commands::Sync) => handle_sync(&cwd)?,
        Some(Commands::Skills { args }) => skills_cmd::run_skills(&cwd, &args)?,
        Some(Commands::Models { harness, provider }) => {
            models_cmd::list_models(&config, harness.as_deref(), provider.as_deref())?
        }
        Some(Commands::Switch { harness, global }) => {
            switch::switch_default_harness(&cwd, &harness, global)?
        }
        Some(Commands::Run(run_args)) => execute_run(run_args, &config, &cwd).await?,
        None => execute_run(cli.run_args, &config, &cwd).await?,
    }

    Ok(())
}

fn handle_sync(cwd: &std::path::Path) -> Result<()> {
    println!("{}", "=== Synchronizing Rules ===".bold().cyan());
    println!();

    match find_workspace_root(cwd) {
        Some(root) => {
            println!("Workspace ({}):", root.display());
            let rules = sync::sync_workspace_rules(&root)?;
            if rules.agents_md_path.is_none() {
                println!(
                    "  {} No AGENTS.md found (run 'unharness init' to create one)",
                    "[-]".dimmed()
                );
            }
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
        None => println!("No workspace detected (not inside a git or unharness repo)"),
    }

    println!();
    println!(
        "Skills are managed by the `skills` CLI: run 'unharness skills list' or 'unharness skills add <source>'."
    );
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
