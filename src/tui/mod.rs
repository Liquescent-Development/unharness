//! The terminal UI: an event loop over crossterm input, an 8 FPS status ticker, and
//! the active harness session's event stream.

pub mod app;
pub mod code;
pub mod markdown;
pub mod modal;
pub mod transcript;
pub mod ui;

use std::io::{Stdout, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::config::Config;
use crate::core::registry::Registry;
use crate::core::{HarnessId, PermissionPolicy, SessionCommand, SessionConfig, SessionHandle};
use crate::harness::resolve_binary;
use app::{Action, App, AppInit};

pub struct TuiLaunch {
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub registry: Arc<Registry>,
    pub config: Config,
    pub harness: HarnessId,
    pub policy: PermissionPolicy,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub resume: Option<String>,
    pub harness_explicit: bool,
    pub initial_prompt: Option<String>,
}

pub async fn run_tui(launch: TuiLaunch) -> Result<()> {
    // Restore the terminal before the panic message prints, otherwise a panic
    // leaves the user's shell in raw mode on the alternate screen.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let initial_prompt = launch.initial_prompt.clone();
    let mut app = App::new(AppInit {
        cwd: launch.cwd,
        workspace_root: launch.workspace_root,
        registry: launch.registry,
        config: launch.config,
        harness: launch.harness,
        policy: launch.policy,
        provider: launch.provider,
        model: launch.model,
        effort: launch.effort,
        resume: launch.resume,
        harness_explicit: launch.harness_explicit,
    });

    let res = event_loop(&mut terminal, &mut app, initial_prompt).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Some(line) = app.exit_summary() {
        println!("{line}");
    }
    res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    initial_prompt: Option<String>,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(125));
    let mut session: Option<SessionHandle> = None;
    let mut needs_redraw = true;

    if let Some(p) = initial_prompt {
        app.submit_prompt(p);
        run_actions(app, &mut session).await;
    }

    while !app.should_quit {
        if needs_redraw {
            terminal.draw(|f| ui::render(f, app))?;
            needs_redraw = false;
        }

        tokio::select! {
            _ = ticker.tick() => {
                if app.is_generating {
                    app.tick_spinner();
                    needs_redraw = true;
                }
            }
            ev = async {
                match session.as_mut() {
                    Some(s) => s.events.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match ev {
                    Some(ev) => {
                        let exited = matches!(ev, crate::core::AgentEvent::ProcessExited { .. });
                        app.on_event(ev);
                        if exited {
                            session = None;
                        }
                    }
                    None => {
                        // Driver gone without an exit event.
                        app.on_event(crate::core::AgentEvent::ProcessExited { code: None });
                        session = None;
                    }
                }
                needs_redraw = true;
            }
            Some(Ok(event)) = input.next() => {
                needs_redraw = true;
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if app.modal.is_some() {
                        app.handle_modal_key(key);
                    } else {
                        handle_key(app, key.modifiers, key.code);
                    }
                }
            }
        }

        run_actions(app, &mut session).await;
    }

    if let Some(s) = session.take() {
        let _ = s.send(SessionCommand::Shutdown).await;
    }
    Ok(())
}

