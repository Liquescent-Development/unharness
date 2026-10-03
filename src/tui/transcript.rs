//! The conversation as rendered blocks, plus the text bridged into another
//! harness when the user switches mid-conversation.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::code::sanitize;
use crate::core::conversations::BlockRecord;

#[derive(Debug, Clone)]
pub enum Block {
    User {
        text: String,
    },
    Assistant {
        text: String,
        sender: String,
        duration: Option<Duration>,
    },
    Thought {
        text: String,
        duration: Option<Duration>,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        output: String,
        is_error: bool,
        done: bool,
        collapsed: bool,
        started: Instant,
        duration: Option<Duration>,
    },
    System(String),
    Notice(String),
    Error(String),
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub blocks: Vec<Block>,
    thought_start: Option<Instant>,
}

/// Default cap on bridged transcript text.
pub const DEFAULT_BRIDGE_MAX_CHARS: usize = 24_000;

impl Transcript {
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::User {
            text: sanitize(&text.into()),
        });
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::System(sanitize(&text.into())));
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Notice(sanitize(&text.into())));
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Error(sanitize(&text.into())));
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.thought_start = None;
    }

    /// Serializable form for conversation persistence.
    pub fn to_records(&self) -> Vec<BlockRecord> {
        self.blocks
            .iter()
            .map(|b| match b {
                Block::User { text } => BlockRecord::User { text: text.clone() },
                Block::Assistant {
                    text,
                    sender,
                    duration,
                } => BlockRecord::Assistant {
                    text: text.clone(),
                    sender: sender.clone(),
                    secs: duration.map(|d| d.as_secs_f32()),
                },
                Block::Thought { text, duration } => BlockRecord::Thought {
                    text: text.clone(),
                    secs: duration.map(|d| d.as_secs_f32()),
                },
                Block::Tool {
                    id,
                    name,
                    input,
                    output,
                    is_error,
                    ..
                } => BlockRecord::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    output: output.clone(),
                    is_error: *is_error,
                },
                Block::System(t) => BlockRecord::System { text: t.clone() },
                Block::Notice(t) => BlockRecord::Notice { text: t.clone() },
                Block::Error(t) => BlockRecord::Error { text: t.clone() },
            })
            .collect()
    }

    /// Rebuild from persisted records; everything comes back finished and
    /// collapsed.
    pub fn from_records(records: &[BlockRecord]) -> Self {
        let blocks = records
            .iter()
            .map(|r| match r {
                BlockRecord::User { text } => Block::User { text: text.clone() },
                BlockRecord::Assistant { text, sender, secs } => Block::Assistant {
                    text: text.clone(),
                    sender: sender.clone(),
                    duration: Some(Duration::from_secs_f32(secs.unwrap_or(0.0))),
                },
                BlockRecord::Thought { text, secs } => Block::Thought {
                    text: text.clone(),
                    duration: Some(Duration::from_secs_f32(secs.unwrap_or(0.0))),
                },
                BlockRecord::Tool {
                    id,
                    name,
                    input,
                    output,
                    is_error,
                } => Block::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    output: output.clone(),
                    is_error: *is_error,
                    done: true,
                    collapsed: true,
                    started: Instant::now(),
                    duration: Some(Duration::ZERO),
                },
                BlockRecord::System { text } => Block::System(text.clone()),
                BlockRecord::Notice { text } => Block::Notice(text.clone()),
                BlockRecord::Error { text } => Block::Error(text.clone()),
            })
            .collect();
        Transcript {
            blocks,
            thought_start: None,
        }
    }

    pub fn append_assistant(&mut self, sender: &str, delta: &str) {
        let delta = &sanitize(delta);
        self.close_thought();
        if let Some(Block::Assistant {
            text,
            sender: s,
            duration: None,
        }) = self.blocks.last_mut()
            && s == sender
        {
            text.push_str(delta);
            return;
        }
        self.blocks.push(Block::Assistant {
            text: delta.to_string(),
            sender: sender.to_string(),
            duration: None,
        });
    }

    pub fn append_thought(&mut self, delta: &str) {
        let delta = &sanitize(delta);
        if let Some(Block::Thought {
            text,
            duration: None,
        }) = self.blocks.last_mut()
        {
            text.push_str(delta);
            return;
        }
        self.thought_start = Some(Instant::now());
        self.blocks.push(Block::Thought {
            text: delta.to_string(),
            duration: None,
        });
    }

    pub fn is_thinking(&self) -> bool {
        self.thought_start.is_some()
    }

    pub fn thought_elapsed(&self) -> Option<Duration> {
        self.thought_start.map(|s| s.elapsed())
    }

    fn close_thought(&mut self) {
        if let Some(start) = self.thought_start.take()
            && let Some(Block::Thought { duration, .. }) = self.blocks.last_mut()
        {
            *duration = Some(start.elapsed());
        }
    }

    pub fn tool_started(&mut self, id: &str, name: &str, input: Value) {
        self.close_thought();
        self.blocks.push(Block::Tool {
            id: id.to_string(),
            name: name.to_string(),
            input,
            output: String::new(),
            is_error: false,
            done: false,
            collapsed: true,
            started: Instant::now(),
            duration: None,
        });
    }

    fn find_tool(&mut self, id: &str) -> Option<&mut Block> {
        self.blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, Block::Tool { id: tid, .. } if tid == id))
    }

    /// Live output (or streaming args before the call is announced; those are
    /// ignored because `tool_started` carries the final input).
    pub fn tool_delta(&mut self, id: &str, delta: &str) {
        let delta = &sanitize(delta);
        if let Some(Block::Tool { output, done, .. }) = self.find_tool(id)
            && !*done
        {
            output.push_str(delta);
        }
    }

    pub fn tool_result(&mut self, id: &str, result: &str, is_error: bool) {
        let result = &sanitize(result);
        match self.find_tool(id) {
            Some(Block::Tool {
                output,
                is_error: err,
                done,
                started,
                duration,
                ..
            }) => {
                if !result.is_empty() {
                    *output = result.to_string();
                }
                *err = is_error;
                *done = true;
                *duration = Some(started.elapsed());
            }
            _ => {
                // Result for a call we never saw announced (e.g. codex exec).
                self.blocks.push(Block::Tool {
                    id: id.to_string(),
                    name: "tool".to_string(),
                    input: Value::Null,
                    output: result.to_string(),
                    is_error,
                    done: true,
                    collapsed: true,
                    started: Instant::now(),
                    duration: Some(Duration::ZERO),
                });
            }
        }
    }

    /// Called when a turn ends: stamps the duration on the last assistant
    /// block and closes any open thought.
    pub fn finish_turn(&mut self, duration: Duration) {
        self.close_thought();
        if let Some(Block::Assistant {
            duration: d @ None, ..
        }) = self.blocks.last_mut()
        {
            *d = Some(duration);
        }
    }

    pub fn toggle_last_tool(&mut self) -> bool {
        if let Some(Block::Tool { collapsed, .. }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, Block::Tool { .. }))
        {
            *collapsed = !*collapsed;
            return true;
        }
        false
    }

    pub fn toggle_all_tools(&mut self) {
        let any_collapsed = self.blocks.iter().any(|b| {
            matches!(
                b,
                Block::Tool {
                    collapsed: true,
                    ..
                }
            )
        });
        for b in &mut self.blocks {
            if let Block::Tool { collapsed, .. } = b {
                *collapsed = !any_collapsed;
            }
        }
    }

    /// Conversation text from block `from` onward, for seeding another
    /// harness. Keeps the tail when over `max_chars`.
    pub fn bridge_text(&self, from: usize, max_chars: usize) -> Option<String> {
        let mut chunks: Vec<String> = Vec::new();
        for b in self.blocks.iter().skip(from) {
            match b {
                Block::User { text } => chunks.push(format!("User: {text}")),
                Block::Assistant { text, sender, .. } => chunks.push(format!("{sender}: {text}")),
                Block::Tool {
                    name,
                    input,
                    output,
                    is_error,
                    ..
                } => {
                    let status = if *is_error { "error" } else { "ok" };
                    let out = first_lines(output, 3);
                    chunks.push(format!(
                        "[tool {} {} → {}] {}",
                        name,
                        tool_summary(name, input),
                        status,
                        out
                    ));
                }
                _ => {}
            }
        }
        if chunks.is_empty() {
            return None;
        }
        let mut total: usize = chunks.iter().map(|c| c.chars().count() + 2).sum();
        let mut omitted = 0usize;
        while total > max_chars && chunks.len() > 1 {
            let removed = chunks.remove(0);
            total -= removed.chars().count() + 2;
            omitted += 1;
        }
        if let Some(last) = chunks.last_mut()
            && last.chars().count() > max_chars
        {
            let keep: String = last
                .chars()
                .rev()
                .take(max_chars)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            *last = format!("…{keep}");
        }
        let mut text = String::new();
        if omitted > 0 {
            text.push_str(&format!("[… {omitted} earlier messages omitted]\n\n"));
        }
        text.push_str(&chunks.join("\n\n"));
        Some(text)
    }
}

