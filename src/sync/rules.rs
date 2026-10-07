use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
/// content differs from `AGENTS.md`; it warns instead. It runs outside the
/// sandbox, so it never writes through a link: a link may be replaced by
/// another, but a file is only ever created new, never opened through one.
pub fn sync_workspace_rules(repo_root: &Path) -> Result<RulesSyncResult> {
    let mut result = RulesSyncResult::default();

    let agents_md = repo_root.join("AGENTS.md");
    let claude_md = repo_root.join("CLAUDE.md");
    let gemini_md = repo_root.join("GEMINI.md");

    // Not `exists`, which follows a link: one that leads nowhere is there.
    let agents = fs::symlink_metadata(&agents_md).ok();
    let claude_is_file = fs::symlink_metadata(&claude_md).is_ok_and(|m| m.is_file());

    if claude_is_file {
        match agents {
            // A regular CLAUDE.md and no AGENTS.md: CLAUDE.md becomes it.
            None => {
                result.claude_md_created = promote(&claude_md, &agents_md, &mut result.warnings)?;
            }
            // A link that leads nowhere is not ours to write through.
            Some(m) if m.file_type().is_symlink() && !agents_md.exists() => {
                result.warnings.push(format!(
                    "{} is a symbolic link that leads nowhere; {} is left in place.",
                    agents_md.display(),
                    claude_md.display()
                ));
            }
            Some(_) => {}
        }
    }

    if agents_md.exists() {
        result.agents_md_path = Some(agents_md.clone());
        let on_the_way = chain(&agents_md);
        ensure_symlink(
            &claude_md,
            "AGENTS.md",
            &on_the_way,
            &mut result.claude_md_created,
            &mut result.warnings,
        )?;
        ensure_symlink(
            &gemini_md,
            "AGENTS.md",
            &on_the_way,
            &mut result.gemini_md_created,
            &mut result.warnings,
        )?;
    }

    Ok(result)
}

/// Every path a linked `AGENTS.md` passes through to its file, the file
/// included, each as [`resolved`] gives it. Pointing one of them at
/// `AGENTS.md` would make a loop (`AGENTS.md -> CLAUDE.md -> AGENTS.md`).
/// At most 40 links are followed, as the kernel does.
fn chain(agents_md: &Path) -> Vec<PathBuf> {
    let mut hops = Vec::new();
    let mut at = agents_md.to_path_buf();
    for _ in 0..40 {
        let Ok(text) = fs::read_link(&at) else {
            break;
        };
        // Relative to the link's own directory; an absolute text replaces it.
        at = at.parent().unwrap_or_else(|| Path::new(".")).join(text);
        hops.push(resolved(&at));
    }
    hops
}

