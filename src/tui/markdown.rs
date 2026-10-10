//! Markdown rendering for assistant text. pulldown-cmark parses it
//! (CommonMark with GFM tables, strikethrough and task lists); this turns
//! its events into lines: headers, lists, quotes, inline code, bold,
//! italic, links, fenced code (highlighted, wrapped), tables (columns fitted
//! to the pane) and bare diff hunks, which are cut out before parsing.
//! Widths are measured by grapheme, as ratatui places them.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::code::{code_lines, diff_lines, sanitize};

/// `done` when no more of `text` is coming: a fence it leaves open is
/// then not still streaming.
pub fn render_markdown_to_lines(text: &str, max_width: usize, done: bool) -> Vec<Line<'static>> {
    let (text, code) = close_outdented_fences(sanitize(text));
    let text = html_as_text(&text, &code);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let chunks = split_bare_diffs(&text, &code);
    for (i, &chunk) in chunks.iter().enumerate() {
        let (Chunk::Markdown(part) | Chunk::Diff(part)) = chunk;
        // A blank line between a hunk and the text around it, which
        // neither side draws.
        let start = part.as_ptr() as usize - text.as_ptr() as usize;
        let blank = text[..start].ends_with("\n\n") || part.starts_with('\n');
        if blank && !lines.is_empty() && !is_blank(lines.last()) {
            lines.push(Line::default());
        }
        match chunk {
            // Text before a hunk is all there: a fence it leaves open is
            // ended by the hunk.
            Chunk::Markdown(md) => {
                let done = done || i + 1 < chunks.len();
                lines.extend(Renderer::new(md, max_width, done).run());
            }
            Chunk::Diff(diff) => {
                lines.extend(diff_lines(diff, max_width.saturating_sub(2)).into_lines())
            }
        }
    }
    lines
}

fn is_blank(line: Option<&Line<'_>>) -> bool {
    line.is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
}

#[derive(Debug, Clone, Copy)]
enum Chunk<'a> {
    Markdown(&'a str),
    Diff(&'a str),
}

/// Diff hunks written without a fence, which CommonMark would read as
/// lists, headings and paragraphs: a run of lines from a header (`diff
/// --git`, `--- `, `+++ `, `@@`) on while they start with `+`, `-`, a
/// space or `\`, with an `@@` or `diff --git` among them. Fenced code is
/// left alone: `code` says which lines are.
fn split_bare_diffs<'a>(text: &'a str, code: &[bool]) -> Vec<Chunk<'a>> {
    let lines: Vec<(usize, &str)> = text
        .split_inclusive('\n')
        .scan(0, |at, l| {
            let start = *at;
            *at += l.len();
            Some((start, l.trim_end_matches('\n')))
        })
        .collect();
    let header = |l: &str| {
        l.starts_with("diff --git")
            || l.starts_with("--- ")
            || l.starts_with("+++ ")
            || l.starts_with("@@")
    };
    let hunk_line =
        |l: &str| header(l) || (l.starts_with(['+', '-', ' ', '\\']) && !l.trim().is_empty());
    let mut chunks = Vec::new();
    let mut from = 0;
    // The end of a run already found to be no hunk: the headers inside it
    // start no hunk either.
    let mut prose_until = 0;
    let mut i = 0;
    while i < lines.len() {
        let (start, line) = lines[i];
        if !code.get(i).is_some_and(|c| *c) && i >= prose_until && header(line) {
            let end = lines[i..]
                .iter()
                .position(|(_, l)| !hunk_line(l))
                .map_or(lines.len(), |n| i + n);
            prose_until = end;
            if lines[i..end]
                .iter()
                .any(|(_, l)| l.starts_with("@@") || l.starts_with("diff --git"))
            {
                if start > from {
                    chunks.push(Chunk::Markdown(&text[from..start]));
                }
                let stop = lines.get(end).map_or(text.len(), |(s, _)| *s);
                chunks.push(Chunk::Diff(&text[start..stop]));
                from = stop;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if from < text.len() || chunks.is_empty() {
        chunks.push(Chunk::Markdown(&text[from..]));
    }
    chunks
}

/// A line's text after the quote markers, list markers and indentation
/// in front of it.
fn block_text(line: &str) -> &str {
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start_matches([' ', '>']);
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        let marker = if trimmed.starts_with(['-', '*', '+']) {
            1
        } else if (1..=9).contains(&digits) && trimmed[digits..].starts_with(['.', ')']) {
            digits + 1
        } else {
            0
        };
        let after = &trimmed[marker..];
        if marker > 0 && (after.is_empty() || after.starts_with(' ')) {
            rest = after;
        } else {
            return trimmed;
        }
    }
}

/// A terminal cannot draw HTML, and CommonMark takes a line that starts
/// with a tag for the start of an HTML block, which can run to its closing
/// tag and swallow the markdown after it (`<script> tags are blocked.`).
/// Such a `<` is escaped, so the line is text; autolinks, fenced code (the
/// lines `code` marks) and indented code are left alone, and a tag inside
/// a line is still inline HTML (`<br>`).
fn html_as_text(text: &str, code: &[bool]) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split_inclusive('\n').enumerate() {
        match tag_at(line) {
            Some(at) if !code.get(i).is_some_and(|c| *c) => {
                out.push_str(&line[..at]);
                out.push('\\');
                out.push_str(&line[at..]);
            }
            _ => out.push_str(line),
        }
    }
    out
}

/// Where the tag a line starts with is, unless the line is indented code
/// or the tag an autolink.
fn tag_at(line: &str) -> Option<usize> {
    let rest = block_text(line);
    let at = line.len() - rest.len();
    let indented_code = at >= 4 && line[..at].chars().all(|c| c == ' ');
    (!indented_code && rest.starts_with('<') && !is_autolink(rest)).then_some(at)
}

