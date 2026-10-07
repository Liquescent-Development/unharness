//! What agy keeps of a conversation under `~/.gemini/antigravity-cli/brain/
//! <conversation>/.system_generated/` (agy 1.3.1), read for what its stream
//! leaves out about subagents. A subagent reports with `send_message`, which
//! agy files in its parent's `messages/<id>.json` (`sender`, `recipient`,
//! `timestamp`, `content`) and delivers as a `system_message` step that
//! carries none of them. A subagent that never reports ends in its own
//! `logs/transcript.jsonl`, one step a line: an `ERROR` step when a call of
//! its was refused.
//!
//! The directory is writable from the session's sandbox, so what is read
//! here is shown and never acted on, only from a sender the parser started,
//! and within bounds: a directory on the way that is a link is not entered
//! (checked before the read, so a link swapped in meanwhile is followed
//! once), a file that is not a regular one or is large is skipped
//! (`sync::open_regular`: a FIFO cannot hang the transport), and one call
//! looks at a bounded number of entries and bytes.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// A message is a report of a few sentences; a transcript's last step is
/// read from at most this far before its end.
const READ_LIMIT: u64 = 1 << 20;
/// The most directory entries one call looks at.
const SCAN_LIMIT: usize = 4096;
/// The most bytes of messages one call reads; what is left is read by the
/// next.
const BUDGET: u64 = 16 << 20;

pub fn default_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini/antigravity-cli/brain"))
}

/// The brain directory a subagent's `log_uri` names
/// (`file://<brain>/<conversation>/.system_generated/logs/transcript.jsonl`),
/// if it names one in that shape for `conversation`.
pub fn dir_from_log_uri(uri: &str, conversation: &str) -> Option<PathBuf> {
    if !is_id(conversation) || uri.contains('%') {
        return None;
    }
    let suffix = format!("/{conversation}/.system_generated/logs/transcript.jsonl");
    let dir = Path::new(uri.strip_prefix("file://")?.strip_suffix(&suffix)?);
    let plain = dir
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    (dir.is_absolute() && plain && dir.file_name()? == "brain").then(|| dir.to_path_buf())
}

/// Conversation ids are UUIDs; anything else is not made into a path.
fn is_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A directory itself, not a link to one.
fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// `brain/<conversation>/.system_generated/<sub>`, when no directory on the
/// way from `brain` is a link.
fn generated(brain: &Path, conversation: &str, sub: &str) -> Option<PathBuf> {
    if !is_id(conversation) {
        return None;
    }
    let mut path = brain.to_path_buf();
    for part in [conversation, ".system_generated", sub] {
        if !real_dir(&path) {
            return None;
        }
        path.push(part);
    }
    real_dir(&path).then_some(path)
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

#[derive(Debug, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub sender: String,
    #[serde(default)]
    recipient: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    pub content: String,
}

impl Message {
    /// RFC 3339 in UTC with nanoseconds whose trailing zeros are left out
    /// (`…:34.5Z` is later than `…:34.456Z`): the fraction is padded.
    fn order(&self) -> (&str, String) {
        let t = self.timestamp.trim_end_matches('Z');
        let (seconds, fraction) = t.split_once('.').unwrap_or((t, ""));
        (seconds, format!("{fraction:0<9}"))
    }
}

/// The messages already looked at, by file name.
#[derive(Debug, Default)]
pub struct Inbox {
    read: HashSet<String>,
    /// Files that did not parse, by their length then: one is read again
    /// only once its length changed (agy still writing it).
    unreadable: HashMap<String, u64>,
}

/// The messages to `parent` from one of `senders` that `inbox` has not
/// had, oldest first. A message from anyone else is passed over for good.
pub fn new_messages(
    brain: &Path,
    parent: &str,
    senders: &[String],
    inbox: &mut Inbox,
) -> Vec<Message> {
    let Some(dir) = generated(brain, parent, "messages") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut budget = BUDGET;
    let mut messages = Vec::new();
    for entry in entries.take(SCAN_LIMIT).flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(id) = name.strip_suffix(".json").filter(|id| is_id(id)) else {
            continue;
        };
        if inbox.read.contains(id) {
            continue;
        }
        // Not followed: the length of a link is not the file's.
        let Ok(len) = entry.metadata().map(|m| m.len()) else {
            continue;
        };
        if inbox.unreadable.get(id) == Some(&len) || len > budget {
            continue;
        }
        budget -= len;
        let Some(message) =
            read_capped(&entry.path()).and_then(|b| serde_json::from_slice::<Message>(&b).ok())
        else {
            inbox.unreadable.insert(id.to_string(), len);
            continue;
        };
        inbox.unreadable.remove(id);
        inbox.read.insert(id.to_string());
        let to_parent = message.recipient.is_empty() || message.recipient == parent;
        if to_parent && senders.contains(&message.sender) {
            messages.push(message);
        }
    }
    messages.sort_by(|a, b| a.order().cmp(&b.order()));
    messages
}

