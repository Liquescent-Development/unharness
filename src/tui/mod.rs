//! The terminal UI: an event loop over crossterm input, an 8 FPS status ticker, and
//! the active harness session's event stream.

pub mod app;
pub mod bridge;
pub mod clipboard;
pub mod code;
pub mod drop;
pub mod editor;
pub mod files;
pub mod herdr;
pub mod history;
pub mod markdown;
pub mod modal;
pub mod prompt;
pub mod selection;
pub mod shell;
pub mod transcript;
pub mod ui;

use std::io::{Stdout, Write, stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
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
use futures::{FutureExt, StreamExt};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::config::Config;
use crate::core::Rules;
use crate::core::process::{LineProcess, RawLine};
use crate::core::registry::Registry;
use crate::core::sandbox::SandboxSetup;
use crate::core::{
    HarnessId, McpServer, PermissionPolicy, SessionCommand, SessionConfig, SessionHandle,
};
use crate::harness::resolve_binary;
use app::{Action, App, AppInit};

pub struct TuiLaunch {
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub registry: Arc<Registry>,
    pub config: Config,
    pub harness: HarnessId,
    /// The policy named on the command line, if one was.
    pub policy: Option<PermissionPolicy>,
    pub sandbox: SandboxSetup,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub resume: Option<String>,
    pub harness_explicit: bool,
    pub initial_prompt: Option<String>,
    pub rules: Rules,
}

/// Take over the terminal: raw mode, alternate screen, paste and key
/// reporting. `restore_terminal` is the reverse.
fn enter_terminal(out: &mut Stdout) -> Result<()> {
    enable_raw_mode()?;
    // Bracketed paste: a paste arrives as one event instead of as keys, so
    // its newlines do not submit the prompt.
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    // Where the terminal speaks the kitty keyboard protocol, ask it to
    // report modified keys unambiguously: Shift+Enter then differs from
    // Enter, and Ctrl+M / Ctrl+H from Enter / Backspace. Elsewhere the
    // terminal sends what it always did.
    // Asked once: the answer cannot change, and asking again after the
    // event reader was stopped for an editor fails.
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    if *SUPPORTED.get_or_init(|| supports_keyboard_enhancement().unwrap_or(false)) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }
    if MOUSE.load(Ordering::SeqCst) {
        out.write_all(ENABLE_MOUSE)?;
        out.flush()?;
    }
    Ok(())
}

/// Whether the TUI takes the mouse (the `mouse` setting).
static MOUSE: AtomicBool = AtomicBool::new(false);

