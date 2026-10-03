//! Rendering. Reads `App` state only; no side effects beyond the frame.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::app::App;
use super::code::{
    clamp_lines, code_lines, diff_lines, looks_like_diff, plain_lines, replacement_lines, sanitize,
    wrap_words,
};
use super::markdown::render_markdown_to_lines;
use super::modal::{ListPicker, Modal};
use super::transcript::{Block as TBlock, tool_summary, tool_summary_full, truncate_chars};
use crate::core::{HarnessId, PermissionKind, PermissionPolicy, PlanStatus};

pub fn render(frame: &mut Frame, app: &mut App) {
    let warning = app.policy_warning();
    let bottom_height = 1 + 1 + 1 + 2 + usize::from(warning.is_some()) + 1; // rule, prompt, rule, info x2, warning?, footer
    let pinned = pinned_lines(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(pinned.len() as u16),
            Constraint::Length(bottom_height as u16),
        ])
        .split(frame.area());

    render_transcript(frame, app, chunks[0]);
    if !pinned.is_empty() {
        frame.render_widget(Paragraph::new(pinned), chunks[1]);
    }
    let prompt_row = render_bottom(frame, app, chunks[2], warning.as_deref());

    if !app.suggestions.is_empty() && app.modal.is_none() {
        render_suggestions(frame, app, prompt_row);
    }
    if app.modal.is_some() {
        render_modal(frame, app, frame.area());
    }
}

/// Most plan rows shown above the prompt; the rest are summarised.
const PLAN_ROWS: usize = 6;

/// Lines pinned between the transcript and the prompt.
fn pinned_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if app.show_plan && !app.plan.is_empty() {
        // When the plan is long, start at the first unfinished step.
        let first_open = app
            .plan
            .iter()
            .position(|e| e.status != PlanStatus::Completed)
            .unwrap_or(0);
        let start = if app.plan.len() > PLAN_ROWS {
            first_open.min(app.plan.len() - PLAN_ROWS)
        } else {
            0
        };
        let done = app
            .plan
            .iter()
            .filter(|e| e.status == PlanStatus::Completed)
            .count();
        lines.push(Line::from(Span::styled(
            format!("  Plan · {done}/{} done", app.plan.len()),
            Style::default().fg(Color::DarkGray),
        )));
        for entry in app.plan.iter().skip(start).take(PLAN_ROWS) {
            let style = match entry.status {
                PlanStatus::Completed => Style::default().fg(Color::DarkGray),
                PlanStatus::InProgress => Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                PlanStatus::Pending => Style::default(),
            };
            lines.push(Line::from(Span::styled(
                format!("  {} {}", entry.status.marker(), sanitize(&entry.text)),
                style,
            )));
        }
    }
    lines
}

fn harness_color(id: HarnessId) -> Color {
    match id {
        HarnessId::Agy => Color::Cyan,
        HarnessId::Claude => Color::Magenta,
        HarnessId::Codex => Color::Green,
        HarnessId::Pi => Color::Yellow,
    }
}