/// The last step `conversation`'s transcript holds.
pub fn last_step(brain: &Path, conversation: &str) -> Option<serde_json::Value> {
    let path = generated(brain, conversation, "logs")?.join("transcript.jsonl");
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

    fn senders() -> Vec<String> {
        vec!["70c6".into(), "3e31".into()]
    }

    fn contents(got: &[Message]) -> Vec<&str> {
        got.iter().map(|m| m.content.as_str()).collect()
    }

    #[test]
    fn messages_come_once_and_oldest_first() {
        let brain = tempfile::tempdir().unwrap();
        let dir = messages_dir(brain.path());
        std::fs::write(
            dir.join("b06c478e.json"),
            r#"{"sender":"70c6","timestamp":"2026-10-07T20:21:34.546Z","content":"second"}"#,
        )
        .unwrap();
        // Trailing zeros left out: earlier, though it sorts later as text.
        std::fs::write(
            dir.join("5ec40763.json"),
            r#"{"sender":"3e31","timestamp":"2026-10-07T20:21:34.5Z","content":"first"}"#,
        )
        .unwrap();
        // Not a message.
        std::fs::write(dir.join("read.json"), r#"{"5ec40763":true}"#).unwrap();
        // From someone the parser did not start, or to another agent.
        std::fs::write(dir.join("aaaa.json"), r#"{"sender":"ffff","content":"no"}"#).unwrap();
        std::fs::write(
            dir.join("bbbb.json"),
            r#"{"sender":"70c6","recipient":"ffff","content":"no"}"#,
        )
        .unwrap();
        let mut inbox = Inbox::default();
        let got = new_messages(brain.path(), PARENT, &senders(), &mut inbox);
        assert_eq!(contents(&got), ["first", "second"]);
        assert!(new_messages(brain.path(), PARENT, &senders(), &mut inbox).is_empty());
        assert!(new_messages(brain.path(), "../x", &senders(), &mut inbox).is_empty());
    }

    #[test]
    fn a_message_that_did_not_parse_is_read_again_once_it_grows() {
        let brain = tempfile::tempdir().unwrap();
        let file = messages_dir(brain.path()).join("cccc.json");
        std::fs::write(&file, r#"{"sender":"70c6","#).unwrap();
        let mut inbox = Inbox::default();
        assert!(new_messages(brain.path(), PARENT, &senders(), &mut inbox).is_empty());
        assert_eq!(inbox.unreadable.get("cccc"), Some(&17));
        std::fs::write(&file, r#"{"sender":"70c6","content":"done"}"#).unwrap();
        let got = new_messages(brain.path(), PARENT, &senders(), &mut inbox);
        assert_eq!(contents(&got), ["done"]);
        assert!(inbox.unreadable.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_or_a_link_is_skipped() {
        let brain = tempfile::tempdir().unwrap();
        let dir = messages_dir(brain.path());
        crate::sync::testing::mkfifo(&dir.join("aaaa.json"));
        let outside = brain.path().join("outside.json");
        std::fs::write(&outside, r#"{"sender":"70c6","content":"no"}"#).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("bbbb.json")).unwrap();
        let got = crate::sync::testing::within(move || {
            new_messages(brain.path(), PARENT, &senders(), &mut Inbox::default())
        });
        assert!(got.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_directory_is_not_entered() {
        let brain = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("aaaa.json"),
            r#"{"sender":"70c6","content":"no"}"#,
        )
        .unwrap();
        std::fs::write(
            outside.path().join("transcript.jsonl"),
            "{\"status\":\"ERROR\"}\n",
        )
        .unwrap();
        let generated = brain.path().join(PARENT).join(".system_generated");
        std::fs::create_dir_all(&generated).unwrap();
        std::os::unix::fs::symlink(outside.path(), generated.join("messages")).unwrap();
        std::os::unix::fs::symlink(outside.path(), generated.join("logs")).unwrap();
        assert!(new_messages(brain.path(), PARENT, &senders(), &mut Inbox::default()).is_empty());
        assert!(last_step(brain.path(), PARENT).is_none());

        // Nor a conversation directory, nor the brain directory itself.
        let real = tempfile::tempdir().unwrap();
        let logs = real.path().join(PARENT).join(".system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("transcript.jsonl"), "{\"status\":\"ERROR\"}\n").unwrap();
        std::fs::write(
            messages_dir(real.path()).join("aaaa.json"),
            r#"{"sender":"70c6","content":"yes"}"#,
        )
        .unwrap();
        assert!(last_step(real.path(), PARENT).is_some());
        let got = new_messages(real.path(), PARENT, &senders(), &mut Inbox::default());
        assert_eq!(contents(&got), ["yes"]);
        let links = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(real.path().join(PARENT), links.path().join("eeee")).unwrap();
        assert!(last_step(links.path(), "eeee").is_none());
        let link = links.path().join("brain");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        assert!(last_step(&link, PARENT).is_none());
        assert!(new_messages(&link, PARENT, &senders(), &mut Inbox::default()).is_empty());
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

    #[test]
    fn the_brain_directory_from_a_log_uri() {
        let id = "07b35f72-9201";
        let uri = |dir: &str| format!("file://{dir}/{id}/.system_generated/logs/transcript.jsonl");
        assert_eq!(
            dir_from_log_uri(&uri("/Users/a/.gemini/antigravity-cli/brain"), id),
            Some(PathBuf::from("/Users/a/.gemini/antigravity-cli/brain"))
        );
        for bad in ["/a/b/../brain", "brain", "/a/notbrain", "/a/my%20dir/brain"] {
            assert_eq!(dir_from_log_uri(&uri(bad), id), None, "{bad}");
        }
        assert_eq!(dir_from_log_uri(&uri("/a/brain"), "other"), None);
        assert_eq!(dir_from_log_uri(&uri("/a/brain"), "../x"), None);
    }
}
