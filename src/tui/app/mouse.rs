//! The mouse: scrolling, selecting and copying in the transcript.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::{App, Scrollbar};
use crate::tui::selection::{self, Granularity, Point, Selection};

/// Lines scrolled per wheel notch, in the transcript and in a dialog's plan
/// or preview.
const WHEEL_LINES: u16 = 3;

/// Presses this close together on one cell count as a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(500);

/// How long a status-rule message stays up.
const FLASH: Duration = Duration::from_secs(2);

impl App {
    /// Mouse input, when the TUI has the mouse. A dialog takes the wheel,
    /// for what it scrolls, and a release ends a drag it cut short.
    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        if let Some(modal) = &mut self.modal {
            match ev.kind {
                MouseEventKind::ScrollUp => {
                    modal.scroll(-i32::from(WHEEL_LINES));
                }
                MouseEventKind::ScrollDown => {
                    modal.scroll(i32::from(WHEEL_LINES));
                }
                // Nothing is copied: the press may have been on the dialog.
                MouseEventKind::Up(MouseButton::Left) => {
                    self.end_drag();
                }
                _ => {}
            }
            return;
        }
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_down(WHEEL_LINES),
            MouseEventKind::Down(MouseButton::Left) => self.mouse_press(ev.column, ev.row),
            MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(ev.column, ev.row),
            MouseEventKind::Up(MouseButton::Left) => self.mouse_release(),
            MouseEventKind::Down(MouseButton::Right) => self.copy_block_at(ev.column, ev.row),
            _ => {}
        }
    }

    /// A right click on a block copies all of it, also what is not shown
    /// ([`super::transcript::block_text`]).
    fn copy_block_at(&mut self, column: u16, row: u16) {
        let Some(point) = self.transcript_point(column, row, false) else {
            return;
        };
        let Some(i) = self
            .transcript_view
            .blocks
            .iter()
            .position(|b| b.top <= point.line && point.line < b.end)
        else {
            return;
        };
        let block = self.shown_blocks().get(i);
        let code = self
            .transcript_view
            .code
            .iter()
            .find(|c| c.block == i && c.rows.contains(&point.line));
        let text = match (code, block) {
            // On a code block in a response, that code; on an empty one,
            // the response.
            (
                Some(code),
                Some(
                    super::transcript::Block::Assistant { text, .. }
                    | super::transcript::Block::Handoff { text, .. },
                ),
            ) => super::markdown::code_blocks(text)
                .into_iter()
                .nth(code.nth)
                .filter(|c| !c.trim().is_empty()),
            _ => None,
        }
        .or_else(|| block.and_then(super::transcript::block_text));
        match text {
            Some(text) => {
                self.selection = None;
                self.copy_request = Some(text);
            }
            None => self.flash("Nothing to copy here"),
        }
    }

    /// The transcript cell under a screen position. With `clamp`, a position
    /// outside the transcript maps to the nearest cell in view.
    fn transcript_point(&self, column: u16, row: u16, clamp: bool) -> Option<Point> {
        let view = &self.transcript_view;
        let area = view.area;
        if area.width == 0 || area.height == 0 || view.lines.is_empty() {
            return None;
        }
        let inside = column >= area.x
            && column < area.x + area.width
            && row >= area.y
            && row < area.y + area.height;
        if !inside && !clamp {
            return None;
        }
        let column = column.clamp(area.x, area.x + area.width - 1);
        let row = row.clamp(area.y, area.y + area.height - 1);
        let line = view.scroll + (row - area.y) as usize;
        if line >= view.lines.len() && !clamp {
            return None;
        }
        Some(Point {
            line: line.min(view.lines.len() - 1),
            col: (column - area.x) as usize,
        })
    }

    /// Scroll so the scrollbar's thumb follows a pointer on `row`.
    fn scrollbar_follow(&mut self, bar: Scrollbar, row: u16, grab: u16) {
        let top = row as i32 - bar.y as i32 - grab as i32;
        // The renderer goes back to following the end if this reaches it.
        self.auto_scroll = false;
        self.scroll = bar.scroll_for(top);
    }

    pub fn scrollbar_held(&self) -> bool {
        self.scrollbar_grab.is_some()
    }

    fn mouse_press(&mut self, column: u16, row: u16) {
        self.drag_edge = 0;
        if self
            .transcript_view
            .jump
            .is_some_and(|r| r.contains((column, row).into()))
        {
            self.scroll_to_bottom();
            return;
        }
        if let Some(bar) = self.transcript_view.scrollbar
            && bar.contains(column, row)
        {
            // On the thumb: hold it where it was grabbed. On the track:
            // bring the thumb's middle to the pointer, then hold that.
            let offset = row - bar.y;
            let on_thumb = offset >= bar.thumb_top && offset < bar.thumb_top + bar.thumb_len;
            let grab = if on_thumb {
                offset - bar.thumb_top
            } else {
                bar.thumb_len / 2
            };
            self.scrollbar_grab = Some(grab);
            self.scrollbar_follow(bar, row, grab);
            return;
        }
        let Some(point) = self.transcript_point(column, row, false) else {
            self.selection = None;
            self.last_click = None;
            return;
        };
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, p, n)) if p == point && now.duration_since(at) < MULTI_CLICK && n < 3 => {
                n + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, point, count));
        self.selection = Some(Selection {
            anchor: point,
            focus: point,
            granularity: match count {
                1 => Granularity::Char,
                2 => Granularity::Word,
                _ => Granularity::Line,
            },
        });
    }

    fn mouse_drag(&mut self, column: u16, row: u16) {
        if let (Some(grab), Some(bar)) = (self.scrollbar_grab, self.transcript_view.scrollbar) {
            self.scrollbar_follow(bar, row, grab);
            return;
        }
        let Some(point) = self.transcript_point(column, row, true) else {
            return;
        };
        let Some(sel) = self.selection.as_mut() else {
            return;
        };
        sel.focus = point;
        let area = self.transcript_view.area;
        self.drag_edge = if row < area.y {
            -1
        } else if row >= area.y + area.height {
            1
        } else {
            0
        };
    }

    /// Stop scrolling at an edge and let go of the scrollbar; true when its
    /// thumb was held.
    fn end_drag(&mut self) -> bool {
        self.drag_edge = 0;
        self.scrollbar_grab.take().is_some()
    }

    fn mouse_release(&mut self) {
        if self.end_drag() {
            return;
        }
        match self.selected_text() {
            Some(text) if !text.trim().is_empty() => self.copy_request = Some(text),
            _ => {
                // A plain click, or only blank cells: nothing is selected.
                if let Some(s) = self
                    .selection
                    .filter(|s| s.granularity == Granularity::Char)
                {
                    self.selection = None;
                    if s.anchor == s.focus {
                        self.toggle_tool_at(s.anchor.line);
                    }
                }
            }
        }
    }

    /// Expand or collapse the tool call whose first row is rendered line
    /// `line` (a click on it).
    fn toggle_tool_at(&mut self, line: usize) -> bool {
        let Some(i) = self
            .transcript_view
            .blocks
            .iter()
            .position(|b| b.top == line && b.top < b.end)
        else {
            return false;
        };
        match self.shown_transcript_mut().blocks.get_mut(i) {
            // A call that spawned a subagent opens that subagent's transcript.
            Some(super::transcript::Block::Tool {
                id, agent: Some(_), ..
            }) => {
                let id = id.clone();
                self.open_subagent(&id);
                true
            }
            Some(super::transcript::Block::Tool { collapsed, .. }) => {
                *collapsed = !*collapsed;
                true
            }
            _ => false,
        }
    }

    /// The selection as first and last cell, for the highlight.
    pub fn selection_range(&self) -> Option<(Point, Point)> {
        self.selection?
            .range(&self.transcript_view.lines, &self.transcript_view.copy)
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        Some(selection::text(
            &self.transcript_view.lines,
            &self.transcript_view.copy,
            start,
            end,
        ))
    }

    #[cfg(test)]
    pub(crate) fn last_click_forget(&mut self) {
        self.last_click = None;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.drag_edge = 0;
    }

    pub fn take_copy_request(&mut self) -> Option<String> {
        self.copy_request.take()
    }

    /// Show `message` in the status rule for a moment.
    pub fn flash(&mut self, message: impl Into<String>) {
        self.flash = Some((message.into(), Instant::now()));
    }

    pub fn flash_text(&self) -> Option<&str> {
        self.flash.as_ref().map(|(m, _)| m.as_str())
    }

    /// Timed mouse work: keep scrolling while a drag is held past an edge,
    /// and take down an expired flash. True when the screen changed.
    pub fn tick_mouse(&mut self) -> bool {
        let mut changed = false;
        if self.drag_edge != 0 && self.selection.is_some() {
            let view = &self.transcript_view;
            let last = view.lines.len().saturating_sub(1);
            let line = if self.drag_edge < 0 {
                view.scroll.saturating_sub(1)
            } else {
                (view.scroll + view.area.height as usize).min(last)
            };
            if self.drag_edge < 0 {
                self.scroll_up(1);
            } else {
                self.scroll_down(1);
            }
            if let Some(sel) = self.selection.as_mut() {
                sel.focus.line = line;
            }
            changed = true;
        }
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= FLASH)
        {
            self.flash = None;
            changed = true;
        }
        changed
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.auto_scroll = false;
        self.scroll = self.scroll.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
    }
}