fn policy_color(p: PermissionPolicy) -> Color {
    match p {
        PermissionPolicy::Ask => Color::Green,
        PermissionPolicy::AcceptEdits => Color::Cyan,
        PermissionPolicy::Auto => Color::Yellow,
        PermissionPolicy::Bypass => Color::Red,
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn render_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " unharness ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let width = (inner.width.saturating_sub(4)).max(10) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    let thinking_live = app.is_thinking();
    let elapsed = app.elapsed_secs();

    for b in &app.transcript.blocks {
        match b {
            TBlock::User { text } => {
                lines.push(Line::from(Span::styled(
                    "❯ You",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(wrap_prefixed_text("  ", text, width, Style::default()));
                lines.push(Line::default());
            }
            TBlock::Assistant {
                text,
                sender,
                duration,
            } => {
                let dur = duration
                    .map(|d| format!(" ({:.1}s)", d.as_secs_f32()))
                    .unwrap_or_default();
                lines.push(Line::from(Span::styled(
                    format!("● {sender}{dur}"),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(render_markdown_to_lines(text, width));
                lines.push(Line::default());
            }
            TBlock::Thought { text, duration } => {
                let title = match duration {
                    Some(d) => format!("  ┌─ 💭 Thought for {:.1}s ", d.as_secs_f32()),
                    None if thinking_live => format!("  ┌─ 💭 Thinking ({elapsed:.1}s) "),
                    None => "  ┌─ 💭 Thinking ".to_string(),
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{title}{}",
                        "─".repeat(width.saturating_sub(title.len()).min(40))
                    ),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
                let inner_w = width.saturating_sub(6).max(10);
                for raw in text.lines() {
                    for w in wrap_words(raw, inner_w) {
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                w,
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::ITALIC),
                            ),
                        ]));
                    }
                }
                lines.push(Line::from(Span::styled(
                    "  └".to_string() + &"─".repeat(width.saturating_sub(3).min(50)),
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::default());
            }
            TBlock::Tool {
                name,
                input,
                output,
                is_error,
                done,
                collapsed,
                duration,
                ..
            } => {
                let status = if !*done {
                    Span::styled(" ⠿ running", Style::default().fg(Color::Yellow))
                } else if *is_error {
                    Span::styled(" ✗", Style::default().fg(Color::Red))
                } else {
                    Span::styled(
                        format!(
                            " ✓{}",
                            duration
                                .map(|d| format!(" {:.1}s", d.as_secs_f32()))
                                .unwrap_or_default()
                        ),
                        Style::default().fg(Color::Green),
                    )
                };
                let summary = tool_summary_full(name, input);
                let head_width = width.saturating_sub(name.len() + 18).max(10);
                let mut summary_lines = wrap_words(&summary, head_width).into_iter();
                let mut first = vec![
                    Span::styled("  ⚡ ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        summary_lines.next().unwrap_or_default(),
                        Style::default().fg(Color::Gray),
                    ),
                ];
                let rest: Vec<String> = summary_lines.collect();
                if rest.is_empty() {
                    first.push(status);
                    lines.push(Line::from(first));
                } else {
                    lines.push(Line::from(first));
                    let n = rest.len();
                    for (i, seg) in rest.into_iter().enumerate() {
                        let mut l = vec![
                            Span::raw(" ".repeat(name.len() + 7)),
                            Span::styled(seg, Style::default().fg(Color::Gray)),
                        ];
                        if i + 1 == n {
                            l.push(status.clone());
                        }
                        lines.push(Line::from(l));
                    }
                }

                let body = tool_body_lines(name, input, output, *is_error, width.saturating_sub(2));
                let limit = if *collapsed { 4 } else { usize::MAX };
                let total = body.len();
                lines.extend(body.into_iter().take(limit));
                if *collapsed && total > limit {
                    lines.push(Line::from(Span::styled(
                        format!("  │ … {} more lines (Ctrl+O to expand)", total - limit),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            TBlock::System(t) => {
                lines.extend(wrap_prefixed_text(
                    "  ℹ ",
                    t,
                    width,
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::ITALIC),
                ));
                lines.push(Line::default());
            }
            TBlock::Notice(t) => {
                lines.extend(wrap_prefixed_text(
                    "  · ",
                    t,
                    width,
                    Style::default().fg(Color::DarkGray),
                ));
            }
            TBlock::Error(t) => {
                lines.extend(wrap_prefixed_text(
                    "  ✗ ",
                    t,
                    width,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::default());
            }
        }
    }

    clamp_lines(&mut lines, inner.width as usize);
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner.height);
    if app.auto_scroll || app.scroll >= max_scroll {
        app.scroll = max_scroll;
        app.auto_scroll = true;
    }
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((app.scroll, 0)),
        area,
    );
}

/// Body of a tool block: file edits as syntax-coloured diffs, file writes and
/// reads highlighted by extension, diffs red/green, everything else plain.
fn tool_body_lines(
    name: &str,
    input: &serde_json::Value,
    output: &str,
    is_error: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let get = |k: &str| input.get(k).and_then(serde_json::Value::as_str);
    let path = get("file_path").or_else(|| get("path")).unwrap_or("");
    let lname = name.to_lowercase();
    let mut lines = Vec::new();

    if (lname.contains("edit") || lname == "str_replace" || lname == "replace")
        && let (Some(old), Some(new)) = (
            get("old_string").or_else(|| get("old_str")),
            get("new_string").or_else(|| get("new_str")),
        )
    {
        lines.extend(replacement_lines(old, new, path, width));
    } else if (lname == "write" || lname == "write_file" || lname == "create_file")
        && let Some(content) = get("content").or_else(|| get("contents"))
    {
        lines.extend(replacement_lines("", content, path, width));
    } else if (lname == "read" || lname == "read_file") && !output.is_empty() {
        lines.extend(code_lines(output, path, width));
    }

    if !output.is_empty() && (lines.is_empty() || is_error) {
        let style = if is_error {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Gray)
        };
        if !is_error && looks_like_diff(output) {
            lines.extend(diff_lines(output, width));
        } else {
            lines.extend(plain_lines(output, width, style));
        }
    }
    lines
}

pub fn wrap_prefixed_text(
    prefix: &'static str,
    text: &str,
    max_width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let prefix_width = UnicodeWidthStr::width(prefix);
    let indent = " ".repeat(prefix_width);
    let content_width = max_width.saturating_sub(prefix_width).max(10);
    for raw in text.lines() {
        if raw.is_empty() {
            lines.push(Line::default());
            continue;
        }
        for (i, seg) in wrap_words(raw, content_width).into_iter().enumerate() {
            let head = if i == 0 {
                Span::styled(prefix, style)
            } else {
                Span::raw(indent.clone())
            };
            lines.push(Line::from(vec![head, Span::styled(seg, style)]));
        }
    }
    lines
}

/// Status rule, prompt, rule, context lines, optional warning, key hints.
/// Returns the prompt row's rect (for the suggestions popup).
fn render_bottom(frame: &mut Frame, app: &App, area: Rect, warning: Option<&str>) -> Rect {
    let mut constraints = vec![
        Constraint::Length(1), // status rule
        Constraint::Length(1), // prompt
        Constraint::Length(1), // rule
        Constraint::Length(1), // where / what
        Constraint::Length(1), // usage / session
    ];
    if warning.is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // footer
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let width = area.width as usize;
    let rule_style = Style::default().fg(Color::DarkGray);

    // Status rule: ── ⠇ Running shell · 12s ─────
    let status = if app.is_generating {
        Span::styled(
            format!(
                " {} {} · {:.0}s ",
                app.spinner(),
                app.status_label(),
                app.elapsed_secs()
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if app
        .modal
        .as_ref()
        .is_some_and(super::modal::Modal::is_prompt)
    {
        Span::styled(" Waiting for you ", Style::default().fg(Color::Cyan))
    } else if let Some(d) = app.generation_duration {
        Span::styled(
            format!(" Ready · last turn {:.1}s ", d.as_secs_f32()),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::styled(" Ready ", Style::default().fg(Color::DarkGray))
    };
    let status_w = UnicodeWidthStr::width(status.content.as_ref());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("──", rule_style),
            status,
            Span::styled("─".repeat(width.saturating_sub(status_w + 2)), rule_style),
        ])),
        rows[0],
    );

    // Prompt line
    let prompt_style = if app.is_generating {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", prompt_style),
            Span::raw(sanitize(&app.input)),
        ])),
        rows[1],
    );
    if app.modal.is_none() {
        let before: String = app.input.chars().take(app.cursor).collect();
        let x = rows[1].x + 2 + UnicodeWidthStr::width(before.as_str()) as u16;
        frame.set_cursor_position((
            x.min(rows[1].x + rows[1].width.saturating_sub(1)),
            rows[1].y,
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("─".repeat(width), rule_style))),
        rows[2],
    );

    // Line 1: where (left) · harness/model/effort/policy (right)
    let mut cwd = app.cwd.to_string_lossy().to_string();
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = app.cwd.strip_prefix(&home)
    {
        cwd = format!("~/{}", rest.to_string_lossy());
    }
    let left1 = Line::from(vec![
        Span::styled(cwd, Style::default().fg(Color::Gray)),
        Span::styled(
            app.git_branch
                .as_ref()
                .map(|b| format!(" ({b})"))
                .unwrap_or_default(),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let effective = app.effective_policy();
    let policy_text = if effective == app.policy_requested {
        format!("policy {effective}")
    } else {
        format!("policy {}→{}", app.policy_requested, effective)
    };
    let right1 = Line::from(vec![
        Span::styled(
            format!("[{}] ", app.active.short_name()),
            Style::default()
                .fg(harness_color(app.active))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(app.model_label(), Style::default().fg(Color::Yellow)),
        Span::styled(
            format!(" · effort {}", app.current_effort().unwrap_or("default")),
            Style::default().fg(Color::LightBlue),
        ),
        Span::styled(
            format!(" · {policy_text}"),
            Style::default().fg(policy_color(effective)),
        ),
    ]);
    render_split(frame, rows[3], left1, right1);

    // Line 2: usage (left) · session id (right)
    let t = &app.turn_usage;
    let s = &app.session_usage;
    let mut usage = format!(
        "↑{} ↓{} · session {}",
        fmt_tokens(t.input + t.cache_read + t.cache_write),
        fmt_tokens(t.output),
        fmt_tokens(s.total_tokens())
    );
    if let Some(c) = s.cost_usd {
        usage.push_str(&format!(" · ${c:.2}"));
    }
    if let Some(pct) = app.context.percent() {
        usage.push_str(&format!(" · ctx {pct}%"));
    }
    let left2 = Line::from(Span::styled(usage, Style::default().fg(Color::Gray)));
    let right2 = Line::from(Span::styled(
        app.session_ids
            .get(&app.active)
            .map(|id| {
                format!(
                    "conv {} · session {}",
                    &app.conversation.id[..8.min(app.conversation.id.len())],
                    truncate_chars(id, 12)
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "conv {}",
                    &app.conversation.id[..8.min(app.conversation.id.len())]
                )
            }),
        Style::default().fg(Color::DarkGray),
    ));
    render_split(frame, rows[4], left2, right2);

    let mut next = 5;
    if let Some(w) = warning {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("⚠ {w}"),
                Style::default().fg(Color::Yellow),
            ))),
            rows[next],
        );
        next += 1;
    }

    let k = |s: &'static str, c: Color| {
        Span::styled(s, Style::default().fg(c).add_modifier(Modifier::BOLD))
    };
    let footer = Line::from(vec![
        k("Enter", Color::Cyan),
        Span::raw(" send · "),
        k("^H", Color::Yellow),
        Span::raw(" harness · "),
        k("^M", Color::Magenta),
        Span::raw(" model · "),
        k("^E", Color::LightBlue),
        Span::raw(" effort · "),
        k("^P", Color::Green),
        Span::raw(" policy · "),
        k("^R", Color::Blue),
        Span::raw(" resume · "),
        k("^O", Color::Gray),
        Span::raw(" expand · "),
        k("Esc", Color::Red),
        Span::raw(" interrupt · "),
        k("^D", Color::Red),
        Span::raw(" quit"),
    ]);
    let mut footer_lines = vec![footer];
    clamp_lines(&mut footer_lines, width);
    let footer = footer_lines.remove(0);
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        rows[next],
    );

    rows[1]
}

/// Left text and right text on one row; the right side is dropped first
/// when the terminal is too narrow for both.
fn render_split(frame: &mut Frame, row: Rect, left: Line<'static>, right: Line<'static>) {
    let width = row.width as usize;
    let lw = left.width();
    let rw = right.width();
    if lw + 2 + rw <= width {
        let mut spans = left.spans;
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right.spans);
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    } else if rw + 12 <= width {
        // Keep the right side; show the tail of the left (paths end in the
        // interesting part).
        let room = width - rw - 2;
        let text: String = left.spans.iter().map(|s| s.content.as_ref()).collect();
        let tail: String = text
            .chars()
            .rev()
            .take(room.saturating_sub(1))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let style = left.spans.first().map(|s| s.style).unwrap_or_default();
        let mut spans = vec![Span::styled(format!("…{tail}"), style)];
        let used = unicode_width::UnicodeWidthStr::width(tail.as_str()) + 1;
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used + rw))));
        spans.extend(right.spans);
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    } else {
        let mut lines = vec![left];
        clamp_lines(&mut lines, width);
        frame.render_widget(Paragraph::new(lines.remove(0)), row);
    }
}

