use anyhow::{Result, bail};
use colored::*;
use std::path::Path;

use crate::config::Config;
use crate::harness::HarnessKind;
use crate::sync::find_workspace_root;

pub fn switch_default_harness(cwd: &Path, target: &str, global: bool) -> Result<()> {
    let kind = match HarnessKind::parse_str(target) {
        Some(k) => k,
        None => bail!(
            "Unknown harness '{}'. Supported: agy, claude, codex",
            target
        ),
    };

    if global {
        let mut cfg = Config::load_global().unwrap_or_default();
        cfg.default_harness = Some(kind.as_str().to_string());
        let path = cfg.save_global()?;
        println!(
            "{} Set global default harness to {} in {}",
            "[✓]".green().bold(),
            kind.display_name().bold(),
            path.display()
        );
    } else {
        let root = find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
        let mut cfg = Config::load_from_dir(&root).unwrap_or_default();
        cfg.default_harness = Some(kind.as_str().to_string());
        let path = cfg.save_to_dir(&root)?;
        println!(
            "{} Set workspace default harness to {} in {}",
            "[✓]".green().bold(),
            kind.display_name().bold(),
            path.display()
        );
    }

    Ok(())
}
