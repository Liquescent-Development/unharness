//! Rendering. Reads `App` state only; no side effects beyond the frame.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use unicode_width::UnicodeWidthStr;

use super::app::{App, Scrollbar};
use super::code::{
    clamp_lines, code_lines, diff_lines, looks_like_diff, plain_lines, replacement_lines, sanitize,
    wrap_words,
};
use super::markdown::render_markdown_to_lines;
use super::modal::{ListPicker, Modal};
use super::prompt;
use super::transcript::{Block as TBlock, tool_summary, tool_summary_full, truncate_chars};
use crate::core::SandboxLevel;
use crate::core::{HarnessId, PermissionKind, PermissionPolicy, PlanStatus, SubagentStatus};

pub fn render(frame: &mut Frame, app: &mut App) {
    // The subagent in view may be gone (a rewind, /clear, another conversation).
    if app.viewing.is_some() && app.viewed().is_none() {
        app.close_subagent_view();
    }
    let warning = app.status_warning();
    let area = frame.area();
    // The prompt grows with its content up to a cap (less on a short
    // terminal), then scrolls to keep the cursor's row in view.
    app.prompt_width = (area.width as usize).saturating_sub(PROMPT_INDENT);
    let rows = app.prompt_rows();
    let prompt_height = rows
        .len()
        .min(prompt::MAX_ROWS)
        .min((area.height as usize / 3).max(1));
    let cursor_row = prompt::cursor_row(&rows, app.cursor);
    app.prompt_scroll = app
        .prompt_scroll
        .clamp((cursor_row + 1).saturating_sub(prompt_height), cursor_row)
        .min(rows.len() - prompt_height);
    let subagents = subagent_list_lines(app);
    let bottom_height =
        1 + prompt_height + 1 + subagents.len() + 2 + usize::from(warning.is_some()) + 1; // rule, prompt, rule, subagents, info x2, warning?, footer
    let pinned = pinned_lines(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(pinned.len() as u16),
            Constraint::Length(bottom_height as u16),
        ])
        .split(area);

    render_transcript(frame, app, chunks[0]);
    if !pinned.is_empty() {
        frame.render_widget(Paragraph::new(pinned), chunks[1]);
    }
    let prompt_row = render_bottom(frame, app, chunks[2], warning.as_deref(), subagents);

    if !app.suggestions.is_empty() && app.modal.is_none() {
        render_suggestions(frame, app, prompt_row);
    }
    if app.modal.is_some() {
        render_modal(frame, app, frame.area());
    }
}

/// Columns taken by the prompt marker.
const PROMPT_INDENT: usize = 2;

/// Most queued prompts shown above the prompt.
const QUEUE_ROWS: usize = 3;

/// Most plan rows shown above the prompt; the rest are summarised.
const PLAN_ROWS: usize = 6;

/// Most subagents shown under the prompt at once.
const SUBAGENT_ROWS: usize = 4;

