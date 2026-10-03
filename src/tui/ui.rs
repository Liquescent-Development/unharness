use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::app::{ActivePopup, App, MessageRole, EFFORT_LEVELS};
use super::markdown::render_markdown_to_lines;

pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(5),    // Messages viewport
            Constraint::Length(3), // Input box
            Constraint::Length(1), // Footer status hints
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);
    render_messages(frame, app, chunks[1]);
    render_input(frame, app, chunks[2]);
    render_footer(frame, app, chunks[3]);

    // Render suggestions above input if active
    if !app.suggestions.is_empty() && app.popup.is_none() {
        render_suggestions(frame, app, chunks[2]);
    }

    // Render modal popup if open
    if let Some(popup) = app.popup {
        match popup {
            ActivePopup::HarnessPicker => render_harness_picker(frame, app, frame.area()),
            ActivePopup::ModelPicker => render_model_picker(frame, app, frame.area()),
            ActivePopup::EffortPicker => render_effort_picker(frame, app, frame.area()),
        }
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let harness_color = match app.active_harness {
        crate::harness::HarnessKind::Agy => Color::Cyan,
        crate::harness::HarnessKind::Claude => Color::Magenta,
        crate::harness::HarnessKind::Codex => Color::Green,
    };

    let auto_color = if app.auto_approve {
        Color::Green
    } else {
        Color::DarkGray
    };

    let model_str = app.current_model().unwrap_or_else(|| "default".to_string());
    let effort_str = app.current_effort();

    let status_span = if app.is_generating {
        let label = if app.is_thinking { "Thinking" } else { "Working" };
        let elapsed = app.current_elapsed_secs();
        Span::styled(
            format!(" {} {} ({:.1}s) ", app.current_spinner(), label, elapsed),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else if let Some(dur) = app.generation_duration {
        Span::styled(
            format!(" Done ({:.1}s) ", dur.as_secs_f32()),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::styled(" Idle ", Style::default().fg(Color::DarkGray))
    };

    let title_line = Line::from(vec![
        Span::styled(" UNHARNESS ", Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(
            format!("[{}]", app.active_harness.display_name()),
            Style::default().fg(harness_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[Model: {}]", model_str),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[Effort: {}]", effort_str),
            Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[Auto: {}]", if app.auto_approve { "ON" } else { "OFF" }),
            Style::default().fg(auto_color),
        ),
        Span::raw(" "),
        status_span,
    ]);

    let cwd_display = app.cwd.to_string_lossy();
    let header_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title_line)
        .title_alignment(ratatui::layout::Alignment::Left);

    let info_paragraph = Paragraph::new(format!("  Repository: {}", cwd_display))
        .style(Style::default().fg(Color::Gray))
        .block(header_block);

    frame.render_widget(info_paragraph, area);
}

fn render_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Activity ");

    let inner_area = block.inner(area);
    let max_text_width = (inner_area.width.saturating_sub(4)).max(10) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();

    for msg in &app.messages {
        match msg.role {
            MessageRole::User => {
                lines.push(Line::from(vec![
                    Span::styled("❯ You", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                ]));
                lines.extend(wrap_prefixed_text("  ", &msg.content, max_text_width, Style::default().fg(Color::White)));
                lines.push(Line::default());
            }
            MessageRole::Assistant => {
                let dur_str = msg.duration.map(|d| format!(" ({:.1}s)", d.as_secs_f32())).unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("● {}{}", msg.sender, dur_str),
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    ),
                ]));
                let md_lines = render_markdown_to_lines(&msg.content, max_text_width);
                lines.extend(md_lines);
                lines.push(Line::default());
            }
            MessageRole::Thought => {
                let is_live_thought = app.is_generating && app.is_thinking;
                let title = if is_live_thought {
                    format!("  ┌─ 💭 Thinking ({:.1}s) ──────────────────────────", app.current_elapsed_secs())
                } else if let Some(dur) = msg.duration {
                    format!("  ┌─ 💭 Thought for {:.1}s ────────────────────────", dur.as_secs_f32())
                } else {
                    "  ┌─ 💭 Thinking Process ──────────────────────────".to_string()
                };

                lines.push(Line::from(Span::styled(
                    title,
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )));

                let inner_thought_width = max_text_width.saturating_sub(6).max(10);
                for raw_l in msg.content.lines() {
                    let wrapped = wrap_words_helper(raw_l, inner_thought_width);
                    for w in wrapped {
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                            Span::styled(w, Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
                        ]));
                    }
                }

                lines.push(Line::from(Span::styled(
                    "  └────────────────────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::default());
            }
            MessageRole::Tool => {
                lines.extend(wrap_prefixed_text(
                    "  ⚡ ",
                    &msg.content,
                    max_text_width,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
                ));
            }
            MessageRole::System => {
                lines.extend(wrap_prefixed_text(
                    "  ℹ ",
                    &msg.content,
                    max_text_width,
                    Style::default().fg(Color::Blue).add_modifier(Modifier::ITALIC),
                ));
                lines.push(Line::default());
            }
            MessageRole::Error => {
                lines.extend(wrap_prefixed_text(
                    "  ✗ ",
                    &msg.content,
                    max_text_width,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::default());
            }
        }
    }

    let total_visual_lines = lines.len() as u16;
    let available_height = inner_area.height;
    let max_scroll = total_visual_lines.saturating_sub(available_height);

    if app.auto_scroll {
        app.scroll = max_scroll;
    } else if app.scroll >= max_scroll {
        app.scroll = max_scroll;
        app.auto_scroll = true;
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((app.scroll, 0));

    frame.render_widget(paragraph, area);
}

fn wrap_prefixed_text(
    prefix: &'static str,
    text: &str,
    max_width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let prefix_width = UnicodeWidthStr::width(prefix);
    let indent = " ".repeat(prefix_width);
    let content_width = max_width.saturating_sub(prefix_width).max(10);

    for raw_line in text.lines() {
        if raw_line.is_empty() {
            lines.push(Line::default());
            continue;
        }

        let wrapped = wrap_words_helper(raw_line, content_width);
        for (i, segment) in wrapped.into_iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(segment, style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(segment, style),
                ]));
            }
        }
    }
    lines
}

