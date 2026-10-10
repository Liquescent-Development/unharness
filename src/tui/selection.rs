//! Text selection in the transcript. With the mouse reported to unharness
//! the terminal no longer selects text itself, so this does: positions are
//! cells of the rendered transcript lines, which stay put while it scrolls.

use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A cell of a rendered transcript line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub line: usize,
    pub col: usize,
}

/// What a drag extends by: set by single, double and triple click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Char,
    Word,
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the button went down.
    pub anchor: Point,
    /// Where the pointer is now.
    pub focus: Point,
    pub granularity: Granularity,
}

impl Selection {
    /// The selected cells, first to last inclusive, or `None` when nothing
    /// is selected (a plain click that did not move). `rows` says which
    /// cells are text: a word is text, and a word or a line wrapped onto
    /// the next row goes on there.
    pub fn range(&self, lines: &[String], rows: &[RowCopy]) -> Option<(Point, Point)> {
        let (a, b) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        let line = |i: usize| lines.get(i).map_or("", String::as_str);
        let join = |i: usize| match rows.get(i) {
            Some(RowCopy::Text { join, .. }) => *join,
            _ => Join::Line,
        };
        match self.granularity {
            Granularity::Char => (a != b).then_some((a, b)),
            Granularity::Word => {
                let word = |p: Point| word_cols(line(p.line), rows.get(p.line), p.col);
                let mut a = Point {
                    line: a.line,
                    col: word(a).0,
                };
                let mut b = Point {
                    line: b.line,
                    col: word(b).1,
                };
                // A word broken across rows: from its first piece to its last.
                while a.line > 0
                    && join(a.line) == Join::Nothing
                    && text_cells(rows.get(a.line)).is_some_and(|(first, _)| first == a.col)
                    && word_char_at(line(a.line), rows.get(a.line), a.col)
                    && let Some((_, last)) = text_cells(rows.get(a.line - 1))
                    && word_char_at(line(a.line - 1), rows.get(a.line - 1), last)
                {
                    a = Point {
                        line: a.line - 1,
                        col: word_cols(line(a.line - 1), rows.get(a.line - 1), last).0,
                    };
                }
                while join(b.line + 1) == Join::Nothing
                    && b.line + 1 < lines.len()
                    && text_cells(rows.get(b.line)).is_some_and(|(_, last)| last == b.col)
                    && word_char_at(line(b.line), rows.get(b.line), b.col)
                    && let Some((first, _)) = text_cells(rows.get(b.line + 1))
                    && word_char_at(line(b.line + 1), rows.get(b.line + 1), first)
                {
                    b = Point {
                        line: b.line + 1,
                        col: word_cols(line(b.line + 1), rows.get(b.line + 1), first).1,
                    };
                }
                Some((a, b))
            }
            Granularity::Line => {
                // The whole of a line wrapped over several rows.
                let mut first = a.line;
                while first > 0 && join(first) != Join::Line {
                    first -= 1;
                }
                let mut last = b.line;
                while last + 1 < lines.len() && join(last + 1) != Join::Line {
                    last += 1;
                }
                Some((
                    Point {
                        line: first,
                        col: 0,
                    },
                    Point {
                        line: last,
                        col: line(last).width().saturating_sub(1),
                    },
                ))
            }
        }
    }
}

/// How a row goes on from the row above it in a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// A line of its own.
    Line,
    /// The same line, wrapped at a space the wrapping dropped.
    Space,
    /// The same line, wrapped inside a word.
    Nothing,
}

/// Cells of a row that a copy takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub cells: Range<usize>,
    /// What a copy takes in place of the cells (a diff's `+ ` as `+`), when
    /// not their text.
    pub copied_as: Option<&'static str>,
}

/// What a copy takes from one rendered row. A row is text and drawing: the
/// gutters, labels, markers and rules around the text are left out, as is
/// a row that is only drawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowCopy {
    /// A block's header, a box's border, a code block's frame, a "… N more
    /// lines": nothing to copy, not even a line.
    Drawing,
    /// The pieces in order; the cells between them are drawing. A blank
    /// row has none and is an empty line.
    Text { join: Join, pieces: Vec<Piece> },
}

