//! Code rendering: syntax highlighting (syntect), diff colouring, and
//! wrapping that never truncates.

use std::sync::LazyLock;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use unicode_width::UnicodeWidthChar;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let mut themes = ThemeSet::load_defaults();
    themes
        .themes
        .remove("base16-ocean.dark")
        .or_else(|| themes.themes.into_values().next())
        .expect("syntect ships default themes")
});

const GUTTER: &str = "  │ ";
const CONT: &str = "  │ ↪ ";

/// Pick a syntax from a language token (`rust`, `py`) or a file path.
pub fn syntax_for(hint: &str) -> Option<&'static SyntaxReference> {
    let hint = hint.trim();
    if hint.is_empty() {
        return None;
    }
    let by_ext = std::path::Path::new(hint)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| SYNTAXES.find_syntax_by_extension(e));
    by_ext
        .or_else(|| SYNTAXES.find_syntax_by_token(hint))
        .or_else(|| SYNTAXES.find_syntax_by_extension(hint))
        .or_else(|| match hint.to_lowercase().as_str() {
            "sh" | "shell" | "zsh" | "console" => SYNTAXES.find_syntax_by_token("bash"),
            "ts" | "tsx" => SYNTAXES.find_syntax_by_token("typescript"),
            "yml" => SYNTAXES.find_syntax_by_token("yaml"),
            _ => None,
        })
}

fn to_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// One source line as styled spans (no gutter), or plain if no syntax matched.
fn highlight_spans(hl: &mut Option<HighlightLines<'static>>, line: &str) -> Vec<(Style, String)> {
    if let Some(hl) = hl
        && let Ok(ranges) = hl.highlight_line(&format!("{line}\n"), &SYNTAXES)
    {
        return ranges
            .into_iter()
            .map(|(st, text)| {
                let mut style = Style::default().fg(to_color(st.foreground));
                if st.font_style.contains(FontStyle::BOLD) {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if st.font_style.contains(FontStyle::ITALIC) {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                (style, text.trim_end_matches('\n').to_string())
            })
            .collect();
    }
    vec![(Style::default().fg(Color::White), line.to_string())]
}

/// Wrap styled segments to `width` display cells, continuing with `cont`.
fn wrap_spans(
    segments: Vec<(Style, String)>,
    width: usize,
    first_prefix: Vec<Span<'static>>,
    cont_prefix: Vec<Span<'static>>,
    prefix_width: usize,
) -> Vec<Line<'static>> {
    let avail = width.saturating_sub(prefix_width).max(8);
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = first_prefix.clone();
    let mut used = 0usize;
    let mut buf = String::new();
    let mut buf_style = Style::default();

    let flush = |current: &mut Vec<Span<'static>>, buf: &mut String, style: Style| {
        if !buf.is_empty() {
            current.push(Span::styled(std::mem::take(buf), style));
        }
    };

    for (style, text) in segments {
        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if used + w > avail {
                flush(&mut current, &mut buf, buf_style);
                lines.push(Line::from(std::mem::replace(
                    &mut current,
                    cont_prefix.clone(),
                )));
                used = 0;
            }
            if buf_style != style && !buf.is_empty() {
                flush(&mut current, &mut buf, buf_style);
            }
            buf_style = style;
            buf.push(ch);
            used += w;
        }
    }
    flush(&mut current, &mut buf, buf_style);
    lines.push(Line::from(current));
    lines
}

fn gutter() -> Vec<Span<'static>> {
    vec![Span::styled(GUTTER, Style::default().fg(Color::DarkGray))]
}

fn cont() -> Vec<Span<'static>> {
    vec![Span::styled(CONT, Style::default().fg(Color::DarkGray))]
}

/// Highlighted, wrapped source lines with a gutter. `hint` is a language
/// token or file path; `diff` content is routed to [`diff_lines`].
pub fn code_lines(text: &str, hint: &str, width: usize) -> Vec<Line<'static>> {
    if hint.eq_ignore_ascii_case("diff") || hint.eq_ignore_ascii_case("patch") {
        return diff_lines(text, width);
    }
    let mut hl = syntax_for(hint).map(|s| HighlightLines::new(s, &THEME));
    let mut out = Vec::new();
    for line in text.lines() {
        let segs = highlight_spans(&mut hl, line);
        out.extend(wrap_spans(segs, width, gutter(), cont(), CONT.len()));
    }
    out
}

/// Plain (unhighlighted) wrapped lines with a gutter, e.g. command output.
pub fn plain_lines(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in text.lines() {
        out.extend(wrap_spans(
            vec![(style, line.to_string())],
            width,
            gutter(),
            cont(),
            CONT.len(),
        ));
    }
    out
}

