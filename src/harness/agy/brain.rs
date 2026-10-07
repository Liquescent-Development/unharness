//! What agy keeps of a conversation under `~/.gemini/antigravity-cli/brain/
//! <conversation>/.system_generated/` (agy 1.3.1), read for what its stream
//! leaves out about subagents. A subagent reports with `send_message`, which
//! agy files in its parent's `messages/<id>.json` (`sender`, `content`) and
//! delivers as a `system_message` step that carries neither. A subagent that
//! never reports ends in its own `logs/transcript.jsonl`, one step a line:
//! an `ERROR` step when a call of its was refused.
//!
//! The directory is writable from the session's sandbox, so what is read
//! here is shown and never acted on, and a file that is not a regular one,
//! or a large one, is skipped (`sync::open_regular`: a FIFO cannot hang the
//! transport).

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A message is a report of a few sentences; a transcript's last step is
/// read from at most this far before its end.
const READ_LIMIT: u64 = 1 << 20;

pub fn default_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini/antigravity-cli/brain"))
}

/// Conversation ids are UUIDs; anything else is not made into a path.
fn is_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn generated(brain: &Path, conversation: &str) -> Option<PathBuf> {
    is_id(conversation).then(|| brain.join(conversation).join(".system_generated"))
}

fn read_capped(path: &Path) -> Option<Vec<u8>> {
    let file = crate::sync::open_regular(path, false).ok()?;
    if file.metadata().ok()?.len() > READ_LIMIT {
        return None;
    }
    let mut content = Vec::new();
    file.take(READ_LIMIT).read_to_end(&mut content).ok()?;
    Some(content)
}

/// The messages filed for `parent` that are not in `seen`, oldest first;
/// each is added to `seen`.
pub fn new_messages(brain: &Path, parent: &str, seen: &mut HashSet<String>) -> Vec<Value> {
    let Some(dir) = generated(brain, parent).map(|d| d.join("messages")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut messages: Vec<Value> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json").filter(|id| is_id(id))?;
            if seen.contains(id) {
                return None;
            }
            let message: Value = serde_json::from_slice(&read_capped(&entry.path())?).ok()?;
            seen.insert(id.to_string());
            Some(message)
        })
        .collect();
    messages.sort_by(|a, b| {
        let at = |m: &Value| {
            m.get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        at(a).cmp(&at(b))
    });
    messages
}

/// The last step `conversation`'s transcript holds.
pub fn last_step(brain: &Path, conversation: &str) -> Option<Value> {
    let path = generated(brain, conversation)?.join("logs/transcript.jsonl");
    let mut file = crate::sync::open_regular(&path, false).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(READ_LIMIT)))
        .ok()?;
    let mut tail = Vec::new();
    file.take(READ_LIMIT).read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str(l).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "d8a5df5b-cb3d-4358-b8ad-8d63750349a4";

    fn messages_dir(brain: &Path) -> PathBuf {
        let dir = brain.join(PARENT).join(".system_generated/messages");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn messages_come_once_and_oldest_first() {
        let brain = tempfile::tempdir().unwrap();
        let dir = messages_dir(brain.path());
        std::fs::write(
            dir.join("b06c478e.json"),
            r#"{"sender":"70c6","timestamp":"2026-10-07T20:21:49Z","content":"second"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("5ec40763.json"),
            r#"{"sender":"3e31","timestamp":"2026-10-07T20:21:34Z","content":"first"}"#,
        )
        .unwrap();
        // Not a message.
        std::fs::write(dir.join("read.json"), r#"{"5ec40763":true}"#).unwrap();
        let mut seen = HashSet::new();
        let got = new_messages(brain.path(), PARENT, &mut seen);
        let contents: Vec<_> = got.iter().map(|m| m["content"].as_str().unwrap()).collect();
        assert_eq!(contents, ["first", "second"]);
        assert!(new_messages(brain.path(), PARENT, &mut seen).is_empty());
        assert!(new_messages(brain.path(), "../x", &mut seen).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_or_a_link_is_skipped() {
        let brain = tempfile::tempdir().unwrap();
        let dir = messages_dir(brain.path());
        crate::sync::testing::mkfifo(&dir.join("aaaa.json"));
        let outside = brain.path().join("outside.json");
        std::fs::write(&outside, r#"{"sender":"x","content":"no"}"#).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("bbbb.json")).unwrap();
        let got = crate::sync::testing::within(move || {
            new_messages(brain.path(), PARENT, &mut HashSet::new())
        });
        assert!(got.is_empty());
    }

    #[test]
    fn the_last_step_is_the_last_line() {
        let brain = tempfile::tempdir().unwrap();
        let logs = brain.path().join("70c6-1").join(".system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("transcript.jsonl"),
            "{\"step_index\":0}\n{\"step_index\":4,\"status\":\"ERROR\"}\n\n",
        )
        .unwrap();
        let step = last_step(brain.path(), "70c6-1").unwrap();
        assert_eq!(step["status"], "ERROR");
        assert!(last_step(brain.path(), "ffff").is_none());
    }
}
