//! Fixture replay helpers for protocol parser tests.
//!
//! A fixture is `fixtures/<case>.jsonl` (raw lines as the harness emitted
//! them; lines starting with `>>` are what we *sent* and are skipped, lines
//! starting with `#` are comments) paired with `fixtures/<case>.events`, one
//! `AgentEvent::summary()` line per expected event.

#![cfg(test)]

use std::path::Path;

use super::event::AgentEvent;

/// Anything that turns one raw stdout line into zero or more events.
pub trait LineParser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent>;
    /// Called for stderr lines; most parsers map these to `Notice`/`Error`.
    fn feed_stderr(&mut self, _line: &str) -> Vec<AgentEvent> {
        Vec::new()
    }
}

/// Lines in a fixture that are protocol input rather than output.
pub fn is_sent_line(line: &str) -> bool {
    line.starts_with(">>")
}

pub fn replay<P: LineParser>(parser: &mut P, fixture: &str) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    for raw in fixture.lines() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') || is_sent_line(line) {
            continue;
        }
        if let Some(err) = line.strip_prefix("!!") {
            events.extend(parser.feed_stderr(err.trim_start()));
        } else {
            events.extend(parser.feed(line));
        }
    }
    events
}

/// Replay `<dir>/<case>.jsonl` and compare against `<dir>/<case>.events`.
/// Panics with a line diff on mismatch. Set `UNHARNESS_UPDATE_FIXTURES=1` to
/// rewrite the `.events` file from the current parser output instead.
pub fn assert_fixture<P: LineParser>(parser: &mut P, dir: &Path, case: &str) {
    let jsonl_path = dir.join(format!("{case}.jsonl"));
    let events_path = dir.join(format!("{case}.events"));
    let fixture = std::fs::read_to_string(&jsonl_path)
        .unwrap_or_else(|e| panic!("read {}: {}", jsonl_path.display(), e));
    let actual: Vec<String> = replay(parser, &fixture)
        .iter()
        .map(AgentEvent::summary)
        .collect();

    if std::env::var("UNHARNESS_UPDATE_FIXTURES").is_ok() {
        std::fs::write(&events_path, actual.join("\n") + "\n").unwrap();
        return;
    }

    let expected_text = std::fs::read_to_string(&events_path).unwrap_or_else(|e| {
        panic!(
            "read {}: {} (set UNHARNESS_UPDATE_FIXTURES=1 to create)",
            events_path.display(),
            e
        )
    });
    let expected: Vec<&str> = expected_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();

    if expected.iter().map(|s| s.to_string()).collect::<Vec<_>>() != actual {
        let mut diff = String::new();
        let n = expected.len().max(actual.len());
        for i in 0..n {
            let e = expected.get(i).copied().unwrap_or("<none>");
            let a = actual.get(i).map(String::as_str).unwrap_or("<none>");
            if e != a {
                diff.push_str(&format!(
                    "line {}:\n  expected: {}\n  actual:   {}\n",
                    i + 1,
                    e,
                    a
                ));
            }
        }
        panic!(
            "fixture {} mismatch:\n{}\n(set UNHARNESS_UPDATE_FIXTURES=1 to accept)",
            case, diff
        );
    }
}

/// Directory of a harness module's fixtures, given the module's `file!()`.
pub fn fixtures_dir(module_file: &str) -> std::path::PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let module = Path::new(module_file);
    let parent = module.parent().unwrap_or(Path::new(""));
    manifest.join(parent).join("fixtures")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl LineParser for Echo {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            vec![AgentEvent::TextDelta(line.to_string())]
        }
        fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
            vec![AgentEvent::Error(line.to_string())]
        }
    }

    #[test]
    fn replay_skips_comments_and_sent_lines() {
        let fx = "# comment\n>> {\"sent\":1}\nhello\n\n!! bad thing\nworld\r\n";
        let ev = replay(&mut Echo, fx);
        assert_eq!(
            ev,
            vec![
                AgentEvent::TextDelta("hello".into()),
                AgentEvent::Error("bad thing".into()),
                AgentEvent::TextDelta("world".into()),
            ]
        );
    }
}