/// `path` with its directory resolved and its last component as it is,
/// so a link compares by where it lies and not by where it leads.
fn resolved(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => fs::canonicalize(dir)
            .map(|dir| dir.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// Moves a regular `CLAUDE.md` to `AGENTS.md`, which is not there, and
/// links it back. `AGENTS.md` is created new and not through a link.
/// Whether it was moved; a `CLAUDE.md` that cannot be read stays.
fn promote(claude_md: &Path, agents_md: &Path, warnings: &mut Vec<String>) -> Result<bool> {
    use std::io::Write;
    let read = super::open_regular(claude_md, false).and_then(|file| {
        let permissions = file.metadata()?.permissions();
        Ok((super::read_capped(&file)?, permissions))
    });
    let (content, permissions) = match read {
        Ok(read) => read,
        Err(e) => {
            warnings.push(format!(
                "{} cannot be read ({e}); it is left in place.",
                claude_md.display()
            ));
            return Ok(false);
        }
    };
    // A write that fails leaves CLAUDE.md as it was and no AGENTS.md.
    super::create_new_with(agents_md, |file| {
        file.write_all(&content)?;
        file.set_permissions(permissions)
    })
    .with_context(|| format!("create {}", agents_md.display()))?;
    fs::remove_file(claude_md)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("AGENTS.md", claude_md)?;
    Ok(true)
}

fn ensure_symlink(
    link_path: &Path,
    target_rel_str: &str,
    on_the_way: &[PathBuf],
    created_flag: &mut bool,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(link_path) else {
        // `symlink` does not follow a link where it creates one.
        #[cfg(unix)]
        std::os::unix::fs::symlink(target_rel_str, link_path)?;
        *created_flag = true;
        return Ok(());
    };

    // A link or file a linked AGENTS.md passes through already leads to
    // what AGENTS.md says, and is left as it is.
    if on_the_way.contains(&resolved(link_path)) {
        return Ok(());
    }

    if meta.file_type().is_symlink() {
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

    // A regular file is replaced only when its content is identical, read
    // from both, and both are regular files.
    let target_path = link_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(target_rel_str);
    if meta.is_file() {
        match (
            super::read_regular(link_path, false),
            super::read_regular(&target_path, true),
        ) {
            (Ok(link_content), Ok(target_content)) if link_content == target_content => {
                fs::remove_file(link_path)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(target_rel_str, link_path)?;
                *created_flag = true;
                return Ok(());
            }
            (Ok(_), Ok(_)) => {}
            (Err(e), _) | (_, Err(e)) => {
                warnings.push(format!(
                    "{} is not a link and cannot be compared with {} ({e}). Leaving it in place to prevent data loss.",
                    link_path.display(),
                    target_rel_str
                ));
                return Ok(());
            }
        }
    }
    warnings.push(format!(
        "{} is not a link and differs from {}. Leaving it in place to prevent data loss.",
        link_path.display(),
        target_rel_str
    ));
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
    fn claude_md_is_never_copied_through_a_link() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("CLAUDE.md"), "# from claude").unwrap();
        let escape = outside.path().join("escaped.md");
        std::os::unix::fs::symlink(&escape, root.join("AGENTS.md")).unwrap();

        for _ in 0..2 {
            let r = sync_workspace_rules(root).unwrap();
            assert!(!escape.exists());
            assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
            assert!(r.warnings[0].contains("leads nowhere"));
            assert!(!root.join("CLAUDE.md").is_symlink());
            assert_eq!(
                fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
                "# from claude"
            );
            assert!(!root.join("GEMINI.md").exists());
        }
    }

    #[test]
    fn the_file_a_linked_agents_md_leads_to_stays() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The other way round: AGENTS.md -> CLAUDE.md.
        fs::write(root.join("CLAUDE.md"), "# rules").unwrap();
        std::os::unix::fs::symlink("CLAUDE.md", root.join("AGENTS.md")).unwrap();

        let r = sync_workspace_rules(root).unwrap();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(!root.join("CLAUDE.md").is_symlink());
        assert_eq!(
            fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
            "# rules"
        );
        assert_eq!(
            fs::read_to_string(root.join("GEMINI.md")).unwrap(),
            "# rules"
        );
    }

    #[test]
    fn a_link_a_linked_agents_md_passes_through_stays() {
        for via in ["CLAUDE.md", "GEMINI.md"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            fs::create_dir(root.join("docs")).unwrap();
            fs::write(root.join("docs/rules.md"), "# rules").unwrap();
            std::os::unix::fs::symlink("docs/rules.md", root.join(via)).unwrap();
            std::os::unix::fs::symlink(via, root.join("AGENTS.md")).unwrap();

            // Pointing it at AGENTS.md made a loop that was never undone.
            for _ in 0..2 {
                let r = sync_workspace_rules(root).unwrap();
                assert!(r.warnings.is_empty(), "{:?}", r.warnings);
                assert_eq!(
                    link_target(&root.join(via)).unwrap(),
                    Path::new("docs/rules.md")
                );
                for file in ["AGENTS.md", "CLAUDE.md", "GEMINI.md"] {
                    assert_eq!(fs::read_to_string(root.join(file)).unwrap(), "# rules");
                }
            }
        }
    }

    #[test]
    fn only_a_regular_file_of_bounded_size_is_read() {
        use crate::sync::testing::{mkfifo, within};
        // A FIFO, whose read waits for a writer, and a device without end.
        for agents in [None, Some("/dev/zero")] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            match agents {
                None => mkfifo(&root.join("AGENTS.md")),
                Some(to) => std::os::unix::fs::symlink(to, root.join("AGENTS.md")).unwrap(),
            }
            fs::write(root.join("CLAUDE.md"), "# mine").unwrap();
            let r = within(move || sync_workspace_rules(&root).unwrap());
            assert!(
                r.warnings.iter().any(|w| w.contains("not a regular file")),
                "{:?}",
                r.warnings
            );
            assert_eq!(
                fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
                "# mine"
            );
        }

        // Identical, but too large to be read.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let large = vec![b'x'; (super::super::READ_LIMIT + 1) as usize];
        fs::write(root.join("AGENTS.md"), &large).unwrap();
        fs::write(root.join("CLAUDE.md"), &large).unwrap();
        let r = sync_workspace_rules(root).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("larger than")),
            "{:?}",
            r.warnings
        );
        assert!(!root.join("CLAUDE.md").is_symlink());

        // A FIFO CLAUDE.md is not promoted or read.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        mkfifo(&root.join("CLAUDE.md"));
        let r = within(move || sync_workspace_rules(&root).unwrap());
        assert!(r.agents_md_path.is_none());
    }

    #[test]
    fn what_cannot_be_read_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // An empty AGENTS.md matched anything that could not be read.
        fs::write(root.join("AGENTS.md"), "").unwrap();
        fs::create_dir(root.join("GEMINI.md")).unwrap();
        let claude_md = root.join("CLAUDE.md");
        fs::write(&claude_md, "# mine").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&claude_md, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = fs::read(&claude_md).is_err();

        let r = sync_workspace_rules(root).unwrap();
        fs::set_permissions(&claude_md, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(root.join("GEMINI.md").is_dir());
        assert!(r.warnings.iter().any(|w| w.contains("GEMINI.md")));
        // Root reads it anyway.
        if unreadable {
            assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
            assert_eq!(fs::read_to_string(&claude_md).unwrap(), "# mine");
        }
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
