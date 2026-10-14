//! The vendor's remote control (`/remote-control`): the command, and what
//! the status bar says. It is the controller's (`core::controller`,
//! `remote.rs`).

use super::App;

impl App {
    /// `/remote-control [on [name] | off | status]`.
    pub fn remote_control_command(&mut self, cmd: &str, args: &str) {
        // A command of the harness's own by that name, if it has one.
        if !self.caps().remote_control {
            self.pass_command(cmd, false);
            return;
        }
        self.controller.remote_control_command(args);
    }

    /// What the status bar says of Remote Control.
    pub fn remote_status(&self) -> Option<String> {
        self.controller.remote_status()
    }
}
