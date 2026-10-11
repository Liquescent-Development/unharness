//! Rendered rows that know what a copy takes from them: which of their
//! cells are text and which are drawing (gutters, labels, markers, rules),
//! and whether a row goes on from the row above it, wrapped.

use std::ops::Range;

use ratatui::text::{Line, Span};

use super::code::clamp_lines;
use super::selection::{Join, Piece, RowCopy, width};

/// A span of a row being built, and what a copy makes of it.
#[derive(Debug, Clone)]
pub enum Part {
    /// Copied as drawn.
    Text(Span<'static>),
    /// Left out of a copy: a gutter, a label, a marker, padding.
    Draw(Span<'static>),
    /// Copied as the given text instead (a diff's `+ ` as `+`).
    Stand(Span<'static>, &'static str),
}

impl Part {
    pub fn span(&self) -> &Span<'static> {
        match self {
            Part::Text(s) | Part::Draw(s) | Part::Stand(s, _) => s,
        }
    }

    pub fn span_mut(&mut self) -> &mut Span<'static> {
        match self {
            Part::Text(s) | Part::Draw(s) | Part::Stand(s, _) => s,
        }
    }
}

/// The line `parts` draw and what a copy takes from it.
fn build(parts: Vec<Part>, join: Join) -> (Line<'static>, RowCopy) {
    let mut spans = Vec::with_capacity(parts.len());
    let mut pieces: Vec<Piece> = Vec::new();
    let mut at = 0;
    for part in parts {
        let cells = at..at + width(&part.span().content);
        at = cells.end;
        let span = match part {
            Part::Text(span) => {
                match pieces.last_mut() {
                    Some(last) if last.copied_as.is_none() && last.cells.end == cells.start => {
                        last.cells.end = cells.end;
                    }
                    _ if cells.is_empty() => {}
                    _ => pieces.push(Piece {
                        cells,
                        copied_as: None,
                    }),
                }
                span
            }
            Part::Stand(span, copied_as) => {
                if !cells.is_empty() {
                    pieces.push(Piece {
                        cells,
                        copied_as: Some(copied_as),
                    });
                }
                span
            }
            Part::Draw(span) => span,
        };
        spans.push(span);
    }
    (Line::from(spans), RowCopy::Text { join, pieces })
}

/// Cuts a row wider than `width` as [`clamp_lines`] does, and what a copy
/// takes from it with it: not the `…` that ends it, which is a cell short
/// of the edge when the cut fell inside a wide character.
pub fn clamp_row(line: &mut Line<'static>, copy: &mut RowCopy, width: usize) {
    let drawn = |line: &Line<'_>| -> usize {
        line.spans
            .iter()
            .map(|s| super::selection::width(&s.content))
            .sum()
    };
    let before = drawn(line);
    clamp_lines(std::slice::from_mut(line), width);
    let after = drawn(line);
    if after < before {
        *copy = std::mem::replace(copy, RowCopy::Drawing).clipped(after.saturating_sub(1));
    }
}

/// Rendered rows, each with what a copy takes from it.
#[derive(Debug, Default, Clone)]
pub struct Rows {
    lines: Vec<Line<'static>>,
    copy: Vec<RowCopy>,
    /// The rows of each fenced code block, frame included, in order.
    code: Vec<Range<usize>>,
}

impl Rows {
    pub fn new() -> Self {
        Self::default()
    }

    /// A row of text and drawing, going on from the row above as `join`
    /// says.
    pub fn text(&mut self, parts: Vec<Part>, join: Join) {
        let (line, copy) = build(parts, join);
        self.push(line, copy);
    }

    /// A row of drawing, nothing of which is copied, unless one of `parts`
    /// is not drawing (a list item's number before a code block's frame):
    /// then a line with that on it.
    pub fn drawing(&mut self, parts: Vec<Part>) {
        if parts.iter().all(|p| matches!(p, Part::Draw(_))) {
            let spans: Vec<_> = parts.into_iter().map(|p| p.span().clone()).collect();
            self.push(Line::from(spans), RowCopy::Drawing);
        } else {
            self.text(parts, Join::Line);
        }
    }

    /// An empty row, which is an empty line in a copy.
    pub fn blank(&mut self) {
        self.push(
            Line::default(),
            RowCopy::Text {
                join: Join::Line,
                pieces: Vec::new(),
            },
        );
    }

    pub fn push(&mut self, line: Line<'static>, copy: RowCopy) {
        self.lines.push(line);
        self.copy.push(copy);
    }

    /// A row drawn elsewhere, with `prefix` in front of it.
    pub fn push_after(&mut self, prefix: Vec<Part>, line: Line<'static>, copy: RowCopy) {
        let join = match &copy {
            RowCopy::Text { join, .. } => *join,
            RowCopy::Drawing => Join::Line,
        };
        let (mut head, head_copy) = build(prefix, join);
        let RowCopy::Text {
            pieces: mut head_pieces,
            ..
        } = head_copy
        else {
            unreachable!("build makes a text row")
        };
        let shift = head.spans.iter().map(|s| width(&s.content)).sum::<usize>();
        let copy = match copy.shifted(shift as isize) {
            RowCopy::Drawing if head_pieces.is_empty() => RowCopy::Drawing,
            RowCopy::Drawing => RowCopy::Text {
                join,
                pieces: head_pieces,
            },
            RowCopy::Text { join, pieces } => {
                head_pieces.extend(pieces);
                RowCopy::Text {
                    join,
                    pieces: head_pieces,
                }
            }
        };
        head.spans.extend(line.spans);
        self.push(head, copy);
    }