/// Report presses, releases, the wheel, and motion while a button is held,
/// in SGR coordinates. Motion without a button is not asked for: nothing
/// here reacts to hovering, and multiplexers lag when every movement is
/// forwarded.
const ENABLE_MOUSE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// Whether the kitty keyboard protocol was switched on and has to be
/// switched off again, including from the panic hook.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// Undo everything `enter_terminal` did. Every step is tried even
/// if an earlier one fails.
fn restore_terminal() -> std::io::Result<()> {
    let mut out = stdout();
    let popped = if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        execute!(out, PopKeyboardEnhancementFlags)
    } else {
        Ok(())
    };
    let mouse = if MOUSE.load(Ordering::SeqCst) {
        out.write_all(DISABLE_MOUSE).and_then(|()| out.flush())
    } else {
        Ok(())
    };
    let left = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
    disable_raw_mode()?;
    popped.and(mouse).and(left)
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

    MOUSE.store(launch.config.mouse.unwrap_or(true), Ordering::SeqCst);
    let mut herdr = launch
        .config
        .herdr
        .unwrap_or(true)
        .then(|| herdr::Pane::from_env(|k| std::env::var(k).ok()))
        .flatten()
        .map(|pane| herdr::Reporter::start(pane, herdr::default_log()));
    let mut out = stdout();
    enter_terminal(&mut out)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let initial_prompt = launch.initial_prompt.clone();
    let default_providers = launch
        .registry
        .all()
        .filter_map(|h| Some((h.descriptor().id, h.default_provider()?)))
        .collect();
    let mut app = App::new(AppInit {
        cwd: launch.cwd,
        workspace_root: launch.workspace_root,
        registry: launch.registry,
        config: launch.config,
        harness: launch.harness,
        policy: launch.policy,
        sandbox: launch.sandbox,
        provider: launch.provider,
        model: launch.model,
        effort: launch.effort,
        resume: launch.resume,
        harness_explicit: launch.harness_explicit,
        checkpoint_store: None,
        rules: launch.rules,
        default_providers,
    });

    let res = event_loop(&mut terminal, &mut app, initial_prompt, herdr.as_mut()).await;
    // A list still being asked for is not waited for.
    crate::core::process::ProbeProcess::kill_all();
    if let Some(h) = herdr {
        h.release().await;
    }

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
    mut herdr: Option<&mut herdr::Reporter>,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(125));
    let mut session: Option<SessionHandle> = None;
    // The `!` command at work.
    let mut shell: Option<LineProcess> = None;
    let mut needs_redraw = true;
    // File lists for `@` completion, walked off this task.
    let (files_tx, mut files_rx) = tokio::sync::mpsc::unbounded_channel();
    // Provider and model lists, which may start the harness's CLI.
    let (lists_tx, mut lists_rx) = tokio::sync::mpsc::unbounded_channel();

    if let Some(p) = initial_prompt {
        app.submit_prompt(p);
        run_actions(app, &mut session, &mut shell).await;
    }

    while !app.should_quit {
        if let Some(h) = herdr.as_deref_mut() {
            h.update(app.herdr_report());
        }
        if needs_redraw {
            terminal.draw(|f| ui::render(f, app))?;
            needs_redraw = false;
        }

        tokio::select! {
            _ = ticker.tick() => {
                if app.is_busy() {
                    app.tick_spinner();
                    needs_redraw = true;
                }
                needs_redraw |= app.tick_mouse();
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
            line = async {
                match shell.as_mut() {
                    Some(p) => p.lines.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                take_shell_lines(app, &mut shell, line);
                needs_redraw = true;
            }
            Some(Ok(event)) = input.next() => {
                needs_redraw = true;
                handle_event(app, event);
                // A drag or a spin of the wheel arrives as a burst: take
                // everything already waiting and draw once for all of it.
                while let Some(Some(Ok(event))) = input.next().now_or_never() {
                    handle_event(app, event);
                }
            }
            Some((request, result)) = lists_rx.recv() => {
                app.on_list(request, result);
                needs_redraw = true;
            }
            Some(paths) = files_rx.recv() => {
                match paths {
                    Some(paths) => app.set_file_index(paths),
                    None => app.file_walk_failed(),
                }
                needs_redraw = true;
            }
        }

        for job in app.take_list_jobs() {
            let tx = lists_tx.clone();
            // As for the file walk: quitting does not wait for it.
            std::thread::spawn(move || {
                let _ = tx.send(job.run());
            });
        }

        if let Some(root) = app.take_file_index_request() {
            let tx = files_tx.clone();
            // A thread of its own, not the runtime's: quitting does not
            // wait for a walk to end.
            std::thread::spawn(move || {
                let walk = || files::walk(&root, files::LISTED);
                let _ = tx.send(std::panic::catch_unwind(walk).ok());
            });
        }

        if let Some(text) = app.take_copy_request() {
            let rows = text.lines().count().max(1);
            let message = match clipboard::copy(&text, clipboard::Desktop::detect(), &mut stdout())
            {
                Ok(_) if rows == 1 => "Copied".to_string(),
                Ok(_) => format!("Copied {rows} lines"),
                Err(e) => format!("Not copied: {e}"),
            };
            app.flash(message);
        }

        if app.take_clipboard_request() {
            let pasted = clipboard::read(clipboard::Desktop::detect());
            app.attach_pasted(pasted, &clipboard::default_image_dir());
            needs_redraw = true;
        }

        if app.take_edit_request() {
            // The editor needs the keyboard to itself: stop our reader for
            // as long as it runs.
            drop(input);
            edit_prompt(terminal, app)?;
            input = EventStream::new();
            needs_redraw = true;
        }

        run_actions(app, &mut session, &mut shell).await;
    }

    if let Some(s) = session.take() {
        let _ = s.send(SessionCommand::Shutdown).await;
    }
    if let Some(mut p) = shell.take() {
        // Killed, and reaped before the runtime goes.
        p.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(line) = p.lines.recv().await {
                if matches!(line, RawLine::Exited(_)) {
                    break;
                }
            }
        })
        .await;
    }
    Ok(())
}

