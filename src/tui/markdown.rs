//! Markdown rendering for assistant text: headers, lists, quotes, inline
//! code/bold/italic, fenced code blocks (syntax highlighted, wrapped) and
//! bare diff hunks.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::code::{code_lines, diff_lines, is_diff_line};

pub fn render_markdown_to_lines(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();

    let flush_code = |lines: &mut Vec<Line<'static>>, lang: &str, buf: &str| {
        lines.extend(code_lines(buf, lang, max_width.saturating_sub(2)));
    };

    for raw_line in text.lines() {
        let trimmed = raw_line.trim_end();

        if let Some(rest) = trimmed.trim_start().strip_prefix("```") {
            match code_lang.take() {
                Some(lang) => {
                    flush_code(&mut lines, &lang, &code_buf);
                    code_buf.clear();
                    lines.push(Line::from(Span::styled(
                        format!("  └{}", "─".repeat(max_width.saturating_sub(4).min(40))),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                None => {
                    let lang = rest.trim().to_lowercase();
                    let label = if lang.is_empty() {
                        "code"
                    } else {
                        lang.as_str()
                    };
                    lines.push(Line::from(vec![
                        Span::styled("  ┌─ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            label.to_string(),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(
                                " {}",
                                "─".repeat(max_width.saturating_sub(8 + label.len()).min(34))
                            ),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                    code_lang = Some(lang);
                }
            }
            continue;
        }

        if code_lang.is_some() {
            code_buf.push_str(raw_line);
            code_buf.push('\n');
            continue;
        }

        // Bare diff output outside a fence.
        if is_diff_line(trimmed) && !trimmed.starts_with("- ") && !trimmed.starts_with("-- ") {
            lines.extend(diff_lines(trimmed, max_width.saturating_sub(2)));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("### ") {
            lines.push(Line::from(Span::styled(
                format!("  ### {}", rest.trim()),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            lines.push(Line::from(Span::styled(
                format!("  ## {}", rest.trim()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            lines.push(Line::from(Span::styled(
                format!("  # {}", rest.trim()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ") {
            for wrapped in wrap_text_preserving(rest.trim(), max_width.saturating_sub(4)) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        wrapped,
                        Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
            continue;
        }

        // Bullets: "- ", "* ", and indented variants.
        let indent = trimmed.len() - trimmed.trim_start().len();
        let body = trimmed.trim_start();
        if let Some(item) = body.strip_prefix("- ").or_else(|| body.strip_prefix("* ")) {
            let pad = " ".repeat(indent.min(8));
            for (idx, wrapped) in
                wrap_text_preserving(item.trim(), max_width.saturating_sub(4 + pad.len()))
                    .into_iter()
                    .enumerate()
            {
                let mut spans = vec![if idx == 0 {
                    Span::styled(
                        format!("  {pad}• "),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw(format!("  {pad}  "))
                }];
                spans.extend(parse_inline_formatting(&wrapped));
                lines.push(Line::from(spans));
            }
            continue;
        }

        // Numbered lists: "1. "
        if let Some(pos) = body.find(". ")
            && pos > 0
            && pos <= 3
            && body[..pos].chars().all(|c| c.is_ascii_digit())
        {
            let num = &body[..pos];
            let item = &body[pos + 2..];
            for (idx, wrapped) in
                wrap_text_preserving(item.trim(), max_width.saturating_sub(5 + num.len()))
                    .into_iter()
                    .enumerate()
            {
                let mut spans = vec![if idx == 0 {
                    Span::styled(
                        format!("  {num}. "),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw(" ".repeat(4 + num.len()))
                }];
                spans.extend(parse_inline_formatting(&wrapped));
                lines.push(Line::from(spans));
            }
            continue;
        }

        if trimmed.is_empty() {
            lines.push(Line::default());
            continue;
        }

        for wrapped in wrap_text_preserving(trimmed, max_width.saturating_sub(2)) {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(parse_inline_formatting(&wrapped));
            lines.push(Line::from(spans));
        }
    }

    // Still streaming inside a fence: render what we have so far.
    if let Some(lang) = code_lang {
        flush_code(&mut lines, &lang, &code_buf);
        lines.push(Line::from(Span::styled(
            "  ┆ …",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    lines
}

/// Inline `code`, **bold**, *italic* / _italic_.
pub fn parse_inline_formatting(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let code = rest.find('`');
        let bold = rest.find("**");
        let next = [code, bold].into_iter().flatten().min();
        let Some(idx) = next else {
            spans.extend(italic_spans(rest));
            break;
        };
        if idx > 0 {
            spans.extend(italic_spans(&rest[..idx]));
        }
        let after = &rest[idx..];
        if let Some(inner) = after.strip_prefix("**") {
            if let Some(end) = inner.find("**") {
                spans.push(Span::styled(
                    inner[..end].to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                rest = &inner[end + 2..];
            } else {
                spans.push(Span::raw(after.to_string()));
                break;
            }
        } else if let Some(end) = after[1..].find('`') {
            spans.push(Span::styled(
                format!(" {} ", &after[1..1 + end]),
                Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::Rgb(35, 38, 48)),
            ));
            rest = &after[1 + end + 1..];
        } else {
            spans.push(Span::raw(after.to_string()));
            break;
        }
    }
    spans
}

fn italic_spans(text: &str) -> Vec<Span<'static>> {
    // *word* only when delimiters hug the text (avoid "a * b").
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('*') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('*') else { break };
        let inner = &after[..end];
        if inner.is_empty() || inner.starts_with(' ') || inner.ends_with(' ') {
            break;
        }
        spans.push(Span::raw(rest[..start].to_string()));
        spans.push(Span::styled(
            inner.to_string(),
            Style::default().add_modifier(Modifier::ITALIC),
        ));
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    spans
}

fn wrap_text_preserving(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for word in text.split(' ') {
        let w = UnicodeWidthStr::width(word);
        if current_width == 0 {
            current.push_str(word);
            current_width = w;
        } else if current_width + 1 + w <= max_width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + w;
        } else {
            lines.push(std::mem::replace(&mut current, word.to_string()));
            current_width = w;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::code::line_text;

    #[test]
    fn test_render_headers() {
        let lines = render_markdown_to_lines("# Title\n## Sub\n### Deep", 80);
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0]).contains("# Title"));
    }

    #[test]
    fn test_render_diff() {
        let lines = render_markdown_to_lines("```diff\n-old\n+new\n```", 80);
        // fence header, two diff lines, fence footer
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Green));
    }

    #[test]
    fn test_fenced_code_is_highlighted_and_wrapped_not_truncated() {
        let long = format!("let s = \"{}\";", "a".repeat(120));
        let lines = render_markdown_to_lines(&format!("```rust\n{long}\n```"), 60);
        let body: String = lines[1..lines.len() - 1]
            .iter()
            .map(|l| line_text(l).replace("  │ ↪ ", "").replace("  │ ", ""))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            body.contains(&"a".repeat(120)),
            "code must not be truncated"
        );
        assert!(
            lines[1].spans.len() > 2,
            "expected syntax highlighting spans"
        );
    }

    #[test]
    fn test_inline_code_parsing() {
        let spans = parse_inline_formatting("use `cargo test` and **now** or *later*");
        let texts: Vec<String> = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(texts.contains(&" cargo test ".to_string()));
        assert!(
            spans
                .iter()
                .any(|s| s.content == "now" && s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content == "later" && s.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn test_lists_and_streaming_fence() {
        let lines = render_markdown_to_lines("- one\n  - nested\n1. first\n```py\nprint(1)", 80);
        assert!(line_text(&lines[0]).contains("• one"));
        assert!(line_text(&lines[1]).contains("• nested"));
        assert!(line_text(&lines[2]).contains("1. first"));
        assert!(line_text(lines.last().unwrap()).contains("…"));
    }
}