fn wrap_words_helper(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;

    for word in text.split(' ') {
        let word_len = UnicodeWidthStr::width(word);

        // If a single word is wider than max_width, force-break it by characters
        if word_len > max_width {
            if !current.is_empty() {
                lines.push(current);
                current = String::new();
                current_len = 0;
            }
            let mut chunk = String::new();
            let mut chunk_len = 0;
            for c in word.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
                if chunk_len + cw > max_width {
                    lines.push(chunk);
                    chunk = String::new();
                    chunk_len = 0;
                }
                chunk.push(c);
                chunk_len += cw;
            }
            if !chunk.is_empty() {
                current = chunk;
                current_len = chunk_len;
            }
            continue;
        }

        if current_len == 0 {
            current.push_str(word);
            current_len += word_len;
        } else if current_len + 1 + word_len <= max_width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push(current);
            current = word.to_string();
            current_len = word_len;
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(if app.is_generating {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Cyan)
        })
        .title(" Prompt (type / for commands) ");

    let input_paragraph = Paragraph::new(app.input.as_str())
        .style(Style::default().fg(Color::White))
        .block(input_block);

    frame.render_widget(input_paragraph, area);

    if !app.is_generating && app.popup.is_none() {
        let cursor_x = area.x + 1 + app.cursor_pos as u16;
        let cursor_y = area.y + 1;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_footer(frame: &mut Frame, _app: &App, area: Rect) {
    let footer_text = Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" Send  "),
        Span::styled("Ctrl+H", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(" Harness  "),
        Span::styled("Ctrl+M", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::raw(" Model  "),
        Span::styled("Ctrl+E", Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD)),
        Span::raw(" Effort/Think  "),
        Span::styled("Ctrl+P", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" Auto  "),
        Span::styled("PgUp/PgDn", Style::default().fg(Color::DarkGray)),
        Span::raw(" Scroll  "),
        Span::styled("Ctrl+C / Esc", Style::default().fg(Color::Red)),
        Span::raw(" Cancel/Quit"),
    ]);

    let footer = Paragraph::new(footer_text);
    frame.render_widget(footer, area);
}

fn render_suggestions(frame: &mut Frame, app: &App, input_area: Rect) {
    let count = app.suggestions.len().min(8) as u16;
    let height = count + 2;
    if input_area.y < height {
        return;
    }

    let width = 72.min(input_area.width.saturating_sub(4));
    let area = Rect {
        x: input_area.x + 1,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };

    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Suggestions [Tab to Complete] ");

    let mut lines = Vec::new();
    for (i, (cmd, desc)) in app.suggestions.iter().take(count as usize).enumerate() {
        let is_selected = i == app.selected_suggestion;
        let prefix = if is_selected { "❯ " } else { "  " };

        let style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{}{:<22} ", prefix, cmd), style),
            Span::styled(format!("─ {}", desc), Style::default().fg(if is_selected { Color::DarkGray } else { Color::Gray })),
        ]));
    }

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, area);
}

