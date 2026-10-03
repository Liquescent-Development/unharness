pub mod app;
pub mod markdown;
pub mod stream;
pub mod ui;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{Stdout, stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::harness::{HarnessKind, get_adapter};
use crate::skills::{discover_skills_in_dir, global_skills_dir, workspace_skills_dir};
use crate::sync::find_workspace_root;
use app::App;
use stream::{StreamEvent, StreamRunConfig, spawn_stream_task};

pub async fn run_tui(
    cwd: &Path,
    initial_harness: HarnessKind,
    auto_approve: bool,
    initial_prompt: Option<String>,
    config: &Config,
) -> Result<()> {
    // Restore the terminal before the panic message prints, otherwise a panic
    // leaves the user's shell in raw mode on the alternate screen.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(cwd.to_path_buf(), initial_harness, auto_approve, config);

    let res = event_loop(&mut terminal, &mut app, initial_prompt, config).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    initial_prompt: Option<String>,
    config: &Config,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(40)); // 25 FPS throttle

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(200);
    let mut active_child: Option<tokio::process::Child> = None;
    let mut needs_redraw = true;

    // If an initial prompt was passed, kick it off immediately!
    if let Some(prompt) = initial_prompt {
        app.add_user_message(prompt.clone());
        app.start_generation();
        let (effective_prompt, is_continuation) = app.prepare_prompt_for_dispatch(&prompt);
        let run_cfg = build_stream_cfg(app, effective_prompt, is_continuation, config);
        if let Ok(child) = spawn_stream_task(run_cfg, stream_tx.clone()).await {
            active_child = Some(child);
        } else {
            app.finish_generation();
            app.add_error_message("Failed to spawn harness process".to_string());
        }
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
            Some(evt) = stream_rx.recv() => {
                match evt {
                    StreamEvent::TextDelta(delta) => {
                        app.append_assistant_text(&delta);
                    }
                    StreamEvent::ThoughtDelta(delta) => {
                        app.append_thought_text(&delta);
                    }
                    StreamEvent::ToolCall { name, summary } => {
                        app.add_tool_message(&name, &summary);
                    }
                    StreamEvent::Status(status) => {
                        app.add_system_message(status);
                    }
                    StreamEvent::Error(err) => {
                        app.add_error_message(err);
                    }
                    StreamEvent::Done => {
                        app.finish_generation();
                        active_child = None;
                    }
                }
                needs_redraw = true;
            }
            Some(Ok(event)) = event_stream.next() => {
                needs_redraw = true;
                if let Event::Key(key) = event {
                    // Modal popup active: intercept keys for modal
                    if app.popup.is_some() {
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.picker_up();
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                app.picker_down();
                            }
                            KeyCode::Enter => {
                                app.confirm_picker();
                            }
                            KeyCode::Esc | KeyCode::Char('q') => {
                                app.close_popup();
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Main chat mode key handling
                    match (key.modifiers, key.code) {
                        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                            if app.is_generating {
                                if let Some(mut child) = active_child.take() {
                                    let _ = child.kill().await;
                                }
                                app.finish_generation();
                                app.add_system_message("Generation cancelled by user.".to_string());
                            } else {
                                app.should_quit = true;
                            }
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                            app.open_harness_picker();
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('m')) => {
                            app.open_model_picker();
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                            app.open_effort_picker();
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                            app.toggle_auto_approve();
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
                        (_, KeyCode::PageUp) => {
                            app.scroll_up(10);
                        }
                        (_, KeyCode::PageDown) => {
                            app.scroll_down(10);
                        }
                        (_, KeyCode::Home) => {
                            if app.suggestions.is_empty() {
                                app.move_cursor_home();
                            }
                        }
                        (_, KeyCode::End) => {
                            if app.suggestions.is_empty() {
                                if app.input.is_empty() {
                                    app.scroll_to_bottom();
                                } else {
                                    app.move_cursor_end();
                                }
                            }
                        }
                        (_, KeyCode::Esc) => {
                            if app.is_generating {
                                if let Some(mut child) = active_child.take() {
                                    let _ = child.kill().await;
                                }
                                app.finish_generation();
                                app.add_system_message("Cancelled.".to_string());
                            } else if !app.suggestions.is_empty() {
                                app.suggestions.clear();
                            } else {
                                app.should_quit = true;
                            }
                        }
                        (_, KeyCode::Enter) => {
                            if !app.is_generating && !app.input.trim().is_empty() {
                                let input = app.input.trim().to_string();
                                app.input.clear();
                                app.cursor_pos = 0;
                                app.suggestions.clear();

                                if input.starts_with('/') {
                                    handle_slash_command(&input, app);
                                } else {
                                    app.add_user_message(input.clone());
                                    app.start_generation();
                                    let (effective_prompt, is_continuation) = app.prepare_prompt_for_dispatch(&input);
                                    let run_cfg = build_stream_cfg(app, effective_prompt, is_continuation, config);
                                    match spawn_stream_task(run_cfg, stream_tx.clone()).await {
                                        Ok(child) => {
                                            active_child = Some(child);
                                        }
                                        Err(e) => {
                                            app.finish_generation();
                                            app.add_error_message(format!("Failed to spawn harness: {}", e));
                                        }
                                    }
                                }
                            }
                        }
                        (_, KeyCode::Char(c)) => {
                            if !app.is_generating {
                                app.insert_char(c);
                            }
                        }
                        (_, KeyCode::Backspace) => {
                            if !app.is_generating {
                                app.delete_backwards();
                            }
                        }
                        (_, KeyCode::Delete) => {
                            if !app.is_generating {
                                app.delete_forwards();
                            }
                        }
                        (_, KeyCode::Left) => {
                            app.move_cursor_left();
                        }
                        (_, KeyCode::Right) => {
                            app.move_cursor_right();
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if let Some(mut child) = active_child.take() {
        let _ = child.kill().await;
    }

    Ok(())
}

fn build_stream_cfg(
    app: &App,
    prompt: String,
    is_continuation: bool,
    config: &Config,
) -> StreamRunConfig {
    let adapter = get_adapter(app.active_harness);
    let binary_override = config.binary_override(app.active_harness.as_str());
    let binary = adapter
        .resolve_binary(binary_override)
        .unwrap_or_else(|| PathBuf::from(adapter.binary_name()));

    StreamRunConfig {
        harness: app.active_harness,
        binary,
        prompt,
        is_continuation,
        auto_approve: app.auto_approve,
        model: app.current_model(),
        effort: Some(app.current_effort()),
        cwd: app.cwd.clone(),
    }
}

fn handle_slash_command(cmd: &str, app: &mut App) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts[0] {
        "/switch" => {
            if parts.len() > 1 {
                if let Some(kind) = HarnessKind::parse_str(parts[1]) {
                    app.switch_harness(kind);
                } else {
                    app.add_error_message(format!(
                        "Unknown harness '{}'. Supported: agy, claude, codex",
                        parts[1]
                    ));
                }
            } else {
                app.open_harness_picker();
            }
        }
        "/model" => {
            if parts.len() > 1 {
                let model_id = parts[1].to_string();
                app.set_model_for_active_harness(model_id);
            } else {
                app.open_model_picker();
            }
        }
        "/effort" | "/think" => {
            if parts.len() > 1 {
                let effort_id = parts[1].to_string();
                app.set_effort_for_active_harness(effort_id);
            } else {
                app.open_effort_picker();
            }
        }
        "/skills" => {
            let ws_root = find_workspace_root(&app.cwd);
            let mut list = Vec::new();
            if let Some(ref root) = ws_root {
                for s in discover_skills_in_dir(&workspace_skills_dir(root)) {
                    list.push(format!("• [workspace] {}", s.name));
                }
            }
            if let Some(dir) = global_skills_dir() {
                for s in discover_skills_in_dir(&dir) {
                    list.push(format!("• [global] {}", s.name));
                }
            }
            if list.is_empty() {
                app.add_system_message("No skills discovered in .agents/skills".to_string());
            } else {
                app.add_system_message(format!(
                    "Available Skills ({}):\n{}",
                    list.len(),
                    list.join("\n")
                ));
            }
        }
        "/clear" => {
            app.messages.clear();
            app.add_system_message("Screen cleared.".to_string());
        }
        "/auto" => {
            app.toggle_auto_approve();
        }
        "/help" => {
            let help = "Unharness Commands:\n  /switch [harness] - Switch harness (or open picker)\n  /model [name] - Choose model (or open picker)\n  /effort [level] - Set thinking effort (or open picker)\n  /skills - List loaded skills\n  /auto - Toggle auto-approval\n  /clear - Clear messages\n  /quit - Exit\nShortcuts:\n  Ctrl+H: Harness | Ctrl+M: Model | Ctrl+E: Effort/Think | Ctrl+P: Auto Toggle | Tab: Complete";
            app.add_system_message(help.to_string());
        }
        "/quit" | "/exit" => {
            app.should_quit = true;
        }
        _ => {
            app.add_error_message(format!(
                "Unknown command '{}'. Type /help for available commands.",
                cmd
            ));
        }
    }
}
