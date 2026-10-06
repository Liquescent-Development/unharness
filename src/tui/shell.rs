//! `!command` at the prompt: a command the user runs themselves, outside
//! the agent. It runs in the session's directory under the session's
//! sandbox, its output goes into the transcript, and the agent sees the
//! command and its output in front of the next prompt.

use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;
use tokio::process::Command;

use crate::core::conversations::ShellStatus;
use crate::core::process::LineProcess;
use crate::core::sandbox::Sandbox;

/// Output kept in the transcript: its tail, once it is longer.
pub const KEEP_BYTES: usize = 64 * 1024;
/// A line longer than this is cut.
pub const LINE_MAX_CHARS: usize = 2_000;
/// Output an agent is given, per command: its tail, once it is longer.
pub const CONTEXT_MAX_CHARS: usize = 8_000;

/// What a line typed at the prompt is.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed<'a> {
    /// `!command`: run it. The command is trimmed and may be empty.
    Shell(&'a str),
    /// A prompt for the agent, a leading `\!` taken as a plain `!`.
    Prompt(&'a str),
}

pub fn classify(text: &str) -> Typed<'_> {
    if text.starts_with("\\!") {
        Typed::Prompt(&text[1..])
    } else if let Some(command) = text.strip_prefix('!') {
        Typed::Shell(command.trim())
    } else {
        Typed::Prompt(text)
    }
}

/// The shell a command runs in: `$SHELL`, else `/bin/sh`.
pub fn user_shell(var: Option<OsString>) -> OsString {
    var.filter(|s| !s.is_empty())
        .unwrap_or_else(|| OsString::from("/bin/sh"))
}

/// Start `command` with `$SHELL -c` in `cwd`, confined by `sandbox`, with
/// no input and in a process group of its own.
pub fn spawn(command: &str, cwd: &Path, sandbox: &Sandbox) -> Result<LineProcess> {
    let mut cmd = Command::new(user_shell(std::env::var_os("SHELL")));
    cmd.arg("-c").arg(command).current_dir(cwd);
    LineProcess::spawn_group(cmd, sandbox)
}

/// Add a line of output, keeping `output` under `KEEP_BYTES` by dropping
/// whole lines from its front; `dropped` counts them.
pub fn push_line(output: &mut String, dropped: &mut usize, line: &str) {
    if line.chars().count() > LINE_MAX_CHARS {
        output.extend(line.chars().take(LINE_MAX_CHARS));
        output.push_str("…[cut]");
    } else {
        output.push_str(line);
    }
    output.push('\n');
    if output.len() > KEEP_BYTES {
        // Down to three quarters, so this does not happen on every line.
        let mut cut = 0;
        while output.len() - cut > KEEP_BYTES * 3 / 4 {
            match output[cut..].find('\n') {
                Some(i) => {
                    cut += i + 1;
                    *dropped += 1;
                }
                None => break,
            }
        }
        output.drain(..cut);
    }
}

/// The command as an agent is told about it: what was run, its output (the
/// tail, past `max_chars`) and how it ended.
pub fn context_entry(
    command: &str,
    output: &str,
    dropped: usize,
    status: &ShellStatus,
    max_chars: usize,
) -> String {
    let mut omitted = dropped;
    let mut shown = output;
    if output.chars().count() > max_chars {
        let start = output
            .char_indices()
            .rev()
            .nth(max_chars - 1)
            .map_or(0, |(i, _)| i);
        // From the start of a line.
        let start = output[start..].find('\n').map_or(start, |i| start + i + 1);
        omitted += output[..start].lines().count();
        shown = &output[start..];
    }
    let mut text = format!("$ {command}\n");
    if omitted > 0 {
        text.push_str(&format!("[… {omitted} earlier lines of output omitted]\n"));
    }
    text.push_str(shown);
    if !shown.is_empty() && !shown.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("[{}]", status.label()));
    text
}

