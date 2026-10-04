//! Editing the prompt in the user's own editor (`$VISUAL`, else `$EDITOR`).

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// The editor command line from the environment, if one is set.
pub fn command() -> Option<String> {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|v| !v.trim().is_empty())
}

/// Run `editor` on a temporary file in `dir` holding `text` and return what
/// it saved, or `None` if it exited with an error. `editor` goes through the
/// shell, as git does, so it may carry arguments (`code -w`). The file is
/// private to the user and removed afterwards. Blocks until the editor exits;
/// the caller hands it the terminal first.
pub fn edit(editor: &str, text: &str, dir: &Path) -> Result<Option<String>> {
    let path = dir.join(format!("unharness-prompt-{}.md", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        drop(file);
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&path)
            .status()
            .with_context(|| format!("run {editor}"))?;
        if !status.success() {
            return Ok(None);
        }
        let edited =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        // Editors end the file with a newline the prompt does not want.
        Ok(Some(edited.trim_end_matches(['\n', '\r']).to_string()))
    })();
    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn leftovers(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn returns_what_the_editor_saved_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        // An "editor" with arguments of its own that appends two lines.
        let editor = r#"sh -c 'printf "second\nthird\n" >> "$0"'"#;
        let edited = edit(editor, "first\n", tmp.path()).unwrap();
        assert_eq!(edited.as_deref(), Some("first\nsecond\nthird"));
        assert_eq!(leftovers(tmp.path()), 0);
    }

    #[test]
    fn a_failing_editor_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(edit("false", "keep me", tmp.path()).unwrap(), None);
        assert_eq!(
            edit("unharness-no-such-editor", "keep me", tmp.path()).unwrap(),
            None
        );
        assert_eq!(leftovers(tmp.path()), 0);
        assert!(edit("true", "x", &tmp.path().join("missing")).is_err());
    }
}
