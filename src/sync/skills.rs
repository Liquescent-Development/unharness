use std::fs;
use std::path::{Path, PathBuf};
use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: Option<String>,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub is_global: bool,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Default)]
pub struct SkillsSyncReport {
    pub total_discovered: usize,
    pub symlinks_created: usize,
    pub symlinks_pruned: usize,
    pub warnings: Vec<String>,
}

pub fn parse_skill_md(path: &Path) -> (Option<String>, Option<String>) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };

    parse_skill_content(&content)
}

pub fn parse_skill_content(content: &str) -> (Option<String>, Option<String>) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, None);
    }

    let rest = &trimmed[3..];
    if let Some(end_idx) = rest.find("---") {
        let yaml_str = &rest[..end_idx];
        if let Ok(fm) = serde_yaml::from_str::<SkillFrontmatter>(yaml_str) {
            return (fm.name, fm.description);
        }
    }

    (None, None)
}

pub fn discover_skills_in_dir(skills_dir: &Path, is_global: bool) -> Vec<SkillInfo> {
    let mut skills = Vec::new();
    if !skills_dir.is_dir() {
        return skills;
    }

    if let Ok(entries) = fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.is_file() {
                    let dir_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();

                    let (name_opt, desc_opt) = parse_skill_md(&skill_md);
                    let name = name_opt.unwrap_or(dir_name);

                    skills.push(SkillInfo {
                        name,
                        description: desc_opt,
                        path,
                        is_global,
                    });
                }
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

pub fn sync_workspace_skills(repo_root: &Path) -> Result<SkillsSyncReport> {
    let mut report = SkillsSyncReport::default();
    let source_dir = repo_root.join(".agents").join("skills");

    if !source_dir.is_dir() {
        return Ok(report);
    }

    let skills = discover_skills_in_dir(&source_dir, false);
    report.total_discovered = skills.len();

    // Target 1: .claude/skills
    let claude_skills_dir = repo_root.join(".claude").join("skills");
    project_skills_to_dir(&source_dir, &claude_skills_dir, "../../.agents/skills", &skills, &mut report)?;

    // Target 2: .codex/skills (if .codex exists or created)
    let codex_dir = repo_root.join(".codex");
    if codex_dir.exists() {
        let codex_skills_dir = codex_dir.join("skills");
        project_skills_to_dir(&source_dir, &codex_skills_dir, "../../.agents/skills", &skills, &mut report)?;
    }

    // Target 3: .pi/skills (if .pi exists)
    let pi_dir = repo_root.join(".pi");
    if pi_dir.exists() {
        let pi_skills_dir = pi_dir.join("skills");
        project_skills_to_dir(&source_dir, &pi_skills_dir, "../../.agents/skills", &skills, &mut report)?;
    }

    Ok(report)
}

pub fn sync_global_skills() -> Result<SkillsSyncReport> {
    let mut report = SkillsSyncReport::default();
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(report),
    };

    let source_dir = home.join(".agents").join("skills");
    if !source_dir.is_dir() {
        return Ok(report);
    }

    let skills = discover_skills_in_dir(&source_dir, true);
    report.total_discovered = skills.len();

    // Target 1: ~/.claude/skills
    let claude_skills = home.join(".claude").join("skills");
    project_skills_to_dir(&source_dir, &claude_skills, "../../.agents/skills", &skills, &mut report)?;

    // Target 2: ~/.gemini/antigravity-cli/.agents/skills
    let agy_agents_dir = home.join(".gemini").join("antigravity-cli").join(".agents");
    if agy_agents_dir.exists() {
        let agy_skills = agy_agents_dir.join("skills");
        project_skills_to_dir(&source_dir, &agy_skills, "../../../../.agents/skills", &skills, &mut report)?;
    }

    // Target 3: ~/.codex/skills
    let codex_dir = home.join(".codex");
    if codex_dir.exists() {
        let codex_skills = codex_dir.join("skills");
        project_skills_to_dir(&source_dir, &codex_skills, "../../.agents/skills", &skills, &mut report)?;
    }

    // Target 4: ~/.pi/agent/skills
    let pi_agent_dir = home.join(".pi").join("agent");
    if pi_agent_dir.exists() {
        let pi_skills = pi_agent_dir.join("skills");
        project_skills_to_dir(&source_dir, &pi_skills, "../../../.agents/skills", &skills, &mut report)?;
    }

    Ok(report)
}

fn project_skills_to_dir(
    _source_dir: &Path,
    target_dir: &Path,
    relative_prefix: &str,
    skills: &[SkillInfo],
    report: &mut SkillsSyncReport,
) -> Result<()> {
    fs::create_dir_all(target_dir)?;

    let mut expected_names = std::collections::HashSet::new();

    for skill in skills {
        let skill_dir_name = skill.path.file_name().unwrap();
        let link_path = target_dir.join(skill_dir_name);
        expected_names.insert(skill_dir_name.to_os_string());

        let target_relative = format!("{}/{}", relative_prefix, skill_dir_name.to_string_lossy());

        if link_path.is_symlink() {
            if let Ok(current_target) = fs::read_link(&link_path) {
                if current_target == Path::new(&target_relative) {
                    continue; // Already correctly linked
                }
            }
            // Wrong target or broken symlink
            let _ = fs::remove_file(&link_path);
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target_relative, &link_path)?;
                report.symlinks_created += 1;
            }
        } else if link_path.exists() {
            // Real file or directory exists in destination
            report.warnings.push(format!(
                "Skipping {} in {}: a regular file or directory already exists at destination",
                skill.name,
                target_dir.display()
            ));
        } else {
            // Create new symlink
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target_relative, &link_path)?;
                report.symlinks_created += 1;
            }
        }
    }

    // Prune stale symlinks in target_dir that point to source_dir
    if let Ok(entries) = fs::read_dir(target_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() {
                let name = entry.file_name();
                if !expected_names.contains(&name) {
                    // Check if it was pointing to our source_dir
                    if let Ok(target) = fs::read_link(&path) {
                        let target_str = target.to_string_lossy();
                        if target_str.contains(".agents/skills") {
                            let _ = fs::remove_file(&path);
                            report.symlinks_pruned += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_content() {
        let content = r#"---
name: sample-skill
description: A sample skill for testing.
---

# Instructions

Do something useful.
"#;
        let (name, desc) = parse_skill_content(content);
        assert_eq!(name.as_deref(), Some("sample-skill"));
        assert_eq!(desc.as_deref(), Some("A sample skill for testing."));
    }

    #[test]
    fn test_parse_skill_content_no_frontmatter() {
        let content = "# Header\n\nNo frontmatter here.";
        let (name, desc) = parse_skill_content(content);
        assert!(name.is_none());
        assert!(desc.is_none());
    }
}