/// What goes in front of a prompt: the commands run since the last one.
pub fn context(entries: &[String]) -> Option<String> {
    (!entries.is_empty()).then(|| {
        format!(
            "[Context: shell commands the user ran from the unharness prompt, with their output]\n{}",
            entries.join("\n\n")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::process::RawLine;

    #[test]
    fn a_bang_is_a_command_and_an_escaped_one_a_prompt() {
        assert_eq!(classify("!ls -la"), Typed::Shell("ls -la"));
        assert_eq!(classify("! ./build.sh  "), Typed::Shell("./build.sh"));
        assert_eq!(classify("!"), Typed::Shell(""));
        assert_eq!(classify("!a\nb"), Typed::Shell("a\nb"));
        assert_eq!(classify("\\!important"), Typed::Prompt("!important"));
        assert_eq!(classify("\\\\!x"), Typed::Prompt("\\\\!x"));
        assert_eq!(classify("fix it!"), Typed::Prompt("fix it!"));
        assert_eq!(classify("\\n"), Typed::Prompt("\\n"));
    }

    #[test]
    fn the_shell_is_the_users_or_sh() {
        assert_eq!(user_shell(Some("/bin/zsh".into())), "/bin/zsh");
        assert_eq!(user_shell(Some("".into())), "/bin/sh");
        assert_eq!(user_shell(None), "/bin/sh");
    }

    #[test]
    fn output_keeps_its_tail_and_counts_what_it_dropped() {
        let (mut out, mut dropped) = (String::new(), 0);
        for i in 0..20_000 {
            push_line(&mut out, &mut dropped, &format!("line {i}"));
        }
        assert!(out.len() <= KEEP_BYTES);
        assert!(out.ends_with("line 19999\n"));
        assert_eq!(dropped + out.lines().count(), 20_000);
        assert_eq!(out.lines().next(), Some(format!("line {dropped}").as_str()));

        let (mut out, mut dropped) = (String::new(), 0);
        push_line(&mut out, &mut dropped, &"é".repeat(LINE_MAX_CHARS + 5));
        assert_eq!(
            out.chars().count(),
            LINE_MAX_CHARS + "…[cut]\n".chars().count()
        );
        assert_eq!(dropped, 0);
    }

    #[test]
    fn the_context_names_the_command_its_tail_and_its_end() {
        let ok = ShellStatus::Exited { code: 0 };
        assert_eq!(
            context_entry("echo hi", "hi\n", 0, &ok, 100),
            "$ echo hi\nhi\n[exit 0]"
        );
        assert_eq!(context_entry("true", "", 0, &ok, 100), "$ true\n[exit 0]");
        let out: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let entry = context_entry("seq", &out, 7, &ShellStatus::Killed, 50);
        assert!(entry.starts_with("$ seq\n[… "));
        assert!(entry.ends_with("line 99\n[stopped by the user]"));
        // Whole lines, and every one not shown is counted.
        let shown = entry.lines().filter(|l| l.starts_with("line ")).count();
        let omitted: usize = entry
            .lines()
            .nth(1)
            .and_then(|l| l.strip_prefix("[… "))
            .and_then(|l| l.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap();
        assert_eq!(omitted, 7 + 100 - shown);
        assert!(entry.lines().nth(2).unwrap().starts_with("line "));

        assert_eq!(context(&[]), None);
        let both = context(&["$ a\n[exit 0]".into(), "$ b\n[exit 1]".into()]).unwrap();
        assert!(both.starts_with("[Context: shell commands"));
        assert!(both.ends_with("$ a\n[exit 0]\n\n$ b\n[exit 1]"));
    }

    #[tokio::test]
    async fn a_command_runs_in_the_directory_with_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("here.txt"), "").unwrap();
        let mut p = spawn(
            "echo hi; test -f here.txt && echo found; echo oops >&2; exit 3",
            dir.path(),
            &Sandbox::off(),
        )
        .unwrap();
        let mut got = Vec::new();
        while let Some(line) = p.lines.recv().await {
            let end = matches!(line, RawLine::Exited(_));
            got.push(line);
            if end {
                break;
            }
        }
        assert!(got.contains(&RawLine::Stdout("hi".into())));
        assert!(got.contains(&RawLine::Stdout("found".into())), "{got:?}");
        assert!(got.contains(&RawLine::Stderr("oops".into())));
        assert_eq!(got.last(), Some(&RawLine::Exited(Some(3))));
    }
}
