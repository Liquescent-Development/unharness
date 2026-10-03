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

/// Make `AGENTS.md` the single source of truth for project instructions and
/// point `CLAUDE.md` and `GEMINI.md` at it.
///
/// Invariant: this never deletes or overwrites a user-authored file whose
/// content differs from `AGENTS.md`; it warns instead.
pub fn sync_workspace_rules(repo_root: &Path) -> Result<RulesSyncResult> {
    let mut result = RulesSyncResult::default();

    let agents_md = repo_root.join("AGENTS.md");
    let claude_md = repo_root.join("CLAUDE.md");
    let gemini_md = repo_root.join("GEMINI.md");

    // Case 1: Neither exists -> nothing to do
    if !agents_md.exists() && !claude_md.exists() {
        return Ok(result);
    }

    // Case 2: Regular CLAUDE.md but no AGENTS.md -> promote CLAUDE.md to AGENTS.md
    if claude_md.exists() && !agents_md.exists() && !claude_md.is_symlink() {
        fs::copy(&claude_md, &agents_md)?;
        fs::remove_file(&claude_md)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("AGENTS.md", &claude_md)?;
        result.claude_md_created = true;
    }

    if agents_md.exists() {
        result.agents_md_path = Some(agents_md.clone());
        ensure_symlink(
            &claude_md,
            "AGENTS.md",
            &mut result.claude_md_created,
            &mut result.warnings,
        )?;
        ensure_symlink(
            &gemini_md,
            "AGENTS.md",
            &mut result.gemini_md_created,
            &mut result.warnings,
        )?;
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
            return Ok(());
        }
        // Points at the wrong target: repoint.
        fs::remove_file(link_path)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target_rel_str, link_path)?;
        *created_flag = true;
        return Ok(());
    }

    if link_path.exists() {
        // Regular file or directory. Replace only when content is identical.
        let target_path = link_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target_rel_str);
        if target_path.exists() {
            let link_content = fs::read_to_string(link_path).unwrap_or_default();
            let target_content = fs::read_to_string(&target_path).unwrap_or_default();
            if link_content == target_content {
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

    #[cfg(unix)]
    std::os::unix::fs::symlink(target_rel_str, link_path)?;
    *created_flag = true;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn link_target(p: &Path) -> Option<PathBuf> {
        fs::read_link(p).ok()
    }

    #[test]
    fn empty_workspace_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let r = sync_workspace_rules(dir.path()).unwrap();
        assert!(r.agents_md_path.is_none());
        assert!(!dir.path().join("CLAUDE.md").exists());
        assert!(!dir.path().join("GEMINI.md").exists());
    }

    #[test]
    fn agents_md_only_creates_both_links() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# rules").unwrap();

        let r = sync_workspace_rules(dir.path()).unwrap();
        assert!(r.claude_md_created && r.gemini_md_created);
        assert!(r.warnings.is_empty());
        assert_eq!(
            link_target(&dir.path().join("CLAUDE.md")).unwrap(),
            Path::new("AGENTS.md")
        );
        assert_eq!(
            link_target(&dir.path().join("GEMINI.md")).unwrap(),
            Path::new("AGENTS.md")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "# rules"
        );

        // Idempotent second run
        let r2 = sync_workspace_rules(dir.path()).unwrap();
        assert!(!r2.claude_md_created && !r2.gemini_md_created);
        assert!(r2.warnings.is_empty());
    }

    #[test]
    fn regular_claude_md_is_promoted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "# from claude").unwrap();

        let r = sync_workspace_rules(dir.path()).unwrap();
        assert!(r.claude_md_created);
        assert_eq!(
            fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "# from claude"
        );
        assert!(dir.path().join("CLAUDE.md").is_symlink());
        assert!(dir.path().join("GEMINI.md").is_symlink());
    }

    #[test]
    fn divergent_regular_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# canonical").unwrap();
        fs::write(dir.path().join("GEMINI.md"), "# something else").unwrap();

        let r = sync_workspace_rules(dir.path()).unwrap();
        assert!(r.claude_md_created);
        assert!(!r.gemini_md_created);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("GEMINI.md"));
        assert!(!dir.path().join("GEMINI.md").is_symlink());
        assert_eq!(
            fs::read_to_string(dir.path().join("GEMINI.md")).unwrap(),
            "# something else"
        );
    }

    #[test]
    fn identical_regular_file_is_replaced_with_link() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# same").unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "# same").unwrap();

        let r = sync_workspace_rules(dir.path()).unwrap();
        assert!(r.claude_md_created);
        assert!(r.warnings.is_empty());
        assert!(dir.path().join("CLAUDE.md").is_symlink());
    }

    #[test]
    fn wrong_target_symlink_is_repointed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# rules").unwrap();
        fs::write(dir.path().join("OTHER.md"), "# other").unwrap();
        std::os::unix::fs::symlink("OTHER.md", dir.path().join("CLAUDE.md")).unwrap();

        let r = sync_workspace_rules(dir.path()).unwrap();
        assert!(r.claude_md_created);
        assert_eq!(
            link_target(&dir.path().join("CLAUDE.md")).unwrap(),
            Path::new("AGENTS.md")
        );
        // The file the old link pointed at is untouched.
        assert_eq!(
            fs::read_to_string(dir.path().join("OTHER.md")).unwrap(),
            "# other"
        );
    }
}
