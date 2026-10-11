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

use super::rows::{Part, Rows};
use super::selection::Join;

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

/// Make text safe to put in a terminal cell grid: strip ANSI/OSC escape
/// sequences, expand tabs, drop carriage returns and other control characters
/// (newlines are kept). Anything that moves the cursor on its own corrupts
/// ratatui's diff and leaves artifacts on screen.
pub fn sanitize(text: &str) -> String {
    clean(text, true)
}

/// Text for the clipboard: escape sequences, carriage returns and other
/// control characters out, as [`sanitize`] does, but tabs kept.
pub fn strip_controls(text: &str) -> String {
    clean(text, false)
}

fn clean(text: &str, expand_tabs: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut col = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI: ESC [ ... final byte 0x40..=0x7E ; OSC: ESC ] ... BEL or ESC \
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for n in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&n) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(n) = chars.next() {
                            if n == '\x07' {
                                break;
                            }
                            if n == '\x1b' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\t' if !expand_tabs => {
                out.push('\t');
                col += 4 - (col % 4);
            }
            '\t' => {
                let pad = 4 - (col % 4);
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            '\r' => {}
            c if c.is_control() => {}
            c => {
                out.push(c);
                col += UnicodeWidthChar::width(c).unwrap_or(1);
            }
        }
    }
    out
}

/// Word wrap that also breaks tokens wider than `max_width` by character.
pub fn wrap_words(text: &str, max_width: usize) -> Vec<String> {
    wrap_words_joined(text, max_width)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// [`wrap_words`], with how each line goes on from the one before: after
/// the spaces a break drops, or inside a word broken by character. The
/// first is a [`Join::Line`].
pub fn wrap_words_joined(text: &str, max_width: usize) -> Vec<(String, Join)> {
    let max_width = max_width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    let mut join = Join::Line;
    for word in text.split(' ') {
        let word_len = unicode_width::UnicodeWidthStr::width(word);
        if word_len > max_width {
            if !current.is_empty() {
                lines.push((std::mem::take(&mut current), join));
                join = Join::Spaces(1);
            } else if let Join::Spaces(n) = &mut join {
                *n += 1;
            }
            let mut chunk_len = 0;
            for c in word.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(1);
                if chunk_len + cw > max_width {
                    lines.push((std::mem::take(&mut current), join));
                    join = Join::Nothing;
                    chunk_len = 0;
                }
                current.push(c);
                chunk_len += cw;
            }
            current_len = chunk_len;
            continue;
        }
        if current_len == 0 {
            // After a break, before any text on the line: the space in
            // front of this word was dropped with the break.
            if let Join::Spaces(n) = &mut join {
                *n += 1;
            }
            current.push_str(word);
            current_len = word_len;
        } else if current_len + 1 + word_len <= max_width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push((std::mem::replace(&mut current, word.to_string()), join));
            join = Join::Spaces(1);
            current_len = word_len;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push((current, join));
    }
    lines
}

/// Safety net: no rendered line may exceed `width` cells, whatever the
/// wrapping code did. Cuts the line to leave room for a `…`, which takes the
/// style of the span it was cut in.
pub fn clamp_lines(lines: &mut [Line<'static>], width: usize) {
    for line in lines.iter_mut() {
        let total: usize = line
            .spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        if total <= width || width == 0 {
            continue;
        }
        let room = width - 1;
        let mut used = 0usize;
        let mut kept: Vec<Span<'static>> = Vec::new();
        for span in std::mem::take(&mut line.spans) {
            let mut text = String::new();
            let mut cut = false;
            for c in span.content.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(1);
                if used + cw > room {
                    cut = true;
                    break;
                }
                text.push(c);
                used += cw;
            }
            if cut {
                text.push('…');
                kept.push(Span::styled(text, span.style));
                break;
            }
            kept.push(Span::styled(text, span.style));
        }
        line.spans = kept;
    }
}

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