/// A model often closes a fence in a list item at column 0, which ends the
/// item and opens a fence that swallows the rest of the message: such a
/// line, a bare fence at column 0 of the fence's quotes right after a
/// fence its item ended, is indented to close that fence. Returns the text
/// and which of its lines are fenced code, as pulldown-cmark reads them
/// once `html_as_text` has run: a list item or a quote ends a fence, which
/// a line scanner cannot tell.
fn close_outdented_fences(mut text: String) -> (String, Vec<bool>) {
    // Each move parses the text again, and this runs on every render of
    // a streaming reply: past this many, the rest are read as CommonMark
    // has them.
    const MOST: usize = 64;
    // Lines up to here are settled, so each line is moved at most once.
    let mut after = 0;
    for moved in 0.. {
        let starts: Vec<usize> = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .filter(|i| *i < text.len())
            .collect();
        let line_of = |at: usize| starts.partition_point(|s| *s <= at).saturating_sub(1);
        let line = |i: usize| {
            let end = starts.get(i + 1).copied().unwrap_or(text.len());
            text[starts[i]..end].trim_end_matches('\n')
        };
        let blocks = fenced_blocks(&text);
        let mut code = vec![false; starts.len()];
        let mut fix = None;
        for b in blocks {
            let (first, last) = (line_of(b.range.start), line_of(b.range.end - 1));
            code[first..=last].fill(true);
            let next = last + 1;
            if fix.is_none()
                && b.in_item
                && next > after
                && next < starts.len()
                && !fence_closed(&text[b.range], b.quotes)
            {
                fix =
                    closing_indent(line(first), line(next), b.quotes).map(|indent| (next, indent));
            }
        }
        let Some((i, indent)) = fix.filter(|_| moved < MOST) else {
            return (text, code);
        };
        let at = line(i).len() - line(i).trim_start_matches([' ', '>']).len();
        text.replace_range(starts[i]..starts[i] + at, &indent);
        after = i;
    }
    unreachable!()
}

/// What goes in front of `line` for it to close the fence `opener` opens
/// in `quotes` quotes: the opener's quote markers and indentation, its list
/// markers as spaces. None unless `line` is a bare fence at column 0 of
/// those quotes.
fn closing_indent(opener: &str, line: &str, quotes: usize) -> Option<String> {
    let fence = block_text(opener);
    let c = fence.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let n = fence.chars().take_while(|x| *x == c).count();
    let rest = unquote(line, quotes)?;
    let rest = if quotes > 0 {
        rest.strip_prefix(' ').unwrap_or(rest)
    } else {
        rest
    };
    is_bare_fence(rest, c, n).then(|| {
        opener[..opener.len() - fence.len()]
            .chars()
            .map(|x| if x == '>' { x } else { ' ' })
            .collect()
    })
}

struct Fenced {
    range: Range<usize>,
    in_item: bool,
    quotes: usize,
}

/// The fenced code blocks of `text`, with the tag lines `html_as_text`
/// escapes read as the text they become.
fn fenced_blocks(text: &str) -> Vec<Fenced> {
    let mut plain = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match tag_at(line) {
            // A letter in place of the `<`, so that offsets hold.
            Some(at) => {
                plain.push_str(&line[..at]);
                plain.push('x');
                plain.push_str(&line[at + 1..]);
            }
            None => plain.push_str(line),
        }
    }
    let mut blocks = Vec::new();
    let (mut items, mut quotes) = (0usize, 0usize);
    let mut fenced = false;
    for (event, range) in Parser::new_ext(&plain, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::Item) => items += 1,
            Event::End(TagEnd::Item) => items = items.saturating_sub(1),
            Event::Start(Tag::BlockQuote(_)) => quotes += 1,
            Event::End(TagEnd::BlockQuote(_)) => quotes = quotes.saturating_sub(1),
            Event::Start(Tag::CodeBlock(kind)) => {
                fenced = matches!(kind, CodeBlockKind::Fenced(_));
            }
            Event::End(TagEnd::CodeBlock) if fenced && !range.is_empty() => {
                blocks.push(Fenced {
                    range,
                    in_item: items > 0,
                    quotes,
                });
            }
            _ => {}
        }
    }
    blocks
}

/// `<https://…>` or `<name@host>` at the start of `text`.
fn is_autolink(text: &str) -> bool {
    text[1..].split_once('>').is_some_and(|(inner, _)| {
        !inner.contains(' ') && (inner.contains(':') || inner.contains('@'))
    })
}

/// What the lines being drawn are inside of, outermost first.
enum Container {
    Quote,
    /// The number of its next item, for an ordered list.
    List(Option<u64>),
    /// `fresh` until the item's first line is out, which carries the marker.
    Item {
        marker: String,
        fresh: bool,
    },
}

/// A table cell's lines (a `<br>` starts one), each styled runs.
type Cell = Vec<Vec<Span<'static>>>;

struct TableBuild {
    aligns: Vec<Alignment>,
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
    in_head: bool,
}

struct CodeBuild {
    label: String,
    fenced: bool,
    text: String,
}

struct Renderer<'a> {
    source: &'a str,
    width: usize,
    /// No more of the source is coming.
    done: bool,
    out: Vec<Line<'static>>,
    containers: Vec<Container>,
    /// The inline text of the block being read; a `\n` in it breaks the
    /// line.
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// Open links: their target and where their text starts in `inline`.
    links: Vec<(String, usize)>,
    code: Option<CodeBuild>,
    table: Option<TableBuild>,
    /// Where the last block ended, or the container the next one is in
    /// started, in the source: a blank line between there and the next
    /// block is drawn.
    cursor: Option<usize>,
}

