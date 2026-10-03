//! Read-only discovery of Agent Skills (`<dir>/<name>/SKILL.md`).
//!
//! Installation and projection into per-harness directories is delegated to
//! the Vercel `skills` CLI (see `skills_cmd.rs`). This module only reads what
//! is on disk so `doctor` and the TUI `/skills` command can list it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Canonical workspace skills directory.
pub fn workspace_skills_dir(root: &Path) -> PathBuf {
    root.join(".agents").join("skills")
}

/// Canonical global skills directory (`~/.agents/skills`).
pub fn global_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".agents").join("skills"))
}

pub fn parse_skill_md(path: &Path) -> (Option<String>, Option<String>) {
    match fs::read_to_string(path) {
        Ok(c) => parse_skill_content(&c),
        Err(_) => (None, None),
    }
}

pub fn parse_skill_content(content: &str) -> (Option<String>, Option<String>) {
    let trimmed = content.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (None, None);
    };
    if let Some(end_idx) = rest.find("---") {
        let yaml_str = &rest[..end_idx];
        if let Ok(fm) = serde_yaml::from_str::<SkillFrontmatter>(yaml_str) {
            return (fm.name, fm.description);
        }
    }
    (None, None)
}

pub fn discover_skills_in_dir(skills_dir: &Path) -> Vec<SkillInfo> {
    let mut skills = Vec::new();
    let Ok(entries) = fs::read_dir(skills_dir) else {
        return skills;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let dir_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let (name_opt, desc_opt) = parse_skill_md(&skill_md);
        skills.push(SkillInfo {
            name: name_opt.unwrap_or(dir_name),
            description: desc_opt,
        });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_content() {
        let content = "---\nname: sample-skill\ndescription: A sample skill for testing.\n---\n\n# Instructions\n";
        let (name, desc) = parse_skill_content(content);
        assert_eq!(name.as_deref(), Some("sample-skill"));
        assert_eq!(desc.as_deref(), Some("A sample skill for testing."));
    }

    #[test]
    fn test_parse_skill_content_no_frontmatter() {
        let (name, desc) = parse_skill_content("# Header\n\nNo frontmatter here.");
        assert!(name.is_none());
        assert!(desc.is_none());
    }

    #[test]
    fn test_discover_skills_in_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path();
        fs::create_dir_all(skills.join("beta")).unwrap();
        fs::write(
            skills.join("beta/SKILL.md"),
            "---\nname: beta\ndescription: Beta skill\n---\n",
        )
        .unwrap();
        fs::create_dir_all(skills.join("alpha")).unwrap();
        fs::write(skills.join("alpha/SKILL.md"), "no frontmatter").unwrap();
        fs::create_dir_all(skills.join("not-a-skill")).unwrap();

        let found = discover_skills_in_dir(skills);
        let names: Vec<_> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(found[1].description.as_deref(), Some("Beta skill"));
        assert!(discover_skills_in_dir(&skills.join("missing")).is_empty());
    }
}
