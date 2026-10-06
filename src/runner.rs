//! Resolves harness/policy/model and dispatches to print, passthrough, or TUI.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, bail};
use colored::*;

use crate::cli::CommonRunArgs;
use crate::config::Config;
use crate::core::Rules;
use crate::core::conversations::ConversationStore;
use crate::core::guard::{self, Watch};
use crate::core::mcp;
use crate::core::registry::Registry;
use crate::core::sandbox::{self, Sandbox, SandboxEnv, SandboxLevel, SandboxRequest, SandboxSetup};
use crate::core::{
    HarnessId, McpServer, ModelRef, PermissionPolicy, PolicyResolution, PolicySupport, ProviderId,
    SessionConfig, resolve_policy,
};
use crate::harness::{Harness, PrintConfig, ProviderSource};
use crate::headless::{self, Headless, RunInfo};
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
    match explicit_policy(args)? {
        Some(p) => Ok(p),
        None => configured_policy(config, harness),
    }
}

/// The policy named on the command line (`--policy`, then `-y`), which
/// holds for every harness of the run.
pub fn explicit_policy(args: &CommonRunArgs) -> Result<Option<PermissionPolicy>> {
    if let Some(p) = &args.policy {
        return PermissionPolicy::parse(p).map(Some).ok_or_else(|| {
            anyhow::anyhow!("unknown policy '{p}' (ask, accept-edits, auto, bypass)")
        });
    }
    Ok(args.auto.then_some(PermissionPolicy::Bypass))
}

/// The configured policy for `harness`: its own table's, the global one,
/// Ask. A harness's own default is for that harness only.
pub fn configured_policy(config: &Config, harness: HarnessId) -> Result<PermissionPolicy> {
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

/// Every policy the config names must be one: the TUI reads them again
/// when the harness is switched, where an error could no longer end the run.
fn check_configured_policies(config: &Config) -> Result<()> {
    let named = config
        .harnesses
        .values()
        .filter_map(|h| h.default_policy.as_deref())
        .chain(config.default_policy.as_deref());
    for p in named {
        if PermissionPolicy::parse(p).is_none() {
            bail!("unknown policy '{p}' in config (ask, accept-edits, auto, bypass)");
        }
    }
    Ok(())
}

/// The policy of a run without the TUI. Nobody is there to choose another
/// one, so a policy the harness does not have, with nothing less permissive
/// in its place, ends the run.
fn print_policy(
    harness: &dyn Harness,
    policies: &[PolicySupport],
    requested: PermissionPolicy,
) -> Result<PolicyResolution> {
    resolve_policy(policies, requested).map_err(|e| {
        let supported: Vec<&str> = e.supported.iter().map(|p| p.as_str()).collect();
        anyhow::anyhow!(
            "policy '{}' is not available for {} without the TUI; pass --policy with one of: {}",
            e.requested,
            harness.descriptor().display_name,
            supported.join(", ")
        )
    })
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

/// What confines `harness` for a session in `workspace`.
pub fn session_sandbox(
    harness: &dyn Harness,
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
            default: harness.default_sandbox(),
            workspace,
            harness: &harness.sandbox_paths(),
            extra_writable: &extra_writable,
            extra_readable: &config.sandbox.readable,
            extra_deny_read: &config.sandbox.deny_read,
        },
        &setup.backend,
        &SandboxEnv::current(),
    )
}

