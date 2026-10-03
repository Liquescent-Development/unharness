//! `unharness skills import`: find `SKILL.md` directories the installed
//! harnesses already have (plugin-bundled Claude skills, Codex skills, pi
//! skills, project-level agent dirs) and install the chosen ones into the
//! canonical `.agents/skills` through the `skills` CLI, which also projects
//! them into every agent directory. Falls back to a plain copy when the CLI
//! is unavailable or `--copy` is given.
//!
//! Plugins and extensions are not skills: they are listed for visibility by
//! `doctor` (see [`installed_plugins`]) but never imported.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use colored::*;
use serde::Serialize;

use crate::skills::parse_skill_md;
use crate::skills_cmd::SkillsCli;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    ClaudeUser,
    ClaudeSynced,
    ClaudePlugin,
    CodexUser,
    CodexSystem,
    Antigravity,
    Pi,
    Project,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::ClaudeUser => "claude",
            Source::ClaudeSynced => "claude (synced)",
            Source::ClaudePlugin => "claude plugin",
            Source::CodexUser => "codex",
            Source::CodexSystem => "codex (system)",
            Source::Antigravity => "agy",
            Source::Pi => "pi",
            Source::Project => "project",
        }
    }

    /// `--from` filter keys that select this source.
    fn matches(&self, key: &str) -> bool {
        match key {
            "claude" => matches!(
                self,
                Source::ClaudeUser | Source::ClaudeSynced | Source::ClaudePlugin
            ),
            "plugins" | "plugin" => matches!(self, Source::ClaudePlugin),
            "codex" => matches!(self, Source::CodexUser | Source::CodexSystem),
            "agy" | "antigravity" => matches!(self, Source::Antigravity),
            "pi" => matches!(self, Source::Pi),
            "project" => matches!(self, Source::Project),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not in the target `.agents/skills`.
    New,
    /// Already there with identical content.
    Installed,
    /// Already there but the content differs.
    Differs,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub description: Option<String>,
    pub path: PathBuf,
    pub source: Source,
    /// Plugin or bucket the skill came from, when applicable.
    pub origin: Option<String>,
    pub hash: String,
    pub status: Status,
}

/// Stable content hash of every regular file in a directory (sorted by
/// relative path). Not cryptographic; it only detects "same bytes".
pub fn dir_hash(dir: &Path) -> String {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if let Ok(bytes) = std::fs::read(&p) {
                let rel = p
                    .strip_prefix(base)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string();
                out.push((rel, bytes));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = DefaultHasher::new();
    for (rel, bytes) in files {
        rel.hash(&mut h);
        bytes.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// A symlink into some `.agents/skills` is a projection, not a source.
fn is_projection(path: &Path) -> bool {
    path.is_symlink()
        && std::fs::read_link(path)
            .map(|t| t.to_string_lossy().contains(".agents/skills"))
            .unwrap_or(false)
}

fn skill_dirs(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("SKILL.md").is_file() && !is_projection(p))
        .collect();
    dirs.sort();
    dirs
}

/// Scan the known harness locations under `home` (and the project, if any).
pub fn discover(
    home: &Path,
    project: Option<&Path>,
    include_system: bool,
    target: &Path,
) -> Vec<Candidate> {
    let mut found: Vec<(Source, Option<String>, PathBuf)> = Vec::new();
    let push_all = |found: &mut Vec<_>, source: Source, origin: Option<String>, parent: &Path| {
        for d in skill_dirs(parent) {
            found.push((source, origin.clone(), d));
        }
    };

    push_all(
        &mut found,
        Source::ClaudeUser,
        None,
        &home.join(".claude/skills"),
    );
    if let Ok(buckets) = std::fs::read_dir(home.join(".claude/skills/synced")) {
        for b in buckets.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let origin = b.file_name().map(|n| n.to_string_lossy().to_string());
            push_all(&mut found, Source::ClaudeSynced, origin, &b);
        }
    }
    // ~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/skills/<skill>
    if let Ok(markets) = std::fs::read_dir(home.join(".claude/plugins/cache")) {
        for m in markets.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let Ok(plugins) = std::fs::read_dir(&m) else {
                continue;
            };
            for p in plugins.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
                let Ok(versions) = std::fs::read_dir(&p) else {
                    continue;
                };
                let mut versions: Vec<PathBuf> = versions
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                versions.sort();
                // Latest version only.
                if let Some(v) = versions.last() {
                    let origin = Some(format!(
                        "{}@{}",
                        p.file_name()
                            .map(|n| n.to_string_lossy())
                            .unwrap_or_default(),
                        m.file_name()
                            .map(|n| n.to_string_lossy())
                            .unwrap_or_default()
                    ));
                    push_all(&mut found, Source::ClaudePlugin, origin, &v.join("skills"));
                }
            }
        }
    }
    push_all(
        &mut found,
        Source::CodexUser,
        None,
        &home.join(".codex/skills"),
    );
    if include_system {
        push_all(
            &mut found,
            Source::CodexSystem,
            None,
            &home.join(".codex/skills/.system"),
        );
    }
    push_all(
        &mut found,
        Source::Antigravity,
        None,
        &home.join(".gemini/antigravity-cli/.agents/skills"),
    );
    push_all(&mut found, Source::Pi, None, &home.join(".pi/agent/skills"));
    if let Some(root) = project {
        for sub in [".claude/skills", ".codex/skills", ".pi/skills"] {
            let origin = Some(sub.to_string());
            push_all(&mut found, Source::Project, origin, &root.join(sub));
        }
    }

    let mut out: Vec<Candidate> = Vec::new();
    for (source, origin, path) in found {
        // Skip anything that *is* the target.
        if path.starts_with(target) {
            continue;
        }
        let dir_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let (name, description) = parse_skill_md(&path.join("SKILL.md"));
        let name = name.unwrap_or(dir_name);
        let hash = dir_hash(&path);
        if out.iter().any(|c| c.name == name && c.hash == hash) {
            continue; // identical copy seen from another location
        }
        let installed = target.join(&name);
        let status = if installed.join("SKILL.md").is_file() {
            if dir_hash(&installed) == hash {
                Status::Installed
            } else {
                Status::Differs
            }
        } else {
            Status::New
        };
        out.push(Candidate {
            name,
            description,
            path,
            source,
            origin,
            hash,
            status,
        });
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// `--from` filters: claude, plugins, codex, agy, pi, project.
    pub from: Vec<String>,
    pub global: bool,
    pub all: bool,
    pub include_system: bool,
    /// Copy directly instead of going through the `skills` CLI.
    pub copy: bool,
    pub dry_run: bool,
}

impl ImportOptions {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut o = ImportOptions::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "-g" | "--global" => o.global = true,
                "--all" | "-y" | "--yes" => o.all = true,
                "--include-system" => o.include_system = true,
                "--copy" => o.copy = true,
                "--dry-run" | "-n" => o.dry_run = true,
                "--from" => {
                    let v = it.next().context("--from needs a value")?;
                    o.from.extend(v.split(',').map(|s| s.trim().to_lowercase()));
                }
                s if s.starts_with("--from=") => {
                    o.from
                        .extend(s[7..].split(',').map(|s| s.trim().to_lowercase()));
                }
                "-h" | "--help" => bail!(
                    "usage: unharness skills import [--from claude,plugins,codex,agy,pi,project] [--global] [--all] [--include-system] [--copy] [--dry-run]"
                ),
                other => bail!("unknown import option '{other}' (see --help)"),
            }
        }
        Ok(o)
    }
}

