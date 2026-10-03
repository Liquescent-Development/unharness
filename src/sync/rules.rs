use std::fs;
use std::path::{Path, PathBuf};
use anyhow::Result;

#[derive(Debug, Default)]
pub struct RulesSyncResult {
    pub agents_md_path: Option<PathBuf>,
    pub claude_md_created: bool,
    pub gemini_md_created: bool,
    pub warnings: Vec<String>,
}

pub fn sync_workspace_rules(repo_root: &Path) -> Result<RulesSyncResult> {
    let mut result = RulesSyncResult::default();

    let agents_md = repo_root.join("AGENTS.md");
    let claude_md = repo_root.join("CLAUDE.md");
    let gemini_md = repo_root.join("GEMINI.md");

    // Case 1: Neither exists
    if !agents_md.exists() && !claude_md.exists() {
        return Ok(result);
    }

    // Case 2: CLAUDE.md exists but AGENTS.md doesn't -> Promote CLAUDE.md to AGENTS.md
    if claude_md.exists() && !agents_md.exists() {
        if claude_md.is_symlink() {
            // Already a symlink pointing somewhere
            if let Ok(target) = fs::read_link(&claude_md) {
                if target == Path::new("AGENTS.md") {
                    // Broken symlink to AGENTS.md, nothing to promote
                }
            }
        } else {
            // Regular file: copy/rename to AGENTS.md and symlink CLAUDE.md -> AGENTS.md
            fs::copy(&claude_md, &agents_md)?;
            fs::remove_file(&claude_md)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink("AGENTS.md", &claude_md)?;
            result.claude_md_created = true;
        }
    }

    if agents_md.exists() {
        result.agents_md_path = Some(agents_md.clone());

        // Ensure CLAUDE.md points to AGENTS.md
        ensure_symlink(&claude_md, "AGENTS.md", &mut result.claude_md_created, &mut result.warnings)?;

        // Ensure GEMINI.md points to AGENTS.md
        ensure_symlink(&gemini_md, "AGENTS.md", &mut result.gemini_md_created, &mut result.warnings)?;
    }

    Ok(result)
}

fn ensure_symlink(
    link_path: &Path,
    target_rel_str: &str,
    created_flag: &mut bool,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if link_path.is_symlink() {
        let current_target = fs::read_link(link_path)?;
        if current_target == Path::new(target_rel_str) {
            // All good, already pointing to target
            return Ok(());
        }
        // Points to wrong target, remove and recreate
        fs::remove_file(link_path)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target_rel_str, link_path)?;
        *created_flag = true;
        return Ok(());
    }

    if link_path.exists() {
        // It's a regular file or directory. Check if it's identical
        let target_path = link_path.parent().unwrap_or_else(|| Path::new(".")).join(target_rel_str);
        if target_path.exists() {
            let link_content = fs::read_to_string(link_path).unwrap_or_default();
            let target_content = fs::read_to_string(&target_path).unwrap_or_default();
            if link_content == target_content {
                // Identical content: safely replace with symlink
                fs::remove_file(link_path)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(target_rel_str, link_path)?;
                *created_flag = true;
                return Ok(());
            }
        }
        warnings.push(format!(
            "{} is a distinct regular file and differs from {}. Leaving it in place to prevent data loss.",
            link_path.display(),
            target_rel_str
        ));
        return Ok(());
    }

    // Does not exist, create symlink
    #[cfg(unix)]
    std::os::unix::fs::symlink(target_rel_str, link_path)?;
    *created_flag = true;

    Ok(())
}