fn handle_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match (ctrl, code) {
        (true, KeyCode::Char('c')) => {
            if app.is_generating {
                app.interrupt();
            } else {
                app.quit();
            }
        }
        (true, KeyCode::Char('d')) => app.quit(),
        (true, KeyCode::Char('h')) => app.open_harness_picker(),
        (true, KeyCode::Char('m')) => app.open_model_picker(),
        (true, KeyCode::Char('e')) => app.open_effort_picker(),
        (true, KeyCode::Char('p')) => app.open_policy_picker(),
        (true, KeyCode::Char('r')) => app.open_resume_picker(),
        (true, KeyCode::Char('o')) => {
            app.transcript.toggle_last_tool();
        }
        (true, KeyCode::Char('a')) => app.move_cursor_home(),
        (true, KeyCode::Char('u')) => {
            app.take_input();
        }
        (_, KeyCode::Tab) => {
            if !app.suggestions.is_empty() {
                app.accept_suggestion();
            }
        }
        (_, KeyCode::Up) => {
            if !app.suggestions.is_empty() {
                app.suggestion_up();
            } else {
                app.scroll_up(2);
            }
        }
        (_, KeyCode::Down) => {
            if !app.suggestions.is_empty() {
                app.suggestion_down();
            } else {
                app.scroll_down(2);
            }
        }
        (_, KeyCode::PageUp) => app.scroll_up(10),
        (_, KeyCode::PageDown) => app.scroll_down(10),
        (_, KeyCode::Home) => app.move_cursor_home(),
        (_, KeyCode::End) => {
            if app.input.is_empty() {
                app.scroll_to_bottom();
            } else {
                app.move_cursor_end();
            }
        }
        (_, KeyCode::Esc) => {
            // Esc cancels; it never quits (use /quit, Ctrl+D, or Ctrl+C when idle).
            if app.is_generating {
                app.interrupt();
            } else if !app.suggestions.is_empty() {
                app.suggestions.clear();
            } else if !app.input.is_empty() {
                app.take_input();
            }
        }
        (_, KeyCode::Enter) => {
            if app.input.trim().is_empty() {
                return;
            }
            // A partial slash command with a highlighted suggestion completes
            // on Enter instead of being sent as-is.
            if app.should_accept_suggestion() {
                app.accept_suggestion();
                return;
            }
            if app.is_generating && !app.input.starts_with('/') {
                return;
            }
            let text = app.take_input();
            if text.starts_with('/') {
                app.handle_slash_command(&text);
            } else {
                app.submit_prompt(text);
            }
        }
        (_, KeyCode::Char(c)) => app.insert_char(c),
        (_, KeyCode::Backspace) => app.delete_backwards(),
        (_, KeyCode::Delete) => app.delete_forwards(),
        (_, KeyCode::Left) => app.move_cursor_left(),
        (_, KeyCode::Right) => app.move_cursor_right(),
        _ => {}
    }
}

async fn run_actions(app: &mut App, session: &mut Option<SessionHandle>) {
    for action in app.take_actions() {
        match action {
            Action::StartSession { resume } => {
                if session.is_some() {
                    continue;
                }
                match start_session(app, resume) {
                    Ok(handle) => {
                        *session = Some(handle);
                        app.session_alive = true;
                    }
                    Err(e) => {
                        app.on_event(crate::core::AgentEvent::TurnCompleted {
                            stop_reason: crate::core::StopReason::Error(format!(
                                "could not start {}: {e:#}",
                                app.active.short_name()
                            )),
                        });
                    }
                }
            }
            Action::SendTurn(text) => {
                if let Some(s) = session.as_ref() {
                    if let Err(e) = s.send(SessionCommand::SendTurn { text }).await {
                        app.on_event(crate::core::AgentEvent::TurnCompleted {
                            stop_reason: crate::core::StopReason::Error(e.to_string()),
                        });
                    }
                } else if app.is_generating {
                    // StartSession failed; the error was already reported.
                }
            }
            Action::Command(cmd) => {
                if let Some(s) = session.as_ref()
                    && s.send(cmd).await.is_err()
                {
                    app.transcript.push_error("session driver is gone");
                    app.session_alive = false;
                    *session = None;
                }
            }
            Action::Shutdown => {
                if let Some(s) = session.take() {
                    let _ = s.send(SessionCommand::Shutdown).await;
                }
                app.session_alive = false;
            }
        }
    }
}

fn start_session(app: &App, resume: Option<String>) -> Result<SessionHandle> {
    let harness = app.harness();
    let desc = harness.descriptor();
    let binary = resolve_binary(desc, app.config.binary_override(desc.id.as_str()))
        .with_context(|| format!("{} not found on PATH", desc.binary_names.join("/")))?;
    let cfg = SessionConfig {
        binary,
        cwd: app.cwd.clone(),
        model: app.current_model().cloned(),
        effort: app.current_effort().map(str::to_string),
        policy: app.effective_policy(),
        resume: if harness.capabilities().resume_by_id {
            resume
        } else {
            None
        },
        extra_args: app.config.extra_args(desc.id.as_str()).to_vec(),
        env: Vec::new(),
    };
    harness.start_session(cfg)
}