impl RowCopy {
    /// The same row drawn `by` cells further right (left when negative).
    pub fn shifted(self, by: isize) -> Self {
        match self {
            RowCopy::Drawing => RowCopy::Drawing,
            RowCopy::Text { join, pieces } => RowCopy::Text {
                join,
                pieces: pieces
                    .into_iter()
                    .filter_map(|p| {
                        let start = p.cells.start.checked_add_signed(by)?;
                        let end = p.cells.end.checked_add_signed(by)?;
                        Some(Piece {
                            cells: start..end,
                            copied_as: p.copied_as,
                        })
                    })
                    .collect(),
            },
        }
    }

    /// The row cut to its first `width` cells.
    pub fn clipped(self, width: usize) -> Self {
        match self {
            RowCopy::Drawing => RowCopy::Drawing,
            RowCopy::Text { join, pieces } => RowCopy::Text {
                join,
                pieces: pieces
                    .into_iter()
                    .filter(|p| p.cells.start < width)
                    .map(|p| Piece {
                        cells: p.cells.start..p.cells.end.min(width),
                        copied_as: p.copied_as,
                    })
                    .collect(),
            },
        }
    }

    /// Whether a copy takes the cell at `col`.
    pub fn covers(&self, col: usize) -> bool {
        match self {
            RowCopy::Drawing => false,
            RowCopy::Text { pieces, .. } => pieces.iter().any(|p| p.cells.contains(&col)),
        }
    }
}

/// Cells a string takes, counted as a selection counts them.
pub fn width(text: &str) -> usize {
    cells(text).map(|(_, w, _)| w).sum()
}

/// Characters a double click keeps together, so that paths, flags and
/// identifiers come out whole, as terminals do.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '.' | '~')
}

/// The chars of `line` with the cell each starts at.
fn cells(line: &str) -> impl Iterator<Item = (usize, usize, char)> + '_ {
    let mut col = 0;
    line.chars().map(move |c| {
        let w = c.width().unwrap_or(0);
        let start = col;
        col += w;
        (start, w, c)
    })
}

/// First and last cell of the word under `col`; just that cell when it
/// is not on a word. A word is text: `copy` says which cells are (all, when
/// `None`).
pub fn word_cols(line: &str, copy: Option<&RowCopy>, col: usize) -> (usize, usize) {
    let cells: Vec<(usize, usize, char)> = cells(line).filter(|(_, w, _)| *w > 0).collect();
    let Some(at) = cells.iter().position(|(s, w, _)| col >= *s && col < s + w) else {
        return (col, col);
    };
    let word = |i: &usize| {
        let (start, _, c) = cells[*i];
        is_word_char(c) && copy.is_none_or(|r| r.covers(start))
    };
    if !word(&at) {
        return (cells[at].0, cells[at].0 + cells[at].1 - 1);
    }
    let first = (0..=at).rev().take_while(word).last().unwrap_or(at);
    let last = (at..cells.len()).take_while(word).last().unwrap_or(at);
    (cells[first].0, cells[last].0 + cells[last].1 - 1)
}

/// Whether the cell at `col` is a word's.
fn word_char_at(line: &str, copy: Option<&RowCopy>, col: usize) -> bool {
    copy.is_none_or(|r| r.covers(col))
        && cells(line).any(|(start, w, c)| col >= start && col < start + w && is_word_char(c))
}

/// The first and last cell of a row's text, if it has any.
fn text_cells(copy: Option<&RowCopy>) -> Option<(usize, usize)> {
    match copy? {
        RowCopy::Text { pieces, .. } => Some((
            pieces.iter().map(|p| p.cells.start).min()?,
            pieces.iter().map(|p| p.cells.end).max()?.checked_sub(1)?,
        )),
        RowCopy::Drawing => None,
    }
}

/// The text in cells `from..=to` of one line. A wide char is taken if any
/// of its cells is in range.
fn slice(line: &str, from: usize, to: usize) -> String {
    cells(line)
        .filter(|(start, w, _)| *start <= to && start + w.max(&1) > from)
        .map(|(_, _, c)| c)
        .collect()
}

