//! `unharness models [--harness H] [--provider P]`: list providers and models
//! through the v2 `Harness` trait.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use colored::*;

use crate::config::Config;
use crate::core::HarnessId;
use crate::core::registry::Registry;
use crate::harness::resolve_binary;

pub fn list_models(config: &Config, harness: Option<&str>, provider: Option<&str>) -> Result<()> {
    let registry = Registry::new();
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
                "Harness '{}' is not available through the v2 model listing yet (ported: {})",
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
        print_models(hz, &binary, provider)?;
        println!();
    }
    Ok(())
}

fn print_models(
    hz: &dyn crate::harness::Harness,
    binary: &Path,
    provider: Option<&str>,
) -> Result<()> {
    let providers = hz.list_providers(binary)?;
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
        match hz.list_models(binary, &pid) {
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
