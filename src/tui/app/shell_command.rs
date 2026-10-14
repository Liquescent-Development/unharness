//! The `!` command typed at the prompt. Running it is the controller's
//! (`core::controller`, `shell_command.rs`).

use super::App;

impl App {
    /// Run `command`, typed after `!`, in the session's directory under its
    /// sandbox. Not during a turn, and one at a time: refused, it goes
    /// back to the prompt box if that is empty.
    pub fn run_shell(&mut self, command: &str) {
        let typed = format!("!{command}");
        if self.with(|c, screen| c.run_shell(command, screen)) && self.input.is_empty() {
            self.set_input(&typed);
        }
    }

    /// Kill the running `!` command (Esc, Ctrl+C).
    pub fn stop_shell(&mut self) {
        self.controller.stop_shell();
    }

    pub fn shell_output(&mut self, line: &str) {
        self.controller.shell_output(line);
    }

    /// The `!` command ended with `code`, `None` when a signal ended it.
    pub fn shell_exited(&mut self, code: Option<i32>) {
        self.with(|c, screen| c.shell_exited(code, screen));
    }

    /// The `!` command could not be started.
    pub fn shell_failed(&mut self, error: String) {
        self.with(|c, screen| c.shell_failed(error, screen));
    }
}
