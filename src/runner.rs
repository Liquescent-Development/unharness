use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use colored::*;

use crate::config::Config;
use crate::harness::{HarnessKind, RunOptions, get_adapter, resolve_active_harness};
use crate::sync::{find_workspace_root, sync_workspace_rules};
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

    // 1. Preflight rules sync
    if !no_sync
        && config.auto_sync
        && let Some(ref root) = ws_root
        && let Ok(report) = sync_workspace_rules(root)
    {
        for w in &report.warnings {
            eprintln!("{} {}", "[unharness]".yellow().bold(), w);
        }
    }

    // 2. Resolve target harness
    let mut overrides = HashMap::new();
    for kind in [HarnessKind::Agy, HarnessKind::Claude, HarnessKind::Codex] {
        if let Some(p) = config.binary_override(kind.as_str()) {
            overrides.insert(kind, p.to_path_buf());
        }
    }

    let (kind, binary) = resolve_active_harness(
        requested_harness.as_deref(),
        config.default_harness.as_deref(),
        &overrides,
    )?;

    // 3. Fill defaults from config
    let id = kind.as_str();
    if opts.model.is_none() {
        opts.model = config.default_model(id).map(str::to_string);
    }
    if opts.effort.is_none() {
        opts.effort = config.default_effort(id).map(str::to_string);
    }
    opts.extra_args
        .extend(config.extra_args(id).iter().cloned());

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
        let adapter = get_adapter(kind);
        let mut cmd = adapter.build_command(&binary, &opts)?;

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            bail!(
                "Failed to execute {} ({:?}): {}",
                adapter.display_name(),
                binary,
                err
            );
        }

        #[cfg(not(unix))]
        {
            let status = cmd.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
    } else {
        tui::run_tui(cwd, kind, opts.auto_approve, opts.prompt, config).await?;
    }

    Ok(())
}