const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);
const CODE: Style = Style::new().fg(Color::Yellow).bg(Color::Rgb(35, 38, 48));
const DIM: Style = Style::new().fg(Color::DarkGray);

impl<'a> Renderer<'a> {
    fn new(source: &'a str, width: usize, done: bool) -> Self {
        Renderer {
            source,
            width,
            done,
            out: Vec::new(),
            containers: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            links: Vec::new(),
            code: None,
            table: None,
            cursor: None,
        }
    }

    fn run(mut self) -> Vec<Line<'static>> {
        for (event, range) in Parser::new_ext(self.source, OPTIONS).into_offset_iter() {
            match event {
                Event::Start(tag) => self.start(tag, range.start),
                Event::End(tag) => self.end(tag, range),
                Event::Text(t) => match &mut self.code {
                    Some(code) => code.text.push_str(&t),
                    None => self.text(&t, self.style()),
                },
                Event::Code(t) => self.text(&t, self.style().patch(CODE)),
                Event::InlineHtml(h) | Event::Html(h) if is_br(&h) => self.text("\n", self.style()),
                Event::InlineHtml(h) | Event::Html(h) => self.text(&h, self.style()),
                // Lines stay as they were written, as in a terminal.
                Event::SoftBreak | Event::HardBreak => self.text("\n", self.style()),
                Event::TaskListMarker(done) => {
                    self.text(if done { "[x] " } else { "[ ] " }, self.style())
                }
                Event::FootnoteReference(r) => self.text(&format!("[^{r}]"), self.style()),
                Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t, self.style()),
                Event::Rule => {
                    self.block_start(range.start);
                    let width = self.avail().min(40);
                    self.push_line(vec![Span::styled("─".repeat(width), DIM)]);
                    self.block_end(range.end);
                }
            }
        }
        self.flush();
        self.out
    }

    fn start(&mut self, tag: Tag<'_>, at: usize) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.block_start(at),
            Tag::Heading { level, .. } => {
                self.block_start(at);
                let style = heading_style(level);
                self.styles.push(style);
                self.text(&format!("{} ", "#".repeat(level as usize)), style);
            }
            Tag::BlockQuote(_) => {
                self.block_start(at);
                self.cursor = Some(at);
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.block_start(at);
                let (label, fenced) = match kind {
                    CodeBlockKind::Fenced(info) => (
                        info.split_whitespace().next().unwrap_or("").to_lowercase(),
                        true,
                    ),
                    CodeBlockKind::Indented => (String::new(), false),
                };
                self.code = Some(CodeBuild {
                    label,
                    fenced,
                    text: String::new(),
                });
            }
            Tag::List(first) => {
                self.block_start(at);
                self.cursor = Some(at);
                self.containers.push(Container::List(first));
            }
            Tag::Item => {
                self.block_start(at);
                let marker = match self.containers.last_mut() {
                    Some(Container::List(Some(n))) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "• ".to_string(),
                };
                self.cursor = Some(at);
                self.containers.push(Container::Item {
                    marker,
                    fresh: true,
                });
            }
            Tag::Table(aligns) => {
                self.block_start(at);
                self.table = Some(TableBuild {
                    aligns,
                    header: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                }
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.styles
                    .push(Style::new().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.styles
                    .push(Style::new().add_modifier(Modifier::UNDERLINED));
                self.links.push((dest_url.to_string(), self.inline.len()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd, range: Range<usize>) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::Item => {
                self.flush();
                // An empty item (or one whose text is still on its way)
                // is its marker alone.
                if let Some(Container::Item { fresh: true, .. }) = self.containers.last() {
                    let mut line = self.prefix();
                    trim_end(&mut line);
                    self.out.push(Line::from(line));
                }
                self.containers.pop();
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    // Not closed yet and nothing after it: still
                    // streaming. One its container ended is complete, as
                    // is one the message ended.
                    let rest = self.source[range.end..]
                        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '>');
                    let closed = !code.fenced
                        || self.done
                        || !rest.is_empty()
                        || fence_closed(&self.source[range.clone()], self.quotes());
                    self.code_block(code, closed);
                }
            }
            TagEnd::TableCell => {
                let spans = std::mem::take(&mut self.inline);
                if let Some(t) = &mut self.table {
                    let bold = t.in_head.then(|| Style::new().add_modifier(Modifier::BOLD));
                    t.row.push(split_lines(spans, bold));
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.header = std::mem::take(&mut t.row);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.table_block(t);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                if let Some((url, from)) = self.links.pop() {
                    let text: String = self.inline[from..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    // Nothing is clickable here, so the target is shown
                    // unless the text already is it.
                    if !url.is_empty() && url.trim_start_matches("mailto:") != text {
                        self.text(&format!(" ({url})"), DIM);
                    }
                }
            }
            _ => {}
        }
        if is_block(&tag) {
            self.block_end(range.end);
        }
    }

    /// A block begins at `at`: the text before it is drawn, and a blank
    /// line goes in where the source had one.
    fn block_start(&mut self, at: usize) {
        self.flush();
        let Some(from) = self.cursor.filter(|from| *from <= at) else {
            return;
        };
        // A line with nothing on it but quote markers, between two others.
        let gap: Vec<&str> = self.source[from..at].split('\n').collect();
        let blank = gap.len() > 2
            && gap[1..gap.len() - 1].iter().any(|l| {
                l.trim_matches(|c: char| c.is_whitespace() || c == '>')
                    .is_empty()
            });
        if blank && !self.out.is_empty() && !is_blank(self.out.last()) {
            self.out.push(Line::default());
        }
    }

    /// A block's range takes the line end after it, and in a quote the
    /// marker of the next line: the cursor goes where its text ends.
    fn block_end(&mut self, end: usize) {
        let text = self.source[..end].trim_end_matches(|c: char| c.is_whitespace() || c == '>');
        self.cursor = Some(text.len());
    }

    /// How many quotes the block being read is in.
    fn quotes(&self) -> usize {
        self.containers
            .iter()
            .filter(|c| matches!(c, Container::Quote))
            .count()
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, s| acc.patch(*s))
    }

    fn text(&mut self, text: &str, style: Style) {
        if !text.is_empty() {
            self.inline.push(Span::styled(text.to_string(), style));
        }
    }

    /// Cells the line prefix takes: the margin, quote bars, item markers.
    fn prefix_width(&self) -> usize {
        2 + self
            .containers
            .iter()
            .map(|c| match c {
                Container::Quote => 2,
                Container::List(_) => 0,
                Container::Item { marker, .. } => text_width(marker),
            })
            .sum::<usize>()
    }

    fn avail(&self) -> usize {
        self.width.saturating_sub(self.prefix_width()).max(1)
    }

    /// The margin, a bar per quote and the item markers: an item's marker
    /// on its first line, spaces as wide on the others.
    fn prefix(&mut self) -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw("  ")];
        for c in &mut self.containers {
            match c {
                Container::Quote => spans.push(Span::styled("│ ", DIM)),
                Container::List(_) => {}
                Container::Item { marker, fresh } => {
                    if std::mem::take(fresh) {
                        spans.push(Span::styled(
                            marker.clone(),
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        spans.push(Span::raw(" ".repeat(text_width(marker))));
                    }
                }
            }
        }
        spans
    }

    fn push_line(&mut self, content: Vec<Span<'static>>) {
        let mut line = self.prefix();
        line.extend(content);
        self.out.push(Line::from(line));
    }

    /// A line drawn elsewhere with its own two-cell margin (code), moved in
    /// by the containers it is inside.
    fn push_drawn(&mut self, line: Line<'static>) {
        if self.containers.is_empty() {
            self.out.push(line);
            return;
        }
        let mut spans = line.spans;
        if let Some(first) = spans.first_mut()
            && let Some(rest) = first.content.strip_prefix("  ")
        {
            first.content = rest.to_string().into();
        }
        self.push_line(spans);
    }

    /// Draws the inline text read so far, wrapped to the room left.
    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        let quoted = self
            .containers
            .iter()
            .any(|c| matches!(c, Container::Quote));
        let base = if quoted {
            Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC)
        } else {
            Style::default()
        };
        let mut lines = split_lines(spans, None);
        while lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
            lines.pop();
        }
        for line in lines {
            let line: Vec<Span<'static>> = line
                .into_iter()
                .map(|s| Span::styled(s.content, base.patch(s.style)))
                .collect();
            for wrapped in wrap_styled(&line, self.avail()) {
                self.push_line(wrapped);
            }
        }
    }

    fn code_block(&mut self, code: CodeBuild, closed: bool) {
        let label = if code.label.is_empty() {
            "code"
        } else {
            code.label.as_str()
        };
        let avail = self.width.saturating_sub(self.prefix_width());
        self.push_line(vec![
            Span::styled("┌─ ", DIM),
            Span::styled(
                label.to_string(),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {}",
                    "─".repeat(avail.saturating_sub(6 + text_width(label)).min(34))
                ),
                DIM,
            ),
        ]);
        for line in code_lines(&code.text, &code.label, avail).into_lines() {
            self.push_drawn(line);
        }
        let end = if closed {
            Span::styled(
                format!("└{}", "─".repeat(avail.saturating_sub(2).min(40))),
                DIM,
            )
        } else {
            Span::styled("┆ …", DIM.add_modifier(Modifier::ITALIC))
        };
        self.push_line(vec![end]);
    }

    fn table_block(&mut self, t: TableBuild) {
        let lines = table_lines(&t, self.avail()).unwrap_or_else(|| {
            // Too narrow for a few cells a column: each row as a line of
            // text, its cells between pipes.
            std::iter::once(&t.header)
                .chain(&t.rows)
                .flat_map(|row| {
                    let mut spans = Vec::new();
                    for (c, cell) in row.iter().enumerate() {
                        if c > 0 {
                            spans.push(Span::styled(" | ", DIM));
                        }
                        for (i, line) in cell.iter().enumerate() {
                            if i > 0 {
                                spans.push(Span::raw(" "));
                            }
                            spans.extend(line.iter().cloned());
                        }
                    }
                    wrap_styled(&spans, self.avail())
                })
                .collect()
        });
        for line in lines {
            self.push_line(line);
        }
    }
}