/// One source line of styled segments wrapped to `width` display cells,
/// after `first_prefix` and then `cont_prefix`, which are drawing.
fn wrap_spans(
    segments: Vec<(Style, String)>,
    width: usize,
    first_prefix: Vec<Part>,
    cont_prefix: Vec<Part>,
) -> Rows {
    let cells = |parts: &[Part]| -> usize {
        parts
            .iter()
            .map(|p| unicode_width::UnicodeWidthStr::width(p.span().content.as_ref()))
            .sum()
    };
    let prefix_width = cells(&first_prefix).max(cells(&cont_prefix));
    let avail = width.saturating_sub(prefix_width).max(8);
    let mut rows = Rows::new();
    let mut current: Vec<Part> = first_prefix;
    let mut join = Join::Line;
    let mut used = 0usize;
    let mut buf = String::new();
    let mut buf_style = Style::default();

    let flush = |current: &mut Vec<Part>, buf: &mut String, style: Style| {
        if !buf.is_empty() {
            current.push(Part::Text(Span::styled(std::mem::take(buf), style)));
        }
    };

    for (style, text) in segments {
        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if used + w > avail {
                flush(&mut current, &mut buf, buf_style);
                let row = std::mem::replace(&mut current, cont_prefix.clone());
                rows.text(row, std::mem::replace(&mut join, Join::Nothing));
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
    rows.text(current, join);
    rows
}

/// Output and the gutter beside it: the terminal's own colour, dimmed, which
/// stays readable on a dark background and a light one alike (a named grey
/// all but vanishes on one of them).
pub fn faint() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn gutter() -> Vec<Part> {
    vec![Part::Draw(Span::styled(GUTTER, faint()))]
}

fn cont() -> Vec<Part> {
    vec![Part::Draw(Span::styled(CONT, faint()))]
}

/// Highlighted, wrapped source lines with a gutter. `hint` is a language
/// token or file path; `diff` content is routed to [`diff_lines`].
pub fn code_lines(text: &str, hint: &str, width: usize) -> Rows {
    if hint.eq_ignore_ascii_case("diff") || hint.eq_ignore_ascii_case("patch") {
        return diff_lines(text, width);
    }
    let text = sanitize(text);
    let mut hl = syntax_for(hint).map(|s| HighlightLines::new(s, &THEME));
    let mut out = Rows::new();
    for line in text.lines() {
        let segs = highlight_spans(&mut hl, line);
        out.append(wrap_spans(segs, width, gutter(), cont()));
    }
    out
}

/// Plain (unhighlighted) wrapped lines with a gutter, e.g. command output.
pub fn plain_lines(text: &str, width: usize, style: Style) -> Rows {
    let text = sanitize(text);
    let mut out = Rows::new();
    for line in text.lines() {
        out.append(wrap_spans(
            vec![(style, line.to_string())],
            width,
            gutter(),
            cont(),
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

/// Red/green diff rendering, wrapped. A copy takes the diff as written.
pub fn diff_lines(text: &str, width: usize) -> Rows {
    let text = sanitize(text);
    let mut out = Rows::new();
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
        let (body, marker) = if marker.trim().is_empty() {
            (
                line.to_string(),
                Part::Draw(Span::styled(marker, style.add_modifier(Modifier::BOLD))),
            )
        } else {
            (
                line[1..].to_string(),
                Part::Stand(
                    Span::styled(marker, style.add_modifier(Modifier::BOLD)),
                    if marker.starts_with('+') { "+" } else { "-" },
                ),
            )
        };
        let first = vec![Part::Draw(Span::styled(GUTTER, faint())), marker];
        out.append(wrap_spans(vec![(style, body)], width, first, cont()));
    }
    out
}

/// Render an old→new replacement as a diff with syntax colouring under the
/// red/green tint. `hint` is the file path for syntax selection. A copy
/// takes it as a diff, each line after its `-` or `+`.
pub fn replacement_lines(old: &str, new: &str, hint: &str, width: usize) -> Rows {
    let mut out = marked_lines(old, hint, width, Marker::Old);
    out.append(marked_lines(new, hint, width, Marker::New));
    out
}

/// A file written whole: as the new side of a replacement, but a copy
/// takes the file, without the `+ ` in front of each line.
pub fn written_lines(content: &str, hint: &str, width: usize) -> Rows {
    marked_lines(content, hint, width, Marker::Written)
}

#[derive(Clone, Copy)]
enum Marker {
    Old,
    New,
    Written,
}

/// `text` highlighted on a red or green tint, each line after a `-` or `+`.
fn marked_lines(text: &str, hint: &str, width: usize, marker: Marker) -> Rows {
    let text = sanitize(text);
    let mut hl = syntax_for(hint).map(|s| HighlightLines::new(s, &THEME));
    let (sign, color, bg) = match marker {
        Marker::Old => ("-", Color::Red, Color::Rgb(60, 20, 20)),
        Marker::New | Marker::Written => ("+", Color::Green, Color::Rgb(15, 50, 20)),
    };
    let mut out = Rows::new();
    for line in text.lines() {
        let segs = highlight_spans(&mut hl, line)
            .into_iter()
            .map(|(s, t)| (s.bg(bg), t))
            .collect();
        let shown = Span::styled(
            format!("{sign} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
        let first = vec![
            Part::Draw(Span::styled(GUTTER, faint())),
            match marker {
                Marker::Written => Part::Draw(shown),
                Marker::Old | Marker::New => Part::Stand(shown, sign),
            },
        ];
        out.append(wrap_spans(segs, width, first, cont()));
    }
    out
}

/// Visible text of a rendered line (for tests and summaries).
pub fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::super::selection::{RowCopy, width};
    use super::*;

    #[test]
    fn highlights_rust_and_wraps_long_lines() {
        let src = "fn main() { let x = \"hello\"; }";
        let lines = code_lines(src, "rust", 80).into_lines();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans.len() > 2,
            "expected multiple highlighted spans"
        );
        assert_eq!(line_text(&lines[0]), format!("{GUTTER}{src}"));

        let long = "x".repeat(50);
        let wrapped = code_lines(&long, "txt", 30).into_lines();
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
        let lines = diff_lines("--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n ctx", 80).into_lines();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[3].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[4].spans[1].style.fg, Some(Color::Green));
        assert!(looks_like_diff("-a\n+b"));
        assert!(!looks_like_diff("hello\nworld"));
    }

    #[test]
    fn replacement_marks_old_and_new() {
        let lines =
            replacement_lines("let a = 1;", "let a = 2;\nlet b = 3;", "main.rs", 80).into_lines();
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0]).contains("- "));
        assert!(line_text(&lines[1]).contains("+ "));
    }

    #[test]
    fn a_copy_takes_code_and_output_without_the_gutter() {
        let src = "fn main() {\n\n    let long = \"abcdefghijklmnopqrstuvwxyz\";\n}";
        let rows = code_lines(src, "rust", 30);
        assert!(rows.len() > 4, "the long line wraps");
        assert_eq!(rows.copied(), src);
        // The gutter and the wrap mark are drawing; a wrapped row goes on
        // inside the line.
        assert!(!rows.copies()[0].covers(0));
        assert!(rows.copies()[0].covers(width(GUTTER)));
        assert!(matches!(
            rows.copies()[3],
            RowCopy::Text {
                join: Join::Nothing,
                ..
            }
        ));
        let out = plain_lines("total 4\ndrwxr-xr-x  2 me me 4096 .", 80, faint());
        assert_eq!(out.copied(), "total 4\ndrwxr-xr-x  2 me me 4096 .");
    }

    #[test]
    fn wrapped_code_fills_the_width_it_is_given() {
        let cells = |l: &Line<'_>| unicode_width::UnicodeWidthStr::width(line_text(l).as_str());
        for rows in [
            code_lines(&"x".repeat(80), "txt", 30),
            diff_lines(&format!("+{}", "x".repeat(80)), 30),
            replacement_lines("", &"x".repeat(80), "a.txt", 30),
        ] {
            let widths: Vec<usize> = rows.lines().iter().map(cells).collect();
            assert!(widths.iter().all(|w| *w <= 30), "{widths:?}");
            assert_eq!(widths[1], 30, "{widths:?}");
        }
    }

    #[test]
    fn a_diff_is_copied_as_written_and_a_written_file_without_markers() {
        let diff = "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n ctx";
        assert_eq!(diff_lines(diff, 80).copied(), diff);
        let edit = replacement_lines("let a = 1;", "let a = 2;\nlet b = 3;", "main.rs", 80);
        assert_eq!(edit.copied(), "-let a = 1;\n+let a = 2;\n+let b = 3;");
        let file = "let a = 2;\n    let b = 3;";
        let written = written_lines(file, "main.rs", 80);
        assert_eq!(written.copied(), file);
        // Drawn as the new side of an edit.
        assert_eq!(
            written.lines().iter().map(line_text).collect::<Vec<_>>(),
            replacement_lines("", file, "main.rs", 80)
                .lines()
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn wrapped_words_say_how_they_go_on() {
        assert_eq!(
            wrap_words_joined("one two three", 8),
            vec![
                ("one two".to_string(), Join::Line),
                ("three".to_string(), Join::Spaces(1))
            ]
        );
        assert_eq!(
            wrap_words_joined("go abcdefghij x", 4),
            vec![
                ("go".to_string(), Join::Line),
                ("abcd".to_string(), Join::Spaces(1)),
                ("efgh".to_string(), Join::Nothing),
                ("ij x".to_string(), Join::Nothing),
            ]
        );
        assert_eq!(
            wrap_words("go abcdefghij x", 4),
            vec!["go", "abcd", "efgh", "ij x"]
        );
        // Every space a break drops is counted: a copy gives them back.
        for text in ["echo 'a    b' c", "a  b", "ab    cd ef"] {
            for width in 1..text.len() {
                let mut rows = Rows::new();
                for (seg, join) in wrap_words_joined(text, width) {
                    rows.text(vec![Part::Text(Span::raw(seg))], join);
                }
                assert_eq!(rows.copied(), text, "{width}");
            }
        }
    }

    #[test]
    fn sanitize_strips_escapes_and_expands_tabs() {
        assert_eq!(sanitize("a\x1b[31mred\x1b[0m b"), "ared b");
        assert_eq!(sanitize("\x1b]0;title\x07x"), "x");
        assert_eq!(sanitize("x\ty\n\tz"), "x   y\n    z");
        assert_eq!(sanitize("line\r\nnext\x07"), "line\nnext");
        assert_eq!(strip_controls("a\x1b[31m\tb\r\n\tc\x07"), "a\tb\n\tc");
    }

    #[test]
    fn wrap_words_breaks_long_tokens_and_clamp_guards() {
        let w = wrap_words(&"x".repeat(25), 10);
        assert_eq!(w.len(), 3);
        assert_eq!(wrap_words("", 10), vec![String::new()]);
        let mut lines = vec![Line::from(vec![Span::raw("ab"), Span::raw("cdefgh")])];
        clamp_lines(&mut lines, 5);
        assert_eq!(line_text(&lines[0]), "abcd…");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(line_text(&lines[0]).as_str()),
            5
        );
        // Full exactly at a span boundary: the `…` still fits.
        let mut lines = vec![Line::from(vec![Span::raw("abcde"), Span::raw("fg")])];
        clamp_lines(&mut lines, 5);
        assert_eq!(line_text(&lines[0]), "abcd…");
        // A wide character that would straddle the edge is dropped.
        let mut lines = vec![Line::from(vec![Span::raw("abc"), Span::raw("界x")])];
        clamp_lines(&mut lines, 4);
        assert_eq!(line_text(&lines[0]), "abc…");
        let mut lines = vec![Line::from(Span::raw("abcde"))];
        clamp_lines(&mut lines, 5);
        assert_eq!(line_text(&lines[0]), "abcde");
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
