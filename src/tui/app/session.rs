//! The active harness's session, as the event loop reaches it. Its
//! lifecycle is the controller's (`core::controller`, `session.rs`).

use super::App;

impl App {
    /// Start the active harness's session before anything is sent to it
    /// (`Controller::start_when_idle`), or ask the harness for its
    /// commands where it starts none before the first prompt.
    pub fn start_when_idle(&mut self) {
        if self.controller.start_when_idle() {
            self.list_commands();
        }
    }

    /// The session could not be started. The next prompt tries again.
    pub fn start_failed(&mut self, why: String) {
        self.with(|c, screen| c.start_failed(why, screen));
    }

    /// The session's driver stopped taking commands.
    pub fn driver_gone(&mut self) {
        self.with(|c, screen| c.driver_gone(screen));
    }

    pub fn quit(&mut self) {
        self.with(|c, screen| c.quit(screen));
    }
}