    pub fn append(&mut self, other: Rows) {
        let at = self.lines.len();
        self.lines.extend(other.lines);
        self.copy.extend(other.copy);
        self.code
            .extend(other.code.into_iter().map(|r| r.start + at..r.end + at));
    }

    /// Says that `rows` are a fenced code block.
    pub fn mark_code(&mut self, rows: Range<usize>) {
        self.code.push(rows);
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// For changes that keep each cell where it is (`│` to `└`).
    pub fn lines_mut(&mut self) -> &mut [Line<'static>] {
        &mut self.lines
    }

    pub fn copies(&self) -> &[RowCopy] {
        &self.copy
    }

    pub fn code(&self) -> &[Range<usize>] {
        &self.code
    }

    /// Keeps the first `n` rows.
    pub fn truncate(&mut self, n: usize) {
        self.lines.truncate(n);
        self.copy.truncate(n);
        self.code.retain(|r| r.start < n);
        for r in &mut self.code {
            r.end = r.end.min(n);
        }
    }

    /// Cuts every row wider than `width` ([`clamp_row`]).
    pub fn clamp(&mut self, width: usize) {
        for (line, copy) in self.lines.iter_mut().zip(&mut self.copy) {
            clamp_row(line, copy, width);
        }
    }

    pub fn into_lines(self) -> Vec<Line<'static>> {
        self.lines
    }

    pub fn into_parts(self) -> (Vec<Line<'static>>, Vec<RowCopy>, Vec<Range<usize>>) {
        (self.lines, self.copy, self.code)
    }

    /// What a copy of all the rows takes.
    #[cfg(test)]
    pub fn copied(&self) -> String {
        let lines: Vec<String> = self.lines.iter().map(super::code::line_text).collect();
        let end = super::selection::Point {
            line: lines.len().saturating_sub(1),
            col: usize::MAX,
        };
        super::selection::text(&lines, &self.copy, Default::default(), end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(cells: Range<usize>) -> Piece {
        Piece {
            cells,
            copied_as: None,
        }
    }

    #[test]
    fn a_row_says_which_of_its_cells_are_text() {
        let mut rows = Rows::new();
        rows.text(
            vec![
                Part::Draw(Span::raw("  │ ")),
                Part::Stand(Span::raw("+ "), "+"),
                Part::Text(Span::raw("let ")),
                Part::Text(Span::raw("a")),
                Part::Draw(Span::raw("  ")),
                Part::Text(Span::raw("日本")),
            ],
            Join::Line,
        );
        assert_eq!(drawn(&rows, 0), "  │ + let a  日本");
        assert_eq!(
            rows.copies()[0],
            RowCopy::Text {
                join: Join::Line,
                pieces: vec![
                    Piece {
                        cells: 4..6,
                        copied_as: Some("+"),
                    },
                    // Text runs that touch are one piece.
                    piece(6..11),
                    piece(13..17),
                ],
            }
        );
        rows.drawing(vec![Part::Draw(Span::raw("┌─ rust"))]);
        assert_eq!(rows.copies()[1], RowCopy::Drawing);
        rows.drawing(vec![
            Part::Text(Span::raw("1. ")),
            Part::Draw(Span::raw("┌─")),
        ]);
        assert_eq!(
            rows.copies()[2],
            RowCopy::Text {
                join: Join::Line,
                pieces: vec![piece(0..3)],
            }
        );
    }

    #[test]
    fn a_cut_row_copies_what_is_left_of_it() {
        let mut rows = Rows::new();
        rows.text(
            vec![Part::Draw(Span::raw("ab")), Part::Text(Span::raw("cdefgh"))],
            Join::Spaces(1),
        );
        rows.clamp(5);
        assert_eq!(drawn(&rows, 0), "abcd…");
        assert_eq!(
            rows.copies()[0],
            RowCopy::Text {
                join: Join::Spaces(1),
                pieces: vec![piece(2..4)],
            }
        );
    }

    #[test]
    fn a_row_cut_inside_a_wide_char_does_not_copy_its_ellipsis() {
        let mut rows = Rows::new();
        rows.text(vec![Part::Text(Span::raw("ab日本"))], Join::Line);
        // Four cells: `日` would take the third and fourth, past the room
        // left for the `…`, which is then the third.
        rows.clamp(4);
        assert_eq!(drawn(&rows, 0), "ab…");
        assert_eq!(
            rows.copies()[0],
            RowCopy::Text {
                join: Join::Line,
                pieces: vec![piece(0..2)],
            }
        );
    }

    #[test]
    fn appended_rows_keep_their_code_blocks() {
        let mut code = Rows::new();
        code.blank();
        code.blank();
        code.mark_code(0..2);
        let mut rows = Rows::new();
        rows.blank();
        rows.append(code);
        assert_eq!(rows.code(), std::slice::from_ref(&(1..3)));
        rows.truncate(2);
        assert_eq!(rows.code(), std::slice::from_ref(&(1..2)));
        assert_eq!(rows.copies().len(), 2);
    }

    fn drawn(rows: &Rows, i: usize) -> String {
        super::super::code::line_text(&rows.lines()[i])
    }
}
