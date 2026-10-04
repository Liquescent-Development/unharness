//! Text selection in the transcript. With the mouse reported to unharness
//! the terminal no longer selects text itself, so this does: positions are
//! cells of the rendered transcript lines, which stay put while it scrolls.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A cell of a rendered transcript line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// is selected (a plain click that did not move).
    pub fn range(&self, lines: &[String]) -> Option<(Point, Point)> {
        let (a, b) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        let width = |p: Point| lines.get(p.line).map_or(0, |l| l.width());
        match self.granularity {
            Granularity::Char => (a != b).then_some((a, b)),
            Granularity::Word => {
                let word =
                    |p: Point| word_cols(lines.get(p.line).map_or("", String::as_str), p.col);
                Some((
                    Point {
                        line: a.line,
                        col: word(a).0,
                    },
                    Point {
                        line: b.line,
                        col: word(b).1,
                    },
                ))
            }
            Granularity::Line => Some((
                Point {
                    line: a.line,
                    col: 0,
                },
                Point {
                    line: b.line,
                    col: width(b).saturating_sub(1),
                },
            )),
        }
    }
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
/// is not on a word.
pub fn word_cols(line: &str, col: usize) -> (usize, usize) {
    let cells: Vec<(usize, usize, char)> = cells(line).filter(|(_, w, _)| *w > 0).collect();
    let Some(at) = cells.iter().position(|(s, w, _)| col >= *s && col < s + w) else {
        return (col, col);
    };
    if !is_word_char(cells[at].2) {
        return (cells[at].0, cells[at].0 + cells[at].1 - 1);
    }
    let first = (0..=at)
        .rev()
        .take_while(|i| is_word_char(cells[*i].2))
        .last()
        .unwrap_or(at);
    let last = (at..cells.len())
        .take_while(|i| is_word_char(cells[*i].2))
        .last()
        .unwrap_or(at);
    (cells[first].0, cells[last].0 + cells[last].1 - 1)
}

/// The text in cells `from..=to` of one line. A wide char is taken if any
/// of its cells is in range.
fn slice(line: &str, from: usize, to: usize) -> String {
    cells(line)
        .filter(|(start, w, _)| *start <= to && start + w.max(&1) > from)
        .map(|(_, _, c)| c)
        .collect()
}

/// The text between two points, inclusive, one line per transcript row,
/// without the padding at the end of each row.
pub fn text(lines: &[String], start: Point, end: Point) -> String {
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
        s.range(&l).map(|(a, b)| text(&l, a, b))
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
        assert_eq!(word_cols(&lines()[0], 5), (5, 5));
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
}