/// One-line description of a tool call for headers and bridges (truncated).
pub fn tool_summary(name: &str, input: &Value) -> String {
    truncate_chars(&tool_summary_full(name, input), 60)
}

/// Untruncated single-line description of a tool call.
pub fn tool_summary_full(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .map(|s| s.lines().next().unwrap_or("").to_string())
    };
    let s = match name {
        "Bash" | "bash" | "shell" | "command_execution" => pick(&["command", "cmd"]),
        "Read" | "Write" | "Edit" | "MultiEdit" | "read" | "write" | "edit" => {
            pick(&["file_path", "path", "filename"])
        }
        "Glob" | "Grep" | "glob" | "grep" => pick(&["pattern", "query"]),
        "apply_patch" | "file_change" => input
            .get("changes")
            .and_then(Value::as_array)
            .map(|c| format!("{} file(s)", c.len()))
            .or_else(|| pick(&["path"])),
        "WebFetch" | "WebSearch" | "web_search" => pick(&["url", "query"]),
        "Agent" | "Task" => pick(&["description", "prompt"]),
        _ => pick(&[
            "command",
            "file_path",
            "path",
            "query",
            "description",
            "prompt",
        ]),
    };
    let s = s.unwrap_or_else(|| match input {
        Value::Null => String::new(),
        v => v.to_string(),
    });
    sanitize(&s).replace('\n', " ⏎ ")
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