/// The subagents listed under the prompt: those at work and those that
/// ended since the last prompt. Down from the prompt moves into it and
/// Enter opens the chosen one's transcript.
fn subagent_list_lines(app: &App) -> Vec<Line<'static>> {
    let rows = app.subagent_rows();
    if rows.is_empty() {
        return Vec::new();
    }
    let focus = app.subagent_focus_index();
    let running = rows.iter().filter(|r| r.status.is_none()).count();
    let count = match running {
        0 => format!("{} done", rows.len()),
        n => format!("{n} running"),
    };
    let hint = if app.viewing.is_some() {
        ""
    } else if focus.is_some() {
        " · ↑/↓ select · Enter open · Del remove a finished one · Esc back to the prompt"
    } else {
        " · ↓ to select one"
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("  Subagents · {count}{hint}"),
        Style::default().fg(Color::DarkGray),
    ))];
    // Keep the chosen row in the window.
    let start = focus
        .map_or(0, |f| (f + 1).saturating_sub(SUBAGENT_ROWS))
        .min(rows.len().saturating_sub(SUBAGENT_ROWS));
    for (i, a) in rows.iter().enumerate().skip(start).take(SUBAGENT_ROWS) {
        let chosen = focus == Some(i);
        let viewed = app.viewing.as_ref() == Some(&a.id);
        let (glyph, color) = match a.status {
            None => (app.spinner(), Color::Yellow),
            Some(SubagentStatus::Completed) => ("✓", Color::Green),
            Some(SubagentStatus::Failed) => ("✗", Color::Red),
            Some(SubagentStatus::Cancelled) => ("◼", Color::DarkGray),
        };
        let mut name = Style::default().add_modifier(Modifier::BOLD);
        if chosen || viewed {
            name = name.fg(Color::Cyan);
        }
        let mut rest = String::new();
        if let Some(kind) = &a.kind {
            rest.push_str(&format!(" ({kind})"));
        }
        rest.push_str(&format!(" · {:.0}s", a.secs));
        if a.tools > 0 {
            let calls = if a.tools == 1 { "call" } else { "calls" };
            rest.push_str(&format!(" · {} tool {calls}", a.tools));
        }
        match (&a.activity, a.status) {
            (Some(activity), None) => {
                rest.push_str(&format!(" · {}", truncate_chars(activity, 60)))
            }
            (_, Some(status)) => rest.push_str(&format!(" · {}", status.label())),
            _ => {}
        }
        lines.push(Line::from(vec![
            Span::styled(
                if chosen { "❯ " } else { "  " },
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  ".repeat(a.depth)),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(sanitize(&truncate_chars(&a.description, 48)), name),
            Span::styled(sanitize(&rest), Style::default().fg(Color::Gray)),
        ]));
    }
    if rows.len() > SUBAGENT_ROWS {
        lines.push(Line::from(Span::styled(
            format!("  … {} more", rows.len() - SUBAGENT_ROWS),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

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
    for (i, q) in app.queued.iter().enumerate() {
        if i == QUEUE_ROWS {
            lines.push(Line::from(Span::styled(
                format!("  … {} more queued", app.queued.len() - QUEUE_ROWS),
                Style::default().fg(Color::DarkGray),
            )));
            break;
        }
        let first_line = q.text.lines().next().unwrap_or("");
        lines.push(Line::from(Span::styled(
            format!("  Queued: {}", sanitize(&truncate_chars(first_line, 90))),
            Style::default().fg(Color::DarkGray),
        )));
    }
    if !app.attachments.is_empty() {
        let names: Vec<String> = app.attachments.iter().map(|a| a.label()).collect();
        lines.push(Line::from(Span::styled(
            format!("  Attached: {}", sanitize(&names.join(", "))),
            Style::default().fg(Color::Cyan),
        )));
    }
    lines
}

fn harness_color(id: HarnessId) -> Color {
    match id {
        HarnessId::AGY => Color::Cyan,
        HarnessId::CLAUDE => Color::Magenta,
        HarnessId::CODEX => Color::Green,
        HarnessId::PI => Color::Yellow,
        // Config-defined harnesses share one colour.
        _ => Color::Blue,
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

fn sandbox_color(level: SandboxLevel) -> Color {
    match level {
        SandboxLevel::ReadOnly => Color::Green,
        SandboxLevel::WorkspaceWrite => Color::Cyan,
        SandboxLevel::Off => Color::Red,
    }
}

/// The level as the status line has room for it.
fn sandbox_label(level: SandboxLevel) -> &'static str {
    match level {
        SandboxLevel::WorkspaceWrite => "ws-write",
        other => other.as_str(),
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

/// The rendered lines of one transcript block. Depends on nothing but its
/// arguments, so the result can be kept until `block_key` changes.
fn block_lines(b: &TBlock, width: usize, thinking_live: bool, elapsed: f32) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
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
            agent,
            ..
        } => {
            let done_in = |d: &Option<std::time::Duration>| {
                d.map(|d| format!(" {:.1}s", d.as_secs_f32()))
                    .unwrap_or_default()
            };
            let running = Span::styled(" ⠿ running", Style::default().fg(Color::Yellow));
            // A call that spawned a subagent shows how the subagent is
            // doing: the call itself may have returned at once.
            let status = match agent.as_ref().map(|a| (a.status, &a.duration)) {
                Some((None, _)) => running,
                Some((Some(SubagentStatus::Completed), d)) => Span::styled(
                    format!(" ✓{}", done_in(d)),
                    Style::default().fg(Color::Green),
                ),
                Some((Some(SubagentStatus::Failed), _)) => {
                    Span::styled(" ✗ failed", Style::default().fg(Color::Red))
                }
                Some((Some(SubagentStatus::Cancelled), _)) => {
                    Span::styled(" ◼ stopped", Style::default().fg(Color::DarkGray))
                }
                None if !*done => running,
                None if *is_error => Span::styled(" ✗", Style::default().fg(Color::Red)),
                None => Span::styled(
                    format!(" ✓{}", done_in(duration)),
                    Style::default().fg(Color::Green),
                ),
            };
            let mut summary = tool_summary_full(name, input);
            if let Some(kind) = agent.as_ref().and_then(|a| a.kind.as_deref()) {
                summary.push_str(&format!(" ({kind})"));
            }
            let marker = "  ⚡ ";
            let indent = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(name.as_str()) + 2;
            let head_width = width.saturating_sub(indent + 11).max(10);
            let mut summary_lines = wrap_words(&summary, head_width).into_iter();
            let mut first = vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
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
            let status = vec![status];
            if rest.is_empty() {
                first.extend(status);
                lines.push(Line::from(first));
            } else {
                lines.push(Line::from(first));
                let n = rest.len();
                for (i, seg) in rest.into_iter().enumerate() {
                    let mut l = vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(seg, Style::default().fg(Color::Gray)),
                    ];
                    if i + 1 == n {
                        l.extend(status.clone());
                    }
                    lines.push(Line::from(l));
                }
            }

            let body_width = width.saturating_sub(2);
            match agent {
                // One line for a subagent: what it does and writes is in a
                // transcript of its own. Only a failed spawn says more here.
                Some(_) => {
                    if *is_error && !output.is_empty() {
                        lines.extend(plain_lines(
                            output,
                            body_width,
                            Style::default().fg(Color::Red),
                        ));
                    }
                }
                None => {
                    let limit = if *collapsed { 4 } else { usize::MAX };
                    let body = tool_body_lines(name, input, output, *is_error, body_width);
                    let total = body.len();
                    lines.extend(body.into_iter().take(limit));
                    if *collapsed && total > limit {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "  │ … {} more lines (click the call, or Ctrl+T, to expand)",
                                total - limit
                            ),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
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
    lines
}

/// A fingerprint of everything `block_lines` reads from a block.
fn block_key(b: &TBlock, thinking_live: bool, elapsed: f32) -> u64 {
    fn value<H: Hasher>(v: &serde_json::Value, h: &mut H) {
        use serde_json::Value;
        std::mem::discriminant(v).hash(h);
        match v {
            Value::Null => {}
            Value::Bool(b) => b.hash(h),
            Value::Number(n) => n.hash(h),
            Value::String(s) => s.hash(h),
            Value::Array(a) => a.iter().for_each(|v| value(v, h)),
            Value::Object(o) => o.iter().for_each(|(k, v)| {
                k.hash(h);
                value(v, h);
            }),
        }
    }
    let mut h = DefaultHasher::new();
    std::mem::discriminant(b).hash(&mut h);
    match b {
        TBlock::User { text } => text.hash(&mut h),
        TBlock::Assistant {
            text,
            sender,
            duration,
        } => (text, sender, duration).hash(&mut h),
        TBlock::Thought { text, duration } => {
            (text, duration).hash(&mut h);
            // A thought in progress shows a running clock.
            if duration.is_none() && thinking_live {
                ((elapsed * 10.0) as u32).hash(&mut h);
            }
        }
        TBlock::Tool {
            name,
            input,
            output,
            is_error,
            done,
            collapsed,
            duration,
            agent,
            ..
        } => {
            (name, output, is_error, done, collapsed, duration).hash(&mut h);
            // Not its transcript: that is drawn only when it is opened.
            agent.is_some().hash(&mut h);
            if let Some(run) = agent {
                (&run.kind, run.status, run.duration).hash(&mut h);
            }
            value(input, &mut h);
        }
        TBlock::System(t) | TBlock::Notice(t) | TBlock::Error(t) => t.hash(&mut h),
    }
    h.finish()
}

fn render_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            match app.viewed() {
                Some(run) => format!(
                    " unharness › subagent · {}{} · {} ",
                    truncate_chars(&run.description, 60),
                    run.kind
                        .as_ref()
                        .map(|k| format!(" ({k})"))
                        .unwrap_or_default(),
                    run.status.map_or("running", |s| s.label()),
                ),
                None => " unharness ".to_string(),
            },
            Style::default()
                .fg(if app.viewing.is_some() {
                    Color::Yellow
                } else {
                    Color::Cyan
                })
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let width = (inner.width.saturating_sub(4)).max(10) as usize;

    // The running clock on a thought is the main agent's.
    let thinking_live = app.viewing.is_none() && app.is_thinking();
    let elapsed = app.elapsed_secs();

    // Rendering a block (markdown, syntax highlighting, diffs) is far too
    // slow to redo on every frame, so the lines are kept and only blocks
    // whose content changed, and those after them, are rendered again.
    let mut view = std::mem::take(&mut app.transcript_view);
    if view.width != inner.width {
        view.width = inner.width;
        view.blocks.clear();
    }
    let mut fresh = 0;
    let shown = app.shown_blocks();
    for (i, b) in shown.iter().enumerate() {
        let key = block_key(b, thinking_live, elapsed);
        if i == fresh && view.blocks.get(i).is_some_and(|(k, _)| *k == key) {
            fresh += 1;
            continue;
        }
        if i == fresh {
            // First stale block: drop its lines and everything after.
            let keep = i.checked_sub(1).map_or(0, |p| view.blocks[p].1);
            view.blocks.truncate(i);
            view.rendered.truncate(keep);
            view.lines.truncate(keep);
        }
        let mut lines = block_lines(b, width, thinking_live, elapsed);
        clamp_lines(&mut lines, inner.width as usize);
        view.lines.extend(lines.iter().map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        }));
        view.rendered.extend(lines);
        view.blocks.push((key, view.rendered.len()));
    }
    if fresh == shown.len() && view.blocks.len() > fresh {
        // Blocks were removed from the end (rewind, /clear).
        let keep = fresh.checked_sub(1).map_or(0, |p| view.blocks[p].1);
        view.blocks.truncate(fresh);
        view.rendered.truncate(keep);
        view.lines.truncate(keep);
    }

    let total = view.rendered.len().min(u16::MAX as usize) as u16;
    let max_scroll = total.saturating_sub(inner.height);
    if app.auto_scroll || app.scroll >= max_scroll {
        app.scroll = max_scroll;
        app.auto_scroll = true;
    }
    view.area = inner;
    view.scroll = app.scroll as usize;
    // On the right border, beside the rows of text.
    view.scrollbar = Scrollbar::new(
        area.x + area.width.saturating_sub(1),
        inner.y,
        inner.height,
        total,
        app.scroll,
    );
    // Only where it can be clicked.
    view.jump = (!app.auto_scroll
        && app.config.mouse.unwrap_or(true)
        && inner.width >= JUMP_LABEL.width() as u16)
        .then(|| Rect {
            x: inner.x + (inner.width - JUMP_LABEL.width() as u16) / 2,
            y: inner.y + inner.height.saturating_sub(1),
            width: JUMP_LABEL.width() as u16,
            height: 1,
        });
    let visible: Vec<Line<'static>> = view
        .rendered
        .iter()
        .skip(app.scroll as usize)
        .take(inner.height as usize)
        .cloned()
        .collect();
    app.transcript_view = view;
    frame.render_widget(Paragraph::new(visible).block(block), area);

    // The mouse selection, drawn over the text it covers.
    if let Some((start, end)) = app.selection_range() {
        let buf = frame.buffer_mut();
        for row in 0..inner.height {
            let line = app.scroll as usize + row as usize;
            if line < start.line || line > end.line {
                continue;
            }
            let from = if line == start.line { start.col } else { 0 };
            let to = if line == end.line {
                end.col
            } else {
                usize::MAX
            };
            for col in from..=to.min(inner.width.saturating_sub(1) as usize) {
                buf[(inner.x + col as u16, inner.y + row)]
                    .set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }

    if let Some(bar) = app.transcript_view.scrollbar {
        let color = if app.scrollbar_held() {
            Color::White
        } else {
            Color::Gray
        };
        let buf = frame.buffer_mut();
        for row in bar.thumb_top..bar.thumb_top + bar.thumb_len {
            buf[(bar.x, bar.y + row)]
                .set_symbol("█")
                .set_style(Style::default().fg(color));
        }
    }
    if let Some(rect) = app.transcript_view.jump {
        frame.render_widget(
            Paragraph::new(Span::styled(
                JUMP_LABEL,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            rect,
        );
    }
}

/// Shown over the transcript's last row while it is scrolled away from the
/// end; clicking it goes back there.
const JUMP_LABEL: &str = " ↓ Jump to bottom ";

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
fn render_bottom(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    warning: Option<&str>,
    subagents: Vec<Line<'static>>,
) -> Rect {
    // Everything but the prompt and the subagent list is one row each.
    let fixed = 5 + u16::from(warning.is_some()) + subagents.len() as u16;
    let mut constraints = vec![
        Constraint::Length(1),                                 // status rule
        Constraint::Length(area.height.saturating_sub(fixed)), // prompt
        Constraint::Length(1),                                 // rule
        Constraint::Length(subagents.len() as u16),            // subagents
        Constraint::Length(1),                                 // where / what
        Constraint::Length(1),                                 // usage / session
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
    let status = if let Some(message) = app.flash_text() {
        Span::styled(format!(" {message} "), Style::default().fg(Color::Green))
    } else if app.is_generating {
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
    } else if let Some(label) = app.subagents_label().filter(|_| {
        !app.modal
            .as_ref()
            .is_some_and(super::modal::Modal::is_prompt)
    }) {
        // The turn is over but its subagents are not: still busy.
        Span::styled(
            format!(
                " {} {} · {:.0}s ",
                app.spinner(),
                label,
                app.subagents_elapsed_secs()
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
    let prompt_style = if app.is_generating || app.subagent_focus_index().is_some() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    let input_rows = app.prompt_rows();
    let chars: Vec<char> = app.input.chars().collect();
    let visible = input_rows
        .iter()
        .enumerate()
        .skip(app.prompt_scroll)
        .take(rows[1].height as usize);
    if app.viewing.is_some() {
        // The prompt is the main agent's; here there are only these keys.
        let stop = if app.caps().subagents.stop {
            " · s stop this subagent"
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("  Esc back to the conversation{stop} · Tab next subagent"),
                Style::default().fg(Color::Yellow),
            )),
            rows[1],
        );
    } else {
        let lines: Vec<Line> = visible
            .map(|(i, r)| {
                let marker = if i == 0 {
                    Span::styled("❯ ", prompt_style)
                } else {
                    Span::raw("  ")
                };
                let text: String = chars[r.start..r.end].iter().collect();
                Line::from(vec![marker, Span::raw(sanitize(&text))])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows[1]);
        // No cursor in the prompt while the keyboard is in the list under it.
        if app.modal.is_none() && app.subagent_focus_index().is_none() {
            let row = prompt::cursor_row(&input_rows, app.cursor);
            let col = prompt::width_between(&app.input, input_rows[row].start, app.cursor);
            let x = rows[1].x + (PROMPT_INDENT + col) as u16;
            let y = rows[1].y + row.saturating_sub(app.prompt_scroll) as u16;
            frame.set_cursor_position((
                x.min(rows[1].x + rows[1].width.saturating_sub(1)),
                y.min(rows[1].y + rows[1].height.saturating_sub(1)),
            ));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("─".repeat(width), rule_style))),
        rows[2],
    );
    frame.render_widget(Paragraph::new(subagents), rows[3]);

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
    let sandbox = app.sandbox_level().0;
    let policy_text = if effective == app.policy_requested {
        format!("policy {effective}")
    } else {
        format!("policy {}→{}", app.policy_requested, effective)
    };
    let right1 = Line::from(vec![
        Span::styled(
            format!("[{}] ", app.short_name()),
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
        Span::styled(
            format!(" · sandbox {}", sandbox_label(sandbox)),
            Style::default().fg(sandbox_color(sandbox)),
        ),
    ]);
    render_split(frame, rows[4], left1, right1);

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
    render_split(frame, rows[5], left2, right2);

    let mut next = 6;
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
        k("^J", Color::Cyan),
        Span::raw(" newline · "),
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
                format!(" Model for {} ({NAV}) ", app.short_name()),
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
        Modal::Subagents(p) => (
            centered_rect(80, 50, area),
            modal_block(format!(" Open a subagent ({NAV}) "), Color::Yellow),
            picker_lines(p, Color::Yellow, |a| {
                let mut detail = a.status.map_or("running", |s| s.label()).to_string();
                if let Some(kind) = &a.kind {
                    detail = format!("{kind} · {detail}");
                }
                (
                    format!("{}{}", "  ".repeat(a.depth), sanitize(&a.description)),
                    sanitize(&detail),
                    Some(&a.id) == app.viewing.as_ref(),
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
        Modal::Rewind(p) => (
            centered_rect(85, 60, area),
            modal_block(
                format!(" Rewind to before… ({NAV} · f = also restore files) "),
                Color::Blue,
            ),
            picker_lines(p, Color::Blue, |r| {
                (
                    truncate_chars(r.text.lines().next().unwrap_or(""), 60),
                    format!(
                        "{}{}",
                        if r.native {
                            "session rewound"
                        } else {
                            "fresh session, context re-sent"
                        },
                        if r.files { " · files: f" } else { "" }
                    ),
                    false,
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

    fn screen(app: &mut App, width: u16, height: u16) -> (Vec<String>, (u16, u16)) {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let cursor = term.get_cursor_position().unwrap();
        let buf = term.backend().buffer();
        let rows = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (rows, (cursor.x, cursor.y))
    }

    #[test]
    fn prompt_grows_with_its_lines_then_scrolls() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.insert_str("alpha\nbeta\ngamma");
        let (rows, cursor) = screen(&mut app, 60, 30);
        let first = rows.iter().position(|r| r == "❯ alpha").unwrap();
        assert_eq!(rows[first + 1], "  beta");
        assert_eq!(rows[first + 2], "  gamma");
        assert!(rows[first - 1].starts_with("──") && rows[first + 3].starts_with("──"));
        assert_eq!(cursor, (7, (first + 2) as u16));

        // Past the cap the box stops growing and follows the cursor.
        for i in 0..20 {
            app.insert_str(&format!("\nline {i}"));
        }
        let (rows, cursor) = screen(&mut app, 60, 30);
        let last = rows.iter().position(|r| r == "  line 19").unwrap();
        assert_eq!(rows[last - (prompt::MAX_ROWS - 1)], "  line 12");
        assert!(rows[last - prompt::MAX_ROWS].starts_with("──"));
        assert_eq!(cursor.1, last as u16);
        assert!(!rows.iter().any(|r| r == "❯ alpha"));

        // Moving back up scrolls the first line into view again.
        while app.move_cursor_up() {}
        let (rows, cursor) = screen(&mut app, 60, 30);
        let first = rows.iter().position(|r| r == "❯ alpha").unwrap();
        assert_eq!(cursor.1, first as u16);

        // A long line wraps instead of running off the edge.
        app.take_input();
        app.insert_str(&"x".repeat(70));
        let (rows, cursor) = screen(&mut app, 60, 30);
        let first = rows.iter().position(|r| r.starts_with("❯ x")).unwrap();
        assert_eq!(rows[first], format!("❯ {}", "x".repeat(58)));
        assert_eq!(rows[first + 1], format!("  {}", "x".repeat(12)));
        assert_eq!(cursor, (14, (first + 1) as u16));
    }

    #[test]
    fn mouse_selects_transcript_text_and_asks_for_a_copy() {
        use crate::tui::app::tests::mouse;
        use crossterm::event::{MouseButton, MouseEventKind};
        let left = MouseButton::Left;

        let mut app = test_app(HarnessId::CLAUDE);
        app.transcript
            .push_user("open src/tui/app.rs please".to_string());
        app.transcript.push_user("second prompt".to_string());
        let (rows, _) = screen(&mut app, 80, 24);
        let y = rows.iter().position(|r| r.contains("open src")).unwrap() as u16;
        let x = rows[y as usize].find("open").unwrap() as u16 - 2; // "│" is 3 bytes, 1 cell

        // Press and release without moving selects nothing.
        app.handle_mouse(mouse(MouseEventKind::Down(left), x, y));
        app.handle_mouse(mouse(MouseEventKind::Up(left), x, y));
        assert!(app.selection.is_none() && app.take_copy_request().is_none());

        // Drag across "open src": highlighted while held, copied on release.
        app.last_click_forget();
        app.handle_mouse(mouse(MouseEventKind::Down(left), x, y));
        app.handle_mouse(mouse(MouseEventKind::Drag(left), x + 7, y));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| render(f, &mut app)).unwrap();
        let reversed = |term: &Terminal<TestBackend>, x: u16| {
            term.backend().buffer()[(x, y)]
                .modifier
                .contains(Modifier::REVERSED)
        };
        assert!(!reversed(&term, x - 1) && reversed(&term, x) && reversed(&term, x + 7));
        assert!(!reversed(&term, x + 8));
        assert!(app.take_copy_request().is_none());
        app.handle_mouse(mouse(MouseEventKind::Up(left), x + 7, y));
        assert_eq!(app.take_copy_request().as_deref(), Some("open src"));
        assert!(app.selection.is_some());

        // Double click: the word, here a whole path. Triple: the line.
        app.last_click_forget();
        for _ in 0..2 {
            app.handle_mouse(mouse(MouseEventKind::Down(left), x + 8, y));
            app.handle_mouse(mouse(MouseEventKind::Up(left), x + 8, y));
        }
        assert_eq!(app.take_copy_request().as_deref(), Some("src/tui/app.rs"));
        app.handle_mouse(mouse(MouseEventKind::Down(left), x + 8, y));
        app.handle_mouse(mouse(MouseEventKind::Up(left), x + 8, y));
        assert_eq!(
            app.take_copy_request().as_deref(),
            Some("  open src/tui/app.rs please")
        );

        // A drag over several rows copies them as rows; one that ends below
        // the transcript keeps to its last row and starts scrolling.
        app.last_click_forget();
        app.handle_mouse(mouse(MouseEventKind::Down(left), x + 5, y));
        app.handle_mouse(mouse(MouseEventKind::Drag(left), x + 5, y + 3));
        assert_eq!(
            app.selected_text().as_deref(),
            Some("src/tui/app.rs please\n\n❯ You\n  second")
        );
        app.handle_mouse(mouse(MouseEventKind::Drag(left), x + 5, 23));
        assert!(app.tick_mouse());
        app.handle_mouse(mouse(MouseEventKind::Up(left), x + 5, 23));
        assert!(!app.tick_mouse());
        assert!(
            app.take_copy_request()
                .is_some_and(|t| t.trim_end().ends_with("  second prompt"))
        );

        // A press outside the transcript, or a key, drops the selection.
        app.handle_mouse(mouse(MouseEventKind::Down(left), 3, 23));
        assert!(app.selection.is_none());

        // The copy result shows in the status rule.
        app.flash("Copied 4 lines");
        let (rows, _) = screen(&mut app, 80, 24);
        assert!(rows.iter().any(|r| r.starts_with("── Copied 4 lines ─")));
    }

    #[test]
    fn a_click_on_a_tool_call_expands_that_call() {
        use super::super::app::tests::mouse;
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("go".into());
        app.take_actions();
        for id in ["t1", "t2"] {
            app.on_event(crate::core::AgentEvent::ToolCallStarted {
                id: id.into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": format!("run {id}")}),
            });
            app.on_event(crate::core::AgentEvent::ToolCallResult {
                id: id.into(),
                output: format!("{id}-1\n{id}-2\n{id}-3\n{id}-4\n{id}-5\n{id}-6"),
                is_error: false,
            });
        }
        let (rows, _) = screen(&mut app, 80, 30);
        assert!(!rows.join("\n").contains("t1-6"));
        // Not the last call: Ctrl+O would not reach it.
        let row = rows.iter().position(|r| r.contains("run t1")).unwrap() as u16;
        let click = |app: &mut App, row: u16| {
            app.last_click_forget();
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, row));
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 10, row));
        };
        click(&mut app, row);
        let (rows, _) = screen(&mut app, 80, 30);
        let text = rows.join("\n");
        assert!(text.contains("t1-6") && !text.contains("t2-6"), "{text}");
        assert!(app.take_copy_request().is_none());
        // Again to collapse; a click on its output does nothing.
        click(&mut app, row + 1);
        let (rows, _) = screen(&mut app, 80, 30);
        assert!(rows.join("\n").contains("t1-6"));
        click(&mut app, row);
        let (rows, _) = screen(&mut app, 80, 30);
        assert!(!rows.join("\n").contains("t1-6"));
    }

    #[test]
    fn scrollbar_and_jump_label_follow_and_move_the_view() {
        use crate::tui::app::tests::mouse;
        use crossterm::event::{MouseButton, MouseEventKind};
        let left = MouseButton::Left;

        // Too little to scroll: no bar, no label.
        let mut app = test_app(HarnessId::CLAUDE);
        let (rows, _) = screen(&mut app, 60, 20);
        assert!(app.transcript_view.scrollbar.is_none() && app.transcript_view.jump.is_none());
        assert!(!rows.iter().any(|r| r.contains('█') || r.contains("Jump")));

        for i in 0..40 {
            app.transcript.push_user(format!("prompt number {i}"));
        }
        let (rows, _) = screen(&mut app, 60, 20);
        let bar = app.transcript_view.scrollbar.unwrap();
        let thumb = |rows: &[String]| -> Vec<usize> {
            (0..rows.len())
                .filter(|y| rows[*y].ends_with('█'))
                .collect()
        };
        // At the end: thumb at the foot of the bar, no label.
        let at_end = thumb(&rows);
        assert_eq!(at_end.len(), bar.thumb_len as usize);
        assert_eq!(*at_end.last().unwrap() as u16, bar.y + bar.height - 1);
        assert!(app.transcript_view.jump.is_none());
        let bottom = app.scroll;

        // Scrolled up a little: the thumb leaves the foot and the label shows.
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 5, 5));
        let (rows, _) = screen(&mut app, 60, 20);
        assert!((*thumb(&rows).last().unwrap() as u16) < bar.y + bar.height - 1);
        let label = app.transcript_view.jump.unwrap();
        assert!(rows[label.y as usize].contains("↓ Jump to bottom"));
        assert_eq!(label.y, bar.y + bar.height - 1);

        // Clicking the label returns to the end and to following it.
        app.handle_mouse(mouse(MouseEventKind::Down(left), label.x + 2, label.y));
        app.handle_mouse(mouse(MouseEventKind::Up(left), label.x + 2, label.y));
        screen(&mut app, 60, 20);
        assert_eq!((app.scroll, app.auto_scroll), (bottom, true));
        assert!(app.selection.is_none() && app.take_copy_request().is_none());

        // Dragging the thumb to the top shows the first line; the pointer
        // may leave the bar's column on the way.
        let grab_at = bar.y + bar.height - 1;
        app.handle_mouse(mouse(MouseEventKind::Down(left), bar.x, grab_at));
        assert!(app.scrollbar_held());
        app.handle_mouse(mouse(MouseEventKind::Drag(left), bar.x - 9, bar.y + 3));
        let (rows, _) = screen(&mut app, 60, 20);
        assert!(app.scroll > 0 && app.scroll < bottom);
        assert!(app.selection.is_none());
        assert!(thumb(&rows).contains(&((bar.y + 3) as usize)));
        app.handle_mouse(mouse(MouseEventKind::Drag(left), bar.x, 0));
        let (rows, _) = screen(&mut app, 60, 20);
        assert_eq!(app.scroll, 0);
        assert!(rows[1].contains("Welcome") && rows[1].ends_with('█'));
        app.handle_mouse(mouse(MouseEventKind::Up(left), bar.x, 0));
        assert!(!app.scrollbar_held());

        // A click on the track brings the thumb's middle there; dragged
        // to the foot it follows the end again.
        let middle = bar.y + bar.height / 2;
        app.handle_mouse(mouse(MouseEventKind::Down(left), bar.x, middle));
        let (rows, _) = screen(&mut app, 60, 20);
        let t = thumb(&rows);
        assert!(*t.first().unwrap() <= middle as usize && middle as usize <= *t.last().unwrap());
        app.handle_mouse(mouse(MouseEventKind::Drag(left), bar.x, 19));
        app.handle_mouse(mouse(MouseEventKind::Up(left), bar.x, 19));
        screen(&mut app, 60, 20);
        assert_eq!((app.scroll, app.auto_scroll), (bottom, true));
    }

    #[test]
    fn scrollbar_thumb_reaches_the_ends_only_at_the_ends() {
        assert_eq!(Scrollbar::new(9, 1, 10, 10, 0), None);
        let at = |scroll| Scrollbar::new(9, 1, 10, 1000, scroll).unwrap();
        assert_eq!((at(0).thumb_top, at(0).thumb_len), (0, 1));
        assert_eq!(at(1).thumb_top, 1);
        assert_eq!(at(989).thumb_top, 8);
        assert_eq!(at(990).thumb_top, 9);
        // Thumb position and scroll offset map onto each other.
        let bar = Scrollbar::new(9, 1, 10, 40, 0).unwrap();
        assert_eq!((bar.thumb_len, bar.max_scroll), (2, 30));
        assert_eq!(bar.scroll_for(-3), 0);
        assert_eq!(bar.scroll_for(4), 15);
        assert_eq!(bar.scroll_for(99), 30);
    }

    #[test]
    fn kept_transcript_lines_match_a_fresh_render() {
        // The same frame, from the lines kept since the last one and from
        // nothing.
        fn check(app: &mut App, width: u16, what: &str) {
            let (kept, _) = screen(app, width, 30);
            app.transcript_view = Default::default();
            let (fresh, _) = screen(app, width, 30);
            assert_eq!(kept, fresh, "{what}");
        }
        let tool = |app: &mut App, id: &str, output: &str| {
            app.on_event(crate::core::AgentEvent::ToolCallStarted {
                id: id.into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": format!("run {id}")}),
            });
            app.on_event(crate::core::AgentEvent::ToolCallResult {
                id: id.into(),
                output: output.into(),
                is_error: false,
            });
        };

        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("first".into());
        app.take_actions();
        tool(&mut app, "t1", "a\nb\nc\nd\ne\nf\ng");
        app.on_event(crate::core::AgentEvent::TextDelta("Some **text**".into()));
        check(&mut app, 70, "first frame");
        let kept_blocks = app.transcript_view.blocks.clone();
        assert_eq!(kept_blocks.len(), app.transcript.blocks.len());

        // Nothing changed: every block is reused as it is.
        screen(&mut app, 70, 30);
        assert_eq!(app.transcript_view.blocks, kept_blocks);

        // The last block grows while streaming; earlier ones are untouched.
        app.on_event(crate::core::AgentEvent::TextDelta(
            " and more\n\n```rust\nfn main() {}\n```\n".into(),
        ));
        check(&mut app, 70, "streamed text");
        let n = kept_blocks.len();
        assert_eq!(app.transcript_view.blocks[..n - 1], kept_blocks[..n - 1]);
        assert_ne!(app.transcript_view.blocks[n - 1], kept_blocks[n - 1]);

        // A block in the middle changes height: all after it move.
        tool(&mut app, "t2", "x\ny");
        app.on_event(crate::core::AgentEvent::TurnCompleted {
            stop_reason: crate::core::StopReason::Done,
        });
        check(&mut app, 70, "second tool and turn end");
        if let Some(TBlock::Tool { collapsed, .. }) = app
            .transcript
            .blocks
            .iter_mut()
            .find(|b| matches!(b, TBlock::Tool { output, .. } if output.starts_with('a')))
        {
            *collapsed = !*collapsed;
        }
        check(&mut app, 70, "tool block expanded");

        // A subagent changes the call that spawned it after later blocks
        // were drawn: its state, its prose, its report.
        app.on_event(crate::core::AgentEvent::ToolCallStarted {
            id: "spawn".into(),
            name: "Agent".into(),
            input: serde_json::json!({"description": "look around"}),
        });
        check(&mut app, 70, "spawn call");
        app.on_event(crate::core::AgentEvent::SubagentStarted {
            id: "spawn".into(),
            description: "look around".into(),
            kind: Some("Explore".into()),
        });
        check(&mut app, 70, "subagent started");
        app.on_event(crate::core::AgentEvent::ToolCallResult {
            id: "spawn".into(),
            output: "launched".into(),
            is_error: false,
        });
        app.on_event(crate::core::AgentEvent::TextDelta("Meanwhile.".into()));
        check(&mut app, 70, "spawn call returned");
        let sub = |event| crate::core::AgentEvent::Sub {
            parent: "spawn".into(),
            event: Box::new(event),
        };
        app.on_event(sub(crate::core::AgentEvent::TextDelta("one\ntwo".into())));
        check(&mut app, 70, "subagent prose");
        app.on_event(sub(crate::core::AgentEvent::ToolCallStarted {
            id: "inner".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "a.txt"}),
        }));
        app.on_event(sub(crate::core::AgentEvent::TextDelta(
            "three\nfour\nfive".into(),
        )));
        check(&mut app, 70, "subagent call and more prose");
        app.on_event(crate::core::AgentEvent::SubagentEnded {
            id: "spawn".into(),
            status: crate::core::SubagentStatus::Completed,
            result: Some("the report".into()),
        });
        check(&mut app, 70, "subagent ended");
        app.on_event(crate::core::AgentEvent::SubagentStarted {
            id: "spawn".into(),
            description: "look around".into(),
            kind: Some("Explore".into()),
        });
        check(&mut app, 70, "subagent back at work");
        app.on_event(crate::core::AgentEvent::SubagentEnded {
            id: "spawn".into(),
            status: crate::core::SubagentStatus::Cancelled,
            result: None,
        });
        check(&mut app, 70, "subagent stopped");

        // Its own transcript is drawn in place of the main one, and the
        // main one again after it.
        app.open_subagent("spawn");
        check(&mut app, 70, "subagent in view");
        assert_eq!(app.transcript_view.blocks.len(), app.shown_blocks().len());
        app.on_event(sub(crate::core::AgentEvent::TextDelta(" six".into())));
        check(&mut app, 70, "subagent writes while in view");
        app.close_subagent_view();
        check(&mut app, 70, "back from the subagent");
        assert_eq!(
            app.transcript_view.blocks.len(),
            app.transcript.blocks.len()
        );

        // A new width lays everything out again.
        check(&mut app, 50, "narrower");
        check(&mut app, 90, "wider");

        // Blocks removed from the end, and all of them.
        app.transcript.blocks.truncate(2);
        check(&mut app, 90, "truncated");
        assert_eq!(app.transcript_view.blocks.len(), 2);
        app.transcript.clear();
        check(&mut app, 90, "cleared");
        assert!(app.transcript_view.lines.is_empty());
        app.transcript.push_user("again");
        check(&mut app, 90, "after clear");
    }

    #[test]
    fn a_subagent_is_one_line_in_the_main_transcript_and_a_transcript_of_its_own() {
        use super::super::app::tests::mouse;
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("delegate".into());
        app.take_actions();
        for (id, description) in [("a", "read the first file"), ("b", "read the second")] {
            app.on_event(crate::core::AgentEvent::ToolCallStarted {
                id: id.into(),
                name: "Agent".into(),
                input: serde_json::json!({ "description": description }),
            });
            app.on_event(crate::core::AgentEvent::SubagentStarted {
                id: id.into(),
                description: description.into(),
                kind: Some("Explore".into()),
            });
            app.on_event(crate::core::AgentEvent::ToolCallResult {
                id: id.into(),
                output: "Async agent launched successfully.".into(),
                is_error: false,
            });
        }
        let sub = |parent: &str, event| crate::core::AgentEvent::Sub {
            parent: parent.into(),
            event: Box::new(event),
        };
        app.on_event(sub(
            "b",
            crate::core::AgentEvent::ToolCallStarted {
                id: "b1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "second.txt"}),
            },
        ));
        app.on_event(sub(
            "a",
            crate::core::AgentEvent::TextDelta("the word is alpha".into()),
        ));
        app.on_event(crate::core::AgentEvent::TextDelta("Both are on it.".into()));

        // The main transcript: a line for each, running though the calls
        // returned, and nothing of what they do or of the launch receipt.
        let (rows, _) = screen(&mut app, 100, 40);
        let text = rows.join("\n");
        for d in ["read the first file", "read the second"] {
            assert!(
                rows.iter()
                    .any(|r| r.contains(&format!("Agent  {d} (Explore) ⠿ running"))),
                "{text}"
            );
        }
        assert!(text.contains("Both are on it."), "{text}");
        // (Below the transcript, the pinned list does say what each is doing.)
        let end = rows.iter().position(|r| r.starts_with('└')).unwrap();
        let transcript = rows[..end].join("\n");
        for absent in ["launched", "second.txt", "the word is alpha", "↳"] {
            assert!(!transcript.contains(absent), "{absent}: {text}");
        }
        assert!(rows[end..].join("\n").contains("Read second.txt"), "{text}");

        // A click on its line opens its own transcript.
        let row = rows
            .iter()
            .position(|r| r.contains("read the second"))
            .unwrap() as u16;
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, row));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 10, row));
        assert_eq!(app.viewing.as_deref(), Some("b"));
        let (rows, _) = screen(&mut app, 100, 40);
        let text = rows.join("\n");
        assert!(
            rows[0].contains("unharness › subagent · read the second (Explore) · running"),
            "{text}"
        );
        assert!(text.contains("Read  second.txt"), "{text}");
        assert!(!text.contains("Both are on it.") && !text.contains("delegate"));
        assert!(
            text.contains("Esc back to the conversation · s stop this subagent"),
            "{text}"
        );

        // The other one's, with its prose; then its end shows in both places.
        app.view_next_subagent(false);
        let (rows, _) = screen(&mut app, 100, 40);
        let end = rows.iter().position(|r| r.starts_with('└')).unwrap();
        let text = rows[..end].join("\n");
        assert!(text.contains("the word is alpha") && !text.contains("second.txt"));
        app.on_event(crate::core::AgentEvent::SubagentEnded {
            id: "a".into(),
            status: crate::core::SubagentStatus::Completed,
            result: Some("the word is alpha".into()),
        });
        app.on_event(crate::core::AgentEvent::SubagentEnded {
            id: "b".into(),
            status: crate::core::SubagentStatus::Cancelled,
            result: None,
        });
        let (rows, _) = screen(&mut app, 100, 40);
        assert!(rows[0].contains("read the first file (Explore) · completed"));
        app.close_subagent_view();
        let (rows, _) = screen(&mut app, 100, 40);
        let text = rows.join("\n");
        assert!(
            rows.iter()
                .any(|r| r.contains("read the first file (Explore) ✓")),
            "{text}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("read the second (Explore) ◼ stopped")),
            "{text}"
        );
        assert!(text.contains("Both are on it."), "{text}");

        // The subagent in view goes away with its conversation.
        app.open_subagent("a");
        app.transcript.clear();
        let (rows, _) = screen(&mut app, 100, 40);
        assert!(app.viewing.is_none() && rows[0].contains("┌ unharness ─"));
    }

    #[test]
    fn subagents_are_listed_under_the_prompt_and_keep_the_status_rule_busy() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("delegate".into());
        app.take_actions();
        for i in 0..6 {
            let id = format!("s{i}");
            app.on_event(crate::core::AgentEvent::ToolCallStarted {
                id: id.clone(),
                name: "Agent".into(),
                input: serde_json::json!({"description": format!("task {i}")}),
            });
            app.on_event(crate::core::AgentEvent::SubagentStarted {
                id: id.clone(),
                description: format!("task {i}"),
                kind: Some("Explore".into()),
            });
            app.on_event(crate::core::AgentEvent::ToolCallResult {
                id,
                output: "launched".into(),
                is_error: false,
            });
        }
        app.on_event(crate::core::AgentEvent::SubagentProgress {
            id: "s0".into(),
            activity: "Reading a.txt".into(),
        });
        // The rows under the prompt, down to the first of the two info lines.
        let list = |app: &mut App| -> Vec<String> {
            let (rows, _) = screen(app, 100, 40);
            let border = rows.iter().rposition(|r| r.starts_with('└')).unwrap();
            let end = rows.iter().rposition(|r| r.contains("[Claude]")).unwrap();
            // (status rule, the one-row prompt, rule, then the list)
            assert!(rows[border + 2].starts_with('❯'));
            rows[border + 4..end].to_vec()
        };

        // During the turn the rule tells of the turn; the list is there
        // already: a header, four rows, and a count of the rest.
        let rows = list(&mut app);
        assert_eq!(rows.len(), 6, "{rows:#?}");
        assert!(rows[0].contains("Subagents · 6 running · ↓ to select one"));
        assert!(
            rows[1].contains("task 0 (Explore) · 0s · Reading a.txt"),
            "{rows:#?}"
        );
        assert!(rows[4].contains("task 3") && rows[5].contains("… 2 more"));

        // The turn ends; the subagents do not.
        app.on_event(crate::core::AgentEvent::TurnCompleted {
            stop_reason: crate::core::StopReason::Done,
        });
        let (rows, cursor) = screen(&mut app, 100, 40);
        let text = rows.join("\n");
        assert!(text.contains("── ") && text.contains("6 subagents running ·"));
        assert!(!text.contains("Ready"), "{text}");
        assert!(app.is_busy() && !app.is_generating);
        // The cursor is in the prompt.
        assert!(rows[cursor.1 as usize].starts_with('❯'));

        // Down from the prompt goes into the list; the window follows.
        assert!(app.subagent_list_down());
        let rows = list(&mut app);
        assert!(rows[0].contains("↑/↓ select · Enter open · Del remove a finished one"));
        assert!(rows[1].starts_with("❯ ") && rows[1].contains("task 0"));
        for _ in 0..5 {
            app.subagent_list_down();
        }
        app.subagent_list_down(); // already on the last: stays
        let rows = list(&mut app);
        assert!(rows[1].contains("task 2"), "{rows:#?}");
        assert!(rows[4].starts_with("❯ ") && rows[4].contains("task 5"));

        // Ended ones stay listed, the latest first, after those still at work.
        for i in 0..6 {
            app.on_event(crate::core::AgentEvent::SubagentEnded {
                id: format!("s{i}"),
                status: if i == 5 {
                    crate::core::SubagentStatus::Cancelled
                } else {
                    crate::core::SubagentStatus::Completed
                },
                result: None,
            });
            if i == 4 {
                let (rows, _) = screen(&mut app, 100, 40);
                assert!(rows.join("\n").contains("1 subagent running ·"));
                let rows = list(&mut app);
                assert!(rows[1].contains("task 5") && rows[2].contains("✓ task 4"));
            }
        }
        let (all, _) = screen(&mut app, 100, 40);
        assert!(all.join("\n").contains("Ready · last turn"));
        let rows = list(&mut app);
        assert!(rows[0].contains("Subagents · 6 done"), "{rows:#?}");
        assert!(rows[1].starts_with("❯ ◼ task 5") && rows[1].contains("stopped"));
        assert!(rows[2].contains("✓ task 4 (Explore)") && rows[2].contains("completed"));

        // Enter opens the chosen one; back from it the keyboard is still on its row.
        app.open_focused_subagent();
        assert_eq!(app.viewing.as_deref(), Some("s5"));
        app.close_subagent_view();
        assert_eq!(app.subagent_focus_index(), Some(0));
        // Up from the first row is the prompt again.
        app.subagent_list_up();
        assert_eq!(app.subagent_focus_index(), None);

        // Another prompt does not clear the list: what they did may still
        // be wanted.
        app.submit_prompt("next".into());
        app.on_event(crate::core::AgentEvent::TurnCompleted {
            stop_reason: crate::core::StopReason::Done,
        });
        assert!(list(&mut app)[0].contains("Subagents · 6 done"));

        // Delete takes a finished one off the list (not out of the
        // conversation); the keyboard moves to the row that takes its place.
        app.subagent_list_down();
        app.dismiss_focused_subagent();
        let rows = list(&mut app);
        assert!(rows[0].contains("Subagents · 5 done"), "{rows:#?}");
        assert!(rows[1].starts_with("❯ ") && rows[1].contains("task 4"));
        assert!(app.transcript.agent("s5").is_some());
        // That it was taken off is saved with the conversation.
        let saved = app.store.load(&app.conversation.id).unwrap();
        let back = super::super::transcript::Transcript::from_records(&saved.blocks);
        assert_eq!(back.agents().len(), 6);
        assert_eq!(back.listed_agents().len(), 5);

        // One back at work is listed again, first, and cannot be removed
        // while it runs.
        app.on_event(crate::core::AgentEvent::SubagentStarted {
            id: "s5".into(),
            description: "task 5".into(),
            kind: Some("Explore".into()),
        });
        let rows = list(&mut app);
        assert!(rows[0].contains("Subagents · 1 running"), "{rows:#?}");
        assert!(rows[1].contains("task 5") && rows[2].starts_with("❯ "));
        app.subagent_list_up();
        app.dismiss_focused_subagent();
        let rows = list(&mut app);
        assert!(rows[1].starts_with("❯ ") && rows[1].contains("task 5"));
        app.on_event(crate::core::AgentEvent::SubagentEnded {
            id: "s5".into(),
            status: crate::core::SubagentStatus::Completed,
            result: None,
        });
        // Removing the last of them puts the keyboard back in the prompt.
        for _ in 0..6 {
            app.dismiss_focused_subagent();
        }
        assert!(list(&mut app).is_empty());
        assert_eq!(app.subagent_focus_index(), None);
        assert!(!app.subagent_list_down());
    }

    #[test]
    fn bottom_cluster_layout() {
        let mut app = test_app(HarnessId::CLAUDE);
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
        let (rows, _) = screen(&mut app, 124, 24);
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
        let mut app = test_app(HarnessId::CLAUDE);
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
        let mut app = test_app(HarnessId::CLAUDE);
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
        app.session_alive = true;
        app.on_event(crate::core::AgentEvent::SubagentStarted {
            id: "spawn".into(),
            description: "look around".into(),
            kind: Some("Explore".into()),
        });
        app.open_subagent_picker();
        assert!(matches!(app.modal, Some(Modal::Subagents(_))));
        term.draw(|f| render(f, &mut app)).unwrap();
        let shown = term.backend().to_string();
        assert!(shown.contains("Open a subagent") && shown.contains("look around"));
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
