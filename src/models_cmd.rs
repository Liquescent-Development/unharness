//! `unharness models [--harness H] [--provider P]`: list providers and models
//! through the `Harness` trait.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use colored::*;

use crate::config::Config;
use crate::core::registry::Registry;
use crate::core::sandbox::SandboxSetup;
use crate::core::{HarnessId, Sandbox};
use crate::harness::resolve_binary;
use crate::sync::find_workspace_root;

pub fn list_models(config: &Config, harness: Option<&str>, provider: Option<&str>) -> Result<()> {
    let registry = Registry::from_config(config);
    let overrides: HashMap<HarnessId, std::path::PathBuf> = registry
        .ids()
        .into_iter()
        .filter_map(|id| {
            config
                .binary_override(id.as_str())
                .map(|p| (id, p.to_path_buf()))
        })
        .collect();

    let targets: Vec<&dyn crate::harness::Harness> = match harness {
        Some(h) => match registry.parse(h) {
            Some(hz) => vec![hz],
            None => bail!(
                "Unknown harness '{}'. Supported: {}",
                h,
                registry
                    .ids()
                    .iter()
                    .map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        None => registry.all().collect(),
    };
    // A harness started for its list runs as its session would.
    let level = match &config.sandbox.level {
        Some(level) => Some(
            crate::core::SandboxLevel::parse(level)
                .ok_or_else(|| anyhow::anyhow!("unknown sandbox level '{level}' in config"))?,
        ),
        None => None,
    };
    let setup = SandboxSetup::detect(level);
    let cwd = std::env::current_dir()?;
    let workspace = find_workspace_root(&cwd).unwrap_or(cwd);

    for hz in targets {
        let desc = hz.descriptor();
        let Some(binary) = resolve_binary(desc, overrides.get(&desc.id).map(|p| p.as_path()))
        else {
            println!(
                "{} {} (binary not found on PATH)",
                "[-]".dimmed(),
                desc.display_name.dimmed()
            );
            continue;
        };
        println!("{}", desc.display_name.bold().cyan());
        let sandbox = crate::runner::session_sandbox(hz, &setup, config, &workspace)?;
        print_models(hz, &binary, provider, &sandbox)?;
        println!();
    }
    Ok(())
}

fn print_models(
    hz: &dyn crate::harness::Harness,
    binary: &Path,
    provider: Option<&str>,
    sandbox: &Sandbox,
) -> Result<()> {
    let providers = hz.list_providers(binary, sandbox)?;
    let selected: Vec<_> = providers
        .into_iter()
        .filter(|(id, _)| provider.is_none_or(|p| p == id.as_str()))
        .collect();
    if selected.is_empty() {
        println!(
            "  {} no providers{}",
            "[-]".dimmed(),
            provider
                .map(|p| format!(" matching '{p}'"))
                .unwrap_or_default()
        );
        return Ok(());
    }
    for (pid, pname) in selected {
        println!("  {} ({})", pname.bold(), pid.as_str().dimmed());
        match hz.list_models(binary, &pid, sandbox) {
            Ok(models) if models.is_empty() => {
                println!("    {} no models reported", "[-]".dimmed())
            }
            Ok(models) => {
                for m in models {
                    let efforts = m
                        .effort_levels
                        .as_ref()
                        .map(|e| format!("  efforts: {}", e.join(",")))
                        .unwrap_or_default();
                    println!(
                        "    {} {}  {}{}",
                        "•".dimmed(),
                        m.model_ref.model.green(),
                        m.description.as_deref().unwrap_or(&m.display_name).dimmed(),
                        efforts.dimmed()
                    );
                }
            }
            Err(e) => println!("    {} {}", "[!]".yellow(), e),
        }
    }
    Ok(())
}
