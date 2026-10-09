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
use std::time::Instant;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{App, KeptBlock, Live, LiveRow, Scrollbar, TranscriptView};
use super::code::{
    clamp_lines, code_lines, diff_lines, faint, looks_like_diff, plain_lines, replacement_lines,
    sanitize, wrap_words,
};
use super::markdown::render_markdown_to_lines;
use super::modal::{AlwaysDraft, ListPicker, Modal, PolicyPicker, QuestionModal, takes_text};
use super::prompt;
use super::transcript::{
    Block as TBlock, HookState, input_beyond_summary, tool_summary, tool_summary_full,
    truncate_chars, waits_on_user,
};
use crate::core::SandboxLevel;
use crate::core::conversations::ShellStatus;
use crate::core::rules::Scope;
use crate::core::{
    HarnessId, HookOutcome, PermissionKind, PermissionPolicy, PlanStatus, SubagentStatus,
};

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
    let area = frame.area();
    match &mut app.modal {
        // Drawing it settles how far its preview can scroll.
        Some(Modal::Question(m)) => render_question(frame, m, area),
        Some(_) => render_modal(frame, app, area),
        None => {}
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

/// The rendered lines of one transcript block, `followed` by another.
#[cfg(test)]
fn block_lines(
    b: &TBlock,
    width: usize,
    followed: bool,
    thinking_live: bool,
    elapsed: f32,
) -> Vec<Line<'static>> {
    block_layout(b, width, followed, thinking_live, elapsed).0
}

/// The rendered lines of one transcript block, `followed` by another, and
/// which span of the first line is its status (a tool call's, a `!`
/// command's), drawn again on every frame while it runs ([`patch_running`]).
/// Depends on nothing but its arguments, so the result can be kept until
/// `block_key` changes.
fn block_layout(
    b: &TBlock,
    width: usize,
    followed: bool,
    thinking_live: bool,
    elapsed: f32,
) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut status_span = None;
    match b {
        TBlock::User { text } => {
            lines.push(Line::from(Span::styled(
                "❯ You",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.extend(wrap_prefixed_text("  ", text, width, Style::default()));
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
            // Text is added only to the last block, until the turn ends.
            let done = followed || duration.is_some();
            lines.extend(render_markdown_to_lines(text, width, done));
        }
        TBlock::Handoff { text, sender, to } => {
            lines.push(Line::from(Span::styled(
                format!("● {sender} · handoff summary for {to}"),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.extend(render_markdown_to_lines(text, width, true));
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
                // The time a question took is the user's, not the tool's.
                None if waits_on_user(name) => {
                    Span::styled(" ✓", Style::default().fg(Color::Green))
                }
                None => Span::styled(
                    format!(" ✓{}", done_in(duration)),
                    Style::default().fg(Color::Green),
                ),
            };
            let mut summary = tool_summary_full(name, input);
            if let Some(kind) = agent.as_ref().and_then(|a| a.kind.as_deref()) {
                summary.push_str(&format!(" ({kind})"));
            }
            // The command in the terminal's own colour, its output dimmed
            // below it, and the status on the first line: at the end of the
            // line's width when the summary wraps, not inside the command. A
            // name that leaves too little room beside it (an MCP tool's) has
            // that line to itself, shortened if need be so the status fits,
            // and the summary goes below, under the name.
            let marker = "  ⚡ ";
            let marker_width = UnicodeWidthStr::width(marker);
            let beside = marker_width + UnicodeWidthStr::width(name.as_str()) + 2;
            let own_line = beside + SUMMARY_MIN + STATUS_ROOM > width;
            let (shown_name, indent, head_width) = if own_line {
                let room = width.saturating_sub(marker_width + STATUS_ROOM).max(1);
                let shown = shorten(name, room);
                (
                    shown,
                    marker_width,
                    width.saturating_sub(marker_width).max(10),
                )
            } else {
                (name.clone(), beside, width - beside - STATUS_ROOM)
            };
            let mut summary_lines = wrap_words(&summary, head_width).into_iter();
            let mut first = vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::styled(
                    shown_name,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if !own_line {
                let mut words = summary_lines.next().unwrap_or_default();
                if summary_lines.len() > 0 {
                    let pad =
                        (head_width + 1).saturating_sub(UnicodeWidthStr::width(words.as_str()));
                    words.push_str(&" ".repeat(pad));
                }
                first.extend([Span::raw("  "), Span::raw(words)]);
            }
            status_span = Some(first.len());
            first.push(status);
            lines.push(Line::from(first));
            for seg in summary_lines.filter(|s| !s.is_empty()) {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::raw(seg),
                ]));
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
                    let body = tool_body_lines(name, input, output, *is_error, body_width);
                    // What the summary line left out of the input, after the
                    // output on an expanded call; a collapsed one counts it.
                    let left_out = if input_beyond_summary(name, input)
                        && let Ok(json) = serde_json::to_string_pretty(input)
                    {
                        code_lines(&json, "json", body_width)
                    } else {
                        Vec::new()
                    };
                    let hidden = if *collapsed {
                        body.len().saturating_sub(4) + left_out.len()
                    } else {
                        0
                    };
                    let shown = if *collapsed { 4 } else { usize::MAX };
                    lines.extend(body.into_iter().take(shown));
                    if !*collapsed {
                        lines.extend(left_out);
                    }
                    if hidden > 0 {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "  │ … {hidden} more lines (click the call, or Ctrl+T, to expand)"
                            ),
                            faint(),
                        )));
                    }
                }
            }
            close_gutter(&mut lines);
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
        }
        TBlock::Hook {
            name,
            state,
            output,
            ..
        } => {
            let (state, color) = match state {
                HookState::Running => (" …", Color::Yellow),
                HookState::Unknown => (" (its end was not reported)", Color::DarkGray),
                HookState::Ended(HookOutcome::Succeeded) => ("", Color::DarkGray),
                HookState::Ended(HookOutcome::Failed) => (" failed", Color::Yellow),
                HookState::Ended(HookOutcome::Blocked) => (" blocked", Color::Red),
            };
            // What a hook said can be long (its whole stderr): its first line.
            let said = output.lines().next().unwrap_or("");
            let text = if said.is_empty() {
                format!("hook {name}{state}")
            } else {
                format!("hook {name}{state}: {said}")
            };
            lines.extend(wrap_prefixed_text(
                "  ⚙ ",
                &text,
                width,
                Style::default().fg(color),
            ));
        }
        TBlock::Shell {
            command,
            output,
            dropped,
            status,
            duration,
            ..
        } => {
            lines.extend(shell_lines(
                command, output, *dropped, status, *duration, width,
            ));
            status_span = Some(1);
        }
    }
    (lines, status_span)
}

