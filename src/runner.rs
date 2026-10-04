//! Resolves harness/policy/model and dispatches to print, passthrough, or TUI.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use colored::*;

use crate::cli::CommonRunArgs;
use crate::config::Config;
use crate::core::conversations::ConversationStore;
use crate::core::registry::Registry;
use crate::core::sandbox::{self, Sandbox, SandboxEnv, SandboxLevel, SandboxRequest, SandboxSetup};
use crate::core::{HarnessId, ModelRef, PermissionPolicy, ProviderId, resolve_policy};
use crate::harness::{Harness, PrintConfig, ProviderSource};
use crate::sync::{find_workspace_root, sync_workspace_rules};
use crate::tui::{self, TuiLaunch};

/// Binary overrides from config for every registered harness.
pub fn binary_overrides(
    registry: &Registry,
    config: &Config,
) -> HashMap<HarnessId, std::path::PathBuf> {
    registry
        .ids()
        .into_iter()
        .filter_map(|id| {
            config
                .binary_override(id.as_str())
                .map(|p| (id, p.to_path_buf()))
        })
        .collect()
}

/// The policy to run with: CLI flag, `-y`, harness config, global config, Ask.
pub fn requested_policy(
    args: &CommonRunArgs,
    config: &Config,
    harness: HarnessId,
) -> Result<PermissionPolicy> {
    if let Some(p) = &args.policy {
        return PermissionPolicy::parse(p).ok_or_else(|| {
            anyhow::anyhow!("unknown policy '{p}' (ask, accept-edits, auto, bypass)")
        });
    }
    if args.auto {
        return Ok(PermissionPolicy::Bypass);
    }
    let configured = config
        .harness(harness.as_str())
        .and_then(|h| h.default_policy.as_deref())
        .or(config.default_policy.as_deref());
    match configured {
        Some(p) => PermissionPolicy::parse(p)
            .ok_or_else(|| anyhow::anyhow!("unknown policy '{p}' in config")),
        None => Ok(PermissionPolicy::Ask),
    }
}

/// The sandbox level the user set: CLI flag, then `[sandbox].level`.
pub fn requested_sandbox(args: &CommonRunArgs, config: &Config) -> Result<Option<SandboxLevel>> {
    let (value, origin) = match (&args.sandbox, &config.sandbox.level) {
        (Some(s), _) => (s, ""),
        (None, Some(s)) => (s, " in config"),
        (None, None) => return Ok(None),
    };
    SandboxLevel::parse(value).map(Some).ok_or_else(|| {
        anyhow::anyhow!("unknown sandbox level '{value}'{origin} (read-only, workspace-write, off)")
    })
}

/// What confines `harness` for a session under `policy` in `workspace`.
pub fn session_sandbox(
    harness: &dyn Harness,
    policy: PermissionPolicy,
    setup: &SandboxSetup,
    config: &Config,
    workspace: &Path,
) -> Result<Sandbox> {
    let mut extra_writable = config.sandbox.writable.clone();
    if let Some(settings) = config.harness(harness.descriptor().id.as_str()) {
        extra_writable.extend(settings.sandbox_writable.iter().cloned());
    }
    sandbox::resolve(
        &SandboxRequest {
            explicit: setup.explicit,
            default: harness.default_sandbox(policy),
            workspace,
            harness: &harness.sandbox_paths(),
            extra_writable: &extra_writable,
            extra_readable: &config.sandbox.readable,
        },
        &setup.backend,
        &SandboxEnv::current(),
    )
}