/// Lines of a `!` command's output taken in before the screen is drawn.
const SHELL_BURST: usize = 4096;

/// Hand the `!` command's `first` line to the app, and whatever else it
/// has already printed: a command that prints fast is drawn once per burst.
fn take_shell_lines(app: &mut App, shell: &mut Option<LineProcess>, first: Option<RawLine>) {
    let mut next = first;
    for _ in 0..SHELL_BURST {
        match next {
            Some(RawLine::Stdout(l) | RawLine::Stderr(l)) => app.shell_output(&l),
            Some(RawLine::Exited(code)) => {
                app.shell_exited(code);
                *shell = None;
                return;
            }
            // Its reader is gone without saying how it ended.
            None => {
                app.shell_exited(None);
                *shell = None;
                return;
            }
        }
        match shell.as_mut().map(|p| p.lines.try_recv()) {
            Some(Ok(l)) => next = Some(l),
            _ => return,
        }
    }
}

fn handle_event(app: &mut App, event: Event) {
    match event {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Release {
                return;
            }
            app.clear_selection();
            if app.modal.is_some() {
                app.handle_modal_key(key);
            } else {
                handle_key(app, key.modifiers, key.code);
            }
        }
        // The prompt is out of view while a subagent is being read.
        Event::Paste(text) if app.viewing.is_none() => app.paste(&text),
        Event::Paste(_) => {}
        Event::Mouse(mouse) => app.handle_mouse(mouse),
        _ => {}
    }
}

/// Hand the terminal to `$VISUAL` / `$EDITOR` with the prompt in a file, and
/// take back what it saved.
fn edit_prompt(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let Some(command) = editor::command() else {
        app.transcript
            .push_notice("set $EDITOR (or $VISUAL) to edit the prompt in an editor");
        return Ok(());
    };
    restore_terminal()?;
    let edited = editor::edit(&command, &app.input, &std::env::temp_dir());
    enter_terminal(&mut stdout())?;
    terminal.clear()?;
    match edited {
        Ok(Some(text)) => app.set_input(&text),
        Ok(None) => app.transcript.push_notice(format!(
            "`{command}` exited with an error; prompt unchanged"
        )),
        Err(e) => app
            .transcript
            .push_error(format!("could not edit the prompt: {e:#}")),
    }
    Ok(())
}

/// Keys while a subagent's transcript is in view: reading it, stopping it,
/// leaving it. The prompt belongs to the main conversation.
fn handle_view_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match (ctrl, code) {
        (false, KeyCode::Esc | KeyCode::Char('q')) | (true, KeyCode::Char('c')) => {
            app.close_subagent_view()
        }
        (true, KeyCode::Char('d')) => app.quit(),
        (false, KeyCode::Char('s')) => app.stop_viewed_subagent(),
        (true, KeyCode::Char('s')) => app.open_subagent_picker(),
        (_, KeyCode::Tab) => app.view_next_subagent(false),
        (_, KeyCode::BackTab) => app.view_next_subagent(true),
        (true, KeyCode::Char('o')) => {
            app.shown_transcript_mut().toggle_last_tool();
        }
        (true, KeyCode::Char('t')) => app.shown_transcript_mut().toggle_all_tools(),
        (_, KeyCode::Up) => app.scroll_up(2),
        (_, KeyCode::Down) => app.scroll_down(2),
        (_, KeyCode::PageUp) => app.scroll_up(10),
        (_, KeyCode::PageDown) => app.scroll_down(10),
        (_, KeyCode::End) => app.scroll_to_bottom(),
        _ => {}
    }
}