pub fn is_diff_line(line: &str) -> bool {
    line.starts_with("diff --git")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@")
        || (line.starts_with('+') && !line.starts_with("+++"))
        || (line.starts_with('-') && !line.starts_with("---"))
}

/// Heuristic: does this text look like a unified diff?
pub fn looks_like_diff(text: &str) -> bool {
    let mut plus = 0;
    let mut minus = 0;
    let mut hunk = false;
    for l in text.lines().take(200) {
        if l.starts_with("@@") || l.starts_with("diff --git") {
            hunk = true;
        }
        if l.starts_with('+') && !l.starts_with("+++") {
            plus += 1;
        }
        if l.starts_with('-') && !l.starts_with("---") {
            minus += 1;
        }
    }
    hunk || (plus > 0 && minus > 0)
}

/// Red/green diff rendering, wrapped.
pub fn diff_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let (marker, style) = if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            (
                "  ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else if line.starts_with("@@") {
            (
                "  ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::ITALIC),
            )
        } else if let Some(_rest) = line.strip_prefix('+') {
            ("+ ", Style::default().fg(Color::Green))
        } else if let Some(_rest) = line.strip_prefix('-') {
            ("- ", Style::default().fg(Color::Red))
        } else {
            ("  ", Style::default().fg(Color::Gray))
        };
        let body = if marker.trim().is_empty() {
            line.to_string()
        } else {
            line[1..].to_string()
        };
        let first = vec![
            Span::styled(GUTTER, Style::default().fg(Color::DarkGray)),
            Span::styled(marker, style.add_modifier(Modifier::BOLD)),
        ];
        out.extend(wrap_spans(
            vec![(style, body)],
            width,
            first,
            cont(),
            CONT.len() + 2,
        ));
    }
    out
}

/// Render an old→new replacement as a diff with syntax colouring under the
/// red/green tint. `hint` is the file path for syntax selection.
pub fn replacement_lines(old: &str, new: &str, hint: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut hl_old = syntax_for(hint).map(|s| HighlightLines::new(s, &THEME));
    let mut hl_new = syntax_for(hint).map(|s| HighlightLines::new(s, &THEME));
    for line in old.lines() {
        let segs = highlight_spans(&mut hl_old, line)
            .into_iter()
            .map(|(s, t)| (s.bg(Color::Rgb(60, 20, 20)), t))
            .collect();
        let first = vec![
            Span::styled(GUTTER, Style::default().fg(Color::DarkGray)),
            Span::styled(
                "- ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ];
        out.extend(wrap_spans(segs, width, first, cont(), CONT.len() + 2));
    }
    for line in new.lines() {
        let segs = highlight_spans(&mut hl_new, line)
            .into_iter()
            .map(|(s, t)| (s.bg(Color::Rgb(15, 50, 20)), t))
            .collect();
        let first = vec![
            Span::styled(GUTTER, Style::default().fg(Color::DarkGray)),
            Span::styled(
                "+ ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        out.extend(wrap_spans(segs, width, first, cont(), CONT.len() + 2));
    }
    out
}

/// Visible text of a rendered line (for tests and summaries).
pub fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_and_wraps_long_lines() {
        let src = "fn main() { let x = \"hello\"; }";
        let lines = code_lines(src, "rust", 80);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans.len() > 2,
            "expected multiple highlighted spans"
        );
        assert_eq!(line_text(&lines[0]), format!("{GUTTER}{src}"));

        let long = "x".repeat(50);
        let wrapped = code_lines(&long, "txt", 30);
        assert!(wrapped.len() >= 2);
        assert!(line_text(&wrapped[1]).starts_with(CONT));
        let joined: String = wrapped
            .iter()
            .map(|l| line_text(l).replace(CONT, "").replace(GUTTER, ""))
            .collect();
        assert_eq!(joined, long, "wrapping must not drop characters");
    }

    #[test]
    fn diff_colours() {
        let lines = diff_lines("--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n ctx", 80);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[3].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[4].spans[1].style.fg, Some(Color::Green));
        assert!(looks_like_diff("-a\n+b"));
        assert!(!looks_like_diff("hello\nworld"));
    }

    #[test]
    fn replacement_marks_old_and_new() {
        let lines = replacement_lines("let a = 1;", "let a = 2;\nlet b = 3;", "main.rs", 80);
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0]).contains("- "));
        assert!(line_text(&lines[1]).contains("+ "));
    }

    #[test]
    fn syntax_lookup_by_path_and_token() {
        assert!(syntax_for("src/main.rs").is_some());
        assert!(syntax_for("python").is_some());
        assert!(syntax_for("sh").is_some());
        assert!(syntax_for("").is_none());
        assert!(syntax_for("no-such-language-xyz").is_none());
    }
}
