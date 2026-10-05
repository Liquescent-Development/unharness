use std::env;

use anyhow::Result;
use clap::Parser;
use colored::*;

use unharness::cli::{Cli, Commands};
use unharness::config::Config;
use unharness::core::conversations::ConversationStore;
use unharness::sync::find_workspace_root;
use unharness::{doctor, init, models_cmd, runner, skills_cmd, skills_import, switch, sync};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir()?;
    let ws_root = find_workspace_root(&cwd);
    let config = Config::load_effective(ws_root.as_deref())?;
    if let Some(warning) = ws_root.as_deref().and_then(Config::legacy_warning) {
        eprintln!("{} {}", "[unharness]".yellow().bold(), warning);
    }

    match cli.command {
        Some(Commands::Init) => init::init_workspace(&cwd)?,
        Some(Commands::Doctor) => doctor::run_doctor(&cwd, &config)?,
        Some(Commands::Sync) => handle_sync(&cwd)?,
        Some(Commands::Skills { args }) => {
            if args.first().map(String::as_str) == Some("import") {
                skills_import::run_import(&cwd, ws_root.as_deref(), &args[1..])?
            } else {
                skills_cmd::run_skills(&cwd, &args)?
            }
        }
        Some(Commands::Models { harness, provider }) => {
            models_cmd::list_models(&config, harness.as_deref(), provider.as_deref())?
        }
        Some(Commands::Sessions { clear }) => handle_sessions(ws_root.as_deref(), &cwd, clear)?,
        Some(Commands::Switch { harness, global }) => {
            switch::switch_default_harness(&cwd, &harness, global)?
        }
        Some(Commands::Run(run_args)) => runner::run(run_args, &config, &cwd).await?,
        None => runner::run(cli.run_args, &config, &cwd).await?,
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

fn handle_sessions(
    ws_root: Option<&std::path::Path>,
    cwd: &std::path::Path,
    clear: bool,
) -> Result<()> {
    let store = ConversationStore::open(ws_root, cwd);
    if clear {
        store.clear()?;
        println!("Cleared saved conversations in {}", store.root().display());
        // Their file checkpoints go with them.
        if let Some(cp) = ws_root.and_then(unharness::core::checkpoints::Checkpoints::open)
            && cp.destroy()?
        {
            println!("Removed file checkpoints in {}", cp.shadow_dir().display());
        }
        return Ok(());
    }
    let rows = store.list();
    println!(
        "Conversations saved in {}",
        store.root().display().to_string().dimmed()
    );
    if rows.is_empty() {
        println!("  none");
        return Ok(());
    }
    for (i, r) in rows.iter().enumerate() {
        let harnesses = r
            .harnesses
            .iter()
            .map(|h| h.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {}  {}  [{}]  {}{}",
            r.id[..8].bold(),
            r.updated_at.dimmed(),
            harnesses.cyan(),
            r.title,
            if i == 0 {
                " (latest)".green().to_string()
            } else {
                String::new()
            }
        );
    }
    println!("Resume with 'unharness --resume [id]' or /resume inside the TUI.");
    Ok(())
}
