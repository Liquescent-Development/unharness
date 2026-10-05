//! The extension that makes pi ask before a tool acts.
//!
//! pi runs its tools without asking. `gate.ts`, loaded with `-e`, puts every
//! call that is not a read to the client as a select dialog; this module
//! installs the file and reads the dialog back into a tool request.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::core::ToolAction;

pub const SOURCE: &str = include_str!("gate.ts");

/// What the gate's dialog titles start with; the rest is the call as JSON.
const TITLE_PREFIX: &str = "unharness-gate:";
pub const ALLOW: &str = "Allow";
pub const DENY: &str = "Deny";

/// Where the extension is kept: under unharness's state directory, which an
/// agent cannot write. There is no fallback to a directory one could.
pub fn default_dir() -> Result<PathBuf> {
    let state = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("no state directory to keep pi's gate extension in")?;
    Ok(state.join("unharness").join("pi"))
}

/// Put the extension in `dir`, replacing a copy that differs, and return
/// its path.
pub fn install(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("gate.ts");
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == SOURCE) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Renamed into place: a running pi never reads half a file.
    let partial = dir.join(format!("gate-{}.partial", uuid::Uuid::new_v4()));
    std::fs::write(&partial, SOURCE)
        .and_then(|_| std::fs::rename(&partial, &path))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// A tool call the gate asks about.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedCall {
    pub tool_call_id: String,
    pub tool: String,
    pub input: Value,
}

/// The call behind a select dialog, if the dialog is the gate's.
pub fn parse_request(title: &str, options: &[String]) -> Option<GatedCall> {
    if options != [ALLOW, DENY] {
        return None;
    }
    let call: Value = serde_json::from_str(title.strip_prefix(TITLE_PREFIX)?).ok()?;
    let text = |key: &str| {
        call.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    Some(GatedCall {
        tool_call_id: text("toolCallId")?.to_string(),
        tool: text("toolName")?.to_string(),
        input: call.get("input").cloned().unwrap_or(Value::Null),
    })
}

/// What one of pi's tools does, for allow rules. Input fields are those of
/// pi 0.87.1; an input with any other field is not one a rule was written
/// for.
pub fn tool_action(tool: &str, input: &Value) -> ToolAction {
    let only = |known: &[&str]| {
        input
            .as_object()
            .is_some_and(|o| o.keys().all(|k| known.contains(&k.as_str())))
    };
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    match tool {
        "bash" => match text("command") {
            Some(command) if only(&["command", "timeout"]) => ToolAction::Shell {
                command: command.to_string(),
                cwd: None,
            },
            _ => ToolAction::Opaque,
        },
        "write" | "edit" => match text("path") {
            Some(path)
                if only(&["path", "content", "edits", "oldText", "newText"])
                    && path_is_literal(path) =>
            {
                ToolAction::Edit {
                    paths: vec![path.into()],
                }
            }
            _ => ToolAction::Opaque,
        },
        // A tool of some extension, known by its name alone.
        _ => ToolAction::Other,
    }
}

/// Whether pi uses `path` as written. Before it touches a file pi 0.87.1
/// turns Unicode spaces into ASCII ones, strips a leading `@`, expands `~`
/// and reads `file://` URLs: such a path names another file than the one a
/// rule would be matched against.
fn path_is_literal(path: &str) -> bool {
    !(path.starts_with('@')
        || path.starts_with('~')
        || path.starts_with("file:")
        || path.chars().any(|c| c.is_whitespace() && c != ' '))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options() -> Vec<String> {
        vec![ALLOW.to_string(), DENY.to_string()]
    }

    #[test]
    fn install_writes_once_and_repairs() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("state/pi");
        let path = install(&nested).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);
        std::fs::write(&path, "tampered").unwrap();
        assert_eq!(install(&nested).unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);
        assert_eq!(std::fs::read_dir(&nested).unwrap().count(), 1);
    }

    #[test]
    fn the_extension_and_this_module_agree() {
        assert!(SOURCE.contains(&format!("const PREFIX = \"{TITLE_PREFIX}\";")));
        assert!(SOURCE.contains(&format!("[\"{ALLOW}\", \"{DENY}\"]")));
    }

    #[test]
    fn only_the_gates_own_dialog_is_a_tool_request() {
        let title =
            r#"unharness-gate:{"toolCallId":"c1","toolName":"bash","input":{"command":"ls"}}"#;
        assert_eq!(
            parse_request(title, &options()),
            Some(GatedCall {
                tool_call_id: "c1".into(),
                tool: "bash".into(),
                input: json!({"command": "ls"}),
            })
        );
        assert_eq!(parse_request(title, &["Yes".into(), "No".into()]), None);
        assert_eq!(parse_request("Allow bash?", &options()), None);
        assert_eq!(parse_request("unharness-gate:{", &options()), None);
        assert_eq!(
            parse_request(r#"unharness-gate:{"toolName":"bash"}"#, &options()),
            None
        );
    }

    #[test]
    fn tool_actions() {
        assert_eq!(
            tool_action("bash", &json!({"command": "cargo test", "timeout": 5})),
            ToolAction::Shell {
                command: "cargo test".into(),
                cwd: None
            }
        );
        // A field no recording had.
        assert_eq!(
            tool_action("bash", &json!({"command": "ls", "cwd": "/"})),
            ToolAction::Opaque
        );
        assert_eq!(tool_action("bash", &json!({})), ToolAction::Opaque);
        let edit = ToolAction::Edit {
            paths: vec!["/w/a.rs".into()],
        };
        assert_eq!(
            tool_action("write", &json!({"path": "/w/a.rs", "content": "x"})),
            edit
        );
        // Both shapes pi 0.87.1 takes for an edit.
        assert_eq!(
            tool_action(
                "edit",
                &json!({"path": "/w/a.rs", "edits": [{"oldText": "a", "newText": "b"}]})
            ),
            edit
        );
        assert_eq!(
            tool_action(
                "edit",
                &json!({"path": "/w/a.rs", "oldText": "a", "newText": "b"})
            ),
            edit
        );
        assert_eq!(
            tool_action("write", &json!({"content": "x"})),
            ToolAction::Opaque
        );
        assert_eq!(tool_action("deploy", &json!({})), ToolAction::Other);
    }

    #[test]
    fn a_path_pi_rewrites_is_not_one_to_match_rules_on() {
        for path in [
            "/w/docs/a\u{a0}b/x",
            "/w/a\u{3000}b",
            "/w/a\tb",
            "@/w/a.rs",
            "~/a.rs",
            "file:///w/a.rs",
        ] {
            assert_eq!(
                tool_action("write", &json!({"path": path, "content": "x"})),
                ToolAction::Opaque,
                "{path:?}"
            );
        }
        assert_eq!(
            tool_action("write", &json!({"path": "/w/a b.rs", "content": "x"})),
            ToolAction::Edit {
                paths: vec!["/w/a b.rs".into()]
            }
        );
    }
}