fn render_harness_picker(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(65, 45, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .title(" Switch AI Harness (↑/↓ Select, Enter Confirm, Esc Cancel) ")
        .title_alignment(ratatui::layout::Alignment::Center);

    let mut lines = Vec::new();
    lines.push(Line::default());

    for (i, opt) in app.harness_options.iter().enumerate() {
        let is_selected = i == app.picker_selected;
        let is_active = opt.kind == app.active_harness;

        let cursor = if is_selected { " ❯ " } else { "   " };
        let active_badge = if is_active { " [Active]" } else { "" };

        let status_str = if opt.installed {
            let ver = opt.version.as_deref().unwrap_or("detected");
            format!("(v{})", ver)
        } else {
            "(not found on PATH)".to_string()
        };

        let style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else if !opt.installed {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };

        let active_style = if is_active {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        lines.push(Line::from(vec![
            Span::styled(cursor, style),
            Span::styled(format!("{:<24}", opt.kind.display_name()), style),
            Span::styled(format!(" {:<20}", status_str), Style::default().fg(if opt.installed { Color::Gray } else { Color::Red })),
            Span::styled(active_badge, active_style),
        ]));
        lines.push(Line::default());
    }

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, popup_area);
}

fn render_model_picker(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(75, 55, area);
    frame.render_widget(Clear, popup_area);

    let title = format!(
        " Select Model for {} (↑/↓ Select, Enter Confirm, Esc Cancel) ",
        app.active_harness.display_name()
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
        .title(title)
        .title_alignment(ratatui::layout::Alignment::Center);

    let mut lines = Vec::new();
    lines.push(Line::default());

    let current_model_id = app.current_model();

    for (i, model) in app.model_options.iter().enumerate() {
        let is_selected = i == app.model_picker_selected;
        let is_active = current_model_id.as_deref() == Some(&model.id);

        let cursor = if is_selected { " ❯ " } else { "   " };
        let active_badge = if is_active { " [Active]" } else { "" };

        let style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let active_style = if is_active {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let desc = model.description.as_deref().unwrap_or(model.display_name.as_str());

        lines.push(Line::from(vec![
            Span::styled(cursor, style),
            Span::styled(format!("{:<24} ", model.id), style),
            Span::styled(format!("─ {:<30} ", desc), Style::default().fg(if is_selected { Color::White } else { Color::Gray })),
            Span::styled(active_badge, active_style),
        ]));
        lines.push(Line::default());
    }

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, popup_area);
}

fn render_effort_picker(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(75, 45, area);
    frame.render_widget(Clear, popup_area);

    let title = format!(
        " Select Reasoning Effort / Think Mode for {} (↑/↓ Select, Enter Confirm, Esc Cancel) ",
        app.active_harness.display_name()
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD))
        .title(title)
        .title_alignment(ratatui::layout::Alignment::Center);

    let mut lines = Vec::new();
    lines.push(Line::default());

    let current_effort = app.current_effort();

    for (i, eff) in EFFORT_LEVELS.iter().enumerate() {
        let is_selected = i == app.effort_picker_selected;
        let is_active = current_effort == eff.id;

        let cursor = if is_selected { " ❯ " } else { "   " };
        let active_badge = if is_active { " [Active]" } else { "" };

        let style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::LightBlue).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let active_style = if is_active {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        lines.push(Line::from(vec![
            Span::styled(cursor, style),
            Span::styled(format!("{:<10} ", eff.display_name), style),
            Span::styled(format!("─ {:<44} ", eff.description), Style::default().fg(if is_selected { Color::White } else { Color::Gray })),
            Span::styled(active_badge, active_style),
        ]));
        lines.push(Line::default());
    }

    let p = Paragraph::new(lines).block(block);
    frame.render_widget(p, popup_area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_prefixed_text() {
        let long_err = "jetski: no output produced — a tool required the \"mcp\" permission that headless mode cannot prompt for, so it was auto-denied. Add an allow-rule under permissions.allow in settings.json (e.g. mcp(<target>)).";
        let lines = wrap_prefixed_text("  ✗ ", long_err, 60, Style::default().fg(Color::Red));
        assert!(lines.len() >= 3);
        // Prefix is on first line
        assert_eq!(lines[0].spans[0].content, "  ✗ ");
        // Continuation lines start with spaces
        assert_eq!(lines[1].spans[0].content, "    ");
    }
}