fn is_block(tag: &TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::Table
    )
}

fn is_br(html: &str) -> bool {
    let tag: String = html
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    matches!(tag.as_str(), "<br>" | "<br/>")
}

/// Whether a fenced block's source ends with its closing fence; one that
/// does not is still streaming, or was ended by its list item or quote.
/// The source starts at the opening fence, and its other lines with the
/// markers of the `quotes` quotes it is in.
fn fence_closed(source: &str, quotes: usize) -> bool {
    let mut lines = source.trim_end().lines();
    let (Some(opener), Some(last)) = (lines.next(), lines.last()) else {
        return false;
    };
    let Some(c) = opener.chars().next().filter(|c| matches!(c, '`' | '~')) else {
        return false;
    };
    let n = opener.chars().take_while(|x| *x == c).count();
    unquote(last, quotes).is_some_and(|rest| is_bare_fence(rest.trim_start_matches(' '), c, n))
}

/// `line` after the markers of `quotes` quotes, if it has as many.
fn unquote(line: &str, quotes: usize) -> Option<&str> {
    (0..quotes).try_fold(line, |rest, _| {
        rest.trim_start_matches(' ').strip_prefix('>')
    })
}

/// `c` at least `n` times and nothing else: a closing fence.
fn is_bare_fence(text: &str, c: char, n: usize) -> bool {
    let text = text.trim_end();
    text.len() >= n && text.chars().all(|x| x == c)
}