/// What a copy takes from the cells between two points, inclusive: their
/// text without the drawing around it, a line wrapped over several rows
/// joined again, no padding at the end of a line. `rows` says which cells
/// are text; a row past its end is all text. Cells with no text in them (a
/// drag over a label) are copied as drawn.
pub fn text(lines: &[String], rows: &[RowCopy], start: Point, end: Point) -> String {
    let copied = copied_text(lines, rows, start, end);
    if copied.trim().is_empty() {
        drawn_text(lines, start, end)
    } else {
        copied
    }
}

fn copied_text(lines: &[String], rows: &[RowCopy], start: Point, end: Point) -> String {
    let whole = [Piece {
        cells: 0..usize::MAX,
        copied_as: None,
    }];
    let mut out = String::new();
    let mut started = false;
    for (i, line) in lines.iter().enumerate().take(end.line + 1).skip(start.line) {
        let (join, pieces) = match rows.get(i) {
            Some(RowCopy::Drawing) => continue,
            Some(RowCopy::Text { join, pieces }) => (*join, pieces.as_slice()),
            None => (Join::Line, &whole[..]),
        };
        if std::mem::replace(&mut started, true) {
            match join {
                Join::Line => {
                    out.truncate(out.trim_end_matches(' ').len());
                    out.push('\n');
                }
                Join::Space => {
                    out.truncate(out.trim_end_matches(' ').len());
                    out.push(' ');
                }
                Join::Nothing => {}
            }
        }
        let from = if i == start.line { start.col } else { 0 };
        let to = if i == end.line { end.col } else { usize::MAX };
        for piece in pieces {
            let (a, b) = (
                piece.cells.start.max(from),
                piece.cells.end.saturating_sub(1).min(to),
            );
            if piece.cells.is_empty() || a > b {
                continue;
            }
            match piece.copied_as {
                Some(text) => out.push_str(text),
                None => out.push_str(&slice(line, a, b)),
            }
        }
    }
    out.trim_start_matches('\n').trim_end().to_string()
}

