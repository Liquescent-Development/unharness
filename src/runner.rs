use std::collections::HashMap;
use std::path::Path;
use anyhow::{bail, Result};
use colored::*;

use crate::config::Config;
use crate::harness::{get_adapter, resolve_active_harness, HarnessKind, RunOptions};
use crate::sync::{find_workspace_root, run_full_sync};
use crate::tui;

pub async fn run_harness(
    requested_harness: Option<String>,
    mut opts: RunOptions,
    config: &Config,
    no_sync: bool,
    no_tui: bool,
    cwd: &Path,
) -> Result<()> {
    let ws_root = find_workspace_root(cwd);

    // 1. Preflight auto-sync
    if !no_sync && config.auto_sync {
        if let Ok(report) = run_full_sync(ws_root.as_deref(), true) {
            let total_created = report.workspace_skills.symlinks_created + report.global_skills.symlinks_created;
            if total_created > 0 {
                eprintln!(
                    "{} Synchronized {} new skill symlink(s)",
                    "[unharness]".cyan().bold(),
                    total_created
                );
            }
        }
    }

    // 2. Resolve target harness
    let mut overrides = HashMap::new();
    if let Some(ref p) = config.harnesses.agy.binary {
        overrides.insert(HarnessKind::Agy, p.clone());
    }
    if let Some(ref p) = config.harnesses.claude.binary {
        overrides.insert(HarnessKind::Claude, p.clone());
    }
    if let Some(ref p) = config.harnesses.codex.binary {
        overrides.insert(HarnessKind::Codex, p.clone());
    }

    let (kind, binary) = resolve_active_harness(
        requested_harness.as_deref(),
        config.default_harness.as_deref(),
        &overrides,
    )?;

    // 3. Fill defaults from config
    let harness_config = match kind {
        HarnessKind::Agy => &config.harnesses.agy,
        HarnessKind::Claude => &config.harnesses.claude,
        HarnessKind::Codex => &config.harnesses.codex,
    };

    if opts.model.is_none() {
        opts.model = harness_config.default_model.clone();
    }
    if opts.effort.is_none() {
        opts.effort = harness_config.default_effort.clone();
    }
    for extra in &harness_config.extra_args {
        opts.extra_args.push(extra.clone());
    }

    if opts.cwd.is_none() {
        opts.cwd = Some(cwd.to_path_buf());
    }

    // 4. Branch: Print mode vs TUI vs Raw CLI
    if opts.print_mode {
        let adapter = get_adapter(kind);
        let mut cmd = adapter.build_command(&binary, &opts)?;
        let status = cmd.status()?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    } else if no_tui {
        // Direct execution of underlying harness CLI
        let adapter = get_adapter(kind);
        let mut cmd = adapter.build_command(&binary, &opts)?;

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            bail!("Failed to execute {} ({:?}): {}", adapter.display_name(), binary, err);
        }

        #[cfg(not(unix))]
        {
            let status = cmd.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
    } else {
        // Standard unharness unified TUI
        tui::run_tui(cwd, kind, opts.auto_approve, opts.prompt, config).await?;
    }

    Ok(())
}