fn heading_style(level: HeadingLevel) -> Style {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 => bold.fg(Color::Yellow).add_modifier(Modifier::UNDERLINED),
        HeadingLevel::H2 => bold.fg(Color::Cyan),
        _ => bold.fg(Color::Magenta),
    }
}

/// Styled runs split into lines at their `\n`s, `extra` patched over each.
fn split_lines(spans: Vec<Span<'static>>, extra: Option<Style>) -> Vec<Vec<Span<'static>>> {
    let mut lines = vec![Vec::new()];
    for span in spans {
        let style = extra.map_or(span.style, |e| span.style.patch(e));
        for (i, part) in span.content.split('\n').enumerate() {
            if i > 0 {
                lines.push(Vec::new());
            }
            if !part.is_empty()
                && let Some(line) = lines.last_mut()
            {
                line.push(Span::styled(part.to_string(), style));
            }
        }
    }
    lines
}

/// Cells a string takes, grapheme by grapheme as ratatui places it: an
/// emoji with a variation selector or joined by ZWJ is one wide grapheme,
/// not its code points' widths added up.
fn text_width(s: &str) -> usize {
    s.graphemes(true).map(UnicodeWidthStr::width).sum()
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| text_width(&s.content)).sum()
}

/// Word wrap of styled text to `width` cells. Spaces between words on a
/// line are kept as written (inside inline code, say); where a line breaks
/// they are dropped. A word wider than a line is broken by grapheme.
/// Always at least one (maybe empty) line.
fn wrap_styled(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let graphemes: Vec<(&str, Style, usize)> = spans
        .iter()
        .flat_map(|s| {
            s.content
                .graphemes(true)
                .map(move |g| (g, s.style, UnicodeWidthStr::width(g)))
        })
        .collect();
    let mut lines: Vec<Vec<(&str, Style)>> = Vec::new();
    let mut line: Vec<(&str, Style)> = Vec::new();
    let mut used = 0;
    let mut i = 0;
    while i < graphemes.len() {
        let gap_start = i;
        while i < graphemes.len() && graphemes[i].0 == " " {
            i += 1;
        }
        let word_start = i;
        while i < graphemes.len() && graphemes[i].0 != " " {
            i += 1;
        }
        let (gap, word) = (&graphemes[gap_start..word_start], &graphemes[word_start..i]);
        if word.is_empty() {
            break;
        }
        let gap_width: usize = gap.iter().map(|g| g.2).sum();
        let word_width: usize = word.iter().map(|g| g.2).sum();
        if used + gap_width + word_width <= width {
            line.extend(gap.iter().chain(word).map(|g| (g.0, g.1)));
            used += gap_width + word_width;
            continue;
        }
        if used > 0 {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for &(g, style, w) in word {
            if used > 0 && used + w > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push((g, style));
            used += w;
        }
    }
    lines.push(line);
    lines
        .into_iter()
        .map(|l| {
            let mut out: Vec<Span<'static>> = Vec::new();
            for (g, style) in l {
                match out.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push_str(g),
                    _ => out.push(Span::styled(g.to_string(), style)),
                }
            }
            out
        })
        .collect()
}

