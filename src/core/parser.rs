//! The one shape every protocol parser has, so that a driver and the
//! fixture replay take any of them.

use super::event::AgentEvent;

/// Anything that turns one raw output line into zero or more events.
pub trait Parser: Send {
    /// Called for stdout lines.
    fn feed(&mut self, line: &str) -> Vec<AgentEvent>;
    /// Called for stderr lines; most parsers map these to `Notice`/`Error`.
    fn feed_stderr(&mut self, _line: &str) -> Vec<AgentEvent> {
        Vec::new()
    }
}
