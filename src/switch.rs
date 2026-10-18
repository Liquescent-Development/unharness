use anyhow::{Result, bail};
use colored::*;
use std::path::Path;

use crate::config::Config;
use crate::core::registry::Registry;
use crate::sync::find_workspace_root;

pub fn switch_default_harness(cwd: &Path, target: &str, global: bool) -> Result<()> {
    let ws_root = find_workspace_root(cwd);
    let registry = Registry::from_config(&Config::load_effective(ws_root.as_deref())?)?;
    let Some(harness) = registry.parse(target) else {
        bail!(
            "Unknown harness '{}'. Supported: {}",
            target,
            registry
                .ids()
                .iter()
                .map(|i| i.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let kind = harness.descriptor();

    if global {
        let mut cfg = Config::load_global()?;
        cfg.default_harness = Some(kind.id.as_str().to_string());
        let path = cfg.save_global()?;
        println!(
            "{} Set global default harness to {} in {}",
            "[✓]".green().bold(),
            kind.display_name.bold(),
            path.display()
        );
    } else {
        // Settings saved for a directory that is no workspace root would be
        // read by no run there (`Config::load_effective`).
        let Some(root) = ws_root else {
            bail!(
                "there is no workspace here to save the default harness for: {}; or run \
                 'unharness switch {target} --global'",
                crate::sync::no_workspace_reason(cwd)
            );
        };
        let store = Config::workspace_store()
            .ok_or_else(|| anyhow::anyhow!("Could not determine user config directory"))?;
        let mut cfg = Config::load_workspace_in(&store, &root)?.unwrap_or_default();
        cfg.default_harness = Some(kind.id.as_str().to_string());
        let path = cfg.save_workspace_in(&store, &root)?;
        println!(
            "{} Set workspace default harness to {} in {}",
            "[✓]".green().bold(),
            kind.display_name.bold(),
            path.display()
        );
    }

    Ok(())
}