/// Parse "1,3-5" / "all" into zero-based indexes within `n`.
pub fn parse_selection(input: &str, n: usize) -> Result<Vec<usize>> {
    let t = input.trim().to_lowercase();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    if t == "all" || t == "*" {
        return Ok((0..n).collect());
    }
    let mut picks = Vec::new();
    for part in t.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<usize>()?, b.trim().parse::<usize>()?),
            None => {
                let v = part.parse::<usize>()?;
                (v, v)
            }
        };
        if a == 0 || b == 0 || a > n || b > n || a > b {
            bail!("'{part}' is out of range 1..{n}");
        }
        for i in a..=b {
            if !picks.contains(&(i - 1)) {
                picks.push(i - 1);
            }
        }
    }
    Ok(picks)
}

#[derive(Serialize)]
struct Provenance<'a> {
    source: &'a Path,
    origin: Option<&'a str>,
    hash: &'a str,
    imported_at: String,
}

/// Plain copy into `target/<name>` with a provenance file.
pub fn copy_skill(c: &Candidate, target: &Path) -> Result<PathBuf> {
    let dest = target.join(&c.name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("replace {}", dest.display()))?;
    }
    copy_dir(&c.path, &dest)?;
    let prov = Provenance {
        source: &c.path,
        origin: c.origin.as_deref(),
        hash: &c.hash,
        imported_at: crate::core::conversations::now_rfc3339(),
    };
    std::fs::write(
        dest.join(".unharness-import.json"),
        serde_json::to_vec_pretty(&prov)?,
    )?;
    Ok(dest)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let p = e.path();
        let dest = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dest)?;
        } else {
            std::fs::copy(&p, &dest).with_context(|| format!("copy {}", p.display()))?;
        }
    }
    Ok(())
}