fn render_suggestions(frame: &mut Frame, app: &App, prompt_row: Rect) {
    let count = app.suggestions.len().min(8) as u16;
    let height = count + 2;
    // Sits above the status rule so it never covers the prompt.
    let top = prompt_row.y.saturating_sub(1);
    if top < height {
        return;
    }
    let area = Rect {
        x: prompt_row.x + 1,
        y: top - height,
        width: 76.min(prompt_row.width.saturating_sub(4)),
        height,
    };
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (i, (cmd, desc)) in app.suggestions.iter().take(count as usize).enumerate() {
        let selected = i == app.selected_suggestion;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}{:<26} ", if selected { "❯ " } else { "  " }, cmd),
                style,
            ),
            Span::styled(format!("─ {desc}"), Style::default().fg(Color::Gray)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Suggestions [Tab to complete] "),
        ),
        area,
    );
}

// ------------------------------------------------------------------ modals

fn modal_block(title: String, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .title(title)
        .title_alignment(Alignment::Center)
}

fn picker_lines<T>(
    picker: &ListPicker<T>,
    color: Color,
    row: impl Fn(&T) -> (String, String, bool),
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    for (i, item) in picker.items.iter().enumerate() {
        let (label, desc, active) = row(item);
        let selected = i == picker.selected;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { " ❯ " } else { "   " }, style),
            Span::styled(format!("{label:<28} "), style),
            Span::styled(format!("─ {desc}"), Style::default().fg(Color::Gray)),
            Span::styled(
                if active { "  [active]" } else { "" },
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines
}

fn render_modal(frame: &mut Frame, app: &App, area: Rect) {
    let Some(modal) = &app.modal else { return };
    const NAV: &str = "↑/↓ select · Enter confirm · Esc cancel";
    let (popup, block, lines): (Rect, Block, Vec<Line<'static>>) = match modal {
        Modal::Harness(p) => (
            centered_rect(70, 50, area),
            modal_block(format!(" Harness ({NAV}) "), Color::Cyan),
            picker_lines(p, Color::Cyan, |o| {
                let status = if o.installed {
                    format!(
                        "v{}{}",
                        o.version.as_deref().unwrap_or("?"),
                        if o.interactive_permissions {
                            ", interactive permissions"
                        } else {
                            ", no permission prompts"
                        }
                    )
                } else {
                    "not found on PATH".to_string()
                };
                (o.display_name.to_string(), status, o.id == app.active)
            }),
        ),
        Modal::Provider(p) => (
            centered_rect(60, 50, area),
            modal_block(format!(" Provider ({NAV}) "), Color::Yellow),
            picker_lines(p, Color::Yellow, |o| {
                (
                    o.name.clone(),
                    o.id.clone(),
                    app.current_provider().is_some_and(|c| c.0 == o.id),
                )
            }),
        ),
        Modal::Model(p) => (
            centered_rect(80, 60, area),
            modal_block(
                format!(" Model for {} ({NAV}) ", app.active.short_name()),
                Color::Magenta,
            ),
            picker_lines(p, Color::Magenta, |m| {
                let mut desc = m
                    .description
                    .clone()
                    .unwrap_or_else(|| m.display_name.clone());
                if let Some(e) = &m.effort_levels {
                    desc.push_str(&format!("  [{}]", e.join("/")));
                }
                (
                    m.model_ref.model.clone(),
                    desc,
                    app.current_model()
                        .is_some_and(|c| c.model == m.model_ref.model),
                )
            }),
        ),
        Modal::Effort(p) => (
            centered_rect(60, 50, area),
            modal_block(format!(" Reasoning effort ({NAV}) "), Color::LightBlue),
            picker_lines(p, Color::LightBlue, |e| {
                (
                    e.clone(),
                    String::new(),
                    app.current_effort() == Some(e.as_str()),
                )
            }),
        ),
        Modal::Policy(p) => {
            let caps = app.caps();
            (
                centered_rect(80, 50, area),
                modal_block(format!(" Permission policy ({NAV}) "), Color::Green),
                picker_lines(p, Color::Green, |pol| {
                    let support = match caps.supports_policy(*pol) {
                        Some(s) => s
                            .degraded
                            .map(|d| format!(" (degraded: {d})"))
                            .unwrap_or_default(),
                        None => " (not supported here; falls back)".to_string(),
                    };
                    (
                        pol.as_str().to_string(),
                        format!("{}{}", pol.description(), support),
                        *pol == app.policy_requested,
                    )
                }),
            )
        }
        Modal::Resume(p) => (
            centered_rect(85, 60, area),
            modal_block(format!(" Resume conversation ({NAV}) "), Color::Blue),
            picker_lines(p, Color::Blue, |r| {
                (
                    format!("{}  {}", &r.id[..8.min(r.id.len())], r.updated_at),
                    format!(
                        "[{}]  {}",
                        r.harnesses
                            .iter()
                            .map(|h| h.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        truncate_chars(&r.title, 50)
                    ),
                    r.id == app.conversation.id,
                )
            }),
        ),
        Modal::Permission(m) => {
            let popup = centered_rect(80, 55, area);
            let width = (popup.width.saturating_sub(6)).max(20) as usize;
            let mut lines = vec![Line::default()];
            if let PermissionKind::ToolUse {
                tool,
                input,
                suggestions,
                description,
            } = &m.request.kind
            {
                lines.push(Line::from(vec![
                    Span::styled("  Tool: ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        tool.clone(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                let summary = description
                    .clone()
                    .unwrap_or_else(|| tool_summary(tool, input));
                lines.extend(wrap_prefixed_text("  ", &summary, width, Style::default()));
                lines.push(Line::default());
                let pretty = if m.show_input {
                    serde_json::to_string_pretty(input).unwrap_or_default()
                } else {
                    input
                        .as_object()
                        .map(|o| {
                            o.iter()
                                .map(|(k, v)| {
                                    let s = match v {
                                        serde_json::Value::String(s) => s.clone(),
                                        v => v.to_string(),
                                    };
                                    format!("{k}: {}", truncate_chars(&s.replace('\n', "⏎"), 100))
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_else(|| input.to_string())
                };
                lines.extend(wrap_prefixed_text(
                    "    ",
                    &pretty,
                    width,
                    Style::default().fg(Color::DarkGray),
                ));
                lines.push(Line::default());
                if m.denying {
                    lines.push(Line::from(vec![
                        Span::styled("  Reason: ", Style::default().fg(Color::Red)),
                        Span::raw(m.reason.clone()),
                        Span::styled("▏", Style::default().fg(Color::Red)),
                    ]));
                    lines.push(Line::from(Span::styled(
                        "  Enter send · Esc back",
                        Style::default().fg(Color::Gray),
                    )));
                } else {
                    let always = if suggestions.is_some() {
                        "a allow always · "
                    } else {
                        ""
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  y/Enter allow once · {always}n deny · i {} input · Esc cancel",
                            if m.show_input { "hide" } else { "full" }
                        ),
                        Style::default().fg(Color::Gray),
                    )));
                }
            }
            (
                popup,
                modal_block(" Permission request ".into(), Color::Red),
                lines,
            )
        }
        Modal::Question(m) => {
            let popup = centered_rect(80, 60, area);
            let width = (popup.width.saturating_sub(6)).max(20) as usize;
            let q = m.current();
            let mut lines = vec![
                Line::default(),
                Line::from(Span::styled(
                    format!("  {} ({}/{})", q.header, m.idx + 1, m.questions.len()),
                    Style::default().fg(Color::Gray),
                )),
            ];
            lines.extend(wrap_prefixed_text(
                "  ",
                &q.text,
                width,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::default());
            for (i, (label, desc)) in q.options.iter().enumerate() {
                let selected = i == m.cursor;
                let chosen = m.chosen[m.idx][i];
                let mark = if q.multi {
                    if chosen { "[x]" } else { "[ ]" }
                } else if chosen {
                    "(•)"
                } else {
                    "( )"
                };
                let style = if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} {mark} {label:<24}", if selected { " ❯" } else { "  " }),
                        style,
                    ),
                    Span::styled(format!("  {desc}"), Style::default().fg(Color::Gray)),
                ]));
            }
            if q.allow_other {
                let selected = m.is_other_row();
                let style = if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let text = &m.other[m.idx];
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} [other] ", if selected { " ❯" } else { "  " }),
                        style,
                    ),
                    Span::raw(text.clone()),
                    Span::styled(
                        if m.editing_other { "▏" } else { "" },
                        Style::default().fg(Color::Cyan),
                    ),
                ]));
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                if m.editing_other {
                    "  type your answer · Enter done".to_string()
                } else if q.multi {
                    "  Space toggle · Enter next/submit · ←/→ page · Esc dismiss".to_string()
                } else {
                    "  Enter choose · ←/→ page · Esc dismiss".to_string()
                },
                Style::default().fg(Color::Gray),
            )));
            (
                popup,
                modal_block(" The agent has a question ".into(), Color::Cyan),
                lines,
            )
        }
        Modal::Confirm(m) => {
            let popup = centered_rect(60, 30, area);
            let width = (popup.width.saturating_sub(6)).max(20) as usize;
            let mut lines = vec![Line::default()];
            lines.extend(wrap_prefixed_text(
                "  ",
                &m.title,
                width,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            if let Some(msg) = &m.message {
                lines.extend(wrap_prefixed_text("  ", msg, width, Style::default()));
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  y/Enter yes · n no · Esc dismiss",
                Style::default().fg(Color::Gray),
            )));
            (popup, modal_block(" Confirm ".into(), Color::Yellow), lines)
        }
        Modal::Select(m) => (
            centered_rect(60, 50, area),
            modal_block(
                format!(" {} ({NAV}) ", truncate_chars(&m.title, 50)),
                Color::Yellow,
            ),
            picker_lines(&m.picker, Color::Yellow, |o| {
                (o.clone(), String::new(), false)
            }),
        ),
        Modal::Input(m) => {
            let popup = centered_rect(70, 40, area);
            let width = (popup.width.saturating_sub(6)).max(20) as usize;
            let mut lines = vec![Line::default()];
            lines.extend(wrap_prefixed_text(
                "  ",
                &m.title,
                width,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::default());
            lines.extend(wrap_prefixed_text("  > ", &m.text, width, Style::default()));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                if m.multiline {
                    "  Ctrl+Enter submit · Enter newline · Esc dismiss"
                } else {
                    "  Enter submit · Esc dismiss"
                },
                Style::default().fg(Color::Gray),
            )));
            (
                popup,
                modal_block(" Input requested ".into(), Color::Yellow),
                lines,
            )
        }
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
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
        .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::test_app;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn wrap_prefixed_text_indents_continuations() {
        let long = "jetski: no output produced — a tool required the mcp permission that headless mode cannot prompt for, so it was auto-denied.";
        let lines = wrap_prefixed_text("  ✗ ", long, 60, Style::default());
        assert!(lines.len() >= 2);
        assert_eq!(lines[0].spans[0].content, "  ✗ ");
        assert_eq!(lines[1].spans[0].content, "    ");
    }

    #[test]
    fn wrap_words_breaks_long_tokens() {
        let w = wrap_words(&"x".repeat(25), 10);
        assert_eq!(w.len(), 3);
        assert_eq!(wrap_words("", 10), vec![String::new()]);
    }

    #[test]
    fn bottom_cluster_layout() {
        let mut app = test_app(HarnessId::Claude);
        app.submit_prompt("hello".into());
        app.take_actions();
        app.on_event(crate::core::AgentEvent::TextDelta("hi there".into()));
        app.on_event(crate::core::AgentEvent::Usage(crate::core::Usage {
            input: 1200,
            output: 40,
            cost_usd: Some(0.12),
            ..Default::default()
        }));
        app.on_event(crate::core::AgentEvent::TurnCompleted {
            stop_reason: crate::core::StopReason::Done,
        });
        for c in "/mo".chars() {
            app.insert_char(c);
        }
        let backend = TestBackend::new(110, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        if std::env::var("UNHARNESS_DUMP_UI").is_ok() {
            eprintln!("{}", rows.join("\n"));
        }
        let n = rows.len();
        assert!(
            rows[n - 1].contains("Enter") && rows[n - 1].ends_with("^D quit"),
            "footer: {}",
            rows[n - 1]
        );
        assert!(
            rows[n - 2].starts_with("↑1.2k ↓40")
                && rows[n - 2].contains("$0.12")
                && rows[n - 2].contains("conv "),
            "usage: {}",
            rows[n - 2]
        );
        assert!(
            rows[n - 3].contains("[Claude]") && rows[n - 3].contains("policy ask"),
            "context: {}",
            rows[n - 3]
        );
        assert!(
            rows[n - 4].chars().all(|c| c == '─'),
            "rule: {}",
            rows[n - 4]
        );
        assert!(rows[n - 5].starts_with("❯ /mo"), "prompt: {}", rows[n - 5]);
        assert!(
            rows[n - 6].contains("Ready · last turn"),
            "status: {}",
            rows[n - 6]
        );
        assert!(
            rows[0].contains("unharness"),
            "transcript title: {}",
            rows[0]
        );
        // suggestions popup sits above the status rule
        assert!(
            rows[..n - 6].iter().any(|r| r.contains("/model")),
            "suggestions missing"
        );
    }

    #[test]
    fn plan_and_context_are_shown() {
        use crate::core::{AgentEvent, ContextUsage, PlanEntry, PlanStatus};
        let mut app = test_app(HarnessId::Claude);
        let entries = (0..9)
            .map(|i| PlanEntry {
                text: format!("step {i}"),
                status: if i < 5 {
                    PlanStatus::Completed
                } else {
                    PlanStatus::Pending
                },
            })
            .collect();
        app.on_event(AgentEvent::PlanUpdated {
            entries,
            explanation: None,
        });
        app.on_event(AgentEvent::Context(ContextUsage {
            used: Some(50_000),
            window: Some(200_000),
        }));
        let screen = |app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
            term.draw(|f| render(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..30)
                .map(|y| {
                    (0..100)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = screen(&mut app);
        assert!(text.contains("Plan · 5/9 done"));
        // Long plans scroll to the unfinished part.
        assert!(!text.contains("step 2") && text.contains("[ ] step 8"));
        assert!(text.contains("ctx 25%"));

        app.handle_slash_command("/plan");
        assert!(!screen(&mut app).contains("Plan ·"));
    }

    #[test]
    fn renders_without_panicking_in_every_modal() {
        let mut app = test_app(HarnessId::Claude);
        app.submit_prompt("hello **world**".into());
        app.take_actions();
        app.on_event(crate::core::AgentEvent::ThinkingDelta("hmm".into()));
        app.on_event(crate::core::AgentEvent::ToolCallStarted {
            id: "t".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command":"ls"}),
        });
        app.on_event(crate::core::AgentEvent::ToolCallResult {
            id: "t".into(),
            output: "a\nb\nc\nd\ne".into(),
            is_error: false,
        });
        app.on_event(crate::core::AgentEvent::TextDelta("done".into()));
        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut app)).unwrap();

        app.open_harness_picker();
        term.draw(|f| render(f, &mut app)).unwrap();
        app.open_effort_picker();
        term.draw(|f| render(f, &mut app)).unwrap();
        app.open_policy_picker();
        term.draw(|f| render(f, &mut app)).unwrap();
        app.open_model_picker();
        term.draw(|f| render(f, &mut app)).unwrap();
        app.modal = None;
        app.on_event(crate::core::AgentEvent::PermissionRequest(
            crate::core::PermissionRequest {
                id: "p".into(),
                kind: PermissionKind::ToolUse {
                    tool: "Write".into(),
                    input: serde_json::json!({"file_path":"/x","content":"line1\nline2"}),
                    suggestions: Some(serde_json::json!([])),
                    description: None,
                },
                tool_call_id: None,
            },
        ));
        term.draw(|f| render(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Permission request"));
        assert!(text.contains("Write"));
    }
}