/// The cells between two points as drawn, one line per row, without the
/// padding at the end of each row.
fn drawn_text(lines: &[String], start: Point, end: Point) -> String {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end.line + 1).skip(start.line) {
        let from = if i == start.line { start.col } else { 0 };
        let to = if i == end.line { end.col } else { usize::MAX };
        out.push(slice(line, from, to).trim_end().to_string());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<String> {
        ["  run src/tui/app.rs --flag now", "", "  日本語 text   "]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn sel(a: (usize, usize), f: (usize, usize), granularity: Granularity) -> Selection {
        Selection {
            anchor: Point {
                line: a.0,
                col: a.1,
            },
            focus: Point {
                line: f.0,
                col: f.1,
            },
            granularity,
        }
    }

    fn selected(s: Selection) -> Option<String> {
        let l = lines();
        s.range(&l, &[]).map(|(a, b)| text(&l, &[], a, b))
    }

    #[test]
    fn drag_selects_cells_in_either_direction_across_lines() {
        assert_eq!(selected(sel((0, 2), (0, 2), Granularity::Char)), None);
        assert_eq!(
            selected(sel((0, 2), (0, 4), Granularity::Char)).as_deref(),
            Some("run")
        );
        assert_eq!(
            selected(sel((0, 4), (0, 2), Granularity::Char)).as_deref(),
            Some("run")
        );
        // Rows lose their trailing padding; the empty row stays a row.
        assert_eq!(
            selected(sel((2, 9), (0, 27), Granularity::Char)).as_deref(),
            Some(" now\n\n  日本語 t")
        );
        // Half of a wide char takes the whole char.
        assert_eq!(
            selected(sel((2, 3), (2, 4), Granularity::Char)).as_deref(),
            Some("日本")
        );
    }

    #[test]
    fn double_click_takes_the_word_and_triple_the_line() {
        assert_eq!(
            selected(sel((0, 10), (0, 10), Granularity::Word)).as_deref(),
            Some("src/tui/app.rs")
        );
        assert_eq!(
            selected(sel((0, 22), (0, 22), Granularity::Word)).as_deref(),
            Some("--flag")
        );
        // On a space: only that cell.
        assert_eq!(word_cols(&lines()[0], None, 5), (5, 5));
        // Past the end of the row: nothing to take.
        assert_eq!(
            selected(sel((1, 7), (1, 7), Granularity::Word)).as_deref(),
            Some("")
        );
        // Dragging after a double click extends word by word.
        assert_eq!(
            selected(sel((0, 3), (0, 9), Granularity::Word)).as_deref(),
            Some("run src/tui/app.rs")
        );
        assert_eq!(
            selected(sel((2, 4), (2, 4), Granularity::Word)).as_deref(),
            Some("日本語")
        );
        assert_eq!(
            selected(sel((2, 1), (0, 30), Granularity::Line)).as_deref(),
            Some("  run src/tui/app.rs --flag now\n\n  日本語 text")
        );
    }

    /// A header, a wrapped line in a gutter (inside a word, then at a
    /// space), a blank row and a diff line.
    fn drawn() -> (Vec<String>, Vec<RowCopy>) {
        let piece = |cells: Range<usize>| Piece {
            cells,
            copied_as: None,
        };
        let lines = [
            "## head",
            "  │ abc def",
            "  │ ↪ ghi",
            "  │ jkl",
            "",
            "  + new",
        ];
        let rows = vec![
            RowCopy::Drawing,
            RowCopy::Text {
                join: Join::Line,
                pieces: vec![piece(4..11)],
            },
            RowCopy::Text {
                join: Join::Nothing,
                pieces: vec![piece(6..9)],
            },
            RowCopy::Text {
                join: Join::Space,
                pieces: vec![piece(4..7)],
            },
            RowCopy::Text {
                join: Join::Line,
                pieces: Vec::new(),
            },
            RowCopy::Text {
                join: Join::Line,
                pieces: vec![
                    Piece {
                        cells: 2..4,
                        copied_as: Some("+"),
                    },
                    piece(4..7),
                ],
            },
        ];
        (lines.iter().map(|s| s.to_string()).collect(), rows)
    }

    fn copied(s: Selection) -> Option<String> {
        let (lines, rows) = drawn();
        s.range(&lines, &rows)
            .map(|(a, b)| text(&lines, &rows, a, b))
    }

    #[test]
    fn a_copy_takes_the_text_and_joins_what_was_wrapped() {
        assert_eq!(
            copied(sel((0, 0), (5, 99), Granularity::Char)).as_deref(),
            Some("abc defghi jkl\n\n+new")
        );
        assert_eq!(
            copied(sel((1, 6), (3, 5), Granularity::Char)).as_deref(),
            Some("c defghi jk")
        );
        // Starting in the drawing above, the copy starts at the text.
        assert_eq!(
            copied(sel((0, 3), (1, 6), Granularity::Char)).as_deref(),
            Some("abc")
        );
        // Nothing but drawing: copied as drawn.
        assert_eq!(
            copied(sel((0, 0), (0, 3), Granularity::Char)).as_deref(),
            Some("## h")
        );
        assert_eq!(
            copied(sel((5, 2), (5, 2), Granularity::Word)).as_deref(),
            Some("+")
        );
    }

    #[test]
    fn a_word_and_a_line_go_on_where_they_were_wrapped() {
        assert_eq!(
            copied(sel((2, 7), (2, 7), Granularity::Word)).as_deref(),
            Some("defghi")
        );
        assert_eq!(
            copied(sel((1, 9), (1, 9), Granularity::Word)).as_deref(),
            Some("defghi")
        );
        // The gutter is no word: that cell, as drawn.
        assert_eq!(
            copied(sel((1, 2), (1, 2), Granularity::Word)).as_deref(),
            Some("│")
        );
        assert_eq!(
            copied(sel((2, 7), (2, 7), Granularity::Line)).as_deref(),
            Some("abc defghi jkl")
        );
    }
}
