use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

pub fn render_markdown_to_lines(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();

    for raw_line in text.lines() {
        let trimmed = raw_line.trim_end();

        // 1. Code block delimiter (```)
        if trimmed.starts_with("```") {
            if in_code_block {
                // Closing code block
                in_code_block = false;
                lines.push(Line::from(Span::styled(
                    "  └────────────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                // Opening code block
                in_code_block = true;
                code_lang = trimmed
                    .strip_prefix("```")
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                let lang_label = if code_lang.is_empty() {
                    "code".to_string()
                } else {
                    code_lang.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled("  ┌─ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        lang_label,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " ──────────────────────────────────",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            continue;
        }

        // 2. Inside code block
        if in_code_block {
            let is_diff = code_lang == "diff" || is_diff_line(trimmed);
            if is_diff {
                lines.push(style_diff_line(trimmed, max_width));
            } else {
                lines.push(style_code_line(trimmed, &code_lang, max_width));
            }
            continue;
        }

        // 3. Standalone diff output (outside code blocks)
        if is_diff_line(trimmed) {
            lines.push(style_diff_line(trimmed, max_width));
            continue;
        }

        // 4. Headers (#, ##, ###)
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim();
            lines.push(Line::from(Span::styled(
                format!("  # {}", title),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            let title = rest.trim();
            lines.push(Line::from(Span::styled(
                format!("  ## {}", title),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("### ") {
            let title = rest.trim();
            lines.push(Line::from(Span::styled(
                format!("  ### {}", title),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }

        // 5. Blockquotes (> ...)
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let quote = rest.trim();
            for wrapped in wrap_text_preserving(quote, max_width.saturating_sub(4)) {
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

        // 6. Bullet lists (- or * )
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let item = trimmed[2..].trim();
            let wrapped_items = wrap_text_preserving(item, max_width.saturating_sub(4));
            for (idx, wrapped) in wrapped_items.into_iter().enumerate() {
                if idx == 0 {
                    let mut spans = vec![Span::styled(
                        "  • ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )];
                    spans.extend(parse_inline_formatting(&wrapped));
                    lines.push(Line::from(spans));
                } else {
                    let mut spans = vec![Span::raw("    ")];
                    spans.extend(parse_inline_formatting(&wrapped));
                    lines.push(Line::from(spans));
                }
            }
            continue;
        }

        // 7. Empty line
        if trimmed.is_empty() {
            lines.push(Line::default());
            continue;
        }

        // 8. Normal prose line with inline formatting and word wrap
        for wrapped in wrap_text_preserving(trimmed, max_width.saturating_sub(2)) {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(parse_inline_formatting(&wrapped));
            lines.push(Line::from(spans));
        }
    }

    // If still in code block at end of streaming turn, draw open border indicator
    if in_code_block {
        lines.push(Line::from(Span::styled(
            "  ┆ ... (code block in progress)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    lines
}

fn is_diff_line(line: &str) -> bool {
    line.starts_with("diff --git")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@ ")
        || (line.starts_with('+') && !line.starts_with("++"))
        || (line.starts_with('-') && !line.starts_with("--"))
}

fn style_diff_line(line: &str, max_width: usize) -> Line<'static> {
    let truncated = truncate_or_pad(line, max_width.saturating_sub(4));

    if line.starts_with('+') {
        Line::from(vec![
            Span::styled(
                "  │+ ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncated[1..].to_string(),
                Style::default().fg(Color::Green),
            ),
        ])
    } else if line.starts_with('-') {
        Line::from(vec![
            Span::styled(
                "  │- ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(truncated[1..].to_string(), Style::default().fg(Color::Red)),
        ])
    } else if line.starts_with("@@ ") {
        Line::from(vec![
            Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncated,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])
    } else if line.starts_with("diff --git") || line.starts_with("--- ") || line.starts_with("+++ ")
    {
        Line::from(vec![
            Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncated,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("  │  ", Style::default().fg(Color::DarkGray)),
            Span::styled(truncated, Style::default().fg(Color::Gray)),
        ])
    }
}

fn style_code_line(line: &str, _lang: &str, max_width: usize) -> Line<'static> {
    let truncated = truncate_or_pad(line, max_width.saturating_sub(4));
    let mut spans = vec![Span::styled("  │ ", Style::default().fg(Color::DarkGray))];

    // Simple syntax highlight for keywords, strings, comments
    let trimmed = truncated.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('#') {
        spans.push(Span::styled(
            truncated,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
        return Line::from(spans);
    }

    let words = split_tokens(&truncated);
    for word in words {
        if is_code_keyword(word) {
            spans.push(Span::styled(
                word.to_string(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if word.starts_with('"') || word.starts_with('\'') {
            spans.push(Span::styled(
                word.to_string(),
                Style::default().fg(Color::Green),
            ));
        } else if word.chars().all(|c| c.is_ascii_digit()) {
            spans.push(Span::styled(
                word.to_string(),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            spans.push(Span::styled(
                word.to_string(),
                Style::default().fg(Color::White),
            ));
        }
    }

    Line::from(spans)
}

fn is_code_keyword(word: &str) -> bool {
    matches!(
        word,
        "fn" | "let"
            | "pub"
            | "struct"
            | "impl"
            | "enum"
            | "mut"
            | "match"
            | "if"
            | "else"
            | "return"
            | "const"
            | "type"
            | "use"
            | "mod"
            | "crate"
            | "async"
            | "await"
            | "for"
            | "while"
            | "loop"
            | "break"
            | "def"
            | "class"
            | "import"
            | "from"
            | "self"
            | "None"
            | "True"
            | "False"
            | "function"
            | "export"
            | "default"
            | "interface"
    )
}

fn split_tokens(s: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut in_word = false;

    for (idx, ch) in s.char_indices() {
        let is_sep = ch.is_whitespace() || "(),;{}:.[]<>+-*/%=&|^!".contains(ch);
        if is_sep {
            if in_word {
                tokens.push(&s[start..idx]);
                in_word = false;
            }
            tokens.push(&s[idx..idx + ch.len_utf8()]);
        } else if !in_word {
            start = idx;
            in_word = true;
        }
    }
    if in_word {
        tokens.push(&s[start..]);
    }
    tokens
}

fn parse_inline_formatting(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        // Inline code `...`
        if let Some(start_idx) = rest.find('`') {
            if start_idx > 0 {
                spans.push(Span::raw(rest[..start_idx].to_string()));
            }
            let code_rest = &rest[start_idx + 1..];
            if let Some(end_idx) = code_rest.find('`') {
                let code_content = &code_rest[..end_idx];
                spans.push(Span::styled(
                    format!(" {} ", code_content),
                    Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(35, 38, 48)),
                ));
                rest = &code_rest[end_idx + 1..];
            } else {
                spans.push(Span::raw(rest[start_idx..].to_string()));
                break;
            }
        }
        // Bold **...**
        else if let Some(start_idx) = rest.find("**") {
            if start_idx > 0 {
                spans.push(Span::raw(rest[..start_idx].to_string()));
            }
            let bold_rest = &rest[start_idx + 2..];
            if let Some(end_idx) = bold_rest.find("**") {
                let bold_content = &bold_rest[..end_idx];
                spans.push(Span::styled(
                    bold_content.to_string(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                rest = &bold_rest[end_idx + 2..];
            } else {
                spans.push(Span::raw(rest[start_idx..].to_string()));
                break;
            }
        } else {
            spans.push(Span::raw(rest.to_string()));
            break;
        }
    }

    spans
}

fn wrap_text_preserving(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;

    for word in text.split(' ') {
        let word_len = UnicodeWidthStr::width(word);
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
    lines
}

fn truncate_or_pad(s: &str, max_len: usize) -> String {
    let width = UnicodeWidthStr::width(s);
    if width > max_len && max_len > 3 {
        // truncate with ellipsis
        let mut cur = String::new();
        let mut len = 0;
        for c in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
            if len + cw + 3 <= max_len {
                cur.push(c);
                len += cw;
            } else {
                break;
            }
        }
        cur.push_str("...");
        cur
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_headers() {
        let text = "# Main Title\n## Sub Title\nRegular text";
        let lines = render_markdown_to_lines(text, 80);
        assert!(lines.len() >= 3);
    }

    #[test]
    fn test_render_diff() {
        let diff_text =
            "```diff\n--- a/file.rs\n+++ b/file.rs\n@@ -1,2 +1,3 @@\n-old line\n+new line\n```";
        let lines = render_markdown_to_lines(diff_text, 80);
        assert!(lines.len() >= 6);
    }

    #[test]
    fn test_inline_code_parsing() {
        let text = "Use `cargo test` to verify.";
        let spans = parse_inline_formatting(text);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "Use ");
        assert_eq!(spans[1].content, " cargo test ");
    }
}