/// A table's lines in `avail` cells: columns as wide as their content when
/// that fits, else the widest narrowed first and each cell wrapped in its
/// column, with a rule under the header and, when a row wraps, between
/// rows. `None` when there is no room for a few cells a column.
fn table_lines(t: &TableBuild, avail: usize) -> Option<Vec<Vec<Span<'static>>>> {
    let n = t.aligns.len().max(1);
    let fill = |row: &[Cell]| -> Vec<Cell> {
        let mut row = row.to_vec();
        row.resize(n, vec![Vec::new()]);
        row
    };
    let header = fill(&t.header);
    let rows: Vec<Vec<Cell>> = t.rows.iter().map(|r| fill(r)).collect();
    let room = avail.checked_sub(3 * (n - 1))?;
    if room < 3 * n {
        return None;
    }
    let natural: Vec<usize> = (0..n)
        .map(|c| {
            std::iter::once(&header)
                .chain(&rows)
                .flat_map(|r| r[c].iter().map(|l| spans_width(l)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let widths = fit_columns(&natural, room);
    let wrap = |row: &[Cell]| -> Vec<Cell> {
        row.iter()
            .zip(&widths)
            .map(|(cell, w)| cell.iter().flat_map(|l| wrap_styled(l, *w)).collect())
            .collect()
    };
    let header = wrap(&header);
    let rows: Vec<Vec<Cell>> = rows.iter().map(|r| wrap(r)).collect();
    let rule = vec![Span::styled(
        widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("─┼─"),
        DIM,
    )];
    // Rows that wrap run into each other without a rule between them.
    let ruled = rows.iter().any(|r| r.iter().any(|c| c.len() > 1));
    let mut out = row_lines(&header, &widths, &t.aligns);
    out.push(rule.clone());
    for (i, row) in rows.iter().enumerate() {
        if ruled && i > 0 {
            out.push(rule.clone());
        }
        out.extend(row_lines(row, &widths, &t.aligns));
    }
    Some(out)
}

fn row_lines(row: &[Cell], widths: &[usize], aligns: &[Alignment]) -> Vec<Vec<Span<'static>>> {
    let height = row.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|k| {
            let mut spans = Vec::new();
            for (c, cell) in row.iter().enumerate() {
                if c > 0 {
                    spans.push(Span::styled(" │ ", DIM));
                }
                let content = cell.get(k).cloned().unwrap_or_default();
                let pad = widths[c].saturating_sub(spans_width(&content));
                let (left, right) = match aligns.get(c) {
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    Some(Alignment::Right) => (pad, 0),
                    _ => (0, pad),
                };
                spans.push(Span::raw(" ".repeat(left)));
                spans.extend(content);
                spans.push(Span::raw(" ".repeat(right)));
            }
            trim_end(&mut spans);
            spans
        })
        .collect()
}

/// Drops the spaces a row ends with, so that a copy has no padding at the
/// end of each line.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::code::line_text;

    #[test]
    fn test_render_headers() {
        let lines = render_markdown_to_lines("# Title\n## Sub\n### Deep", 80, false);
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0]).contains("# Title"));
    }

    #[test]
    fn test_render_diff() {
        let lines = render_markdown_to_lines("```diff\n-old\n+new\n```", 80, false);
        // fence header, two diff lines, fence footer
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Green));
    }

    #[test]
    fn test_fenced_code_is_highlighted_and_wrapped_not_truncated() {
        let long = format!("let s = \"{}\";", "a".repeat(120));
        let lines = render_markdown_to_lines(&format!("```rust\n{long}\n```"), 60, false);
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
    fn inline_code_bold_italic_and_links() {
        let lines = render_markdown_to_lines(
            "use `cargo test` and **now** or *later*, see [docs](https://d.example) or <https://u.example>",
            200,
            false,
        );
        let spans = &lines[0].spans;
        assert!(
            spans
                .iter()
                .any(|s| s.content == "cargo test" && s.style.bg.is_some())
        );
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
        assert!(
            line_text(&lines[0]).ends_with("see docs (https://d.example) or https://u.example"),
            "{:?}",
            line_text(&lines[0])
        );
        // Spaces inside inline code are kept as written; a copy gets them.
        let lines = render_markdown_to_lines("set `x  =  1` now", 80, false);
        assert_eq!(line_text(&lines[0]), "  set x  =  1 now");
    }

    #[test]
    fn blocks_keep_the_blank_lines_and_line_breaks_written() {
        assert_eq!(
            table_rows("first\nsecond\n\n> quoted\n\n---\n\n1. a\n2. b", 80),
            [
                "  first",
                "  second",
                "",
                "  │ quoted",
                "",
                "  ────────────────────────────────────────",
                "",
                "  1. a",
                "  2. b",
            ]
        );
    }

    #[test]
    fn a_tight_item_keeps_its_code_and_sublist_close() {
        assert_eq!(
            table_rows(
                "Steps:\n\n1. Fix:\n   ```sh\n   make\n   ```\n2. Then\n   - sub\n\n- loose\n\n- items",
                60
            ),
            [
                "  Steps:",
                "",
                "  1. Fix:",
                "     ┌─ sh ──────────────────────────────────",
                "     │ make",
                "     └────────────────────────────────────────",
                "  2. Then",
                "     • sub",
                "",
                "  • loose",
                "",
                "  • items",
            ]
        );
    }

    #[test]
    fn a_bare_hunk_is_a_diff_and_a_dash_list_is_not() {
        let lines = render_markdown_to_lines(
            "Change:\n\n@@ -1 +1 @@\n-old\n+new\n\nDone.\n- item",
            80,
            false,
        );
        let rows: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(rows[0], "  Change:");
        assert_eq!(rows[1], "");
        let old = lines
            .iter()
            .position(|l| line_text(l).contains("old"))
            .unwrap();
        assert!(
            lines[old]
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Red))
        );
        assert!(rows.contains(&"  Done.".to_string()), "{rows:?}");
        assert!(rows.contains(&"  • item".to_string()), "{rows:?}");
        // Without a hunk header a line of dashes is prose.
        assert_eq!(table_rows("-x and +y", 80), ["  -x and +y"]);
    }

    #[test]
    fn test_lists_and_streaming_fence() {
        let lines =
            render_markdown_to_lines("- one\n  - nested\n1. first\n```py\nprint(1)", 80, false);
        assert!(line_text(&lines[0]).contains("• one"));
        assert!(line_text(&lines[1]).contains("• nested"));
        assert!(line_text(&lines[2]).contains("1. first"));
        assert!(line_text(lines.last().unwrap()).contains("…"));
        // Ended by its list item and quote, not streaming: in a list item
        // or a quote that runs to the end it still is.
        for text in [
            "- item\n  ```bash\n  ls\n\nPlease confirm.",
            "> ```\n> ls\n\nPlease confirm.",
        ] {
            let rows = table_rows(text, 80);
            assert!(!rows.iter().any(|r| r.contains('…')), "{rows:?}");
            assert!(rows.iter().any(|r| r.contains('└')), "{rows:?}");
            assert_eq!(rows.last().unwrap(), "  Please confirm.");
        }
        // A no-break space is no blank line to CommonMark: its paragraph
        // ends the item.
        let rows = table_rows("- item\n  ```\n  ls\n\u{a0}", 80);
        assert!(!rows.iter().any(|r| r.contains('…')), "{rows:?}");
        for text in ["- item\n  ```bash\n  ls\n", "> ```\n> ls\n>"] {
            let lines = render_markdown_to_lines(text, 80, false);
            assert!(line_text(lines.last().unwrap()).contains("…"), "{text:?}");
        }
    }

    #[test]
    fn a_fence_its_list_item_ends_is_complete() {
        // The reply of #93: the plain fence closes the outer block, the
        // last one opens an empty block the paragraph ends.
        let rows = table_rows(
            "- **Description:**\n  ```markdown\n  ### Command\n  ```bash\n  some-command --flag\n  ```\n  - result line\n  ```\n\nPlease confirm.",
            80,
        );
        assert!(!rows.iter().any(|r| r.contains('…')), "{rows:?}");
        assert_eq!(
            rows.iter().filter(|r| r.contains('└')).count(),
            2,
            "{rows:?}"
        );
        assert_eq!(rows.last().unwrap(), "  Please confirm.");
    }

    #[test]
    fn a_fence_closed_at_column_0_in_a_list_item_closes_it() {
        let rows = table_rows("1. Run:\n   ```bash\n   ls\n```\n\nDone.", 80);
        assert_eq!(
            rows,
            [
                "  1. Run:",
                "     ┌─ bash ──────────────────────────────────",
                "     │ ls",
                "     └────────────────────────────────────────",
                "",
                "  Done.",
            ]
        );
        // Also in a quote, with the next line right after it, and as the
        // last line while it streams.
        for text in [
            "> - Run:\n>   ```\n>   ls\n> ```\nDone.",
            "1. Run:\n   ```bash\n   ls\n```",
        ] {
            let rows = table_rows(text, 80);
            assert_eq!(
                rows.iter().filter(|r| r.contains('┌')).count(),
                1,
                "{rows:?}"
            );
            assert!(!rows.iter().any(|r| r.contains('…')), "{rows:?}");
        }
        // A fence with an info string opens a block of its own, and one
        // at the item's indentation is read as before.
        let rows = table_rows("1. Run:\n   ```bash\n   ls\n```sh\npwd\n```", 80);
        assert!(rows.iter().any(|r| r.contains("─ sh ")), "{rows:?}");
        assert!(rows.iter().any(|r| r.ends_with("│ pwd")), "{rows:?}");
        let rows = table_rows("1. Run:\n   ```bash\n   ls\n   ```\n```\nx", 80);
        assert_eq!(
            rows.iter().filter(|r| r.contains('┌')).count(),
            2,
            "{rows:?}"
        );
    }

    #[test]
    fn at_most_64_fences_are_moved() {
        // Past them, the fence at column 0 opens a block of its own.
        let step = "1. Run:\n   ```\n   ls\n```\n";
        let opened = |n: usize| {
            let rows = table_rows(&step.repeat(n), 80);
            rows.iter().filter(|r| r.starts_with("  ┌")).count()
        };
        assert_eq!(opened(64), 0);
        assert_eq!(opened(65), 1);
    }

    #[test]
    fn only_a_bare_fence_at_column_0_of_its_quotes_is_moved() {
        // An item's first line, a two-digit number, a nested list, a
        // longer fence: each closed by the fence at column 0.
        for text in [
            "- ```bash\n  ls\n```\nDone.",
            "10. Run:\n    ```bash\n    ls\n```\nDone.",
            "- a\n  - Run:\n    ```\n    ls\n```\nDone.",
            "- Run:\n  ```\n  ls\n`````\nDone.",
        ] {
            let rows = table_rows(text, 80);
            assert_eq!(
                rows.iter().filter(|r| r.contains('┌')).count(),
                1,
                "{rows:?}"
            );
            assert_eq!(rows.last().unwrap(), "  Done.", "{rows:?}");
        }
        // A quote's fence, and one in the outer item of a nested list,
        // stay where they are.
        let rows = table_rows("- a\n  ```\n  x\n> ```\n> quoted", 80);
        assert!(rows.iter().any(|r| r.starts_with("  │ ┌")), "{rows:?}");
        let rows = table_rows(
            "- outer\n  - inner\n    ```\n    x\n  ```\n  y\n  ```\nEnd.",
            80,
        );
        assert!(rows.iter().any(|r| r.contains("│ y")), "{rows:?}");
        // A last code line that reads as a fence in a list or a quote does
        // not close the block.
        for line in ["- ```", "1. ```", "> ```"] {
            let rows = table_rows(&format!("- a\n  ```\n  {line}\n```\nDone."), 80);
            assert_eq!(rows.last().unwrap(), "  Done.", "{rows:?}");
        }
    }

    #[test]
    fn a_fence_its_container_ended_leaves_the_rest_markdown() {
        // A tag line is escaped and a bare hunk cut out after it, as with
        // no fence before them.
        let tail =
            "Then:\n\n<script> tags are blocked.\n\n**bold** after.\n\n@@ -1 +1 @@\n-x\n+y\n\nEnd.";
        let alone = table_rows(tail, 80);
        assert!(alone.contains(&"  <script> tags are blocked.".to_string()));
        assert!(alone.contains(&"  │ - x".to_string()), "{alone:?}");
        for fence in ["- item\n  ```\n  ls\n\n", "> ```\n> ls\n\n"] {
            let rows = table_rows(&format!("{fence}{tail}"), 80);
            assert!(rows.ends_with(&alone), "{rows:?}");
        }
    }

    #[test]
    fn a_fence_a_bare_hunk_ends_is_complete() {
        for fence in ["- a\n  ```\n  x\n", "> ```\n> x\n"] {
            let rows = table_rows(&format!("{fence}@@ -1 +1 @@\n-x\n+y\n\nDone."), 80);
            assert!(!rows.iter().any(|r| r.contains('…')), "{rows:?}");
            assert!(rows.contains(&"  │ - x".to_string()), "{rows:?}");
        }
    }

    #[test]
    fn a_fence_left_open_in_a_finished_message_is_complete() {
        let rows: Vec<String> = render_markdown_to_lines("```py\nprint(1)", 80, true)
            .iter()
            .map(line_text)
            .collect();
        assert!(rows.last().unwrap().contains('└'), "{rows:?}");
    }

    fn table_rows(text: &str, width: usize) -> Vec<String> {
        render_markdown_to_lines(text, width, false)
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
        let lines = render_markdown_to_lines("| a | b |\n|---|---|\n| 1 | 2 |", 80, false);
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
        // Narrow: words are broken, nothing goes past the edge.
        let rows = table_rows(text, 14);
        assert!(rows.iter().all(|r| text_width(r) <= 14), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("│ First")), "{rows:#?}");
        // No room for a few cells a column: the table as written.
        let many = format!(
            "|{}\n|{}\n|{}",
            "a|".repeat(12),
            "-|".repeat(12),
            "1|".repeat(12)
        );
        let rows = table_rows(&many, 40);
        assert!(rows.iter().all(|r| text_width(r) <= 40), "{rows:#?}");
        assert_eq!(rows[0], "  a | a | a | a | a | a | a | a | a | a");
        assert_eq!(
            table_rows("> |a|b|c|d|\n> |-|-|-|-|\n> |1|2|3|4|", 20),
            ["  │ a | b | c | d", "  │ 1 | 2 | 3 | 4"]
        );
    }

    #[test]
    fn emoji_in_cells_are_measured_as_drawn() {
        // A variation selector and a ZWJ sequence: two cells each.
        let rows = table_rows("| k | v |\n|---|---|\n| a | ⚠️ warn ⚠️ |", 14);
        assert!(rows.iter().all(|r| text_width(r) <= 14), "{rows:#?}");
        let rows = table_rows(
            "| who | ok |\n|---|---|\n| 👨‍👩‍👧 family | ✅ ok |\n| me | no |",
            80,
        );
        assert_eq!(
            rows,
            [
                "  who       │ ok",
                "  ──────────┼──────",
                "  👨‍👩‍👧 family │ ✅ ok",
                "  me        │ no",
            ]
        );
    }

    #[test]
    fn tables_in_quotes_and_lists_are_tables() {
        let rows = table_rows("> | a | b |\n> |---|---|\n> | 1 | 2 |", 80);
        assert_eq!(rows, ["  │ a │ b", "  │ ──┼──", "  │ 1 │ 2"]);
        let rows = table_rows("- item\n\n  | a | b |\n  |---|---|\n  | 1 | 2 |", 80);
        assert_eq!(
            rows,
            ["  • item", "", "    a │ b", "    ──┼──", "    1 │ 2"]
        );
    }

    #[test]
    fn table_cells_keep_inline_code_and_escaped_pipes() {
        let lines = render_markdown_to_lines(
            "| Where | Note |\n|---|---|\n| `form.tsx:177-192` | `a \\| b` and **bold** |",
            80,
            false,
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
    fn quoted_code_closes_and_quoted_lists_keep_their_blank_lines() {
        let rows = table_rows("> ```\n> x\n> ```", 40);
        assert!(rows.last().unwrap().contains('└'), "{rows:?}");
        let rows = table_rows("> - item\n>   ```\n>   code\n>   ```", 40);
        assert!(rows.last().unwrap().contains('└'), "{rows:?}");
        assert_eq!(
            table_rows("> - a\n>\n> - b\n>\n> after", 40),
            ["  │ • a", "", "  │ • b", "", "  │ after"]
        );
    }

    #[test]
    fn a_line_that_starts_with_a_tag_is_text() {
        assert_eq!(
            table_rows(
                "<script> tags are blocked.\n\n## Fix\n\n- **a** item\n\n```js\nx()\n```",
                40
            )[..5],
            [
                "  <script> tags are blocked.",
                "",
                "  ## Fix",
                "",
                "  • a item"
            ]
        );
        let lines =
            render_markdown_to_lines("<li> elements need a key.\nSo **add** one.", 40, false);
        assert_eq!(line_text(&lines[0]), "  <li> elements need a key.");
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.content == "add" && s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert_eq!(
            table_rows("> <div> in a quote", 40),
            ["  │ <div> in a quote"]
        );
        // An autolink is still a link, and code is left as written.
        assert_eq!(
            table_rows("<https://x.example> docs", 40),
            ["  https://x.example docs"]
        );
        assert_eq!(table_rows("```html\n<p>\n```", 40)[1], "  │ <p>");
    }

    #[test]
    fn an_empty_item_keeps_its_marker() {
        assert_eq!(
            table_rows("1. a\n2.\n3. c", 40),
            ["  1. a", "  2.", "  3. c"]
        );
        // Streaming: the marker is there before the text.
        assert_eq!(table_rows("- a\n-", 40), ["  • a", "  •"]);
    }

    #[test]
    fn a_longer_fence_holds_a_shorter_one() {
        let rows = table_rows("````md\n```diff\n@@ -1 +1 @@\n-a\n+b\n```\n````", 40);
        assert!(rows[0].contains("┌─ md"), "{rows:?}");
        assert!(rows.last().unwrap().contains('└'), "{rows:?}");
        assert_eq!(
            rows.iter().filter(|r| r.contains('┌')).count(),
            1,
            "{rows:?}"
        );
    }

    #[test]
    fn br_inside_inline_code_is_code() {
        let rows = table_rows("| a |\n|---|\n| `x<br>y` |", 80);
        assert_eq!(rows[2], "  x<br>y");
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
