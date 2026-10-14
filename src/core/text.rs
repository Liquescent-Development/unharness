//! Text as it is put in a terminal cell grid and kept in the transcript:
//! escape sequences and other control characters out.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

/// Make text safe to put in a terminal cell grid: strip ANSI/OSC escape
/// sequences, expand tabs, drop carriage returns and other control characters
/// (newlines are kept). Anything that moves the cursor on its own corrupts
/// ratatui's diff and leaves artifacts on screen.
pub fn sanitize(text: &str) -> String {
    clean(text, true, None)
}

/// [`sanitize`], with where each tab was in what it returns: the spaces
/// it became.
pub fn sanitize_tabs(text: &str) -> (String, Vec<Range<usize>>) {
    let mut tabs = Vec::new();
    (clean(text, true, Some(&mut tabs)), tabs)
}

/// Text for the clipboard: escape sequences, carriage returns and other
/// control characters out, as [`sanitize`] does, but tabs kept.
pub fn strip_controls(text: &str) -> String {
    clean(text, false, None)
}

fn clean(text: &str, expand_tabs: bool, mut tabs: Option<&mut Vec<Range<usize>>>) -> String {
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
                if let Some(tabs) = &mut tabs {
                    tabs.push(out.len()..out.len() + pad);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_escapes_and_expands_tabs() {
        assert_eq!(sanitize("a\x1b[31mred\x1b[0m b"), "ared b");
        assert_eq!(sanitize("\x1b]0;title\x07x"), "x");
        assert_eq!(sanitize("x\ty\n\tz"), "x   y\n    z");
        assert_eq!(sanitize("line\r\nnext\x07"), "line\nnext");
        assert_eq!(strip_controls("a\x1b[31m\tb\r\n\tc\x07"), "a\tb\n\tc");
        assert_eq!(
            sanitize_tabs("x\ty\n\x1b[1m\tz"),
            ("x   y\n    z".to_string(), vec![1..4, 6..10])
        );
    }
}