fn handle_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) {
    if app.viewing.is_some() {
        return handle_view_key(app, modifiers, code);
    }
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    // The keyboard is in the list of subagents under the prompt.
    if app.subagent_focus_index().is_some() {
        match code {
            KeyCode::Up if modifiers.is_empty() => return app.subagent_list_up(),
            KeyCode::Down if modifiers.is_empty() => {
                app.subagent_list_down();
                return;
            }
            KeyCode::Enter if modifiers.is_empty() => return app.open_focused_subagent(),
            KeyCode::Delete | KeyCode::Backspace => return app.dismiss_focused_subagent(),
            KeyCode::Esc => {
                app.subagent_focus = None;
                return;
            }
            // Anything else is meant for the prompt.
            _ => app.subagent_focus = None,
        }
    }
    match (ctrl, code) {
        (false, KeyCode::Up) if alt => app.unqueue_last(),
        (false, KeyCode::Up) if shift => app.scroll_up(2),
        (false, KeyCode::Down) if shift => app.scroll_down(2),
        // Ctrl+J is a newline everywhere. Shift+Enter only arrives as such
        // from terminals that report modifiers on Enter.
        (true, KeyCode::Char('j')) => app.insert_newline(),
        (true, KeyCode::Char('g')) => app.request_edit(),
        // Terminals paste text themselves and send nothing for an image.
        (true, KeyCode::Char('v')) => app.request_clipboard(),
        (false, KeyCode::Enter) if shift && !alt => app.insert_newline(),
        (true, KeyCode::Char('c')) => {
            if app.is_generating {
                app.interrupt();
            } else if app.shell.is_some() {
                app.stop_shell();
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
        (true, KeyCode::Char('t')) => app.transcript.toggle_all_tools(),
        (true, KeyCode::Char('s')) => app.open_subagent_picker(),
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
            } else if !app.move_cursor_down() && !app.history_newer() {
                // Past the prompt's last row: the subagents listed under it.
                app.subagent_list_down();
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
            // An open list is what Esc closes, also while a turn runs.
            if !app.suggestions.is_empty() {
                app.close_suggestions();
            } else if app.is_generating {
                app.interrupt();
            } else if app.shell.is_some() {
                app.stop_shell();
            } else if !app.input.is_empty() {
                app.take_input();
            }
        }
        (_, KeyCode::Enter) => {
            if app.input.trim().is_empty() {
                // Idle with prompts held back (after an interrupt or error,
                // or for want of a policy, which is then asked for again).
                if !app.send_next_queued() && !app.queued.is_empty() {
                    app.require_policy();
                }
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
                return;
            }
            match shell::classify(&text) {
                shell::Typed::Shell(command) => app.run_shell(command),
                shell::Typed::Prompt(text) if alt => app.steer(text.to_string()),
                // Sent now when idle, after the running turn otherwise.
                shell::Typed::Prompt(text) => app.queue_prompt(text.to_string()),
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

async fn run_actions(
    app: &mut App,
    session: &mut Option<SessionHandle>,
    shell: &mut Option<LineProcess>,
) {
    for action in app.take_actions() {
        match action {
            Action::RunShell { command } => {
                // The sandbox the active harness's next process gets: a
                // command typed here is confined no less than the agent.
                let started = app
                    .session_sandbox()
                    .and_then(|sandbox| self::shell::spawn(&command, &app.cwd, &sandbox));
                match started {
                    Ok(p) => *shell = Some(p),
                    Err(e) => app.shell_failed(format!("{e:#}")),
                }
            }
            Action::StopShell => {
                if let Some(p) = shell.as_mut() {
                    p.kill().await;
                }
            }
            Action::StartSession { resume } => {
                if session.is_some() {
                    continue;
                }
                let prepared = app.harness().prepare();
                // After `prepare`, which may itself create a watched file.
                app.arm_guard();
                if let Ok(Some(note)) = &prepared {
                    app.transcript.push_notice(note.clone());
                }
                let mcp_servers = app.session_mcp_servers();
                match prepared.and_then(|_| start_session(app, resume, mcp_servers)) {
                    Ok(handle) => {
                        *session = Some(handle);
                        app.session_alive = true;
                        app.session_sandbox_level = Some(app.sandbox_level().0);
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

fn start_session(
    app: &App,
    resume: Option<String>,
    mcp_servers: Vec<McpServer>,
) -> Result<SessionHandle> {
    let harness = app.harness();
    let desc = harness.descriptor();
    let binary = resolve_binary(desc, app.config.binary_override(desc.id.as_str()))
        .with_context(|| format!("{} not found on PATH", desc.binary_names.join("/")))?;
    let cfg = SessionConfig {
        binary,
        cwd: app.cwd.clone(),
        model: app.current_model().cloned(),
        provider: app.chosen_provider().cloned(),
        effort: app.current_effort().map(str::to_string),
        policy: app
            .effective_policy()
            .context("no permission policy is chosen")?,
        fork: resume.is_some() && app.fork_pending(),
        resume: if app.caps().resume_by_id {
            resume
        } else {
            None
        },
        extra_args: app.config.extra_args(desc.id.as_str()).to_vec(),
        env: Vec::new(),
        mcp_servers,
        sandbox: app.session_sandbox()?,
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

    fn files(app: &mut App, paths: &[&str]) {
        app.set_file_index(paths.iter().map(|p| p.to_string()).collect());
    }

    #[test]
    fn at_lists_files_and_tab_or_enter_takes_one() {
        let mut app = test_app(HarnessId::CODEX);
        files(&mut app, &["Cargo.toml", "src/lib.rs", "src/main.rs"]);
        type_text(&mut app, "fix @src");
        assert_eq!(app.suggestions.len(), 2);
        // The arrows are the list's, not the history's.
        handle_key(&mut app, NONE, KeyCode::Down);
        assert_eq!(app.selected_suggestion, 1);
        handle_key(&mut app, NONE, KeyCode::Tab);
        assert_eq!(app.input, "fix src/main.rs ");

        // Enter takes the file; the next Enter sends.
        type_text(&mut app, "and @carg");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(app.input, "fix src/main.rs and Cargo.toml ");
        assert!(app.take_actions().is_empty());
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert!(app.input.is_empty());
        assert!(!app.take_actions().is_empty());
    }

    #[test]
    fn esc_closes_a_file_list_before_anything_else() {
        let mut app = test_app(HarnessId::CODEX);
        files(&mut app, &["Cargo.toml"]);
        type_text(&mut app, "first");
        handle_key(&mut app, NONE, KeyCode::Enter);
        app.take_actions();
        assert!(app.is_generating);

        // While a turn runs, Esc closes the list and leaves the turn alone.
        type_text(&mut app, "hi @c");
        assert_eq!(app.suggestions.len(), 1);
        handle_key(&mut app, NONE, KeyCode::Esc);
        assert!(app.suggestions.is_empty());
        assert!(app.take_actions().is_empty());
        // Closed, Enter sends the text as typed.
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(app.queued.len(), 1);
        assert_eq!(app.queued[0].text, "hi @c");

        // A word no file matches does not hold Enter back either.
        type_text(&mut app, "thanks @zzz");
        assert!(app.suggestions.is_empty());
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(app.queued.len(), 2);

        // Left moves the cursor out of the word: no list to act on.
        type_text(&mut app, "@c");
        handle_key(&mut app, NONE, KeyCode::Left);
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

    #[test]
    fn esc_never_stops_a_subagent_and_ctrl_s_offers_the_choice() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(crate::core::AgentEvent::SubagentStarted {
            id: "spawn".into(),
            description: "look around".into(),
            kind: None,
        });
        // Esc clears a draft and then does nothing more, however often.
        type_text(&mut app, "draft");
        for _ in 0..3 {
            handle_key(&mut app, KeyModifiers::NONE, KeyCode::Esc);
        }
        assert!(app.input.is_empty() && app.take_actions().is_empty());
        assert_eq!(app.subagents.len(), 1);
        assert!(app.modal.is_none() && !app.should_quit);

        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('s'));
        assert!(matches!(app.modal, Some(modal::Modal::Subagents(_))));
        app.close_modal();

        // Idle, Ctrl+C quits as it always did.
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('c'));
        assert!(app.should_quit);
    }

    #[test]
    fn down_from_the_prompt_reaches_the_subagents_and_enter_opens_one() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.prompt_width = 40;
        // Nothing listed: Down at the prompt does nothing new.
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        assert!(app.subagent_focus.is_none());

        app.session_alive = true;
        for id in ["one", "two"] {
            app.on_event(crate::core::AgentEvent::SubagentStarted {
                id: id.into(),
                description: format!("task {id}"),
                kind: None,
            });
        }
        // In a prompt of two rows, Down first moves within the prompt.
        type_text(&mut app, "a");
        app.insert_newline();
        type_text(&mut app, "b");
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Up);
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        assert!(app.subagent_focus.is_none());
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        assert_eq!(app.subagent_focus.as_deref(), Some("one"));
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        assert_eq!(app.subagent_focus.as_deref(), Some("two"));
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Up);
        assert_eq!(app.subagent_focus.as_deref(), Some("one"));

        // Enter opens it instead of sending the draft; Esc comes back to its row.
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Enter);
        assert_eq!(app.viewing.as_deref(), Some("one"));
        assert!(app.take_actions().is_empty() && app.input == "a\nb");
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Esc);
        assert!(app.viewing.is_none());
        assert_eq!(app.subagent_focus.as_deref(), Some("one"));

        // Up from the first row is the prompt again; so is Esc, which stops nothing.
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Up);
        assert!(app.subagent_focus.is_none());
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Esc);
        assert!(app.subagent_focus.is_none() && app.input == "a\nb");
        assert_eq!(app.subagents.len(), 2);

        // Delete in the list leaves a running one, and the draft, alone.
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Delete);
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Backspace);
        assert_eq!(app.subagent_rows().len(), 2);
        assert_eq!(app.input, "a\nb");
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Esc);

        // Typing while in the list goes to the prompt.
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        type_text(&mut app, "c");
        assert!(app.subagent_focus.is_none() && app.input == "a\nbc");

        // Stepping back through earlier prompts still ends at the draft
        // before Down leaves the prompt.
        app.history.push("earlier");
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Up);
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Up);
        assert_eq!(app.input, "earlier");
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Down);
        assert!(app.input == "a\nbc" && app.subagent_focus.is_none());
    }

    #[test]
    fn in_a_subagents_view_the_keys_are_for_reading_stopping_and_leaving() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        for id in ["one", "two"] {
            app.on_event(crate::core::AgentEvent::SubagentStarted {
                id: id.into(),
                description: format!("task {id}"),
                kind: None,
            });
        }
        type_text(&mut app, "draft");
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('s'));
        app.handle_modal_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.viewing.as_deref(), Some("one"));

        // Typing does not reach the prompt, and Enter sends nothing.
        type_text(&mut app, "x");
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Enter);
        handle_event(&mut app, Event::Paste("pasted".into()));
        assert_eq!(app.input, "draft");
        assert!(app.take_actions().is_empty());

        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Tab);
        assert_eq!(app.viewing.as_deref(), Some("two"));
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Char('s'));
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::StopSubagent {
                id: "two".into()
            })]
        );
        // Esc leaves the view and stops nothing; the draft is still there.
        handle_key(&mut app, KeyModifiers::NONE, KeyCode::Esc);
        assert!(app.viewing.is_none() && app.take_actions().is_empty());
        assert_eq!(app.subagents.len(), 2);
        assert_eq!(app.input, "draft");
    }

    /// Run the actions the app queued, and the `!` command among them to
    /// its end, as the event loop would.
    async fn run_to_end(app: &mut App) {
        let mut session = None;
        let mut shell = None;
        run_actions(app, &mut session, &mut shell).await;
        while shell.is_some() {
            let line = shell.as_mut().unwrap().lines.recv().await;
            take_shell_lines(app, &mut shell, line);
            run_actions(app, &mut session, &mut shell).await;
        }
    }

    fn sent_turns(app: &mut App) -> Vec<String> {
        app.take_actions()
            .into_iter()
            .filter_map(|a| match a {
                Action::SendTurn { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn shell_block(app: &App) -> (String, crate::core::conversations::ShellStatus) {
        app.transcript
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                transcript::Block::Shell { output, status, .. } => {
                    Some((output.clone(), status.clone()))
                }
                _ => None,
            })
            .expect("a shell block")
    }

    #[tokio::test]
    async fn a_bang_runs_in_the_workspace_and_its_output_goes_with_the_next_prompt() {
        use crate::core::conversations::ShellStatus;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::write(dir.join("marker.txt"), "").unwrap();
        let mut app =
            crate::tui::app::tests::test_app_in(dir.clone(), HarnessId::CLAUDE, None, false);
        type_text(&mut app, "! echo hi; test -f marker.txt && echo here");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert!(app.shell.is_some() && app.is_busy() && !app.is_generating);
        run_to_end(&mut app).await;
        assert!(app.shell.is_none());
        assert_eq!(
            shell_block(&app),
            ("hi\nhere\n".into(), ShellStatus::Exited { code: 0 })
        );
        // Nothing went to the agent, no session was started for it.
        assert!(!app.session_alive);

        type_text(&mut app, "what did it print?");
        handle_key(&mut app, NONE, KeyCode::Enter);
        let sent = sent_turns(&mut app);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].starts_with("[Context: shell commands the user ran"));
        assert!(
            sent[0].contains(
                "$ echo hi; test -f marker.txt && echo here\nhi\nhere\n[exit 0]\n\n[Current task for Claude]:\nwhat did it print?"
            ),
            "{}",
            sent[0]
        );
        // The transcript shows the prompt as typed.
        assert!(app.transcript.blocks.iter().any(
            |b| matches!(b, transcript::Block::User { text } if text == "what did it print?")
        ));

        // Once: the next prompt goes alone.
        app.session_alive = true;
        app.on_event(crate::core::AgentEvent::TurnCompleted {
            stop_reason: crate::core::StopReason::Done,
        });
        type_text(&mut app, "and now?");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(sent_turns(&mut app), vec!["and now?".to_string()]);

        // Saved with the conversation and back on resume, already sent.
        let id = app.conversation.id.clone();
        app.persist();
        let back = crate::tui::app::tests::test_app_in(dir, HarnessId::CLAUDE, Some(id), false);
        assert_eq!(
            shell_block(&back),
            ("hi\nhere\n".into(), ShellStatus::Exited { code: 0 })
        );
        assert!(
            back.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, transcript::Block::Shell { sent: true, .. }))
        );
    }

    #[tokio::test]
    async fn esc_stops_a_bang_and_a_prompt_meanwhile_waits_for_it() {
        use crate::core::conversations::ShellStatus;
        let mut app = test_app(HarnessId::CLAUDE);
        type_text(&mut app, "!echo started; sleep 30");
        handle_key(&mut app, NONE, KeyCode::Enter);
        let mut session = None;
        let mut shell = None;
        run_actions(&mut app, &mut session, &mut shell).await;
        let line = shell.as_mut().unwrap().lines.recv().await;
        take_shell_lines(&mut app, &mut shell, line);
        assert_eq!(shell_block(&app).0, "started\n");

        // A prompt is held for the output; another `!` is refused and kept.
        type_text(&mut app, "explain");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(app.queued.len(), 1);
        type_text(&mut app, "!ls");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(app.input, "!ls");
        app.take_input();
        assert!(sent_turns(&mut app).is_empty());

        handle_key(&mut app, NONE, KeyCode::Esc);
        assert!(!app.should_quit);
        let started = std::time::Instant::now();
        while shell.is_some() {
            run_actions(&mut app, &mut session, &mut shell).await;
            let line = shell.as_mut().unwrap().lines.recv().await;
            take_shell_lines(&mut app, &mut shell, line);
        }
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(shell_block(&app).1, ShellStatus::Killed);
        // The held prompt goes now, with what the command printed.
        let sent = sent_turns(&mut app);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("$ echo started; sleep 30\nstarted\n[stopped by the user]"));
        assert!(sent[0].ends_with("explain"));
    }

    #[tokio::test]
    async fn a_bang_is_confined_by_the_sessions_sandbox() {
        use crate::core::conversations::ShellStatus;
        use crate::core::sandbox::{SandboxLevel, SandboxSetup};
        let setup = SandboxSetup::detect(Some(SandboxLevel::ReadOnly));
        if let Err(why) = &setup.backend {
            eprintln!("skipping: {why}");
            return;
        }
        // Not under the temp directory, which the sandbox leaves writable.
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&target).unwrap();
        let tmp = tempfile::tempdir_in(&target).unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut app =
            crate::tui::app::tests::test_app_in(dir.clone(), HarnessId::CLAUDE, None, false);
        app.sandbox = setup;
        type_text(&mut app, "!echo x > made.txt");
        handle_key(&mut app, NONE, KeyCode::Enter);
        run_to_end(&mut app).await;
        assert!(matches!(shell_block(&app).1, ShellStatus::Exited { code } if code != 0));
        assert!(!dir.join("made.txt").exists());

        // The same command at the level that lets the agent write there.
        app.sandbox = SandboxSetup::detect(Some(SandboxLevel::WorkspaceWrite));
        type_text(&mut app, "!echo x > made.txt");
        handle_key(&mut app, NONE, KeyCode::Enter);
        run_to_end(&mut app).await;
        assert_eq!(shell_block(&app).1, ShellStatus::Exited { code: 0 });
        assert!(dir.join("made.txt").exists());
    }

    #[test]
    fn a_bang_waits_for_the_turn_and_an_escaped_one_is_a_prompt() {
        let mut app = test_app(HarnessId::CLAUDE);
        type_text(&mut app, "\\!important");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert_eq!(sent_turns(&mut app), vec!["!important".to_string()]);
        assert!(app.is_generating);

        // During a turn a `!` neither runs nor is queued: it is handed back.
        type_text(&mut app, "!make");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert!(app.shell.is_none() && app.queued.is_empty());
        assert_eq!(app.input, "!make");
        assert!(app.take_actions().is_empty());
        // Recalled like any prompt.
        app.take_input();
        handle_key(&mut app, NONE, KeyCode::Up);
        assert_eq!(app.input, "!make");
        app.take_input();

        // A bare `!` runs nothing.
        finish_turn(&mut app);
        type_text(&mut app, "!  ");
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert!(app.shell.is_none() && app.take_actions().is_empty());
    }

    #[test]
    fn a_bang_left_unsent_reaches_every_harness_once() {
        use crate::core::conversations::ShellStatus;
        let mut app = test_app(HarnessId::CLAUDE);
        type_text(&mut app, "hello");
        handle_key(&mut app, NONE, KeyCode::Enter);
        app.take_actions();
        app.session_alive = true;
        app.on_event(crate::core::AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        finish_turn(&mut app);

        // Run on Claude, then a switch before any prompt.
        app.run_shell("git status");
        app.take_actions();
        app.shell_output("clean");
        app.shell_exited(Some(0));
        assert_eq!(shell_block(&app).1, ShellStatus::Exited { code: 0 });
        app.switch_harness(HarnessId::CODEX);
        app.take_actions();
        app.session_alive = false;

        // Codex has it from the bridge, and only once.
        type_text(&mut app, "on codex");
        handle_key(&mut app, NONE, KeyCode::Enter);
        let sent = sent_turns(&mut app);
        assert_eq!(sent[0].matches("$ git status").count(), 1, "{}", sent[0]);
        assert!(sent[0].contains("[Context: shell commands the user ran from the unharness prompt, with their output]\n$ git status\nclean\n[exit 0]"));
        app.session_alive = true;
        app.on_event(crate::core::AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        finish_turn(&mut app);

        // Claude never saw it: it is in the bridge back.
        app.switch_harness(HarnessId::CLAUDE);
        app.take_actions();
        app.session_alive = false;
        type_text(&mut app, "back");
        handle_key(&mut app, NONE, KeyCode::Enter);
        let sent = sent_turns(&mut app);
        assert_eq!(sent[0].matches("$ git status").count(), 1, "{}", sent[0]);
        assert!(sent[0].contains("User ran a shell command:\n$ git status\nclean\n[exit 0]"));
        assert!(sent[0].contains("User: on codex"));
    }

    #[test]
    fn ctrl_v_asks_for_the_clipboard_and_types_nothing() {
        let mut app = test_app(HarnessId::CLAUDE);
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('v'));
        assert!(app.input.is_empty());
        assert!(app.take_clipboard_request() && !app.take_clipboard_request());
    }

    #[test]
    fn ctrl_g_asks_for_the_editor_and_its_text_replaces_the_prompt() {
        let mut app = test_app(HarnessId::CLAUDE);
        type_text(&mut app, "/mo");
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('g'));
        assert_eq!(app.input, "/mo");
        assert!(app.take_edit_request() && !app.take_edit_request());

        app.set_input("written\r\nin an editor\twith a tab");
        assert_eq!(app.input, "written\nin an editor    with a tab");
        assert_eq!(app.cursor, app.input.chars().count());
        assert!(app.suggestions.is_empty());
        handle_key(&mut app, NONE, KeyCode::Enter);
        assert!(app.is_generating);
    }
}
