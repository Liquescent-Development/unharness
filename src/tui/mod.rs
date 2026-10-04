//! The terminal UI: an event loop over crossterm input, an 8 FPS status ticker, and
//! the active harness session's event stream.

pub mod app;
pub mod code;
pub mod history;
pub mod markdown;
pub mod modal;
pub mod prompt;
pub mod transcript;
pub mod ui;

use std::io::{Stdout, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
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

/// Whether the kitty keyboard protocol was switched on and has to be
/// switched off again, including from the panic hook.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// Undo everything `run_tui` did to the terminal. Every step is tried even
/// if an earlier one fails.
fn restore_terminal() -> std::io::Result<()> {
    let mut out = stdout();
    let popped = if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        execute!(out, PopKeyboardEnhancementFlags)
    } else {
        Ok(())
    };
    let left = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
    disable_raw_mode()?;
    popped.and(left)
}

pub async fn run_tui(launch: TuiLaunch) -> Result<()> {
    // Restore the terminal before the panic message prints, otherwise a panic
    // leaves the user's shell in raw mode on the alternate screen.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut out = stdout();
    // Bracketed paste: a paste arrives as one event instead of as keys, so
    // its newlines do not submit the prompt.
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    // Where the terminal speaks the kitty keyboard protocol, ask it to
    // report modified keys unambiguously: Shift+Enter then differs from
    // Enter, and Ctrl+M / Ctrl+H from Enter / Backspace. Elsewhere the
    // terminal sends what it always did.
    if supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }
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
        checkpoint_store: None,
    });

    let res = event_loop(&mut terminal, &mut app, initial_prompt).await;

    restore_terminal()?;
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
                match event {
                    Event::Key(key) => {
                        if key.kind == KeyEventKind::Release {
                            continue;
                        }
                        if app.modal.is_some() {
                            app.handle_modal_key(key);
                        } else {
                            handle_key(app, key.modifiers, key.code);
                        }
                    }
                    Event::Paste(text) => app.paste(&text),
                    _ => {}
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
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    match (ctrl, code) {
        (false, KeyCode::Up) if alt => app.unqueue_last(),
        (false, KeyCode::Up) if shift => app.scroll_up(2),
        (false, KeyCode::Down) if shift => app.scroll_down(2),
        // Ctrl+J is a newline everywhere. Shift+Enter only arrives as such
        // from terminals that report modifiers on Enter.
        (true, KeyCode::Char('j')) => app.insert_newline(),
        (false, KeyCode::Enter) if shift && !alt => app.insert_newline(),
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
            } else if !app.move_cursor_up() {
                // Already on the first row: go back through sent prompts.
                app.history_older();
            }
        }
        (_, KeyCode::Down) => {
            if !app.suggestions.is_empty() {
                app.suggestion_down();
            } else if !app.move_cursor_down() {
                app.history_newer();
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
                // Idle with prompts held back (after an interrupt or error).
                app.send_next_queued();
                return;
            }
            // A partial slash command with a highlighted suggestion completes
            // on Enter instead of being sent as-is.
            if app.should_accept_suggestion() {
                app.accept_suggestion();
                return;
            }
            let text = app.take_input();
            app.history.push(&text);
            if text.starts_with('/') {
                app.handle_slash_command(&text);
            } else if alt {
                app.steer(text);
            } else {
                // Sent now when idle, after the running turn otherwise.
                app.queue_prompt(text);
            }
        }
        (false, KeyCode::Char(c)) => app.insert_char(c),
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
                                app.short_name()
                            )),
                        });
                    }
                }
            }
            Action::SendTurn { text, attachments } => {
                if let Some(s) = session.as_ref() {
                    if let Err(e) = s.send(SessionCommand::SendTurn { text, attachments }).await {
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
        fork: resume.is_some() && app.fork_pending(),
        resume: if app.caps().resume_by_id {
            resume
        } else {
            None
        },
        extra_args: app.config.extra_args(desc.id.as_str()).to_vec(),
        env: Vec::new(),
    };
    harness.start_session(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::test_app;

    const NONE: KeyModifiers = KeyModifiers::NONE;

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            handle_key(app, NONE, KeyCode::Char(c));
        }
    }

    #[test]
    fn newline_keys_extend_the_prompt_and_enter_sends_all_of_it() {
        let mut app = test_app(HarnessId::CLAUDE);
        type_text(&mut app, "one");
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('j'));
        type_text(&mut app, "two");
        handle_key(&mut app, KeyModifiers::SHIFT, KeyCode::Enter);
        type_text(&mut app, "three");
        assert_eq!(app.input, "one\ntwo\nthree");
        assert!(!app.is_generating);

        handle_key(&mut app, NONE, KeyCode::Enter);
        assert!(app.input.is_empty() && app.is_generating);
        assert!(app.take_actions().iter().any(|a| matches!(
            a,
            Action::SendTurn { text, .. } if text == "one\ntwo\nthree"
        )));
    }

    #[test]
    fn arrows_and_home_end_work_by_line() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.insert_str("first line\nab\nthird line");
        assert_eq!(app.cursor, 24);

        // Up keeps the column where the row is long enough, else its end.
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.cursor, 13);
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.cursor, 2);
        handle_key(&mut app, NONE, KeyCode::End);
        assert_eq!(app.cursor, 10);
        handle_key(&mut app, NONE, KeyCode::Down);
        handle_key(&mut app, NONE, KeyCode::Home);
        assert_eq!(app.cursor, 11);
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('a'));
        assert_eq!(app.cursor, 11);
        handle_key(&mut app, NONE, KeyCode::Down);
        handle_key(&mut app, NONE, KeyCode::End);
        assert_eq!(app.cursor, 24);

        // Backspace at a line start joins the lines.
        handle_key(&mut app, NONE, KeyCode::Home);
        handle_key(&mut app, NONE, KeyCode::Backspace);
        assert_eq!(app.input, "first line\nabthird line");
    }

    #[test]
    fn arrows_follow_wrapped_rows() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.prompt_width = 4;
        app.insert_str("abcdefghij");
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.cursor, 6);
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.cursor, 2);
        handle_key(&mut app, NONE, KeyCode::Down);
        assert_eq!(app.cursor, 6);
    }

    #[test]
    fn slash_suggestions_are_for_single_line_input() {
        let mut app = test_app(HarnessId::CLAUDE);
        type_text(&mut app, "/pol");
        assert_eq!(app.suggestions.len(), 1);
        handle_key(&mut app, NONE, KeyCode::Tab);
        assert_eq!(app.input, "/policy");
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('j'));
        assert!(app.suggestions.is_empty());
    }

    fn finish_turn(app: &mut App) {
        app.take_actions();
        app.on_event(crate::core::AgentEvent::TurnCompleted {
            stop_reason: crate::core::StopReason::Done,
        });
    }

    #[test]
    fn up_and_down_recall_sent_prompts_around_the_draft() {
        let mut app = test_app(HarnessId::CLAUDE);
        // With nothing to recall, Up leaves the prompt and the transcript alone.
        handle_key(&mut app, NONE, KeyCode::Up);
        assert!(app.input.is_empty() && app.auto_scroll);

        type_text(&mut app, "first");
        handle_key(&mut app, NONE, KeyCode::Enter);
        finish_turn(&mut app);
        app.insert_str("second\nwith two lines");
        handle_key(&mut app, NONE, KeyCode::Enter);
        finish_turn(&mut app);

        type_text(&mut app, "draft");
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.input, "second\nwith two lines");
        assert_eq!(app.cursor, app.input.chars().count());
        // Inside a recalled multi-line prompt Up moves the cursor first.
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.input, "second\nwith two lines");
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.input, "first");
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.input, "first");

        handle_key(&mut app, NONE, KeyCode::Down);
        assert_eq!(app.input, "second\nwith two lines");
        handle_key(&mut app, NONE, KeyCode::Down);
        assert_eq!(app.input, "draft");
        handle_key(&mut app, NONE, KeyCode::Down);
        assert_eq!(app.input, "draft");

        // A recalled prompt can be edited and sent; it becomes the newest.
        handle_key(&mut app, NONE, KeyCode::Up);
        handle_key(&mut app, NONE, KeyCode::Up);
        handle_key(&mut app, NONE, KeyCode::Up);
        type_text(&mut app, "!");
        handle_key(&mut app, NONE, KeyCode::Enter);
        finish_turn(&mut app);
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.input, "first!");
    }

    #[test]
    fn history_survives_a_restart_in_the_same_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let mut app =
            crate::tui::app::tests::test_app_in(dir.clone(), HarnessId::CLAUDE, None, false);
        type_text(&mut app, "remember me");
        handle_key(&mut app, NONE, KeyCode::Enter);
        type_text(&mut app, "/plan");
        handle_key(&mut app, NONE, KeyCode::Enter);

        let mut app = crate::tui::app::tests::test_app_in(dir, HarnessId::CODEX, None, false);
        handle_key(&mut app, NONE, KeyCode::Up);
        // A recalled command is shown as sent, without the suggestion list
        // taking over the arrows.
        assert_eq!(app.input, "/plan");
        assert!(app.suggestions.is_empty());
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.input, "remember me");
    }

    #[test]
    fn history_leaves_suggestions_the_queue_and_scrolling_alone() {
        let mut app = test_app(HarnessId::CLAUDE);
        type_text(&mut app, "first");
        handle_key(&mut app, NONE, KeyCode::Enter);
        app.take_actions();

        // The suggestion list still owns the arrows while it is open.
        type_text(&mut app, "/");
        let n = app.suggestions.len();
        assert!(n > 1);
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.selected_suggestion, n - 1);
        handle_key(&mut app, NONE, KeyCode::Down);
        assert_eq!((app.selected_suggestion, app.input.as_str()), (0, "/"));
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('u'));
        assert!(app.input.is_empty() && app.suggestions.is_empty());

        // Enter during the turn queues; Alt+Up takes the prompt back.
        type_text(&mut app, "queued");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(app.queued.len(), 1);
        handle_key(&mut app, KeyModifiers::ALT, KeyCode::Up);
        assert_eq!(app.input, "queued");
        assert!(app.queued.is_empty());
        app.take_input();

        // Scrolling: PageUp/PageDown, and Shift+Up/Down by a couple of lines.
        app.scroll = 20;
        handle_key(&mut app, NONE, KeyCode::PageUp);
        assert_eq!((app.scroll, app.auto_scroll), (10, false));
        handle_key(&mut app, KeyModifiers::SHIFT, KeyCode::Up);
        assert_eq!(app.scroll, 8);
        handle_key(&mut app, KeyModifiers::SHIFT, KeyCode::Down);
        handle_key(&mut app, NONE, KeyCode::PageDown);
        assert_eq!(app.scroll, 20);
        assert!(app.input.is_empty());
    }
}