/// The exit status is returned rather than exited with, so that the
/// runtime is dropped first and with it every child still running
/// (`kill_on_drop`).
pub async fn run(args: CommonRunArgs, config: &Config, cwd: &Path) -> Result<ExitCode> {
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

    check_configured_policies(config)?;
    let policy = requested_policy(&args, config, id)?;
    // The sandbox keeps unharness's config directory from being written,
    // but only knows of it once it exists; allow rules will be kept there.
    if let Some(dir) = Rules::default_dir() {
        let _ = std::fs::create_dir_all(dir);
    }
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

    let workspace = ws_root.as_deref().unwrap_or(cwd);
    let model_ref = model.clone().map(|m| {
        ModelRef::new(
            id,
            ProviderId::new(provider.clone().unwrap_or_else(|| "default".into())),
            m,
        )
    });

    if args.print && !args.native {
        let format = headless::Format::parse(args.format.as_deref())?;
        let prompt = match prompt {
            Some(p) => p,
            None => prompt_from_stdin()?,
        };
        let caps = harness.capabilities();
        if print_resume.is_some() && !caps.resume_by_id {
            bail!(
                "{} cannot resume a session by id",
                harness.descriptor().display_name
            );
        }
        let res = print_policy(harness, &caps.permission_policies, policy)?;
        let setup = RunSetup::new(harness, &sandbox_setup, config, workspace)?;
        let rules = Rules::load(ws_root.as_deref())?;
        let mut run = Headless::new(
            format,
            rules,
            cwd.to_path_buf(),
            harness.descriptor().short_name,
            caps.subagents.report_turn,
            std::io::stdout(),
            std::io::stderr(),
        );
        for w in res.warning.into_iter().chain(setup.warnings) {
            run.warn(w);
        }
        if let Some(note) = harness.prepare()? {
            run.warn(note);
        }
        let info = RunInfo {
            harness: id.as_str().to_string(),
            policy: res.effective.to_string(),
            sandbox: setup.sandbox.level().to_string(),
            cwd: cwd.to_path_buf(),
        };
        let mut watch = Watch::begin(&harness.guarded(workspace), dirs::home_dir().as_deref());
        let handle = harness.start_session(SessionConfig {
            binary,
            cwd: cwd.to_path_buf(),
            model: model_ref,
            effort,
            policy: res.effective,
            resume: print_resume,
            fork: false,
            extra_args: config.extra_args(id.as_str()).to_vec(),
            env: Vec::new(),
            mcp_servers: setup.mcp_servers,
            sandbox: setup.sandbox,
        })?;
        run.begin(&info);
        headless::drive(handle, prompt, &mut run).await;
        for change in watch.changes(&guard::default_keep_dir()) {
            run.warn(change.describe(harness.descriptor().display_name));
        }
        return Ok(ExitCode::from(run.finish()));
    }

    if args.print || args.no_tui {
        let res = print_policy(harness, &harness.print_policies(args.print), policy)?;
        let setup = RunSetup::new(harness, &sandbox_setup, config, workspace)?;
        for w in res.warning.into_iter().chain(setup.warnings) {
            eprintln!("{} {}", "[unharness]".yellow().bold(), w);
        }
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
            mcp_servers: setup.mcp_servers,
            sandbox: setup.sandbox,
        };
        if let Some(note) = harness.prepare()? {
            eprintln!("{} {}", "[unharness]".yellow().bold(), note);
        }
        let mut cmd = cfg.sandbox.wrap(harness.build_print_command(&cfg)?)?;
        if args.print {
            let mut watch = Watch::begin(&harness.guarded(workspace), dirs::home_dir().as_deref());
            let status = cmd.status()?;
            for change in watch.changes(&guard::default_keep_dir()) {
                eprintln!(
                    "{} {}",
                    "[unharness]".red().bold(),
                    change.describe(harness.descriptor().display_name)
                );
            }
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
            return Ok(ExitCode::SUCCESS);
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

    // Read before the terminal is taken over: a file that does not parse
    // ends the run with an error that can be read.
    let rules = Rules::load(ws_root.as_deref())?;
    tui::run_tui(TuiLaunch {
        rules,
        cwd: cwd.to_path_buf(),
        workspace_root: ws_root,
        registry,
        config: config.clone(),
        harness: id,
        policy: explicit_policy(&args)?,
        sandbox: sandbox_setup,
        provider,
        model,
        effort,
        resume,
        harness_explicit: args.harness.is_some(),
        initial_prompt: prompt,
    })
    .await?;
    Ok(ExitCode::SUCCESS)
}

/// What a run without the TUI is confined by and given, and what the user
/// should hear about it.
struct RunSetup {
    sandbox: Sandbox,
    mcp_servers: Vec<McpServer>,
    warnings: Vec<String>,
}

impl RunSetup {
    fn new(
        harness: &dyn Harness,
        sandbox_setup: &SandboxSetup,
        config: &Config,
        workspace: &Path,
    ) -> Result<Self> {
        let sandbox = session_sandbox(harness, sandbox_setup, config, workspace)?;
        let mut warnings = Vec::new();
        match sandbox.warning() {
            Some(w) => warnings.push(w),
            None => eprintln!("{} sandbox: {}", "[unharness]".dimmed(), sandbox.level()),
        }
        let (servers, problems) = config.mcp_servers();
        let (mcp_servers, mcp_warnings) = mcp::for_harness(
            &servers,
            harness.capabilities().mcp,
            harness.descriptor().short_name,
            &harness.own_mcp_servers(),
        );
        warnings.extend(problems.into_iter().chain(mcp_warnings));
        Ok(RunSetup {
            sandbox,
            mcp_servers,
            warnings,
        })
    }
}

/// The prompt of a `--print` run that named none: what is piped in.
fn prompt_from_stdin() -> Result<String> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("--print needs a prompt, as arguments or on stdin");
    }
    let mut prompt = String::new();
    stdin.read_to_string(&mut prompt)?;
    if prompt.trim().is_empty() {
        bail!("--print needs a prompt, as arguments or on stdin");
    }
    Ok(prompt)
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
    fn a_run_without_the_tui_does_not_pick_a_policy() {
        use crate::harness::codex::{CodexHarness, CodexTransport};
        // `--print --native` on Codex is `exec`, which has no `ask`.
        let codex = CodexHarness::new(CodexTransport::AppServer);
        for print_mode in [true, false] {
            let policies = codex.print_policies(print_mode);
            let e = print_policy(&codex, &policies, PermissionPolicy::Ask).unwrap_err();
            assert_eq!(
                e.to_string(),
                "policy 'ask' is not available for Codex (codex) without the TUI; pass --policy with one of: accept-edits, auto, bypass"
            );
        }
        let res = print_policy(&codex, &codex.print_policies(true), PermissionPolicy::Auto);
        assert_eq!(res.unwrap().effective, PermissionPolicy::Auto);
        // Headless `--print` is a session, which asks (and is answered no).
        let policies = codex.capabilities().permission_policies;
        let res = print_policy(&codex, &policies, PermissionPolicy::Ask).unwrap();
        assert_eq!(res.effective, PermissionPolicy::Ask);
        let claude = crate::harness::claude::ClaudeHarness::default();
        let res = print_policy(&claude, &claude.print_policies(true), PermissionPolicy::Ask);
        assert_eq!(res.unwrap().effective, PermissionPolicy::Ask);
    }

    #[test]
    fn a_harness_default_is_not_the_runs_policy() {
        let mut cfg = Config::default();
        cfg.harnesses.insert(
            "agy".into(),
            crate::config::HarnessSettings {
                default_policy: Some("bypass".into()),
                ..Default::default()
            },
        );
        let base = CommonRunArgs::default();
        assert_eq!(explicit_policy(&base).unwrap(), None);
        assert_eq!(
            configured_policy(&cfg, HarnessId::AGY).unwrap(),
            PermissionPolicy::Bypass
        );
        assert_eq!(
            configured_policy(&cfg, HarnessId::CLAUDE).unwrap(),
            PermissionPolicy::Ask
        );
        let y = CommonRunArgs {
            auto: true,
            ..Default::default()
        };
        assert_eq!(explicit_policy(&y).unwrap(), Some(PermissionPolicy::Bypass));

        assert!(check_configured_policies(&cfg).is_ok());
        cfg.harnesses.get_mut("agy").unwrap().default_policy = Some("nah".into());
        assert!(check_configured_policies(&cfg).is_err());
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
