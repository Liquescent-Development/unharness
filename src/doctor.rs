use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use colored::*;

use crate::config::Config;
use crate::core::Rules;
use crate::core::conversations::ConversationStore;
use crate::core::mcp;
use crate::core::registry::Registry;
use crate::core::rules::Scope;
use crate::core::sandbox::{
    self, Sandbox, SandboxEnv, SandboxLevel, SandboxPaths, SandboxRequest, SandboxSetup,
};
use crate::core::{McpChannel, McpSupport, PermissionPolicy};
use crate::runner::binary_overrides;
use crate::skills::{discover_skills_in_dir, global_skills_dir, workspace_skills_dir};
use crate::skills_cmd::SkillsCli;
use crate::skills_import::installed_plugins;
use crate::sync::find_workspace_root;

pub fn run_doctor(cwd: &Path, config: &Config) -> Result<()> {
    println!("{}", "=== unharness doctor ===".bold().cyan());
    println!();
    report_install();

    let registry = Registry::from_config(config);
    let overrides = binary_overrides(&registry, config);
    // An unknown level in the config is reported by `run`; here it reads as unset.
    let sandbox = SandboxSetup::detect(
        config
            .sandbox
            .level
            .as_deref()
            .and_then(SandboxLevel::parse),
    );

    // 1. Harnesses
    println!("{}", "AI Harnesses:".bold());
    // What each installed harness does with MCP servers, for section 1b.
    let mut mcp_support = Vec::new();
    for (h, probe) in registry.probe_all(&overrides) {
        let d = h.descriptor();
        if probe.binary.is_some() {
            mcp_support.push((d.short_name, h.capabilities().mcp, h.own_mcp_servers()));
        }
        match probe.binary {
            Some(path) => {
                println!(
                    "  {} {} (v{}) at {}",
                    "[✓]".green().bold(),
                    d.display_name.bold(),
                    probe.version.as_deref().unwrap_or("unknown"),
                    path.display().to_string().dimmed()
                );
                let details = probe.auth.details.unwrap_or_else(|| {
                    if probe.auth.authenticated {
                        "logged in".into()
                    } else {
                        "not authenticated".into()
                    }
                });
                let details = if probe.auth.authenticated {
                    details.green()
                } else {
                    details.yellow()
                };
                println!("      {} Auth: {}", "↳".dimmed(), details);

                let caps = h.capabilities();
                let policies: Vec<String> = PermissionPolicy::ALL
                    .iter()
                    .map(|p| match caps.supports_policy(*p) {
                        Some(s) if s.degraded.is_some() => format!("{p}*"),
                        Some(_) => p.to_string(),
                        None => format!("{}", p.to_string().dimmed()),
                    })
                    .collect();
                println!(
                    "      {} Permissions: {} · policies: {}{}",
                    "↳".dimmed(),
                    if caps.interactive_permissions {
                        "interactive".green()
                    } else {
                        "not interactive".yellow()
                    },
                    policies.join(" "),
                    if caps.resume_by_id { " · resume" } else { "" }
                );

                println!(
                    "      {} MCP servers: {} · plan mode: {}",
                    "↳".dimmed(),
                    caps.mcp.describe(),
                    if caps.plan_mode { "yes" } else { "no" }
                );

                let (level, _) = sandbox.level(h.default_sandbox());
                let mut own: Vec<String> = h
                    .sandbox_paths()
                    .writable
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                if let Some(settings) = config.harness(d.id.as_str()) {
                    own.extend(
                        settings
                            .sandbox_writable
                            .iter()
                            .map(|p| p.display().to_string()),
                    );
                }
                println!(
                    "      {} Sandbox: {}{}",
                    "↳".dimmed(),
                    match level {
                        SandboxLevel::Off => level.to_string().yellow(),
                        _ => level.to_string().green(),
                    },
                    if level == SandboxLevel::Off || own.is_empty() {
                        String::new()
                    } else {
                        format!(" · own state: {}", own.join(" "))
                            .dimmed()
                            .to_string()
                    }
                );
            }
            None => println!(
                "  {} {} {}",
                "[-]".dimmed(),
                d.display_name.dimmed(),
                format!("(binary '{}' not found on PATH)", d.binary_names.join("/")).dimmed()
            ),
        }
    }
    println!(
        "      {}",
        "* = supported with caveats (shown in the TUI)".dimmed()
    );
    println!();

    match registry.resolve(None, config.default_harness.as_deref(), &overrides) {
        Ok((h, bin)) => println!(
            "Active Default: {} ({})",
            h.descriptor().display_name.bold().green(),
            bin.display().to_string().dimmed()
        ),
        Err(e) => println!("Active Default: {}", format!("Error: {e}").red()),
    }
    println!(
        "Default Policy: {}",
        config.default_policy.as_deref().unwrap_or("ask").bold()
    );
    let requested = match sandbox.explicit {
        Some(level) => level.to_string(),
        None => format!("{} (default)", SandboxLevel::WorkspaceWrite),
    };
    match &sandbox.backend {
        Ok(backend) => println!(
            "Sandbox: {} via {} ({})",
            requested.bold(),
            backend.name(),
            backend.detail()
        ),
        Err(why) if sandbox.explicit == Some(SandboxLevel::Off) => {
            println!("Sandbox: {} {}", "off".bold(), format!("({why})").dimmed())
        }
        Err(why) => println!(
            "Sandbox: {} {}",
            "[!] unavailable, harnesses run unconfined:".yellow().bold(),
            why
        ),
    }
    for (label, paths) in [
        ("writable", &config.sandbox.writable),
        ("readable", &config.sandbox.readable),
        ("denied reads", &config.sandbox.deny_read),
    ] {
        if !paths.is_empty() {
            let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            println!("  {} extra {label}: {}", "↳".dimmed(), list.join(" "));
        }
    }
    report_allow_rules(find_workspace_root(cwd).as_deref());
    println!();

    // The servers are started the way a session would start them: in the
    // workspace, inside the sandbox (without any one harness's own paths).
    let workspace = find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    match sandbox::resolve(
        &SandboxRequest {
            explicit: sandbox.explicit,
            default: SandboxLevel::WorkspaceWrite,
            workspace: &workspace,
            harness: &SandboxPaths::default(),
            extra_writable: &config.sandbox.writable,
            extra_readable: &config.sandbox.readable,
            extra_deny_read: &config.sandbox.deny_read,
        },
        &sandbox.backend,
        &SandboxEnv::current(),
    ) {
        Ok(probe_sandbox) => report_mcp_servers(config, &mcp_support, &probe_sandbox, cwd),
        Err(e) => println!(
            "{} {}\n",
            "MCP Servers:".bold(),
            format!("not checked, the sandbox could not be set up: {e:#}").yellow()
        ),
    }

    // 2. Skills CLI
    println!("{}", "Skills CLI:".bold());
    let skills_cli = SkillsCli::detect();
    let marker = if skills_cli == SkillsCli::Missing {
        "[!]".yellow().bold()
    } else {
        "[✓]".green().bold()
    };
    println!("  {} {}", marker, skills_cli.describe());
    println!();

    // 3. Workspace
    println!("{}", "Workspace Context:".bold());
    let ws_root = find_workspace_root(cwd);
    match &ws_root {
        Some(root) => {
            println!("  Root: {}", root.display());
            if let Some(store) = Config::workspace_store() {
                let path = Config::workspace_path_in(&store, root);
                println!(
                    "  Settings: {} {}",
                    path.display(),
                    if path.exists() { "" } else { "(none yet)" }.dimmed()
                );
            }
            report_rules(root);
            report_skills(
                "Workspace Skills",
                &discover_skills_in_dir(&workspace_skills_dir(root)),
            );
            for (k, settings) in &config.harnesses {
                if settings.protocol.as_deref() == Some("acp") && settings.command.is_empty() {
                    println!(
                        "  {} harness '{}' has protocol = \"acp\" but no command in the config",
                        "[!]".yellow(),
                        k
                    );
                } else if registry.parse(k).is_none() {
                    println!("  {} unknown harness '{}' in the config", "[!]".yellow(), k);
                }
            }
        }
        None => {
            println!("  Not inside a recognized workspace (no .git, .agents or AGENTS.md found)")
        }
    }
    println!();

    // 4. Global skills
    println!("{}", "Global Skills (~/.agents/skills):".bold());
    let global = global_skills_dir()
        .map(|d| discover_skills_in_dir(&d))
        .unwrap_or_default();
    report_skills("", &global);
    println!();

    // 4b. Harness plugins and extensions (informational; not importable)
    if let Some(home) = dirs::home_dir() {
        let plugins = installed_plugins(&home);
        if !plugins.is_empty() {
            println!(
                "{}",
                "Harness plugins (not portable; skills inside them are):".bold()
            );
            for p in &plugins {
                println!(
                    "  {} {:<8} {} {}",
                    "•".dimmed(),
                    p.harness,
                    p.name,
                    p.detail.dimmed()
                );
            }
            println!(
                "  {}",
                "`unharness skills import` finds SKILL.md directories bundled in these.".dimmed()
            );
            println!();
        }
    }

    // 5. Conversations
    let store = ConversationStore::open(ws_root.as_deref(), cwd);
    let rows = store.list();
    println!("{}", "Conversations:".bold());
    if rows.is_empty() {
        println!(
            "  {} none saved in {}",
            "[-]".dimmed(),
            store.root().display()
        );
    } else {
        println!(
            "  {} {} saved in {} (latest: {} [{}])",
            "[✓]".green().bold(),
            rows.len(),
            store.root().display().to_string().dimmed(),
            &rows[0].id[..8],
            rows[0]
                .harnesses
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    println!();
    println!(
        "{}",
        "Run 'unharness \"prompt\"' to start, or 'unharness skills add <source>' to install skills.".green()
    );
    Ok(())
}

/// How long an MCP server gets to answer the handshake.
const MCP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// What an installed harness does with MCP servers: its name, what it
/// declares, and the servers it has itself.
type McpTaker = (&'static str, McpSupport, Vec<String>);

/// What is allowed without asking, and where that is written down.
/// The version and how it updates; `unharness update --check` asks the
/// network, this does not.
fn report_install() {
    print!("unharness {}", env!("CARGO_PKG_VERSION").bold());
    match crate::update::detect() {
        Ok(install) => {
            println!(" ({})", install.exe.display().to_string().dimmed());
            println!(
                "  {} {}; update with: {}",
                "↳".dimmed(),
                install.method.describe(),
                install.method.how()
            );
        }
        Err(e) => println!(" {} {e:#}", "[!]".yellow().bold()),
    }
    println!();
}

fn report_allow_rules(ws_root: Option<&Path>) {
    let rules = match Rules::load(ws_root) {
        Ok(rules) => rules,
        Err(e) => {
            println!("Allow Rules: {} {e:#}", "[!]".yellow().bold());
            return;
        }
    };
    println!("Allow Rules: {}", rules.iter().count().to_string().bold());
    for (scope, rule) in rules.iter() {
        let scope = match scope {
            Scope::Workspace => "workspace",
            Scope::Global => "global",
        };
        println!("  {} [{scope}] {}", "↳".dimmed(), rule.describe());
    }
    for path in [Scope::Workspace, Scope::Global]
        .into_iter()
        .filter_map(|scope| rules.path(scope))
    {
        let missing = if path.exists() { "" } else { "(none yet)" };
        println!("  {} {} {}", "↳".dimmed(), path.display(), missing.dimmed());
    }
}

/// 1b. The configured MCP servers: whether each one answers, and which
/// installed harnesses get it.
fn report_mcp_servers(config: &Config, harnesses: &[McpTaker], sandbox: &Sandbox, cwd: &Path) {
    if config.mcp_servers.is_empty() {
        return;
    }
    println!("{}", "MCP Servers:".bold());
    let (servers, problems) = config.mcp_servers();
    for server in &servers {
        let status = if server.is_http() {
            Ok("remote (not contacted)".to_string())
        } else {
            mcp::probe_stdio(server, MCP_PROBE_TIMEOUT, sandbox, cwd)
                .map(|p| {
                    let more = if p.more_tools { "+" } else { "" };
                    format!("{} · tools: {}{more}", p.server, p.tools)
                })
                .map_err(|e| match sandbox.level() {
                    SandboxLevel::Off => e,
                    level => format!("{e} (started in the {level} sandbox, as a harness would)"),
                })
        };
        let (marker, status) = match status {
            Ok(s) => ("[✓]".green().bold(), s.normal()),
            Err(e) => ("[!]".yellow().bold(), e.yellow()),
        };
        println!(
            "  {} {} {}",
            marker,
            server.name.bold(),
            server.target().dimmed()
        );
        println!("      {} {}", "↳".dimmed(), status);

        let mut reach: [(&str, Vec<&str>); 4] = [
            ("passed to", vec![]),
            ("if the agent takes http servers", vec![]),
            ("its own server of this name is used by", vec![]),
            ("not to", vec![]),
        ];
        for (name, support, own) in harnesses {
            let passed = !mcp::for_harness(std::slice::from_ref(server), *support, name, own)
                .0
                .is_empty();
            // Over a protocol, http servers go along only if the session
            // says it takes them.
            let undecided =
                server.is_http() && support.channel == Some(McpChannel::Protocol) && !support.http;
            let group = match (passed, undecided) {
                (true, false) => 0,
                (true, true) => 1,
                (false, _) if own.contains(&server.name) => 2,
                (false, _) => 3,
            };
            reach[group].1.push(name);
        }
        let reach: Vec<String> = reach
            .iter()
            .filter(|(_, names)| !names.is_empty())
            .map(|(label, names)| format!("{label}: {}", names.join(", ")))
            .collect();
        if !reach.is_empty() {
            println!("      {} {}", "↳".dimmed(), reach.join(" · ").dimmed());
        }
    }
    for (name, _) in config
        .mcp_servers
        .iter()
        .filter(|(_, s)| s.enabled == Some(false))
    {
        println!(
            "  {} {} {}",
            "[-]".dimmed(),
            name.dimmed(),
            "disabled".dimmed()
        );
    }
    for problem in problems {
        println!("  {} {}", "[!]".yellow().bold(), problem);
    }
    println!();
}

fn report_skills(label: &str, skills: &[crate::skills::SkillInfo]) {
    let prefix = if label.is_empty() {
        String::new()
    } else {
        format!("{label}: ")
    };
    if skills.is_empty() {
        println!("  {} {}none", "[-]".dimmed(), prefix);
        return;
    }
    println!(
        "  {} {}{} skills",
        "[✓]".green().bold(),
        prefix,
        skills.len().to_string().bold()
    );
    for s in skills {
        println!(
            "      {} {} {}",
            "•".dimmed(),
            s.name,
            s.description.as_deref().unwrap_or("").dimmed()
        );
    }
}

fn report_rules(root: &Path) {
    let agents_md = root.join("AGENTS.md");
    let claude_md = root.join("CLAUDE.md");
    let gemini_md = root.join("GEMINI.md");

    if !agents_md.exists() {
        if claude_md.exists() {
            println!(
                "  {} CLAUDE.md exists without AGENTS.md (run 'unharness sync' to promote it)",
                "[!]".yellow()
            );
        } else {
            println!(
                "  {} No AGENTS.md found (run 'unharness init' to create one)",
                "[-]".dimmed()
            );
        }
        return;
    }

    println!(
        "  {} AGENTS.md (canonical project instructions)",
        "[✓]".green().bold()
    );
    for (name, path, harness) in [
        ("CLAUDE.md", &claude_md, "Claude Code"),
        ("GEMINI.md", &gemini_md, "Antigravity"),
    ] {
        if path.is_symlink() {
            println!(
                "    {} {} -> AGENTS.md (symlinked for {})",
                "↳".dimmed(),
                name,
                harness
            );
        } else if path.exists() {
            println!(
                "    {} {} is a separate file (may diverge from AGENTS.md)",
                "[!]".yellow(),
                name
            );
        }
    }
}
