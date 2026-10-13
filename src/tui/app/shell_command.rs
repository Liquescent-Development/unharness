//! The `!` command typed at the prompt.

use std::time::Instant;

use super::{Action, App, ShellRun};
use crate::core::conversations::ShellStatus;

impl App {
    /// Run `command`, typed after `!`, in the session's directory under its
    /// sandbox. Not during a turn, and one at a time.
    pub fn run_shell(&mut self, command: &str) {
        let typed = format!("!{command}");
        if command.is_empty() {
            self.transcript
                .push_notice("nothing to run after `!` (`\\!` sends a prompt that starts with !)");
            return;
        }
        if self.is_generating || self.shell.is_some() {
            self.transcript.push_notice(if self.is_generating {
                "a `!` command cannot run during a turn; it is back in the prompt"
            } else {
                "a `!` command is already running (Esc stops it); this one is back in the prompt"
            });
            if self.input.is_empty() {
                self.set_input(&typed);
            }
            return;
        }
        self.transcript.shell_started(command);
        self.shell = Some(ShellRun {
            started: Instant::now(),
            stopping: false,
        });
        self.auto_scroll = true;
        self.actions.push_back(Action::RunShell {
            command: command.to_string(),
        });
    }

    /// Kill the running `!` command (Esc, Ctrl+C).
    pub fn stop_shell(&mut self) {
        if let Some(run) = self.shell.as_mut()
            && !run.stopping
        {
            run.stopping = true;
            self.actions.push_back(Action::StopShell);
        }
    }

    pub fn shell_output(&mut self, line: &str) {
        if self.shell.is_some() {
            self.transcript.shell_output(line);
        }
    }

    /// The `!` command ended with `code`, `None` when a signal ended it.
    pub fn shell_exited(&mut self, code: Option<i32>) {
        let Some(run) = self.shell.take() else {
            return;
        };
        self.transcript.shell_ended(match code {
            Some(code) => ShellStatus::Exited { code },
            None if run.stopping => ShellStatus::Killed,
            None => ShellStatus::Signalled,
        });
        self.after_shell();
    }

    /// The `!` command could not be started.
    pub fn shell_failed(&mut self, error: String) {
        if self.shell.take().is_none() {
            return;
        }
        self.transcript.shell_ended(ShellStatus::Failed { error });
        self.after_shell();
    }

    fn after_shell(&mut self) {
        self.persist();
        // Prompts sent while it ran waited for its output.
        self.send_next_queued();
    }
}