pub async fn run(args: CommonRunArgs, config: &Config, cwd: &Path) -> Result<()> {
    let ws_root = find_workspace_root(cwd);

    if !args.no_sync
        && config.auto_sync
        && let Some(root) = &ws_root
        && let Ok(report) = sync_workspace_rules(root)
    {
        for w in &report.warnings {
            eprintln!("{} {}", "[unharness]".yellow().bold(), w);
        }
    }

    let registry = Arc::new(Registry::from_config(config));
    let overrides = binary_overrides(&registry, config);
    let (harness, binary) = registry.resolve(
        args.harness.as_deref(),
        config.default_harness.as_deref(),
        &overrides,
    )?;
    let id = harness.descriptor().id;
    let settings = config.harness(id.as_str());

    let policy = requested_policy(&args, config, id)?;
    let sandbox_setup = SandboxSetup::detect(requested_sandbox(&args, config)?);
    if let (Some(level), Err(why)) = (sandbox_setup.explicit, &sandbox_setup.backend)
        && level != SandboxLevel::Off
    {
        bail!("sandbox '{level}' was requested but is unavailable: {why}");
    }
    let provider = args
        .provider
        .clone()
        .or_else(|| settings.and_then(|s| s.default_provider.clone()))
        .or_else(|| match harness.descriptor().providers {
            ProviderSource::Static(list) => list.first().map(|(p, _)| p.to_string()),
            ProviderSource::Dynamic => None,
        });
    let model = args
        .model
        .clone()
        .or_else(|| settings.and_then(|s| s.default_model.clone()));
    let effort = args
        .effort
        .clone()
        .or_else(|| settings.and_then(|s| s.default_effort.clone()));
    let prompt = if args.prompt.is_empty() {
        None
    } else {
        Some(args.prompt.join(" "))
    };

    let store = ConversationStore::open(ws_root.as_deref(), cwd);
    let (resume, print_resume) = resolve_resume(&store, args.resume.as_deref(), id)?;

    if args.print || args.no_tui {
        let res = resolve_policy(&harness.capabilities(), policy);
        if let Some(w) = res.warning {
            eprintln!("{} {}", "[unharness]".yellow().bold(), w);
        }
        let sandbox = session_sandbox(
            harness,
            res.effective,
            &sandbox_setup,
            config,
            ws_root.as_deref().unwrap_or(cwd),
        )?;
        match sandbox.warning() {
            Some(w) => eprintln!("{} {}", "[unharness]".yellow().bold(), w),
            None => eprintln!("{} sandbox: {}", "[unharness]".dimmed(), sandbox.level()),
        }
        let model_ref = model.map(|m| {
            ModelRef::new(
                id,
                ProviderId::new(provider.clone().unwrap_or_else(|| "default".into())),
                m,
            )
        });
        let cfg = PrintConfig {
            binary,
            cwd: cwd.to_path_buf(),
            prompt,
            print_mode: args.print,
            model: model_ref,
            effort,
            policy: Some(res.effective),
            format: args.format.clone(),
            resume: print_resume,
            extra_args: config.extra_args(id.as_str()).to_vec(),
            sandbox,
        };
        if let Some(note) = harness.prepare()? {
            eprintln!("{} {}", "[unharness]".yellow().bold(), note);
        }
        let mut cmd = cfg.sandbox.wrap(harness.build_print_command(&cfg)?)?;
        if args.print {
            let status = cmd.status()?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            bail!(
                "failed to execute {}: {}",
                harness.descriptor().display_name,
                err
            );
        }
        #[cfg(not(unix))]
        {
            let status = cmd.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    tui::run_tui(TuiLaunch {
        cwd: cwd.to_path_buf(),
        workspace_root: ws_root,
        registry,
        config: config.clone(),
        harness: id,
        policy,
        sandbox: sandbox_setup,
        provider,
        model,
        effort,
        resume,
        harness_explicit: args.harness.is_some(),
        initial_prompt: prompt,
    })
    .await
}

/// `--resume [id]` → (conversation id for the TUI, vendor session id for
/// print mode). An empty id means the most recent conversation. An id that
/// matches no conversation is passed to print mode as a raw vendor id.
pub fn resolve_resume(
    store: &ConversationStore,
    arg: Option<&str>,
    harness: HarnessId,
) -> Result<(Option<String>, Option<String>)> {
    let Some(r) = arg else {
        return Ok((None, None));
    };
    let row = if r.is_empty() {
        match store.last() {
            Some(row) => row,
            None => bail!("no saved conversation to resume in this workspace"),
        }
    } else {
        match store.resolve(r) {
            Ok(row) => row,
            Err(_) => return Ok((Some(r.to_string()), Some(r.to_string()))),
        }
    };
    let vendor = store
        .load(&row.id)
        .ok()
        .and_then(|c| c.sessions.get(&harness).cloned());
    Ok((Some(row.id), vendor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(Some(dir.path()), dir.path());
        assert_eq!(
            resolve_resume(&store, None, HarnessId::CLAUDE).unwrap(),
            (None, None)
        );
        assert!(resolve_resume(&store, Some(""), HarnessId::CLAUDE).is_err());
        assert_eq!(
            resolve_resume(&store, Some("raw-vendor"), HarnessId::CLAUDE).unwrap(),
            (Some("raw-vendor".into()), Some("raw-vendor".into()))
        );
        let mut c = crate::core::conversations::Conversation::new(HarnessId::CLAUDE);
        c.sessions.insert(HarnessId::CLAUDE, "claude-9".into());
        store.save(&c).unwrap();
        let (conv, vendor) = resolve_resume(&store, Some(""), HarnessId::CLAUDE).unwrap();
        assert_eq!(conv.as_deref(), Some(c.id.as_str()));
        assert_eq!(vendor.as_deref(), Some("claude-9"));
        let (_, vendor) = resolve_resume(&store, Some(&c.id[..8]), HarnessId::CODEX).unwrap();
        assert!(vendor.is_none());
    }

    #[test]
    fn policy_precedence() {
        let mut cfg = Config {
            default_policy: Some("auto".into()),
            ..Default::default()
        };
        let base = CommonRunArgs::default();
        assert_eq!(
            requested_policy(&base, &cfg, HarnessId::CLAUDE).unwrap(),
            PermissionPolicy::Auto
        );

        cfg.harnesses.insert(
            "claude".into(),
            crate::config::HarnessSettings {
                default_policy: Some("accept-edits".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            requested_policy(&base, &cfg, HarnessId::CLAUDE).unwrap(),
            PermissionPolicy::AcceptEdits
        );
        assert_eq!(
            requested_policy(&base, &cfg, HarnessId::AGY).unwrap(),
            PermissionPolicy::Auto
        );

        let y = CommonRunArgs {
            auto: true,
            ..Default::default()
        };
        assert_eq!(
            requested_policy(&y, &cfg, HarnessId::CLAUDE).unwrap(),
            PermissionPolicy::Bypass
        );

        let flag = CommonRunArgs {
            policy: Some("ask".into()),
            auto: true,
            ..Default::default()
        };
        assert_eq!(
            requested_policy(&flag, &cfg, HarnessId::CLAUDE).unwrap(),
            PermissionPolicy::Ask
        );

        let bad = CommonRunArgs {
            policy: Some("nah".into()),
            ..Default::default()
        };
        assert!(requested_policy(&bad, &cfg, HarnessId::CLAUDE).is_err());

        assert_eq!(
            requested_policy(&base, &Config::default(), HarnessId::CLAUDE).unwrap(),
            PermissionPolicy::Ask
        );
    }
}