/// A `!` command: what was run, by whom, how it ended, and its output.
fn shell_lines(
    command: &str,
    output: &str,
    dropped: usize,
    status: &ShellStatus,
    duration: Option<std::time::Duration>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "❯ You ran ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            match status {
                ShellStatus::Running => "⠿ running".to_string(),
                ShellStatus::Exited { code: 0 } => format!(
                    "✓ exit 0{}",
                    duration
                        .map(|d| format!(" {:.1}s", d.as_secs_f32()))
                        .unwrap_or_default()
                ),
                other => format!("✗ {}", other.label()),
            },
            Style::default().fg(match status {
                ShellStatus::Running => Color::Yellow,
                ShellStatus::Exited { code: 0 } => Color::Green,
                ShellStatus::Killed => Color::DarkGray,
                _ => Color::Red,
            }),
        ),
    ])];
    let mut first = true;
    for raw in command.lines() {
        lines.extend(wrap_prefixed_text(
            if first { "  $ " } else { "    " },
            raw,
            width,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        first = false;
    }
    let body_width = width.saturating_sub(2);
    if dropped > 0 {
        lines.push(Line::from(Span::styled(
            format!("  │ … {dropped} earlier lines not kept"),
            faint(),
        )));
    }
    lines.extend(plain_lines(output, body_width, faint()));
    close_gutter(&mut lines);
    lines
}

/// Columns kept for a tool call's status after its summary (` ⠙ 12s`,
/// ` ✓ 274.0s`) and the space before it.
const STATUS_ROOM: usize = 11;

/// The status of a row still running, `secs` into it when it shows the
/// time: a tool call's or a subagent's (` ⠙ 12s`, after its summary), or a
/// `!` command's (`⠙ running 12s`).
fn running_status(spinner: &str, secs: Option<f32>, shell: bool) -> String {
    let time = secs.map(|s| format!(" {s:.0}s")).unwrap_or_default();
    if shell {
        format!("{spinner} running{time}")
    } else {
        format!(" {spinner}{time}")
    }
}

/// What the status of a row still running shows: a tool call that has not
/// returned, a subagent at work (since it started, not since its call
/// returned) or a `!` command.
fn running_since(b: &TBlock) -> Option<Live> {
    match b {
        TBlock::Tool {
            agent: Some(run), ..
        } => run.status.is_none().then_some(Live {
            since: run.started,
            clock: true,
            shell: false,
        }),
        TBlock::Tool {
            name,
            done: false,
            started,
            ..
        } => Some(Live {
            since: *started,
            // The time a question takes is the user's, not the tool's.
            clock: !waits_on_user(name),
            shell: false,
        }),
        TBlock::Shell {
            status: ShellStatus::Running,
            started,
            ..
        } => Some(Live {
            since: *started,
            clock: true,
            shell: true,
        }),
        _ => None,
    }
}

/// Draws the status of each running row again, with the spinner's frame and
/// the time so far, and fits the line to the width. Only that line changes,
/// so the block is not laid out again: a row far up would otherwise lay out
/// all below it on every tick.
fn patch_running(view: &mut TranscriptView, spinner: &str, width: usize) {
    for row in &view.live {
        let Some(top) = view.blocks.get(row.block).map(|k| k.top) else {
            continue;
        };
        let mut line = row.line.clone();
        let live = row.live;
        let secs = live.clock.then(|| live.since.elapsed().as_secs_f32());
        if let Some(span) = line.spans.get_mut(row.span) {
            span.content = running_status(spinner, secs, live.shell).into();
        }
        let mut lines = [line];
        clamp_lines(&mut lines, width);
        let [line] = lines;
        view.lines[top] = line.spans.iter().map(|s| s.content.as_ref()).collect();
        view.rendered[top] = line;
    }
}

/// Fewest columns a tool call's summary gets beside its name; with less, the
/// name has a line of its own.
const SUMMARY_MIN: usize = 25;

/// `text` cut to at most `width` cells, ending in `…` when cut.
fn shorten(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(1);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Blank lines between two blocks: one, except inside a run of tool calls
/// and hooks, which stay compact (each call's gutter is closed off with `└`
/// instead), so a burst of calls does not take a row more per call.
fn block_gap(before: &TBlock, after: &TBlock) -> usize {
    let compact = |b: &TBlock| matches!(b, TBlock::Tool { .. } | TBlock::Hook { .. });
    usize::from(!(compact(before) && compact(after)))
}

fn is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Ends the gutter on a block's last line (`└` for `│`), closing it off
/// from whatever comes next.
fn close_gutter(lines: &mut [Line<'static>]) {
    if let Some(span) = lines.last_mut().and_then(|l| l.spans.first_mut())
        && let Some(rest) = span.content.strip_prefix("  │")
    {
        span.content = format!("  └{rest}").into();
    }
}

/// A fingerprint of everything `block_lines` reads from a block, and of the
/// space above it.
fn block_key(b: &TBlock, gap: usize, followed: bool, thinking_live: bool, elapsed: f32) -> u64 {
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
    gap.hash(&mut h);
    std::mem::discriminant(b).hash(&mut h);
    match b {
        TBlock::User { text } => text.hash(&mut h),
        TBlock::Assistant {
            text,
            sender,
            duration,
        } => (text, sender, duration, followed).hash(&mut h),
        TBlock::Handoff { text, sender, to } => (text, sender, to).hash(&mut h),
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
        TBlock::Hook {
            name,
            state,
            output,
            ..
        } => (name, state, output).hash(&mut h),
        TBlock::Shell {
            command,
            output,
            dropped,
            status,
            duration,
            ..
        } => {
            (command, output, dropped, duration).hash(&mut h);
            status.label().hash(&mut h);
        }
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
    let started = Instant::now();
    let mut view = std::mem::take(&mut app.transcript_view);
    if view.width != inner.width {
        view.width = inner.width;
        view.blocks.clear();
        view.live.clear();
    }
    let mut fresh = 0;
    let shown = app.shown_blocks();
    for (i, b) in shown.iter().enumerate() {
        // The space above a block is laid out with it: it depends on the
        // block before, and a change there lays out this one again anyway.
        let gap = i.checked_sub(1).map_or(0, |p| block_gap(&shown[p], b));
        let followed = i + 1 < shown.len();
        let key = block_key(b, gap, followed, thinking_live, elapsed);
        if i == fresh && view.blocks.get(i).is_some_and(|k| k.key == key) {
            // Its start is read again, not kept: the key leaves it out.
            if let Some(row) = view.live.iter_mut().find(|r| r.block == i)
                && let Some(live) = running_since(b)
            {
                row.live = live;
            }
            fresh += 1;
            continue;
        }
        if i == fresh {
            // First stale block: drop its lines and everything after.
            let keep = i.checked_sub(1).map_or(0, |p| view.blocks[p].end);
            view.blocks.truncate(i);
            view.live.retain(|r| r.block < i);
            view.rendered.truncate(keep);
            view.lines.truncate(keep);
        }
        let (mut lines, status_span) = block_layout(b, width, followed, thinking_live, elapsed);
        // The gap is the only space between blocks.
        while lines.last().is_some_and(is_blank) {
            lines.pop();
        }
        // A running row's first line is kept as it is before being fitted
        // to the width, so that its status can be drawn again and fitted.
        if let Some(live) = running_since(b)
            && let Some(span) = status_span
            && let Some(first) = lines.first()
            && span < first.spans.len()
        {
            view.live.push(LiveRow {
                block: i,
                line: first.clone(),
                span,
                live,
            });
        }
        clamp_lines(&mut lines, inner.width as usize);
        if !lines.is_empty() {
            for _ in 0..gap {
                view.lines.push(String::new());
                view.rendered.push(Line::default());
            }
        }
        let top = view.rendered.len();
        view.lines.extend(lines.iter().map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        }));
        view.rendered.extend(lines);
        view.blocks.push(KeptBlock {
            key,
            top,
            end: view.rendered.len(),
        });
    }
    if fresh == shown.len() && view.blocks.len() > fresh {
        // Blocks were removed from the end (rewind, /clear).
        let keep = fresh.checked_sub(1).map_or(0, |p| view.blocks[p].end);
        view.blocks.truncate(fresh);
        view.live.retain(|r| r.block < fresh);
        view.rendered.truncate(keep);
        view.lines.truncate(keep);
    }
    patch_running(&mut view, app.spinner(), inner.width as usize);
    view.took = started.elapsed();

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
            faint()
        };
        if !is_error && looks_like_diff(output) {
            lines.extend(diff_lines(output, width));
        } else {
            lines.extend(plain_lines(output, width, style));
        }
    }
    lines
}

/// The "allow always" step of a permission request: what would be allowed
/// from now on, and where, before anything is written.
fn always_lines(draft: &AlwaysDraft, width: usize) -> Vec<Line<'static>> {
    let gray = Style::default().fg(Color::Gray);
    let rules = draft.rules();
    if rules.is_empty() {
        let mut lines = wrap_prefixed_text(
            "  ",
            "No rule can cover this request: a command that uses substitution or \
             redirection, or a tool call with nothing to tell it by. It is asked \
             about each time.",
            width,
            Style::default().fg(Color::Yellow),
        );
        lines.push(Line::from(Span::styled("  Esc back", gray)));
        return lines;
    }
    let place = |scope: Scope| match scope {
        Scope::Workspace => "this workspace",
        Scope::Global => "every workspace",
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("  Always allow, in {}, on any harness:", place(draft.scope)),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ))];
    for rule in &rules {
        lines.extend(wrap_prefixed_text(
            "    ",
            &rule.describe(),
            width,
            Style::default(),
        ));
        if rule.pattern().is_none() && rule.tool != "mcp" {
            lines.extend(wrap_prefixed_text(
                "      ",
                "(by this harness's name for the tool; another may call it something else)",
                width,
                gray,
            ));
        }
    }
    if let Some(pattern) = &draft.pattern {
        lines.push(Line::from(vec![
            Span::styled("  Pattern: ", Style::default().fg(Color::Green)),
            Span::raw(pattern.clone()),
            Span::styled("▏", Style::default().fg(Color::Green)),
        ]));
    }
    if let Some(problem) = &draft.problem {
        lines.extend(wrap_prefixed_text(
            "  ",
            problem,
            width,
            Style::default().fg(Color::Red),
        ));
    }
    let mut hint = "  Enter save and allow".to_string();
    if draft.has_workspace {
        hint.push_str(&format!(" · Tab {}", place(draft.other_scope())));
    }
    if draft.pattern.is_some() {
        hint.push_str(" · type to change the pattern");
    }
    hint.push_str(" · Esc back");
    lines.push(Line::from(Span::styled(hint, gray)));
    lines
}

/// Under the policy list: how to save the selected policy as the
/// harness's default, or where it is about to be saved.
fn policy_save_lines(p: &PolicyPicker, harness: &str, width: usize) -> Vec<Line<'static>> {
    let gray = Style::default().fg(Color::DarkGray);
    let place = |scope: Scope| match scope {
        Scope::Workspace => "this workspace",
        Scope::Global => "every workspace",
    };
    let (Some(scope), Some(pol)) = (p.save, p.list.current()) else {
        return vec![Line::from(Span::styled(
            format!(" d save as {harness}'s default"),
            gray,
        ))];
    };
    let mut lines = wrap_prefixed_text(
        " ",
        &format!(
            "Save {pol} as {harness}'s default policy, for {}?",
            place(scope)
        ),
        width,
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    );
    let mut hint = " Enter save and use it".to_string();
    if p.has_workspace {
        let other = match scope {
            Scope::Workspace => Scope::Global,
            Scope::Global => Scope::Workspace,
        };
        hint.push_str(&format!(" · Tab {}", place(other)));
    }
    hint.push_str(" · Esc back");
    lines.push(Line::from(Span::styled(hint, gray)));
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

