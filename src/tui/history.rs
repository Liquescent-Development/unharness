//! Prompts sent from this workspace, recalled with Up/Down in the prompt box.
//! Kept as one JSON string per line so a prompt can hold newlines.

use std::io::Write;
use std::path::PathBuf;

/// Most prompts kept, in memory and on disk.
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Default)]
pub struct PromptHistory {
    /// Oldest first.
    entries: Vec<String>,
    path: Option<PathBuf>,
    /// The entry on show while browsing.
    pos: Option<usize>,
    /// What the prompt held when browsing began; Down past the newest
    /// entry brings it back.
    draft: String,
}

impl PromptHistory {
    /// Read the history at `path`. A missing or damaged file is an empty
    /// history, not an error.
    pub fn load(path: PathBuf) -> Self {
        let mut entries: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if entries.len() > MAX_ENTRIES {
            entries.drain(..entries.len() - MAX_ENTRIES);
            // The file only ever grows by appends; trim it here.
            let text: String = entries.iter().map(|e| line_for(e)).collect();
            let _ = std::fs::write(&path, text);
        }
        PromptHistory {
            entries,
            path: Some(path),
            ..Default::default()
        }
    }

    /// Remember a sent prompt. Failing to save it is not worth interrupting
    /// the user for: the entry still works for this run.
    pub fn push(&mut self, text: &str) {
        self.reset();
        let text = text.trim();
        if text.is_empty() || self.entries.last().is_some_and(|e| e == text) {
            return;
        }
        self.entries.push(text.to_string());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = f.write_all(line_for(text).as_bytes());
            }
        }
    }

    /// Step to the next older entry. `current` is the prompt's text, kept
    /// as the draft when this starts a browse.
    pub fn older(&mut self, current: &str) -> Option<&str> {
        let pos = match self.pos {
            Some(0) => return None,
            Some(p) => p - 1,
            None => {
                let last = self.entries.len().checked_sub(1)?;
                self.draft = current.to_string();
                last
            }
        };
        self.pos = Some(pos);
        Some(&self.entries[pos])
    }

    /// Step to the next newer entry, or back to the draft after the newest.
    /// `None` when not browsing.
    pub fn newer(&mut self) -> Option<String> {
        let pos = self.pos? + 1;
        if pos < self.entries.len() {
            self.pos = Some(pos);
            Some(self.entries[pos].clone())
        } else {
            self.pos = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    /// Stop browsing; the next Up starts from the newest entry again.
    pub fn reset(&mut self) {
        self.pos = None;
        self.draft.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn line_for(entry: &str) -> String {
    format!("{}\n", serde_json::Value::String(entry.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_older_and_newer_and_return_to_the_draft() {
        let mut h = PromptHistory::default();
        assert_eq!(h.older("draft"), None);
        assert_eq!(h.newer(), None);
        h.push("one");
        h.push("  two  ");
        h.push("two");
        h.push("");
        assert_eq!(h.len(), 2);

        assert_eq!(h.older("draft"), Some("two"));
        assert_eq!(h.older("two"), Some("one"));
        assert_eq!(h.older("one"), None);
        assert_eq!(h.newer().as_deref(), Some("two"));
        assert_eq!(h.newer().as_deref(), Some("draft"));
        assert_eq!(h.newer(), None);
    }

    #[test]
    fn persists_multi_line_prompts_and_trims_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state").join("prompt_history.jsonl");
        let mut h = PromptHistory::load(path.clone());
        assert!(h.is_empty());
        h.push("first\nwith a second line");
        h.push("second");

        let mut again = PromptHistory::load(path.clone());
        assert_eq!(again.older(""), Some("second"));
        assert_eq!(again.older(""), Some("first\nwith a second line"));

        // A damaged line is skipped; an overlong file is cut to the newest.
        let mut text = String::from("not json\n");
        for i in 0..MAX_ENTRIES + 10 {
            text.push_str(&line_for(&format!("p{i}")));
        }
        std::fs::write(&path, text).unwrap();
        let mut h = PromptHistory::load(path.clone());
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(h.older(""), Some(format!("p{}", MAX_ENTRIES + 9).as_str()));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            MAX_ENTRIES
        );
    }
}
