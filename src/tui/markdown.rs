//! Markdown rendering for assistant text: headers, lists, quotes, inline
//! code/bold/italic, fenced code blocks (syntax highlighted, wrapped), GFM
//! tables and bare diff hunks.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::code::{code_lines, diff_lines, is_diff_line, sanitize, wrap_words};

pub fn render_markdown_to_lines(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let text = sanitize(text);
    let text = text.as_str();
    let mut lines = Vec::new();
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();

    let flush_code = |lines: &mut Vec<Line<'static>>, lang: &str, buf: &str| {
        lines.extend(code_lines(buf, lang, max_width.saturating_sub(2)));
    };

    let source: Vec<&str> = text.lines().collect();
    let mut next = 0;
    while next < source.len() {
        let raw_line = source[next];
        next += 1;
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

        if let Some((table, used)) = Table::parse(&source[next - 1..]) {
            lines.extend(table.lines(max_width));
            next += used - 1;
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
            // No padding around it: what is copied is what is on screen.
            spans.push(Span::styled(
                after[1..1 + end].to_string(),
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
    wrap_words(text, max_width)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A GFM table: a header row, a delimiter row with as many cells, and the
/// rows after it up to a blank line or one without a pipe.
#[derive(Debug)]
struct Table {
    header: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// The table that starts at `lines[0]` and how many lines it takes, or
    /// `None` when those lines are not one (no delimiter row yet while
    /// streaming, or one with another number of cells).
    fn parse(lines: &[&str]) -> Option<(Table, usize)> {
        let (head, delim) = (lines.first()?, lines.get(1)?);
        if !head.contains('|') || !delim.contains('|') {
            return None;
        }
        let header = split_row(head);
        let aligns = split_row(delim)
            .iter()
            .map(|cell| delimiter_align(cell))
            .collect::<Option<Vec<_>>>()?;
        if header.len() != aligns.len() {
            return None;
        }
        let rows: Vec<Vec<String>> = lines[2..]
            .iter()
            .take_while(|l| !l.trim().is_empty() && l.contains('|'))
            .map(|l| {
                let mut cells = split_row(l);
                cells.resize(header.len(), String::new());
                cells
            })
            .collect();
        let used = 2 + rows.len();
        Some((
            Table {
                header,
                aligns,
                rows,
            },
            used,
        ))
    }

    /// Columns as wide as their content when the table fits in `max_width`,
    /// else the widest narrowed first, each cell wrapped in its column.
    fn lines(&self, max_width: usize) -> Vec<Line<'static>> {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let header: Vec<Cell> = self.header.iter().map(|c| cell(c, Some(bold))).collect();
        let rows: Vec<Vec<Cell>> = self
            .rows
            .iter()
            .map(|r| r.iter().map(|c| cell(c, None)).collect())
            .collect();
        let n = header.len();
        let natural: Vec<usize> = (0..n)
            .map(|c| {
                std::iter::once(&header)
                    .chain(&rows)
                    .flat_map(|r| r[c].iter().map(|seg| spans_width(seg)))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let widths = fit_columns(&natural, max_width.saturating_sub(2 + 3 * (n - 1)));

        let wrapped = |row: &[Cell]| -> Vec<Vec<Vec<Span<'static>>>> {
            row.iter()
                .zip(&widths)
                .map(|(cell, w)| cell.iter().flat_map(|seg| wrap_styled(seg, *w)).collect())
                .collect()
        };
        let header = wrapped(&header);
        let rows: Vec<_> = rows.iter().map(|r| wrapped(r)).collect();
        let rule = Line::from(Span::styled(
            format!(
                "  {}",
                widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─")
            ),
            Style::default().fg(Color::DarkGray),
        ));
        // Rows that wrap run into each other without a rule between them.
        let ruled = rows.iter().any(|r| r.iter().any(|c| c.len() > 1));

        let mut out = self.row_lines(&header, &widths);
        out.push(rule.clone());
        for (i, row) in rows.iter().enumerate() {
            if ruled && i > 0 {
                out.push(rule.clone());
            }
            out.extend(self.row_lines(row, &widths));
        }
        out
    }

    fn row_lines(&self, row: &[Vec<Vec<Span<'static>>>], widths: &[usize]) -> Vec<Line<'static>> {
        let height = row.iter().map(Vec::len).max().unwrap_or(1);
        let sep = Span::styled(" │ ", Style::default().fg(Color::DarkGray));
        (0..height)
            .map(|k| {
                let mut spans = vec![Span::raw("  ")];
                for (c, cell) in row.iter().enumerate() {
                    if c > 0 {
                        spans.push(sep.clone());
                    }
                    let content = cell.get(k).cloned().unwrap_or_default();
                    let pad = widths[c].saturating_sub(spans_width(&content));
                    let (left, right) = match self.aligns[c] {
                        Align::Left => (0, pad),
                        Align::Center => (pad / 2, pad - pad / 2),
                        Align::Right => (pad, 0),
                    };
                    spans.push(Span::raw(" ".repeat(left)));
                    spans.extend(content);
                    spans.push(Span::raw(" ".repeat(right)));
                }
                trim_end(&mut spans);
                Line::from(spans)
            })
            .collect()
    }
}

/// A cell's lines (split at `<br>`, which models put in cells), each its
/// inline formatting.
type Cell = Vec<Vec<Span<'static>>>;

fn cell(text: &str, extra: Option<Style>) -> Cell {
    let text = text.replace("<br />", "<br>").replace("<br/>", "<br>");
    text.split("<br>")
        .map(|seg| {
            parse_inline_formatting(seg.trim())
                .into_iter()
                .map(|s| match extra {
                    Some(st) => Span::styled(s.content, s.style.patch(st)),
                    None => s,
                })
                .collect()
        })
        .collect()
}

/// The cells of a table row: split at pipes that are not escaped, the
/// outer pipes dropped, `\|` read as a pipe.
fn split_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = match line.strip_suffix('|') {
        Some(inner) if !inner.ends_with('\\') => inner,
        _ => line,
    };
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('|') => cur.push('|'),
                Some(other) => {
                    cur.push('\\');
                    cur.push(other);
                }
                None => cur.push('\\'),
            },
            '|' => cells.push(std::mem::take(&mut cur).trim().to_string()),
            c => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// A delimiter cell (`---`, `:---`, `---:`, `:---:`) and its alignment.
fn delimiter_align(cell: &str) -> Option<Align> {
    let left = cell.starts_with(':');
    let right = cell.len() > 1 && cell.ends_with(':');
    let dashes = cell.trim_start_matches(':').trim_end_matches(':');
    if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    })
}

/// Column widths in `avail` cells: as wide as their content when that
/// fits, else every column capped at the widest cap that fits, so narrow
/// columns keep their width and the wide ones share the rest.
fn fit_columns(natural: &[usize], avail: usize) -> Vec<usize> {
    let natural: Vec<usize> = natural.iter().map(|w| (*w).max(1)).collect();
    if natural.iter().sum::<usize>() <= avail {
        return natural;
    }
    let capped = |cap: usize| natural.iter().map(|w| (*w).min(cap)).sum::<usize>();
    let max = natural.iter().copied().max().unwrap_or(1);
    let cap = (1..=max)
        .rev()
        .find(|cap| capped(*cap) <= avail)
        .unwrap_or(1);
    let mut widths: Vec<usize> = natural.iter().map(|w| (*w).min(cap)).collect();
    let mut spare = avail.saturating_sub(capped(cap));
    for (w, nat) in widths.iter_mut().zip(&natural) {
        if spare == 0 {
            break;
        }
        if *nat > *w {
            *w += 1;
            spare -= 1;
        }
    }
    widths
}

/// Drops the spaces a line ends with, so that a copy has no padding at
/// the end of each row.
fn trim_end(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let kept = last.content.trim_end().len();
        if kept > 0 {
            last.content.to_mut().truncate(kept);
            return;
        }
        spans.pop();
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Word wrap of styled text to `width` cells; a word wider than that is
/// broken by character. Always at least one (maybe empty) line.
fn wrap_styled(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let cw = |c: char| UnicodeWidthChar::width(c).unwrap_or(1);
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut line: Vec<(char, Style)> = Vec::new();
    let mut used = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i].0 == ' ' {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].0 != ' ' {
            i += 1;
        }
        let word = &chars[start..i];
        let word_width: usize = word.iter().map(|(c, _)| cw(*c)).sum();
        if used > 0 && used + 1 + word_width <= width {
            // The space keeps its own style (inside inline code, say).
            line.push(chars[start - 1]);
            line.extend_from_slice(word);
            used += 1 + word_width;
            continue;
        }
        if used > 0 {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for &(c, style) in word {
            if used > 0 && used + cw(c) > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push((c, style));
            used += cw(c);
        }
    }
    lines.push(line);
    lines
        .into_iter()
        .map(|l| {
            let mut out: Vec<Span<'static>> = Vec::new();
            for (c, style) in l {
                match out.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => out.push(Span::styled(c.to_string(), style)),
                }
            }
            out
        })
        .collect()
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
        assert!(texts.contains(&"cargo test".to_string()));
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

    fn table_rows(text: &str, width: usize) -> Vec<String> {
        render_markdown_to_lines(text, width)
            .iter()
            .map(line_text)
            .collect()
    }

    #[test]
    fn a_table_is_drawn_in_columns_as_wide_as_their_content() {
        let rows = table_rows(
            "Findings:\n\n| # | Sev | Where |\n|---|---|---|\n| M1 | Med | form.tsx |\n| L10 | Low | a.rs |\n\nDone.",
            80,
        );
        assert_eq!(
            rows,
            [
                "  Findings:",
                "",
                "  #   │ Sev │ Where",
                "  ────┼─────┼─────────",
                "  M1  │ Med │ form.tsx",
                "  L10 │ Low │ a.rs",
                "",
                "  Done.",
            ]
        );
        let lines = render_markdown_to_lines("| a | b |\n|---|---|\n| 1 | 2 |", 80);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content == "a" && s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn a_table_wider_than_the_pane_wraps_its_widest_column() {
        let text = "| # | Finding |\n|---|---|\n| M1 | First inline Controller in the app, wrapped around a field |\n| M2 | Short |";
        let rows = table_rows(text, 30);
        assert!(rows.iter().all(|r| r.width() <= 30), "{rows:#?}");
        assert_eq!(
            rows,
            [
                "  #  │ Finding",
                "  ───┼────────────────────────",
                "  M1 │ First inline Controller",
                "     │ in the app, wrapped",
                "     │ around a field",
                "  ───┼────────────────────────",
                "  M2 │ Short",
            ]
        );
        // Far too narrow: words are broken, nothing goes past the edge.
        let rows = table_rows(text, 14);
        assert!(rows.iter().all(|r| r.width() <= 14), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("│ First")), "{rows:#?}");
    }

    #[test]
    fn table_cells_keep_inline_code_and_escaped_pipes() {
        let lines = render_markdown_to_lines(
            "| Where | Note |\n|---|---|\n| `form.tsx:177-192` | `a \\| b` and **bold** |",
            80,
        );
        let row = &lines[2];
        assert_eq!(line_text(row), "  form.tsx:177-192 │ a | b and bold");
        assert!(
            row.spans
                .iter()
                .any(|s| s.content == "form.tsx:177-192" && s.style.bg.is_some())
        );
        assert!(
            row.spans
                .iter()
                .any(|s| s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn table_alignment_markers_pad_cells() {
        let rows = table_rows(
            "| left | middle | right |\n|:---|:---:|---:|\n| a | b | c |\n| <br> | two<br>lines | 10 |",
            80,
        );
        assert_eq!(
            rows,
            [
                "  left │ middle │ right",
                "  ─────┼────────┼──────",
                "  a    │   b    │     c",
                "  ─────┼────────┼──────",
                "       │  two   │    10",
                "       │ lines  │",
            ]
        );
    }

    #[test]
    fn rows_without_a_delimiter_stay_text() {
        let rows = table_rows("| a | b |\n| 1 | 2 |", 80);
        assert_eq!(rows, ["  | a | b |", "  | 1 | 2 |"]);
        // Another number of cells in the delimiter row is not a table either.
        let rows = table_rows("| a | b |\n|---|\n| 1 | 2 |", 80);
        assert_eq!(rows[0], "  | a | b |");
        // While the delimiter row streams in, the header is text until it
        // is complete, then the rows that follow it are the table's.
        let rows = table_rows("| a | b |\n|---|---|\n| 1 |", 80);
        assert_eq!(rows, ["  a │ b", "  ──┼──", "  1 │"]);
    }
}
