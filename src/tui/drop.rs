//! Reading a paste as files dropped onto the terminal.
//!
//! A terminal delivers a drop as a paste of the files' paths, each in its
//! own way: bare, one per line; quoted or backslash-escaped for a shell,
//! separated by spaces; or as `file://` URIs.

use std::path::PathBuf;

/// The files `text` names, when it is nothing but paths of existing files.
/// Only absolute paths, `~/` paths and `file://` URIs count: a bare word
/// that happens to be a file name here is still text.
pub fn files(text: &str) -> Option<Vec<PathBuf>> {
    readings(text)
        .into_iter()
        .find(|paths| !paths.is_empty() && paths.iter().all(|p| p.is_file()))
}

/// The ways to read `text` as a list of paths: a path per line, taken
/// literally (spaces and quotes included), then shell words.
fn readings(text: &str) -> Vec<Vec<PathBuf>> {
    let text = text.trim();
    let lines = text
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(path)
        .collect::<Option<Vec<_>>>();
    let words = words(text).and_then(|words| words.iter().map(|w| path(w)).collect());
    [lines, words].into_iter().flatten().collect()
}

/// One path or `file://` URI, unquoted already.
fn path(text: &str) -> Option<PathBuf> {
    if let Some(rest) = text.strip_prefix("file://") {
        // `file:///path` or `file://host/path`.
        let path = &rest[rest.find('/')?..];
        return Some(PathBuf::from(percent_decode(path)?));
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Some(dirs::home_dir()?.join(rest));
    }
    text.starts_with('/').then(|| PathBuf::from(text))
}

fn percent_decode(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

/// Split as a shell would: quotes group, a backslash takes the next
/// character as it is. `None` when a quote is left open.
fn words(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => out.extend(word.take()),
            '\'' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        c => word.push(c),
                    }
                }
            }
            '\\' => word.get_or_insert_default().push(chars.next()?),
            c => word.get_or_insert_default().push(c),
        }
    }
    out.extend(word);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(paths: &[PathBuf]) -> Vec<&str> {
        paths.iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[test]
    fn reads_the_forms_terminals_paste() {
        // Bare, with a space: only the line reading keeps it whole.
        assert_eq!(strs(&readings("/a/my shot.png\n")[0]), ["/a/my shot.png"]);
        // Shell-quoted with a trailing space (VTE), an escaped quote inside.
        let vte = readings(r"'/a/my shot.png' '/a/it'\''s.pdf' ");
        assert_eq!(strs(vte.last().unwrap()), ["/a/my shot.png", "/a/it's.pdf"]);
        // Backslash-escaped (macOS), double-quoted.
        assert_eq!(
            strs(readings(r#"/a/my\ shot.png "/a/b c.txt""#).last().unwrap()),
            ["/a/my shot.png", "/a/b c.txt"]
        );
        // URIs, one per line, with and without a host.
        assert_eq!(
            strs(&readings("file:///a/my%20shot.png\r\nfile://host/a/%C3%A9.txt")[0]),
            ["/a/my shot.png", "/a/é.txt"]
        );
    }

    #[test]
    fn text_is_not_a_list_of_paths() {
        assert!(readings("look at /a/b.png").is_empty());
        assert!(readings("b.png").is_empty());
        assert!(readings("'/a/b.png").is_empty());
        assert!(readings("file:///a/%zz.png").is_empty());
        assert!(readings("").iter().all(|r| r.is_empty()));
    }

    #[test]
    fn only_existing_files_count() {
        let tmp = tempfile::tempdir().unwrap();
        let shot = tmp.path().join("my shot.png");
        let notes = tmp.path().join("notes.txt");
        std::fs::write(&shot, b"png").unwrap();
        std::fs::write(&notes, b"txt").unwrap();
        let (s, n) = (shot.to_str().unwrap(), notes.to_str().unwrap());

        assert_eq!(files(s), Some(vec![shot.clone()]));
        assert_eq!(
            files(&format!("'{s}' {n} ")),
            Some(vec![shot.clone(), notes.clone()])
        );
        assert_eq!(
            files(&format!("{s}\n{n}\n")),
            Some(vec![shot.clone(), notes.clone()])
        );
        // A directory, a missing file, one of each.
        assert_eq!(files(tmp.path().to_str().unwrap()), None);
        assert_eq!(files(&format!("{n} {n}.missing")), None);
        assert_eq!(files(""), None);
    }
}