pub fn run_import(cwd: &Path, workspace_root: Option<&Path>, args: &[String]) -> Result<()> {
    let opts = ImportOptions::parse(args)?;
    let home = dirs::home_dir().context("home directory not found")?;
    let project_root = workspace_root.unwrap_or(cwd).to_path_buf();
    let target = if opts.global {
        home.join(".agents/skills")
    } else {
        project_root.join(".agents/skills")
    };

    let mut candidates = discover(&home, Some(&project_root), opts.include_system, &target);
    if !opts.from.is_empty() {
        candidates.retain(|c| opts.from.iter().any(|k| c.source.matches(k)));
    }
    if candidates.is_empty() {
        println!(
            "No importable skills found{}. Harness plugins and pi extensions are not skills; see `unharness doctor`.",
            if opts.include_system {
                ""
            } else {
                " (add --include-system for Codex built-ins)"
            }
        );
        return Ok(());
    }

    println!(
        "{} → {}",
        "Skills found on this machine".bold().cyan(),
        target.display().to_string().dimmed()
    );
    for (i, c) in candidates.iter().enumerate() {
        let status = match c.status {
            Status::New => "new".green().to_string(),
            Status::Installed => "installed".dimmed().to_string(),
            Status::Differs => "differs".yellow().to_string(),
        };
        let desc: String = c
            .description
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let desc = crate::tui::transcript::truncate_chars(&desc, 90);
        println!(
            "  {:>2}. {:<28} {:<16} {:<10} {}",
            i + 1,
            c.name.bold(),
            c.source.label(),
            status,
            desc.dimmed()
        );
        if let Some(o) = &c.origin
            && c.source != Source::ClaudeSynced
        {
            println!("      {} {}", "from".dimmed(), o.dimmed());
        }
    }

    let picks: Vec<usize> = if opts.all {
        candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.status != Status::Installed)
            .map(|(i, _)| i)
            .collect()
    } else {
        print!("\nImport which? (e.g. 1,3-5 or all; Enter to cancel): ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        parse_selection(&line, candidates.len())?
    };
    if picks.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    let cli = SkillsCli::detect();
    let use_cli = !opts.copy && cli != SkillsCli::Missing;
    let mut ok = 0;
    for i in picks {
        let c = &candidates[i];
        if opts.dry_run {
            println!("  would import {} from {}", c.name, c.path.display());
            continue;
        }
        let result = if use_cli {
            let mut cmd = cli.command()?;
            cmd.arg("add").arg(&c.path).arg("-y");
            if opts.global {
                cmd.arg("-g");
            }
            cmd.current_dir(&project_root)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            cmd.status().map_err(anyhow::Error::from).and_then(|s| {
                if s.success() {
                    Ok(())
                } else {
                    bail!("skills CLI exited with {s}")
                }
            })
        } else {
            copy_skill(c, &target).map(|_| ())
        };
        match result {
            Ok(()) => {
                ok += 1;
                println!("  {} {}", "[✓]".green().bold(), c.name);
            }
            Err(e) => println!("  {} {}: {e:#}", "[✗]".red().bold(), c.name),
        }
    }
    if !opts.dry_run {
        println!(
            "Imported {ok} skill(s) into {}{}",
            target.display(),
            if use_cli {
                " and projected into the agent directories"
            } else {
                " (copied; run `unharness skills list` to confirm projection)"
            }
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Plugins / extensions: visibility only.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub harness: &'static str,
    pub name: String,
    pub detail: String,
}

pub fn installed_plugins(home: &Path) -> Vec<PluginInfo> {
    let mut out = Vec::new();

    if let Ok(c) = std::fs::read_to_string(home.join(".claude/plugins/installed_plugins.json"))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&c)
    {
        let plugins = v.get("plugins").unwrap_or(&v);
        if let Some(map) = plugins.as_object() {
            for (name, entry) in map {
                let first = entry.as_array().and_then(|a| a.first()).unwrap_or(entry);
                let version = first
                    .get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        first
                            .get("installPath")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|p| {
                                Path::new(p)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                            })
                    })
                    .unwrap_or_default();
                out.push(PluginInfo {
                    harness: "claude",
                    name: name.clone(),
                    detail: version,
                });
            }
        }
    }

    if let Ok(markets) = std::fs::read_dir(home.join(".codex/plugins/cache")) {
        for m in markets.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            if let Ok(plugins) = std::fs::read_dir(&m) {
                for p in plugins.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
                    out.push(PluginInfo {
                        harness: "codex",
                        name: p
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        detail: m
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    });
                }
            }
        }
    }

    if let Ok(exts) = std::fs::read_dir(home.join(".pi/agent/extensions")) {
        for e in exts.flatten().map(|e| e.path()) {
            let name = e
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.starts_with('.') {
                continue;
            }
            out.push(PluginInfo {
                harness: "pi",
                name,
                detail: "extension".into(),
            });
        }
    }

    out.sort_by(|a, b| a.harness.cmp(b.harness).then(a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(
            dir.join(name).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn discovers_dedupes_and_reports_status() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        skill(&home.join(".claude/skills"), "alpha", "a");
        skill(&home.join(".codex/skills"), "alpha", "a"); // identical → deduped
        skill(&home.join(".codex/skills/.system"), "sys", "s");
        skill(
            &home.join(".claude/plugins/cache/mkt/plug/1.0.0/skills"),
            "beta",
            "b1",
        );
        skill(
            &home.join(".claude/plugins/cache/mkt/plug/1.2.0/skills"),
            "beta",
            "b2",
        ); // latest version wins
        skill(&home.join(".pi/agent/skills"), "gamma", "g");
        skill(&proj.join(".claude/skills"), "delta", "d");
        // A projection symlink into .agents/skills must be ignored.
        let target = proj.join(".agents/skills");
        skill(&target, "gamma", "g"); // installed, identical
        skill(&target, "delta", "old"); // differs
        std::os::unix::fs::symlink(
            "../../.agents/skills/gamma",
            proj.join(".claude/skills/gamma"),
        )
        .unwrap();

        let c = discover(&home, Some(&proj), false, &target);
        let names: Vec<(String, Source, Status)> = c
            .iter()
            .map(|c| (c.name.clone(), c.source, c.status))
            .collect();
        assert!(names.contains(&("alpha".into(), Source::ClaudeUser, Status::New)));
        assert_eq!(names.iter().filter(|n| n.0 == "alpha").count(), 1);
        assert!(names.contains(&("beta".into(), Source::ClaudePlugin, Status::New)));
        let beta = c.iter().find(|c| c.name == "beta").unwrap();
        assert!(beta.path.to_string_lossy().contains("1.2.0"));
        assert_eq!(beta.origin.as_deref(), Some("plug@mkt"));
        assert!(names.contains(&("gamma".into(), Source::Pi, Status::Installed)));
        assert!(names.contains(&("delta".into(), Source::Project, Status::Differs)));
        assert!(!names.iter().any(|n| n.0 == "sys"));
        let with_sys = discover(&home, Some(&proj), true, &target);
        assert!(
            with_sys
                .iter()
                .any(|c| c.name == "sys" && c.source == Source::CodexSystem)
        );
    }

    #[test]
    fn copy_skill_writes_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        skill(&src, "zeta", "z");
        std::fs::create_dir_all(src.join("zeta/sub")).unwrap();
        std::fs::write(src.join("zeta/sub/extra.txt"), "x").unwrap();
        let target = tmp.path().join("target");
        let c = Candidate {
            name: "zeta".into(),
            description: None,
            path: src.join("zeta"),
            source: Source::Pi,
            origin: None,
            hash: dir_hash(&src.join("zeta")),
            status: Status::New,
        };
        let dest = copy_skill(&c, &target).unwrap();
        assert!(dest.join("SKILL.md").is_file());
        assert!(dest.join("sub/extra.txt").is_file());
        let prov: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dest.join(".unharness-import.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(prov["hash"], c.hash);
        // Re-copy replaces.
        copy_skill(&c, &target).unwrap();
    }

    #[test]
    fn selection_and_options() {
        assert_eq!(parse_selection("1,3-4", 5).unwrap(), vec![0, 2, 3]);
        assert_eq!(parse_selection("all", 3).unwrap(), vec![0, 1, 2]);
        assert!(parse_selection("", 3).unwrap().is_empty());
        assert!(parse_selection("0", 3).is_err());
        assert!(parse_selection("4", 3).is_err());
        let o = ImportOptions::parse(&[
            "--from".into(),
            "claude,pi".into(),
            "-g".into(),
            "--all".into(),
        ])
        .unwrap();
        assert_eq!(o.from, vec!["claude", "pi"]);
        assert!(o.global && o.all);
        assert!(ImportOptions::parse(&["--bogus".into()]).is_err());
        assert!(Source::ClaudePlugin.matches("claude") && Source::ClaudePlugin.matches("plugins"));
        assert!(!Source::Pi.matches("claude"));
    }

    #[test]
    fn plugin_inventory() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude/plugins")).unwrap();
        std::fs::write(
            home.join(".claude/plugins/installed_plugins.json"),
            r#"{"version":2,"plugins":{"cq@cq":[{"installPath":"/x/cache/cq/cq/0.15.0"}],"lsp@official":[{"version":"1.0.0"}]}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".codex/plugins/cache/store/helper")).unwrap();
        std::fs::create_dir_all(home.join(".pi/agent/extensions")).unwrap();
        std::fs::write(home.join(".pi/agent/extensions/thing.ts"), "").unwrap();
        let p = installed_plugins(home);
        assert_eq!(p.len(), 4);
        assert_eq!(
            p[0],
            PluginInfo {
                harness: "claude",
                name: "cq@cq".into(),
                detail: "0.15.0".into()
            }
        );
        assert_eq!(p[1].detail, "1.0.0");
        assert_eq!(p[2].harness, "codex");
        assert_eq!(p[3].name, "thing.ts");
    }
}