/// Markdown drawn as the transcript draws it, `indent` cells in instead of
/// the renderer's two-cell margin, on `base`: the markdown's colours
/// override it, its modifiers (`base`'s bold) add to it.
fn markdown_lines(text: &str, indent: usize, max_width: usize, base: Style) -> Vec<Line<'static>> {
    let pad = " ".repeat(indent);
    render_markdown_to_lines(text, (max_width + 2).saturating_sub(indent).max(10), true)
        .into_iter()
        .map(|line| {
            let mut spans = line.spans;
            if let Some(first) = spans.first_mut()
                && let Some(rest) = first.content.strip_prefix("  ")
            {
                first.content = rest.to_string().into();
            }
            spans.insert(0, Span::raw(pad.clone()));
            Line::from(spans).style(base.patch(line.style))
        })
        .collect()
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
    } else if let Some(run) = &app.shell {
        Span::styled(
            format!(
                " {} Running your command · {:.0}s · Esc stops it ",
                app.spinner(),
                run.started.elapsed().as_secs_f32()
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(label) = app.subagents_label().filter(|_| !app.awaiting_answer()) {
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
    } else if app.awaiting_answer() {
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
    let wanted = app.wanted_policy();
    let effective = app.effective_policy();
    let sandbox = app.sandbox_level().0;
    let policy_text = match effective {
        Some(e) if e == wanted => format!("policy {e}"),
        Some(e) => format!("policy {wanted}→{e}"),
        None => format!("policy {wanted} (unavailable)"),
    };
    // Effort goes first when the row is too narrow, then the model, then
    // the sandbox; the harness and its policy always stay.
    let segments = [
        (
            format!("[{}]", app.short_name()),
            Style::default()
                .fg(harness_color(app.active))
                .add_modifier(Modifier::BOLD),
            0,
        ),
        (app.model_label(), Style::default().fg(Color::Yellow), 2),
        (
            format!("effort {}", app.current_effort().unwrap_or("default")),
            Style::default().fg(Color::LightBlue),
            1,
        ),
        (
            policy_text,
            Style::default().fg(effective.map_or(Color::Red, policy_color)),
            0,
        ),
        (
            format!("sandbox {}", sandbox_label(sandbox)),
            Style::default().fg(sandbox_color(sandbox)),
            3,
        ),
    ];
    let right1 = fit_segments(&segments, width.saturating_sub(SPLIT_LEFT_MIN));
    render_split(frame, rows[4], left1, right1, true);

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
    render_split(frame, rows[5], left2, right2, false);

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

/// The status segments that fit in `room` cells: the first, a space, and
/// the rest separated by ` · `. Segments are dropped in the order their
/// number gives (0: never) until the row fits.
fn fit_segments(segments: &[(String, Style, u8)], room: usize) -> Line<'static> {
    let mut kept: Vec<&(String, Style, u8)> = segments.iter().collect();
    let width = |kept: &[&(String, Style, u8)]| {
        kept.iter().map(|(t, _, _)| t.width()).sum::<usize>()
            + kept.len().saturating_sub(2) * 3
            + usize::from(kept.len() > 1)
    };
    while width(&kept) > room {
        let Some(i) = (0..kept.len())
            .filter(|&i| kept[i].2 > 0)
            .min_by_key(|&i| kept[i].2)
        else {
            break;
        };
        kept.remove(i);
    }
    let mut spans = Vec::new();
    for (i, (text, style, _)) in kept.into_iter().enumerate() {
        match i {
            0 => {}
            1 => spans.push(Span::raw(" ")),
            _ => spans.push(Span::styled(" · ", *style)),
        }
        spans.push(Span::styled(text.clone(), *style));
    }
    Line::from(spans)
}

/// Cells the left side of a split row keeps before the right side goes.
const SPLIT_LEFT_MIN: usize = 12;

/// Left text and right text on one row; the left side is shortened first
/// when the terminal is too narrow for both. Then one goes: the right
/// side where it must stay (`keep_right`), else the left.
fn render_split(
    frame: &mut Frame,
    row: Rect,
    left: Line<'static>,
    right: Line<'static>,
    keep_right: bool,
) {
    let width = row.width as usize;
    let lw = left.width();
    let rw = right.width();
    if lw + 2 + rw <= width {
        let mut spans = left.spans;
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right.spans);
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    } else if rw + SPLIT_LEFT_MIN <= width {
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
        let mut lines = vec![if keep_right { right } else { left }];
        clamp_lines(&mut lines, width);
        frame.render_widget(Paragraph::new(lines.remove(0)), row);
    }
}

fn render_suggestions(frame: &mut Frame, app: &App, prompt_row: Rect) {
    let files = app.completing_file.is_some();
    let title = if files {
        " Files [Tab to complete] "
    } else {
        " Suggestions [Tab to complete] "
    };
    let count = app.suggestions.len().min(8) as u16;
    // The window moves down with the selection once it passes the last row.
    let first = (app.selected_suggestion + 1).saturating_sub(count as usize);

    // As wide as the widest entry needs, up to the prompt's width. Every
    // entry is measured, not only those shown, so that the box keeps its
    // size while the list scrolls.
    let max_width = prompt_row.width.saturating_sub(4) as usize;
    let widest = |of: fn(&(String, String)) -> &str| {
        app.suggestions
            .iter()
            .map(|e| of(e).width())
            .max()
            .unwrap_or(0)
    };
    let widest_name = widest(|(c, _)| c);
    let (name_col, content) = if files {
        // Marker and a space after the path.
        (0, widest_name + 3)
    } else {
        // Names get up to 2/5 of the width, 12 columns where that is less
        // and the pane has room for them.
        let inner_max = max_width.saturating_sub(2);
        let cap = (inner_max * 2 / 5).max(12).min(inner_max / 2);
        let name_col = widest_name.max(12).min(cap);
        // Marker, name column, " ─ ", description and a space.
        (name_col, name_col + 5 + widest(|(_, d)| d) + 1)
    };
    let width = (content + 2).max(title.width() + 2).min(max_width);
    let inner = width.saturating_sub(2);
    // A space is left before the border.
    let room = inner.saturating_sub(name_col + 6);

    let mut lines = Vec::new();
    let shown = app.suggestions.iter().enumerate().skip(first);
    for (i, (cmd, desc)) in shown.take(count as usize) {
        let selected = i == app.selected_suggestion;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let marker = if selected { "❯ " } else { "  " };
        if files {
            // A path too long for the list keeps its end: the file's name.
            let path = keep_end(cmd, inner.saturating_sub(3));
            lines.push(Line::styled(format!("{marker}{path} "), style));
            continue;
        }
        // A name keeps its end too: `plugin:` prefixes are shared.
        let name = keep_end(cmd, name_col);
        let pad = " ".repeat(name_col.saturating_sub(name.width()));
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{name}{pad} "), style),
            Span::styled(
                format!("─ {}", keep_start(desc, room)),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    // Where any description is cut, the selected one is shown below the
    // list on up to two lines. The rows are kept for every selection, so
    // that the list does not move with it.
    let mut detail = Vec::new();
    if !files && app.suggestions.iter().any(|(_, d)| d.width() > room) {
        let desc = app
            .suggestions
            .get(app.selected_suggestion)
            .map_or("", |(_, d)| d.as_str());
        let wrap = inner.saturating_sub(2);
        let mut wrapped = wrap_words(desc, wrap);
        if wrapped.len() > 2 {
            wrapped[1] = keep_start(&format!("{}…", wrapped[1]), wrap);
        }
        wrapped.resize(2, String::new());
        let dim = Style::default().fg(Color::DarkGray);
        detail.push(Line::styled("─".repeat(inner), dim));
        detail.extend(
            wrapped
                .into_iter()
                .map(|l| Line::styled(format!(" {l}"), Style::default().fg(Color::Gray))),
        );
    }

    // Sits above the status rule so it never covers the prompt.
    let top = prompt_row.y.saturating_sub(1);
    let list_height = count + 2;
    if top < list_height + detail.len() as u16 {
        detail.clear();
    }
    let height = list_height + detail.len() as u16;
    if top < height {
        return;
    }
    lines.extend(detail);
    let area = Rect {
        x: prompt_row.x + 1,
        y: top - height,
        width: width as u16,
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(title),
        ),
        area,
    );
}

/// The start of `text` that fits in `width` columns, `…` marking a cut.
fn keep_start(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = String::new();
    let mut used = 1;
    for c in text.chars() {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        kept.push(c);
    }
    kept.push('…');
    kept
}

/// `text` cut at the front to `width` columns, an ellipsis for what went.
fn keep_end(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for c in text.chars().rev() {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        kept.push(c);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
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
        // A row that cannot be chosen is dimmed and struck through, with
        // why in place of its description.
        if let Some(why) = picker.disabled_reason(i) {
            let dim = Style::default().fg(Color::DarkGray);
            let pad = 28usize.saturating_sub(label.chars().count());
            lines.push(Line::from(vec![
                Span::styled("   ", dim),
                Span::styled(label, dim.add_modifier(Modifier::CROSSED_OUT)),
                Span::styled(format!("{} ", " ".repeat(pad)), dim),
                Span::styled(format!("─ {why}"), dim),
            ]));
            continue;
        }
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
                let status = format!(
                    "v{}{}",
                    o.version.as_deref().unwrap_or("?"),
                    if o.interactive_permissions {
                        ", interactive permissions"
                    } else {
                        ", no permission prompts"
                    }
                );
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
            let popup = centered_rect(80, 50, area);
            let width = popup.width.saturating_sub(2) as usize;
            let caps = app.caps();
            let effective = app.effective_policy();
            let degraded =
                |pol: &PermissionPolicy| caps.supports_policy(*pol).and_then(|s| s.degraded);
            let mut lines = picker_lines(&p.list, Color::Green, |pol| {
                let note = if degraded(pol).is_some() {
                    " (degraded)"
                } else {
                    ""
                };
                (
                    pol.as_str().to_string(),
                    format!("{}{note}", pol.description()),
                    Some(*pol) == effective,
                )
            });
            // What was asked for cannot be had here: say so above the list.
            let wanted = app.wanted_policy();
            if caps.supports_policy(wanted).is_none() {
                let mut note = format!("{wanted} is not offered by {}", app.display_name());
                if let Some(e) = effective {
                    note.push_str(&format!("; running {e}"));
                }
                let note =
                    wrap_prefixed_text(" ", &note, width, Style::default().fg(Color::Yellow));
                lines.splice(0..1, note);
            }
            // How the selected policy falls short here, in full.
            if let Some(d) = p.list.current().and_then(degraded) {
                lines.push(Line::default());
                lines.extend(wrap_prefixed_text(
                    " ",
                    &format!("degraded: {d}"),
                    width,
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(Line::default());
            lines.extend(policy_save_lines(p, app.short_name(), width));
            (
                popup,
                modal_block(format!(" Permission policy ({NAV}) "), Color::Green),
                lines,
            )
        }
        Modal::Sandbox(p) => {
            let popup = centered_rect(80, 50, area);
            let width = popup.width.saturating_sub(2) as usize;
            let mut lines = picker_lines(p, Color::Magenta, |level| {
                (
                    level.as_str().to_string(),
                    level.description().to_string(),
                    *level == app.sandbox_level().0,
                )
            });
            if let Err(why) = &app.sandbox.backend {
                let note = wrap_prefixed_text(
                    " ",
                    &format!("no sandbox here: {why}"),
                    width,
                    Style::default().fg(Color::Yellow),
                );
                lines.splice(0..1, note);
            }
            (
                popup,
                modal_block(format!(" Sandbox ({NAV}) "), Color::Magenta),
                lines,
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
                description,
                ..
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
                if let Some(draft) = &m.always {
                    lines.extend(always_lines(draft, width));
                } else if m.denying {
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
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  y/Enter allow once · a allow always · n deny · i {} input · Esc cancel",
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
        Modal::Question(_) => return, // `render_question`
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

/// The question modal: a strip naming every question and the review page,
/// the question, its options, and the highlighted option's preview beside
/// them (below them when the terminal is narrow).
fn render_question(frame: &mut Frame, m: &mut QuestionModal, area: Rect) {
    let popup = if m.has_previews() {
        centered_rect(90, 80, area)
    } else {
        centered_rect(80, 60, area)
    };
    let block = modal_block(" The agent has a question ".into(), Color::Cyan);
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let width = inner.width as usize;
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let mut head = Vec::new();
    if m.page_count() > 1 {
        head.push(question_tabs(m, width));
        head.push(Line::default());
    }
    match m.current() {
        Some(q) => head.extend(markdown_lines(&q.text, 0, width, bold)),
        None => head.push(Line::from(Span::styled("Review your answers", bold))),
    }
    let previewing = m
        .current()
        .is_some_and(|q| q.options.iter().any(|o| o.preview.is_some()));
    let hint = wrap_prefixed_text(
        "",
        &question_hint(m, previewing),
        width,
        Style::default().fg(Color::Gray),
    );
    let hint_rows = (hint.len() as u16).min(2);
    // Leave the options at least a few rows on a short terminal.
    let head_rows = (head.len() as u16).min(inner.height.saturating_sub(6).max(1));
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(head_rows),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(hint_rows),
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(head), rows[1]);
    frame.render_widget(Paragraph::new(hint), rows[5]);
    let body = rows[3];

    match m.current() {
        None => frame.render_widget(Paragraph::new(review_lines(m, width)), body),
        Some(_) if previewing => {
            let (options, preview) = if body.width >= 80 {
                let left = (body.width * 2 / 5).max(28);
                let cols = Layout::horizontal([
                    Constraint::Length(left),
                    Constraint::Length(1),
                    Constraint::Min(20),
                ])
                .split(body);
                (cols[0], cols[2])
            } else {
                let (lines, _) = question_option_lines(m, body.width as usize);
                let top = (lines.len() as u16).min(body.height / 2).max(3);
                let parts =
                    Layout::vertical([Constraint::Length(top), Constraint::Min(3)]).split(body);
                (parts[0], parts[1])
            };
            render_question_options(frame, m, options);
            render_question_preview(frame, m, preview);
        }
        Some(_) => {
            m.preview_max = 0;
            m.preview_scroll = 0;
            render_question_options(frame, m, body);
        }
    }
}

/// The keys that work on the current page.
fn question_hint(m: &QuestionModal, previewing: bool) -> String {
    if m.editing_other {
        return "type your answer · Enter done · Esc cancel".into();
    }
    if m.on_review() {
        let back = "←/Shift+Tab back · Esc dismiss";
        return if m.unanswered().is_empty() {
            format!("Enter submit · {back}")
        } else {
            format!("Enter go to the first unanswered · {back}")
        };
    }
    let mut parts = Vec::new();
    if m.row_count() > 1 {
        parts.push("↑/↓ select");
    }
    if m.is_other_row() {
        parts.push("Enter type your answer");
    } else if m.current().is_some_and(|q| q.multi) {
        parts.push("Space toggle · Enter next");
    } else {
        parts.push("Enter choose");
    }
    if m.page_count() > 1 {
        parts.push("←/→ question");
    }
    if previewing {
        parts.push("PgUp/PgDn preview");
    }
    parts.push("Esc dismiss");
    parts.join(" · ")
}

/// `Color ✓ · Size · Submit`, the current page highlighted. Long names are
/// shortened, and with too many pages the strip shows the ones around the
/// current page.
fn question_tabs(m: &QuestionModal, width: usize) -> Line<'static> {
    let mut names: Vec<String> = m
        .questions
        .iter()
        .map(super::herdr::question_label)
        .collect();
    if m.has_review() {
        names.push("Submit".into());
    }
    let n = names.len();
    let cur = m.idx;
    if cur >= n {
        return Line::default();
    }
    let answered: Vec<bool> = (0..n)
        .map(|i| i < m.questions.len() && m.has_answer(i))
        .collect();
    const SEP: &str = " · ";
    let sep = SEP.width();
    let tab_width = |name: &str, done: bool| name.width() + 2 + if done { 2 } else { 0 };
    let widths = |shown: &[String]| -> Vec<usize> {
        shown
            .iter()
            .zip(&answered)
            .map(|(name, done)| tab_width(name, *done))
            .collect()
    };
    // Shorten the longest names, down to ten columns, until the strip fits.
    let mut cap = names.iter().map(|name| name.width()).max().unwrap_or(0);
    let mut shown = names.clone();
    while widths(&shown).iter().sum::<usize>() + sep * (n - 1) > width && cap > 10 {
        cap -= 1;
        shown = names.iter().map(|name| keep_start(name, cap)).collect();
    }
    let mut w = widths(&shown);
    // Columns from tab `first` through the current one, with a `…` on
    // either side where tabs are left out.
    let span = |w: &[usize], first: usize| {
        usize::from(first > 0)
            + w[first..=cur].iter().sum::<usize>()
            + sep * (cur - first)
            + usize::from(cur + 1 < n)
    };
    let mut first = 0;
    while first < cur && span(&w, first) > width {
        first += 1;
    }
    let overflow = span(&w, first).saturating_sub(width);
    if overflow > 0 {
        // Too wide even on its own: the current tab's name gives way.
        let room = shown[cur].width().saturating_sub(overflow).max(1);
        shown[cur] = keep_start(&names[cur], room);
        w = widths(&shown);
    }

    let gray = Style::default().fg(Color::Gray);
    let mut spans = Vec::new();
    let mut used = 0;
    if first > 0 {
        spans.push(Span::styled("…", gray));
        used += 1;
    }
    for (i, name) in shown.iter().enumerate().skip(first) {
        let gap = if i > first { sep } else { 0 };
        if i > cur && used + gap + w[i] + usize::from(i + 1 < n) > width {
            spans.push(Span::styled("…", gray));
            break;
        }
        if gap > 0 {
            spans.push(Span::styled(SEP, Style::default().fg(Color::DarkGray)));
        }
        let style = if i == cur {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if answered[i] || i == m.questions.len() {
            Style::default()
        } else {
            gray
        };
        spans.push(Span::styled(format!(" {name}"), style));
        if answered[i] {
            let check = if i == cur {
                style
            } else {
                Style::default().fg(Color::Green)
            };
            spans.push(Span::styled(" ✓", check));
        }
        spans.push(Span::styled(" ", style));
        used += gap + w[i];
    }
    Line::from(spans)
}

/// The options of the current question, each label with its description
/// under it, and the line range each row takes.
fn question_option_lines(
    m: &QuestionModal,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let mut lines = Vec::new();
    let mut ranges = Vec::new();
    let Some(q) = m.current() else {
        return (lines, ranges);
    };
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    for (i, opt) in q.options.iter().enumerate() {
        let start = lines.len();
        let selected = i == m.cursor;
        let chosen = m.chosen[m.idx][i];
        let mark = match (q.multi, chosen) {
            (true, true) => "[x]",
            (true, false) => "[ ]",
            (false, true) => "(•)",
            (false, false) => "( )",
        };
        let label = truncate_chars(&opt.label, width.saturating_sub(7).max(4));
        lines.push(Line::from(Span::styled(
            format!("{} {mark} {label} ", if selected { "❯" } else { " " }),
            if selected {
                highlight
            } else {
                Style::default()
            },
        )));
        if !opt.description.is_empty() {
            lines.extend(markdown_lines(
                &opt.description,
                6,
                width,
                Style::default().fg(Color::Gray),
            ));
        }
        ranges.push((start, lines.len()));
    }
    if takes_text(q) {
        let start = lines.len();
        let selected = m.is_other_row();
        let text = &m.other[m.idx];
        let label = if q.options.is_empty() {
            "[answer]"
        } else {
            "[other]"
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} {label} ", if selected { "❯" } else { " " }),
                if selected {
                    highlight
                } else {
                    Style::default()
                },
            ),
            Span::raw(keep_end(text, width.saturating_sub(12))),
            Span::styled(
                if m.editing_other { "▏" } else { "" },
                Style::default().fg(Color::Cyan),
            ),
        ]));
        ranges.push((start, lines.len()));
    }
    (lines, ranges)
}

/// The option list, scrolled so the highlighted row is in view.
fn render_question_options(frame: &mut Frame, m: &QuestionModal, area: Rect) {
    let (lines, ranges) = question_option_lines(m, area.width as usize);
    let height = area.height as usize;
    let offset = ranges
        .get(m.cursor)
        .map_or(0, |&(start, end)| end.saturating_sub(height).min(start));
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), area);
}

/// The highlighted option's preview, as markdown, in a pane of its own.
fn render_question_preview(frame: &mut Frame, m: &mut QuestionModal, area: Rect) {
    let pane = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Preview ");
    let inner = pane.inner(area);
    let lines = match m.preview() {
        Some(text) => render_markdown_to_lines(text, inner.width as usize, true),
        None => vec![Line::from(Span::styled(
            "No preview for this option.",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        ))],
    };
    let max = lines.len().saturating_sub(inner.height as usize) as u16;
    m.preview_max = max;
    m.preview_scroll = m.preview_scroll.min(max);
    let scroll = m.preview_scroll;
    let pane = if max > 0 {
        let more = if scroll < max { "↓" } else { "↑" };
        pane.title(
            Line::from(format!(" {more} {}/{} ", scroll + 1, max + 1)).alignment(Alignment::Right),
        )
    } else {
        pane
    };
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(pane), area);
}

/// What is still unanswered (first, so a long list cannot hide it), then
/// every question with its answer.
fn review_lines(m: &QuestionModal, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let missing: Vec<String> = m
        .unanswered()
        .into_iter()
        .map(|i| super::herdr::question_label(&m.questions[i]))
        .collect();
    if !missing.is_empty() {
        lines.extend(wrap_prefixed_text(
            "",
            &format!("Not answered yet: {}", missing.join(", ")),
            width,
            Style::default().fg(Color::Yellow),
        ));
        lines.push(Line::default());
    }
    let bold = Style::default().add_modifier(Modifier::BOLD);
    for (i, q) in m.questions.iter().enumerate() {
        let label = super::herdr::question_label(q);
        match m.answer_text(i) {
            Some(answer) => {
                lines.push(Line::from(vec![
                    Span::styled("✓ ", Style::default().fg(Color::Green)),
                    Span::styled(label, bold),
                ]));
                lines.extend(wrap_prefixed_text("  → ", &answer, width, Style::default()));
            }
            None => {
                lines.push(Line::from(vec![
                    Span::styled("✗ ", Style::default().fg(Color::Red)),
                    Span::styled(label, bold),
                ]));
                lines.push(Line::from(Span::styled(
                    "  (unanswered)",
                    Style::default().fg(Color::Red),
                )));
            }
        }
    }
    lines
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
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn a_row_that_cannot_be_chosen_is_struck_through_with_its_reason() {
        let p = ListPicker::new(vec!["ask", "bypass"])
            .with_disabled(|s| (*s == "ask").then(|| "not offered".to_string()));
        let lines = picker_lines(&p, Color::Green, |s| (s.to_string(), "desc".into(), true));
        let text = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let ask = &lines[1];
        assert!(text(ask).contains("─ not offered"));
        assert!(!text(ask).contains('❯') && !text(ask).contains("[active]"));
        assert!(
            ask.spans[1]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        // The cursor is on the row that can be chosen.
        assert!(text(&lines[2]).starts_with(" ❯ bypass"));
    }

    #[test]
    fn wrap_prefixed_text_indents_continuations() {
        let long = "jetski: no output produced — a tool required the mcp permission that headless mode cannot prompt for, so it was auto-denied.";
        let lines = wrap_prefixed_text("  ✗ ", long, 60, Style::default());
        assert!(lines.len() >= 2);
        assert_eq!(lines[0].spans[0].content, "  ✗ ");
        assert_eq!(lines[1].spans[0].content, "    ");
    }

    #[test]
    fn status_segments_go_in_their_order() {
        let s = Style::default();
        let segments = [
            ("[h]".to_string(), s, 0),
            ("model".to_string(), s, 2),
            ("effort".to_string(), s, 1),
            ("policy".to_string(), s, 0),
            ("sandbox".to_string(), s, 3),
        ];
        let text = |room| {
            fit_segments(&segments, room)
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(text(100), "[h] model · effort · policy · sandbox");
        assert_eq!(text(30), "[h] model · policy · sandbox");
        assert_eq!(text(25), "[h] policy · sandbox");
        assert_eq!(text(10), "[h] policy");
        assert_eq!(text(0), "[h] policy");
    }

    #[test]
    fn a_narrow_status_still_shows_the_harness_and_policy() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.cwd =
            std::path::PathBuf::from("/srv/projects/some-organisation/a-rather-long-repository");
        app.set_model("claude-a-model-with-a-long-name-20991231".into());
        assert!(app.set_policy(PermissionPolicy::Auto));
        for width in [24, 32, 50, 80, 120] {
            let (rows, _) = screen(&mut app, width, 30);
            assert!(
                rows.iter()
                    .any(|r| r.contains("[Claude]") && r.contains("policy auto")),
                "{width}: {rows:#?}"
            );
        }
    }

    #[test]
    fn wrap_words_breaks_long_tokens() {
        let w = wrap_words(&"x".repeat(25), 10);
        assert_eq!(w.len(), 3);
        assert_eq!(wrap_words("", 10), vec![String::new()]);
    }

    /// A reason too long for the picker's width is wrapped, not cut off.
    #[test]
    fn long_picker_notes_are_wrapped() {
        let joined = |rows: Vec<String>| {
            rows.iter()
                .map(|r| r.trim_matches(|c: char| c == '│' || c.is_whitespace()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut app = test_app(HarnessId::CLAUDE);
        let why = "Landlock is not available (Function not implemented (os error 38))";
        app.sandbox.backend = Err(why.into());
        app.open_sandbox_picker();
        let text = joined(screen(&mut app, 60, 30).0);
        assert!(text.contains("(os error 38))"), "{text}");
        assert!(text.contains("no sandbox here"), "{text}");

        // pi's `bypass` and its note, in full while it is selected.
        let mut app = test_app(HarnessId::PI);
        app.open_policy_picker();
        app.handle_modal_key(KeyEvent::from(crossterm::event::KeyCode::Down));
        let text = joined(screen(&mut app, 60, 30).0);
        assert!(text.contains("are auto-accepted"), "{text}");
        assert!(text.contains("d save as pi's default"), "{text}");

        // `d`: where it is about to be saved.
        app.handle_modal_key(KeyEvent::from(crossterm::event::KeyCode::Char('d')));
        let text = joined(screen(&mut app, 100, 30).0);
        assert!(
            text.contains("Save bypass as pi's default policy, for this workspace?"),
            "{text}"
        );
        assert!(text.contains("Tab every workspace"), "{text}");
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

    fn question_app(questions: Vec<crate::core::Question>) -> App {
        use crate::core::{AgentEvent, PermissionRequest};
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
            id: "q".into(),
            kind: PermissionKind::Question { questions },
            tool_call_id: None,
        }));
        app
    }

    fn greeting_and_tone() -> Vec<crate::core::Question> {
        use crate::core::{Question, QuestionOption};
        let previewed = |label: &str, preview: &str| QuestionOption {
            preview: Some(preview.into()),
            ..QuestionOption::new(label, format!("{label} wording"))
        };
        vec![
            Question {
                id: "Which greeting?".into(),
                header: "Greeting".into(),
                text: "Which greeting?".into(),
                options: vec![
                    previewed("Friendly", "# Welcome!\n\nUse `your-feature` today."),
                    QuestionOption::new("Plain", "No heading"),
                ],
                allow_other: true,
                multi: false,
            },
            Question {
                id: "Which tone?".into(),
                header: "Tone".into(),
                text: "Which tone?".into(),
                options: vec![
                    QuestionOption::new("Casual", ""),
                    QuestionOption::new("Formal", ""),
                ],
                allow_other: true,
                multi: false,
            },
        ]
    }

    fn find(rows: &[String], needle: &str) -> (usize, usize) {
        rows.iter()
            .enumerate()
            .find_map(|(y, r)| r.find(needle).map(|x| (y, r[..x].chars().count())))
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", rows.join("\n")))
    }

    #[test]
    fn question_text_and_descriptions_are_markdown() {
        use crate::core::{Question, QuestionOption};
        let long = "Keeps the **current** value, which is `false` unless set. ".repeat(4);
        let mut app = question_app(vec![Question {
            id: "q".into(),
            header: "Flag".into(),
            text: "Set `strict` to `true`?".into(),
            options: vec![
                QuestionOption::new("Yes", "Turns on `strict` mode"),
                QuestionOption::new("No", long.trim()),
            ],
            allow_other: false,
            multi: false,
        }]);
        let mut term = Terminal::new(TestBackend::new(80, 40)).unwrap();
        term.draw(|f| render(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        assert!(
            !rows.iter().any(|r| r.contains('`') || r.contains("**")),
            "{}",
            rows.join("\n")
        );
        // Inline code in the code style, the question still bold, the
        // description still gray and indented under its label.
        let (qy, qx) = find(&rows, "Set strict to true?");
        let (_, label_x) = find(&rows, "( ) Yes");
        assert_eq!(qx, label_x - 2);
        let strict = &buf[(qx as u16 + 4, qy as u16)];
        assert_eq!(strict.fg, Color::Yellow);
        assert!(strict.modifier.contains(Modifier::BOLD));
        let (dy, dx) = find(&rows, "Turns on strict mode");
        assert_eq!(dx, label_x + 4);
        assert_eq!(buf[(dx as u16, dy as u16)].fg, Color::Gray);
        assert_eq!(buf[(dx as u16 + 9, dy as u16)].fg, Color::Yellow);
        // A long description wraps, every line indented.
        let (wy, wx) = find(&rows, "Keeps the current value");
        assert_eq!(wx, dx);
        let wrapped: Vec<Vec<char>> = rows[wy..]
            .iter()
            .map(|r| r.chars().collect::<Vec<char>>())
            .take_while(|c| c[dx] != ' ')
            .collect();
        assert!(wrapped.len() >= 2, "{}", rows.join("\n"));
        for row in wrapped {
            assert!(row[label_x..dx].iter().all(|&c| c == ' '), "{row:?}");
        }
    }

    #[test]
    fn markdown_lines_fill_the_width_and_stay_inside_it() {
        let width = |l: &Line<'_>| l.width();
        // Text exactly as wide as the room left after the indent is one line.
        let lines = markdown_lines("aaaa `bb` cccc", 2, 14, Style::default());
        assert_eq!(lines.len(), 1);
        assert_eq!(width(&lines[0]), 14);
        assert_eq!(lines[0].spans[0].content, "  ");
        // No kind of block goes past the width, whatever the indent.
        let samples = [
            "Some prose that is long enough to wrap more than once here.",
            "- an item that wraps past the edge of the pane\n  - nested one",
            "1. first item that is long\n2. second",
            "> a quote that is long enough to wrap\n> > nested",
            "# A heading that is quite long",
            "```rust\nlet x = some_function_with_a_long_name(argument);\n```",
            "@@ -1 +1 @@\n-old line that is long enough\n+new line that is long enough",
            "| col | another column |\n|---|---|\n| a | a cell that is long |",
            "---",
        ];
        for text in samples {
            for max_width in [20, 30, 60] {
                for indent in [0, 6] {
                    for line in markdown_lines(text, indent, max_width, Style::default()) {
                        assert!(width(&line) <= max_width, "{max_width} {indent}: {line}");
                    }
                }
            }
        }
    }

    #[test]
    fn question_preview_sits_beside_the_options_or_below_them() {
        let mut app = question_app(greeting_and_tone());
        let (rows, _) = screen(&mut app, 140, 40);
        let (label_y, label_x) = find(&rows, "❯ ( ) Friendly");
        find(&rows, "Friendly wording");
        let (pane_y, pane_x) = find(&rows, "Preview");
        let (heading_y, heading_x) = find(&rows, "Welcome!");
        assert!(pane_x > label_x + 30 && heading_x > label_x + 30);
        assert!(pane_y <= label_y && heading_y < label_y + 4);
        // Inline code keeps its text; the markers are rendering, not content.
        find(&rows, "your-feature");

        // Narrow: the pane goes under the options.
        let (rows, _) = screen(&mut app, 70, 40);
        let (other_y, _) = find(&rows, "[other]");
        let (pane_y, _) = find(&rows, "Preview");
        assert!(pane_y > other_y);
        find(&rows, "Welcome!");

        // An option without one says so; a question without any has no pane.
        if let Some(Modal::Question(m)) = &mut app.modal {
            m.down();
        }
        let (rows, _) = screen(&mut app, 140, 40);
        find(&rows, "No preview for this option.");
        if let Some(Modal::Question(m)) = &mut app.modal {
            m.next_page();
        }
        let (rows, _) = screen(&mut app, 140, 40);
        find(&rows, "Which tone?");
        assert!(!rows.iter().any(|r| r.contains("Preview")));
    }

    #[test]
    fn a_question_without_options_shows_what_is_typed() {
        use crate::core::Question;
        use crossterm::event::KeyCode;
        let mut app = question_app(vec![Question {
            id: "name".into(),
            header: "Name".into(),
            text: "What should it be called?".into(),
            options: vec![],
            allow_other: false,
            multi: false,
        }]);
        let (rows, _) = screen(&mut app, 100, 30);
        find(&rows, "❯ [answer]");
        find(&rows, "Enter type your answer");
        assert!(!rows.iter().any(|r| r.contains("↑/↓ select")));
        app.handle_modal_key(KeyEvent::from(KeyCode::Enter));
        for c in "zebra".chars() {
            app.handle_modal_key(KeyEvent::from(KeyCode::Char(c)));
        }
        let (rows, _) = screen(&mut app, 100, 30);
        find(&rows, "[answer] zebra");
        app.handle_modal_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.modal.is_none());
    }

    #[test]
    fn long_preview_scrolls() {
        let mut questions = greeting_and_tone();
        let long: Vec<String> = (1..=60).map(|i| format!("line {i}")).collect();
        questions[0].options[0].preview = Some(long.join("\n\n"));
        let mut app = question_app(questions);
        let (rows, _) = screen(&mut app, 140, 40);
        find(&rows, "line 1");
        find(&rows, "↓ 1/");
        assert!(!rows.iter().any(|r| r.contains("line 60")));
        for _ in 0..40 {
            app.handle_modal_key(KeyEvent::from(crossterm::event::KeyCode::PageDown));
        }
        let (rows, _) = screen(&mut app, 140, 40);
        find(&rows, "line 60");
        find(&rows, "↑ ");
    }

    #[test]
    fn preview_scroll_follows_the_size_it_was_drawn_at() {
        use crossterm::event::KeyCode;
        let mut questions = greeting_and_tone();
        let long: Vec<String> = (1..=60).map(|i| format!("line {i}")).collect();
        questions[0].options[0].preview = Some(long.join("\n\n"));
        let mut app = question_app(questions);
        let scroll = |app: &App| match &app.modal {
            Some(Modal::Question(m)) => (m.preview_scroll, m.preview_max),
            _ => unreachable!(),
        };
        screen(&mut app, 140, 40);
        for _ in 0..40 {
            app.handle_modal_key(KeyEvent::from(KeyCode::PageDown));
        }
        screen(&mut app, 140, 40);
        let (bottom, _) = scroll(&app);
        // Taller: the end comes sooner, and PageUp moves from there.
        screen(&mut app, 140, 60);
        let (at, max) = scroll(&app);
        assert!(at == max && max < bottom);
        app.handle_modal_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(scroll(&app).0, max - 5);

        // A page without a preview has nothing to scroll.
        app.handle_modal_key(KeyEvent::from(KeyCode::Tab));
        screen(&mut app, 140, 40);
        app.handle_modal_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(scroll(&app), (0, 0));
    }

    #[test]
    fn question_tabs_and_review_page() {
        use crossterm::event::KeyCode;
        let mut app = question_app(greeting_and_tone());
        let (rows, _) = screen(&mut app, 140, 40);
        let (tabs_y, _) = find(&rows, "Greeting");
        assert!(rows[tabs_y].contains("Tone") && rows[tabs_y].contains("Submit"));
        assert!(!rows[tabs_y].contains("✓"));

        // Answer the first, skip the second, look at the review page.
        app.handle_modal_key(KeyEvent::from(KeyCode::Enter));
        app.handle_modal_key(KeyEvent::from(KeyCode::Tab));
        let (rows, _) = screen(&mut app, 140, 40);
        find(&rows, "Greeting ✓");
        find(&rows, "Review your answers");
        find(&rows, "✓ Greeting");
        find(&rows, "→ Friendly");
        find(&rows, "✗ Tone");
        find(&rows, "(unanswered)");
        find(&rows, "Not answered yet: Tone");
        find(&rows, "Enter go to the first unanswered");

        // Enter there opens the unanswered question instead of sending.
        app.handle_modal_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.take_actions().is_empty());
        let (rows, _) = screen(&mut app, 140, 40);
        find(&rows, "Which tone?");
    }

    #[test]
    fn question_tabs_fit_and_keep_the_current_page_by_columns() {
        use crate::core::{Question, QuestionOption};
        let headers = ["A rather long header number", "設定の確認と変更について"];
        for header in headers {
            for n in 1..=15 {
                let questions: Vec<Question> = (0..n)
                    .map(|i| Question {
                        id: format!("q{i}"),
                        header: format!("{header} {i}"),
                        text: format!("Question {i}?"),
                        options: vec![QuestionOption::new("Yes", "")],
                        allow_other: false,
                        multi: false,
                    })
                    .collect();
                let mut m = QuestionModal::new("r".into(), questions);
                m.chosen[0][0] = true;
                for page in 0..m.page_count() {
                    m.go_to(page);
                    for width in 16..=140 {
                        let line = question_tabs(&m, width);
                        assert!(line.width() <= width, "{n} {page} {width}: {line}");
                        let current: String = line
                            .spans
                            .iter()
                            .filter(|s| s.style.bg == Some(Color::Cyan))
                            .map(|s| s.content.as_ref())
                            .collect();
                        assert!(
                            current.trim().chars().any(|c| c != '…' && c != '✓'),
                            "{n} {page} {width}: {line}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn question_tabs_shorten_and_follow_the_current_page() {
        use crate::core::{Question, QuestionOption};
        let questions: Vec<Question> = (1..=12)
            .map(|i| Question {
                id: format!("q{i}"),
                header: format!("A rather long header number {i}"),
                text: format!("Question {i}?"),
                options: vec![QuestionOption::new("Yes", "")],
                allow_other: false,
                multi: false,
            })
            .collect();
        let mut app = question_app(questions);
        if let Some(Modal::Question(m)) = &mut app.modal {
            m.go_to(10);
        }
        let (rows, _) = screen(&mut app, 100, 40);
        let (tabs_y, _) = find(&rows, "A rather");
        let strip = rows[tabs_y].trim_matches(|c| c == '│' || c == ' ');
        // Shortened, and starting after the first pages so the current one
        // (highlighted) is in view.
        assert!(
            strip.starts_with("… ") && !strip.contains("number"),
            "{strip}"
        );
        assert!(rows.iter().any(|r| r.contains("Question 11?")));
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
        // The transcript ends with that prompt, so the drag stops in it.
        assert_eq!(
            app.take_copy_request().as_deref(),
            Some("src/tui/app.rs please\n\n❯ You\n  second")
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
    fn a_call_shows_what_its_summary_left_out_and_a_question_no_time() {
        let tool = |name: &str, input: serde_json::Value, output: &str, collapsed: bool| {
            let block = TBlock::Tool {
                id: "t".into(),
                name: name.into(),
                input,
                output: output.into(),
                is_error: false,
                done: true,
                collapsed,
                started: std::time::Instant::now(),
                duration: Some(std::time::Duration::from_secs(274)),
                agent: None,
            };
            block_lines(&block, 80, false, false, 0.0)
                .iter()
                .map(crate::tui::code::line_text)
                .collect::<Vec<_>>()
        };
        let input = serde_json::json!({"limit": 5, "query": "open issues", "repo": "unharness"});
        let collapsed = tool("mcp__forge__search", input.clone(), "3 found", true);
        assert!(collapsed[0].contains("open issues"), "{collapsed:?}");
        assert!(!collapsed[0].contains('{'), "{collapsed:?}");
        // Collapsed, the output and a count of what expanding shows.
        assert_eq!(collapsed.len(), 3, "{collapsed:?}");
        assert!(collapsed[1].contains("3 found"), "{collapsed:?}");
        assert!(collapsed[2].contains("5 more lines"), "{collapsed:?}");
        let expanded = tool("mcp__forge__search", input, "3 found", false).join("\n");
        assert!(expanded.contains(r#""repo": "unharness""#), "{expanded}");

        // A known call shows no input of its own.
        let bash = tool("Bash", serde_json::json!({"command": "ls"}), "a", false);
        assert_eq!(bash.len(), 2, "{bash:?}");

        let question = serde_json::json!({"questions": [{"question": "Verify?", "header": "H",
            "multiSelect": false, "options": [{"label": "Yes", "description": "d"}]}]});
        let asked = tool("AskUserQuestion", question, "User has answered", false);
        assert!(
            asked[0].contains("Verify?") && asked[0].ends_with('✓'),
            "{asked:?}"
        );
        assert!(!asked[0].contains("274"), "{asked:?}");
    }

    #[test]
    fn blocks_are_one_blank_line_apart_and_a_run_of_calls_is_closed_off() {
        use super::super::app::tests::mouse;
        use crate::core::AgentEvent as E;
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("go".into());
        app.take_actions();
        // Text that ends in blank lines of its own gets no second one.
        app.on_event(E::TextDelta("Looking.\n\n\n".into()));
        for id in ["t1", "t2"] {
            app.on_event(E::ToolCallStarted {
                id: id.into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": format!("run {id}")}),
            });
            app.on_event(E::ToolCallResult {
                id: id.into(),
                output: format!("{id}-out\n{id}-end"),
                is_error: false,
            });
        }
        app.on_event(E::HookStarted {
            id: "h".into(),
            name: "PostToolUse:Bash".into(),
        });
        app.on_event(E::ToolCallStarted {
            id: "t3".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "run t3"}),
        });
        app.on_event(E::ThinkingDelta("hmm".into()));
        app.on_event(E::TextDelta("Done.".into()));
        app.transcript.push_notice("a notice");
        app.transcript.push_system("a system line");
        screen(&mut app, 80, 50);
        let lines = app.transcript_view.lines.clone();
        let text = lines.join("\n");
        let blank = |s: &str| s.trim().is_empty();
        assert!(
            !lines.windows(2).any(|w| blank(&w[0]) && blank(&w[1])),
            "{text}"
        );
        assert!(!blank(lines.last().unwrap()), "{text}");
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();

        // Calls, the hook between them and the running one form one run.
        let (t1, t2, hook, t3) = (at("run t1"), at("run t2"), at("hook "), at("run t3"));
        assert!(blank(&lines[t1 - 1]), "{text}");
        assert_eq!(
            lines[t1..=t3].to_vec(),
            [
                lines[t1].clone(),
                "  │ t1-out".into(),
                "  └ t1-end".into(),
                lines[t2].clone(),
                "  │ t2-out".into(),
                "  └ t2-end".into(),
                lines[hook].clone(),
                lines[t3].clone(),
            ],
            "{text}"
        );
        // And everything else is one blank line apart.
        assert!(
            blank(&lines[t3 + 1]) && lines[t3 + 2].contains("💭"),
            "{text}"
        );
        for (before, after) in [("Done.", "a notice"), ("a notice", "a system line")] {
            assert_eq!(at(before) + 2, at(after), "{text}");
        }
        for header in ["● Claude", "❯ You"] {
            assert!(blank(&lines[at(header) - 1]), "{text}");
        }

        // A click on the blank line above a call does nothing; one on its
        // head expands it.
        let top = app.transcript_view.area.y as usize;
        let scroll = app.scroll as usize;
        let row = |line: usize| (top + line - scroll) as u16;
        let click = |app: &mut App, row: u16| {
            app.last_click_forget();
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, row));
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 10, row));
        };
        let collapsed = |app: &App| {
            app.transcript.blocks.iter().find_map(|b| match b {
                TBlock::Tool { id, collapsed, .. } if id == "t1" => Some(*collapsed),
                _ => None,
            })
        };
        let before = collapsed(&app);
        click(&mut app, row(t1 - 1));
        assert_eq!(collapsed(&app), before);
        click(&mut app, row(t1));
        assert_eq!(collapsed(&app), before.map(|c| !c));
    }

    #[test]
    fn an_open_fence_streams_only_in_the_last_unfinished_reply() {
        let reply = |duration| TBlock::Assistant {
            text: "```py\nprint(1)".into(),
            sender: "Claude".into(),
            duration,
        };
        let last = |b: &TBlock, followed| {
            let lines = block_lines(b, 80, followed, false, 0.0);
            crate::tui::code::line_text(lines.last().unwrap())
        };
        assert!(last(&reply(None), false).contains('…'));
        assert!(last(&reply(None), true).contains('└'));
        let finished = reply(Some(std::time::Duration::from_secs(1)));
        assert!(last(&finished, false).contains('└'));
        assert_ne!(
            block_key(&reply(None), 0, false, false, 0.0),
            block_key(&reply(None), 0, true, false, 0.0)
        );
        // Nothing else reads it, so nothing else is drawn again for it.
        let prompt = TBlock::User { text: "hi".into() };
        assert_eq!(
            block_key(&prompt, 0, false, false, 0.0),
            block_key(&prompt, 0, true, false, 0.0)
        );
    }

    #[test]
    fn a_call_shows_its_command_plainly_and_its_output_dimmed() {
        let block = TBlock::Tool {
            id: "t".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": format!("grep {} file", "pattern ".repeat(12))}),
            output: "found it".into(),
            is_error: false,
            done: true,
            collapsed: false,
            started: std::time::Instant::now(),
            duration: Some(std::time::Duration::from_millis(700)),
            agent: None,
        };
        let lines = block_lines(&block, 80, false, false, 0.0);
        let text: Vec<String> = lines.iter().map(crate::tui::code::line_text).collect();
        // The command wraps; its status is on the first line, after its words.
        assert_eq!(text.len(), 3, "{text:?}");
        assert!(text[0].ends_with("  ✓ 0.7s"), "{text:?}");
        assert!(!text[1].contains('✓'), "{text:?}");
        let span = |line: usize, s: &str| {
            lines[line]
                .spans
                .iter()
                .find(|sp| sp.content.contains(s))
                .unwrap()
                .style
        };
        for line in [0, 1] {
            let style = span(line, "pattern");
            assert_eq!(style.fg, None, "{text:?}");
            assert!(!style.add_modifier.contains(Modifier::DIM), "{text:?}");
        }
        // At the end of the first line's width, apart from the command.
        let cells = |s: &str| UnicodeWidthStr::width(s);
        assert_eq!(cells(&text[0]) - cells(" ✓ 0.7s"), 80 - 10, "{text:?}");
        assert_eq!(text[2], "  └ found it");
        for s in ["└", "found it"] {
            assert!(span(2, s).add_modifier.contains(Modifier::DIM), "{text:?}");
        }

        // Collapsed, the gutter ends on the row that counts what is hidden.
        let mut block = block;
        if let TBlock::Tool {
            output, collapsed, ..
        } = &mut block
        {
            *output = "1\n2\n3\n4\n5\n6".into();
            *collapsed = true;
        }
        let text: Vec<String> = block_lines(&block, 80, false, false, 0.0)
            .iter()
            .map(crate::tui::code::line_text)
            .collect();
        assert_eq!(text[2..6], ["  │ 1", "  │ 2", "  │ 3", "  │ 4"], "{text:?}");
        assert!(text[6].starts_with("  └ … 2 more lines"), "{text:?}");
    }

    #[test]
    fn a_long_tool_name_keeps_its_status_in_view() {
        let block = |name: &str, done: bool| TBlock::Tool {
            id: "t".into(),
            name: name.into(),
            input: serde_json::json!({"query": "word ".repeat(30)}),
            output: String::new(),
            is_error: false,
            done,
            collapsed: true,
            started: std::time::Instant::now(),
            duration: Some(std::time::Duration::from_millis(700)),
            agent: None,
        };
        let render = |name: &str, done: bool, width: usize| -> Vec<String> {
            let mut lines = block_lines(&block(name, done), width, false, false, 0.0);
            clamp_lines(&mut lines, width + 4);
            lines.iter().map(crate::tui::code::line_text).collect()
        };
        // Raw MCP names as Claude Code lists them (the longest in
        // `mcp_server.jsonl` is 83), on 70, 80 and 120 columns.
        let names = [
            "mcp__claude_ai_Google_Sheets__copy_sheet_to_another_spreadsheet",
            "mcp__claude_ai_Intuit_QuickBooks__qbo_accounting_get_sales_by_customer_summary_text",
        ];
        for name in names {
            for width in [64, 74, 114] {
                for (done, status) in [(false, "⠿ running"), (true, "✓ 0.7s")] {
                    let text = render(name, done, width);
                    let all = format!("{width}: {text:?}");
                    assert!(text.len() > 1, "{all}");
                    assert!(text.iter().all(|l| l.width() <= width), "{all}");
                    assert!(text[0].ends_with(&format!(" {status}")), "{all}");
                    if 5 + name.len() + 2 + SUMMARY_MIN + STATUS_ROOM <= width {
                        // Room enough beside it: the summary stays there.
                        assert!(text[0].contains(&format!("{name}  word")), "{all}");
                        continue;
                    }
                    // The name, shortened if it must be, and the status on a
                    // line of their own; the summary below, under the name.
                    let fits = 5 + name.len() + STATUS_ROOM <= width;
                    assert_eq!(text[0].contains(name), fits, "{all}");
                    assert!(text[0].starts_with("  ⚡ mcp__claude_ai_"), "{all}");
                    assert!(text[1].starts_with("     word word"), "{all}");
                    assert!(!text[1..].concat().contains('…'), "{all}");
                }
            }
        }

        // The summary stays beside a name that leaves it room enough.
        let at_most = 74 - SUMMARY_MIN - STATUS_ROOM - 2 - 5;
        let text = render(&"n".repeat(at_most), true, 74);
        assert!(text[0].contains("  word") && text[1].starts_with(&" ".repeat(at_most + 6)));
        let text = render(&"n".repeat(at_most + 1), true, 74);
        assert!(
            text[0].ends_with(" ✓ 0.7s") && !text[0].contains("word"),
            "{text:?}"
        );
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
    fn a_question_row_turns_without_counting_the_users_time() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("ask me".into());
        app.take_actions();
        app.on_event(crate::core::AgentEvent::ToolCallStarted {
            id: "q".into(),
            name: "AskUserQuestion".into(),
            input: serde_json::json!({"questions": []}),
        });
        if let Some(TBlock::Tool { started, .. }) = app.transcript.blocks.last_mut() {
            *started = std::time::Instant::now() - std::time::Duration::from_secs(90);
        }
        let (rows, _) = screen(&mut app, 80, 30);
        let row = rows.iter().find(|r| r.contains("AskUserQuestion")).unwrap();
        assert!(row.trim_end_matches([' ', '│']).ends_with(" ⠋"), "{row}");
    }

    #[test]
    fn a_row_stops_when_the_process_that_ran_it_exits() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("go".into());
        app.take_actions();
        app.on_event(crate::core::AgentEvent::ToolCallStarted {
            id: "t".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "sleep 20"}),
        });
        app.on_event(crate::core::AgentEvent::ProcessExited { code: Some(1) });
        let (rows, _) = screen(&mut app, 80, 30);
        let row = rows.iter().find(|r| r.contains("sleep 20")).unwrap();
        assert!(row.contains(" ✗") && !row.contains('⠋'), "{row}");
    }

    #[test]
    fn a_running_row_cut_to_the_width_still_turns() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.transcript.shell_started("sleep 5");
        // Too narrow for the whole status: the line is cut, and the cut
        // line is what is drawn again.
        let (rows, _) = screen(&mut app, 18, 20);
        let row = rows.iter().find(|r| r.contains("You ran")).unwrap().clone();
        assert!(row.contains("⠋"), "{row}");
        app.tick_spinner();
        let (rows, _) = screen(&mut app, 18, 20);
        let next = rows.iter().find(|r| r.contains("You ran")).unwrap();
        assert!(next.contains("⠙") && !next.contains("⠋"), "{next}");
        assert!(UnicodeWidthStr::width(next.as_str()) <= 18, "{next}");
    }

    #[test]
    fn a_running_row_animates_without_a_new_layout() {
        use std::time::Duration;
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("go".into());
        app.take_actions();
        app.on_event(crate::core::AgentEvent::ToolCallStarted {
            id: "spawn".into(),
            name: "Agent".into(),
            input: serde_json::json!({"description": "look around"}),
        });
        app.on_event(crate::core::AgentEvent::SubagentStarted {
            id: "spawn".into(),
            description: "look around".into(),
            kind: Some("Explore".into()),
        });
        app.on_event(crate::core::AgentEvent::ToolCallResult {
            id: "spawn".into(),
            output: "launched".into(),
            is_error: false,
        });
        app.on_event(crate::core::AgentEvent::ToolCallStarted {
            id: "t".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "sleep 20"}),
        });
        app.transcript.shell_started("sleep 5");
        app.transcript.push_notice("below them");
        let (rows, _) = screen(&mut app, 80, 30);
        let text = rows.join("\n");
        assert!(text.contains("(Explore) ⠋ 0s"), "{text}");
        assert!(text.contains("sleep 20"), "{text}");
        assert!(text.contains("You ran ⠋ running 0s"), "{text}");
        let kept = app.transcript_view.blocks.clone();

        // Time passes and the spinner turns: each row says so, and none is
        // laid out again.
        let ago = std::time::Instant::now() - Duration::from_secs(12);
        for b in &mut app.transcript.blocks {
            match b {
                TBlock::Tool {
                    agent: Some(run), ..
                } => run.started = ago,
                TBlock::Tool { started, .. } | TBlock::Shell { started, .. } => *started = ago,
                _ => {}
            }
        }
        app.tick_spinner();
        let (rows, _) = screen(&mut app, 80, 30);
        let text = rows.join("\n");
        assert_eq!(
            app.transcript_view
                .blocks
                .iter()
                .map(|b| b.key)
                .collect::<Vec<_>>(),
            kept.iter().map(|b| b.key).collect::<Vec<_>>()
        );
        assert!(text.contains("(Explore) ⠙ 12s"), "{text}");
        assert!(
            rows.iter()
                .any(|r| r.contains("sleep 20") && r.contains(" ⠙ 12s")),
            "{text}"
        );
        assert!(text.contains("You ran ⠙ running 12s"), "{text}");
        assert!(!text.contains("⠋"), "{text}");
        // What a selection copies is what is on the screen.
        assert!(
            app.transcript_view
                .lines
                .iter()
                .any(|l| l.contains("You ran ⠙ running 12s")),
            "{:?}",
            app.transcript_view.lines
        );

        // Once ended, a row stops.
        app.on_event(crate::core::AgentEvent::ToolCallResult {
            id: "t".into(),
            output: String::new(),
            is_error: false,
        });
        app.transcript.shell_ended(ShellStatus::Exited { code: 0 });
        let (rows, _) = screen(&mut app, 80, 30);
        let text = rows.join("\n");
        assert!(
            rows.iter()
                .any(|r| r.contains("sleep 20") && r.contains(" ✓ 12.")),
            "{text}"
        );
        assert!(text.contains("You ran ✓ exit 0 12."), "{text}");
        assert!(text.contains("(Explore) ⠙ 12s"), "{text}");
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
                    .any(|r| r.contains(&format!("Agent  {d} (Explore) ⠋ 0s"))),
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
    fn suggestion_list_follows_the_selection() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.insert_char('/');
        assert!(app.suggestions.len() > 8);
        let last = app.suggestions.last().unwrap().0.clone();
        let (rows, _) = screen(&mut app, 100, 30);
        assert!(rows.iter().any(|r| r.contains("❯ /")));
        assert!(!rows.iter().any(|r| r.contains(&format!("{last} "))));

        // Up from the first item wraps to the last one, below the window.
        app.suggestion_up();
        let (rows, _) = screen(&mut app, 100, 30);
        assert!(
            rows.iter().any(|r| r.contains(&format!("❯ {last} "))),
            "selected item not on screen"
        );
    }

    #[test]
    fn file_list_shows_paths_and_keeps_the_end_of_long_ones() {
        let mut app = test_app(HarnessId::CLAUDE);
        let long = format!("{}/deep/file.rs", "directory".repeat(12));
        app.set_file_index(vec!["src/main.rs".to_string(), long]);
        app.insert_char('@');
        let (rows, _) = screen(&mut app, 100, 30);
        assert!(rows.iter().any(|r| r.contains("Files [Tab to complete]")));
        let first = rows.iter().find(|r| r.contains("❯ src/main.rs")).unwrap();
        assert!(!first.contains('─'), "no description: {first}");
        let second = rows.iter().find(|r| r.contains("/deep/file.rs")).unwrap();
        assert!(second.contains("│  …"), "cut at the front: {second}");
        assert!(second.contains("/deep/file.rs │"), "{second}");

        assert_eq!(keep_end("src/main.rs", 11), "src/main.rs");
        assert_eq!(keep_end("src/main.rs", 8), "…main.rs");
        assert_eq!(keep_start("src/main.rs", 11), "src/main.rs");
        assert_eq!(keep_start("src/main.rs", 8), "src/mai…");
    }

    const PROVIDER: &str = "Open the provider picker (multi-provider harnesses)";

    #[test]
    fn suggestions_fit_wide_terminals() {
        let mut app = test_app(HarnessId::CLAUDE);
        for c in "/prov".chars() {
            app.insert_char(c);
        }
        let (rows, _) = screen(&mut app, 124, 24);
        let row = rows.iter().find(|r| r.contains("❯ /provider")).unwrap();
        assert!(row.contains(PROVIDER), "{row}");
        assert!(!rows.iter().any(|r| r.contains('…')), "nothing cut");
    }

    #[test]
    fn cut_description_is_shown_below() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.insert_char('/');
        let at = |app: &App, name: &str| app.suggestions.iter().position(|(c, _)| c == name);
        let title = |rows: &[String]| {
            let y = rows
                .iter()
                .position(|r| r.contains("Suggestions [Tab"))
                .unwrap();
            (y, rows[y].chars().count())
        };
        app.selected_suggestion = at(&app, "/provider").unwrap();
        let (rows, _) = screen(&mut app, 60, 30);
        if std::env::var("UNHARNESS_DUMP_UI").is_ok() {
            eprintln!("{}", rows.join("\n"));
        }
        let row = rows.iter().position(|r| r.contains("❯ /provider")).unwrap();
        assert!(rows[row].contains("…"), "{}", rows[row]);
        assert!(
            rows[row..].iter().any(|r| r.contains("harnesses)")),
            "full description below"
        );
        let box_at = title(&rows);

        // A description that fits is shown there too, and the box stays
        // where it was, also once the list scrolls.
        app.selected_suggestion = at(&app, "/model").unwrap();
        let (rows, _) = screen(&mut app, 60, 30);
        let model = rows.iter().filter(|r| r.contains("Open the model picker"));
        assert_eq!(model.count(), 2, "row and detail");
        assert_eq!(title(&rows), box_at);
        app.selected_suggestion = app.suggestions.len() - 1;
        let (rows, _) = screen(&mut app, 60, 30);
        assert_eq!(title(&rows), box_at);
    }

    #[test]
    fn long_harness_name_keeps_its_end() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.insert_char('/');
        let long = format!("/plugin:{}docx", "very-long-skill-name-".repeat(4));
        app.suggestions = vec![
            ("/model".into(), "Open the model picker".into()),
            (long, "claude: a skill".into()),
            ("/effort".into(), "Open the reasoning effort picker".into()),
        ];
        app.selected_suggestion = 0;
        let (rows, _) = screen(&mut app, 100, 30);
        let column = |name: &str| {
            let row = rows.iter().find(|r| r.contains(name)).unwrap();
            row.split(" ─ ").next().unwrap().chars().count()
        };
        assert_eq!(column("/model"), column("/effort"));
        assert_eq!(column("/model"), column("docx"));
        let row = rows.iter().find(|r| r.contains("docx")).unwrap();
        assert!(row.contains("│  …"), "cut at the front: {row}");
        assert!(row.contains("-name-docx ─ claude: a skill"), "{row}");

        // Names that share a prefix stay apart in a narrow pane.
        app.suggestions = ["docx", "pptx", "xlsx"]
            .map(|s| (format!("/anthropic-skills:{s}"), "claude: a skill".into()))
            .to_vec();
        let (rows, _) = screen(&mut app, 44, 30);
        for s in ["skills:docx", "skills:pptx", "skills:xlsx"] {
            assert!(rows.iter().any(|r| r.contains(s)), "{s} missing");
        }
    }

    #[test]
    fn narrow_pane_keeps_rows_inside_the_box() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.insert_char('/');
        let (rows, _) = screen(&mut app, 22, 30);
        if std::env::var("UNHARNESS_DUMP_UI").is_ok() {
            eprintln!("{}", rows.join("\n"));
        }
        let row = rows.iter().find(|r| r.contains("❯ /harness")).unwrap();
        assert!(row.contains("… │"), "cut shown before the border: {row}");
        assert_eq!(keep_start("abc", 0), "");
        assert_eq!(keep_end("abc", 0), "");
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
                    action: crate::core::ToolAction::Opaque,
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

    #[test]
    fn the_permission_modal_says_what_allow_always_would_cover() {
        use crate::core::{AgentEvent, PermissionRequest, ToolAction};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = test_app(HarnessId::CLAUDE);
        let request = |id: &str, tool: &str, action: ToolAction| {
            AgentEvent::PermissionRequest(PermissionRequest {
                id: id.into(),
                kind: PermissionKind::ToolUse {
                    tool: tool.into(),
                    input: serde_json::json!({"command": "cargo test --all"}),
                    action,
                    description: None,
                },
                tool_call_id: None,
            })
        };
        let press = |app: &mut App, code: KeyCode| {
            app.handle_modal_key(KeyEvent::new(code, KeyModifiers::NONE));
            screen(app, 100, 40).0.join("\n")
        };
        let shell = ToolAction::Shell {
            command: "cargo test --all".into(),
            cwd: None,
        };
        app.on_event(request("1", "Bash", shell));
        let text = screen(&mut app, 100, 40).0.join("\n");
        assert!(text.contains("a allow always"), "{text}");

        let text = press(&mut app, KeyCode::Char('a'));
        assert!(
            text.contains("Always allow, in this workspace, on any harness:"),
            "{text}"
        );
        assert!(text.contains("shell commands starting with `cargo test`"));
        assert!(text.contains("Pattern: cargo test"));
        assert!(text.contains("Enter save and allow · Tab every workspace"));
        // What it says follows what is typed, and where it goes.
        press(&mut app, KeyCode::Backspace);
        let text = press(&mut app, KeyCode::Tab);
        assert!(text.contains("Always allow, in every workspace"), "{text}");
        assert!(text.contains("shell commands starting with `cargo tes`"));
        let text = press(&mut app, KeyCode::Enter);
        assert!(text.contains("that would not cover this request"), "{text}");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);

        app.on_event(request("2", "WebFetch", ToolAction::Other));
        let text = press(&mut app, KeyCode::Char('a'));
        assert!(text.contains("every use of `WebFetch`"), "{text}");
        assert!(text.contains("by this harness's name for the tool"));
        assert!(!text.contains("Pattern:"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);

        app.on_event(request("3", "permissions", ToolAction::Opaque));
        let text = press(&mut app, KeyCode::Char('a'));
        assert!(text.contains("No rule can cover this request"), "{text}");
    }
}
