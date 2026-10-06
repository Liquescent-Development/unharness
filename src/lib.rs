//! unharness: a vendor-neutral TUI and CLI runner for AI coding agents.
//!
//! The binary in `main.rs` is a thin wrapper; everything lives here so
//! integration tests can drive sessions through the real code.

pub mod cli;
pub mod config;
pub mod core;
pub mod doctor;
pub mod harness;
pub mod init;
pub mod models_cmd;
pub mod runner;
pub mod skills;
pub mod skills_cmd;
pub mod skills_import;
pub mod switch;
pub mod sync;
pub mod tui;
pub mod update;
