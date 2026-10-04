//! Layout of the prompt box: the input split into visual rows at a width.
//! Pure; positions are char indices into the input, like `App::cursor`.

use unicode_width::UnicodeWidthChar;

/// Most rows the prompt box grows to before it scrolls.
pub const MAX_ROWS: usize = 8;

/// One visual row: chars `start..end` of the input. The newline that ends a
/// row is not part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
}

/// Break `input` at newlines, and wherever a line is wider than `width`.
pub fn rows(input: &str, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut start = 0;
    let mut col = 0;
    let mut count = 0;
    for (i, c) in input.chars().enumerate() {
        count = i + 1;
        if c == '\n' {
            out.push(Row { start, end: i });
            start = i + 1;
            col = 0;
            continue;
        }
        let w = c.width().unwrap_or(0);
        if col + w > width && col > 0 {
            out.push(Row { start, end: i });
            start = i;
            col = 0;
        }
        col += w;
    }
    out.push(Row { start, end: count });
    out
}

/// The row the cursor is on. At a wrap point the cursor belongs to the row
/// it would type into, which is the later one.
pub fn cursor_row(rows: &[Row], cursor: usize) -> usize {
    rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0)
}

/// Display width of chars `from..to` of `input`.
pub fn width_between(input: &str, from: usize, to: usize) -> usize {
    input
        .chars()
        .skip(from)
        .take(to.saturating_sub(from))
        .map(|c| c.width().unwrap_or(0))
        .sum()
}

/// The char index in `row` nearest to display column `col` without passing it.
pub fn index_at_column(input: &str, row: Row, col: usize) -> usize {
    let mut used = 0;
    let mut idx = row.start;
    for c in input.chars().skip(row.start).take(row.end - row.start) {
        used += c.width().unwrap_or(0);
        if used > col {
            break;
        }
        idx += 1;
    }
    idx
}

/// Text as it may sit in the prompt: `\n` is the only control character, so
/// every char has the width the layout assumes. Tabs become four spaces.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_break_at_newlines_and_at_the_width() {
        assert_eq!(rows("", 10), vec![Row { start: 0, end: 0 }]);
        assert_eq!(
            rows("ab\n\ncd", 10),
            vec![
                Row { start: 0, end: 2 },
                Row { start: 3, end: 3 },
                Row { start: 4, end: 6 },
            ]
        );
        assert_eq!(
            rows("abcdefg", 3),
            vec![
                Row { start: 0, end: 3 },
                Row { start: 3, end: 6 },
                Row { start: 6, end: 7 },
            ]
        );
        // A trailing newline opens an empty row for the cursor.
        assert_eq!(rows("ab\n", 10).len(), 2);
        // Wide chars wrap by display width, not char count.
        assert_eq!(
            rows("日本語", 4),
            vec![Row { start: 0, end: 2 }, Row { start: 2, end: 3 }]
        );
    }

    #[test]
    fn cursor_row_prefers_the_later_row_at_a_wrap() {
        let r = rows("abcdef", 3);
        assert_eq!(cursor_row(&r, 2), 0);
        assert_eq!(cursor_row(&r, 3), 1);
        assert_eq!(cursor_row(&r, 6), 1);
        let r = rows("ab\ncd", 10);
        assert_eq!(cursor_row(&r, 2), 0);
        assert_eq!(cursor_row(&r, 3), 1);
    }

    #[test]
    fn columns_map_back_to_char_indices() {
        let input = "日本\nabcdef";
        let r = rows(input, 20);
        assert_eq!(width_between(input, 0, 2), 4);
        assert_eq!(index_at_column(input, r[1], 4), 7);
        // Column 3 falls inside the second wide char: stop before it.
        assert_eq!(index_at_column(input, r[0], 3), 1);
        assert_eq!(index_at_column(input, r[0], 99), 2);
    }

    #[test]
    fn clean_keeps_only_newlines_of_the_control_chars() {
        assert_eq!(clean("a\r\nb\rc\n\td\x1b[0m\x07"), "a\nb\nc\n    d[0m");
    }
}
