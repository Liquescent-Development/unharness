pub mod antigravity;
pub mod claude;
pub mod rules;
pub mod skills;

use std::path::{Path, PathBuf};
use anyhow::Result;

pub use rules::sync_workspace_rules;
pub use skills::{discover_skills_in_dir, parse_skill_md, sync_global_skills, sync_workspace_skills};

#[derive(Debug, Default)]
pub struct FullSyncReport {
    pub workspace_root: Option<PathBuf>,
    pub workspace_skills: skills::SkillsSyncReport,
    pub global_skills: skills::SkillsSyncReport,
    pub rules: rules::RulesSyncResult,
}

pub fn find_workspace_root(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists()
            || current.join(".agents").exists()
            || current.join("unharness.toml").exists()
            || current.join(".unharness.toml").exists()
            || current.join("AGENTS.md").exists()
        {
            return Some(current);
        }

        if !current.pop() {
            break;
        }
    }
    None
}

pub fn run_full_sync(workspace_root: Option<&Path>, sync_global: bool) -> Result<FullSyncReport> {
    let mut report = FullSyncReport::default();

    if let Some(root) = workspace_root {
        report.workspace_root = Some(root.to_path_buf());
        report.workspace_skills = sync_workspace_skills(root)?;
        report.rules = sync_workspace_rules(root)?;
    }

    if sync_global {
        report.global_skills = sync_global_skills()?;
        let _ = antigravity::sync_antigravity_settings(workspace_root);
        let _ = claude::sync_claude_settings(workspace_root);
    }

    Ok(report)
}