fn first_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().take(n).collect();
    let mut out = lines.join(" / ");
    if s.lines().count() > n {
        out.push_str(" …");
    }
    truncate_chars(&out, 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_deltas_merge_and_turn_stamps_duration() {
        let mut t = Transcript::default();
        t.push_user("hi");
        t.append_thought("hmm");
        assert!(t.is_thinking());
        t.append_assistant("Claude", "he");
        t.append_assistant("Claude", "llo");
        assert!(!t.is_thinking());
        t.finish_turn(Duration::from_secs(1));
        assert_eq!(t.blocks.len(), 3);
        assert!(matches!(
            &t.blocks[1],
            Block::Thought {
                duration: Some(_),
                ..
            }
        ));
        assert!(
            matches!(&t.blocks[2], Block::Assistant { text, duration: Some(_), .. } if text == "hello")
        );
        // A new delta after the turn ended starts a fresh block.
        t.append_assistant("Claude", "again");
        assert_eq!(t.blocks.len(), 4);
    }

    #[test]
    fn tool_lifecycle() {
        let mut t = Transcript::default();
        t.tool_started("t1", "Bash", json!({"command":"ls -la"}));
        t.tool_delta("t1", "a\n");
        t.tool_delta("t1", "b\n");
        t.tool_result("t1", "a\nb\nc\n", false);
        match &t.blocks[0] {
            Block::Tool {
                output,
                done,
                duration,
                ..
            } => {
                assert_eq!(output, "a\nb\nc\n");
                assert!(*done && duration.is_some());
            }
            other => panic!("{other:?}"),
        }
        // Unknown result creates a block.
        t.tool_result("zz", "out", true);
        assert_eq!(t.blocks.len(), 2);
        assert!(t.toggle_last_tool());
        assert!(matches!(
            &t.blocks[1],
            Block::Tool {
                collapsed: false,
                ..
            }
        ));
    }

    #[test]
    fn bridge_text_caps_and_marks_omissions() {
        let mut t = Transcript::default();
        for i in 0..10 {
            t.push_user(format!("question {i} {}", "x".repeat(100)));
            t.append_assistant("A", &format!("answer {i}"));
            t.finish_turn(Duration::ZERO);
        }
        let full = t.bridge_text(0, 1_000_000).unwrap();
        assert!(full.starts_with("User: question 0"));
        let capped = t.bridge_text(0, 400).unwrap();
        assert!(capped.starts_with("[… "));
        assert!(capped.contains("answer 9"));
        assert!(!capped.contains("question 0"));
        assert!(t.bridge_text(t.blocks.len(), 1000).is_none());
        let from_mid = t.bridge_text(18, 1000).unwrap();
        assert!(from_mid.starts_with("User: question 9"));
    }

    #[test]
    fn records_roundtrip() {
        let mut t = Transcript::default();
        t.push_user("q");
        t.append_thought("think");
        t.tool_started("t1", "Bash", json!({"command":"ls"}));
        t.tool_result("t1", "a\nb", false);
        t.append_assistant("Claude", "answer");
        t.finish_turn(Duration::from_millis(1500));
        t.push_notice("n");
        let records = t.to_records();
        assert_eq!(records.len(), 5);
        let back = Transcript::from_records(&records);
        assert_eq!(back.blocks.len(), 5);
        assert!(
            matches!(&back.blocks[2], Block::Tool { output, done: true, .. } if output == "a\nb")
        );
        assert!(
            matches!(&back.blocks[3], Block::Assistant { text, duration: Some(d), .. } if text == "answer" && d.as_millis() == 1500)
        );
        assert_eq!(back.to_records(), records);
        assert!(!back.is_thinking());
    }

    #[test]
    fn tool_summaries() {
        assert_eq!(
            tool_summary("Bash", &json!({"command":"cargo test\nmore"})),
            "cargo test"
        );
        assert_eq!(
            tool_summary("Edit", &json!({"file_path":"/a/b.rs"})),
            "/a/b.rs"
        );
        assert_eq!(
            tool_summary("apply_patch", &json!({"changes":[1,2]})),
            "2 file(s)"
        );
        assert_eq!(tool_summary("Weird", &json!({"zzz":1})), "{\"zzz\":1}");
        assert_eq!(tool_summary("Weird", &Value::Null), "");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }
}
