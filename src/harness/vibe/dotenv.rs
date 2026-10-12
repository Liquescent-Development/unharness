//! vibe's `$VIBE_HOME/.env`, read as vibe 2.26.1 reads it: python-dotenv
//! 1.2.2's `dotenv_values` (`dotenv/parser.py`, `variables.py`, `main.py`),
//! then `load_dotenv_values` (`vibe/core/config/vibe_schema.py`), which
//! puts each value that is not empty into vibe's environment unless the
//! environment already gives that key one. The file holds the API key:
//! nothing here keeps, copies or shows a value beyond what the caller asks
//! for.

use std::collections::HashMap;

/// A key and its value, `None` for a key with no `=`.
pub type Binding = (String, Option<String>);

/// Python's `\s` and `str.isspace`: Unicode white space and the
/// separators `\x1c` to `\x1f`.
fn space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `[^\S\r\n]`: white space within a line.
fn blank(c: char) -> bool {
    space(c) && c != '\r' && c != '\n'
}

/// python-dotenv's `Reader` over the text.
struct Reader<'a> {
    text: &'a str,
    pos: usize,
}

/// A statement python-dotenv could not parse: it skips the rest of the line.
struct Unparsed;

impl Reader<'_> {
    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Skips the chars `keep` takes, returning them.
    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> &str {
        let start = self.pos;
        let len = self
            .rest()
            .char_indices()
            .find(|&(_, c)| !keep(c))
            .map_or(self.rest().len(), |(i, _)| i);
        self.pos += len;
        &self.text[start..self.pos]
    }

    /// `\r\n`, `\n`, `\r` or the end, after blanks (`_end_of_line`).
    fn end_of_line(&mut self) -> Result<(), Unparsed> {
        self.take_while(blank);
        let rest = self.rest();
        let len = if rest.starts_with("\r\n") {
            2
        } else if rest.starts_with('\n') || rest.starts_with('\r') {
            1
        } else if rest.is_empty() {
            0
        } else {
            return Err(Unparsed);
        };
        self.pos += len;
        Ok(())
    }

    /// The rest of the line after an error (`_rest_of_line`:
    /// `[^\r\n]*(?:\r|\n|\r\n)?`, which takes `\r` alone first).
    fn skip_line(&mut self) {
        self.take_while(|c| c != '\r' && c != '\n');
        if matches!(self.peek(), Some('\r' | '\n')) {
            self.pos += 1;
        }
    }

    /// The content between `quote`s at the reader: `'([^']+)'` for a key
    /// (`escapable` false; the caller refuses an empty one), else
    /// `'((?:\\'|[^'])*)'` with `quote` for `'`: a quote inside is one
    /// after a backslash, and when the text ends before a closing one the
    /// regex backtracks to the last such quote, which then closes it.
    fn quoted(&mut self, quote: char, escapable: bool) -> Result<String, Unparsed> {
        let start = self.pos + 1;
        let body = &self.text[start..];
        let mut i = 0;
        let mut last_escaped = None;
        let end = loop {
            let rest = &body[i..];
            let mut chars = rest.chars();
            match chars.next() {
                Some('\\') if escapable && chars.next() == Some(quote) => {
                    last_escaped = Some(i + 1);
                    i += 2;
                }
                Some(c) if c == quote => break i,
                Some(c) => i += c.len_utf8(),
                None => match last_escaped {
                    Some(at) => break at,
                    None => return Err(Unparsed),
                },
            }
        };
        self.pos = start + end + quote.len_utf8();
        Ok(body[..end].to_string())
    }
}

/// `\\[\\']` in a single-quoted value, `\\[\\'"abfnrtv]` in a double-quoted
/// one, decoded as Python's `unicode-escape` does.
fn decode_escapes(value: &str, double: bool) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let decoded = match chars.peek() {
            Some('\\') => Some('\\'),
            Some('\'') => Some('\''),
            Some('"') if double => Some('"'),
            Some('a') if double => Some('\u{7}'),
            Some('b') if double => Some('\u{8}'),
            Some('f') if double => Some('\u{c}'),
            Some('n') if double => Some('\n'),
            Some('r') if double => Some('\r'),
            Some('t') if double => Some('\t'),
            Some('v') if double => Some('\u{b}'),
            _ => None,
        };
        match decoded {
            Some(d) => {
                chars.next();
                out.push(d);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// An unquoted value: the line, cut before the first white space that
/// leads to a `#` (`re.sub(r"\s+#.*", "", part)`), stripped at the end.
fn unquoted(line: &str) -> String {
    let mut run_start = None;
    let mut cut = line.len();
    for (i, c) in line.char_indices() {
        if c == '#' && run_start.is_some() {
            cut = run_start.unwrap_or(i);
            break;
        }
        if space(c) {
            run_start.get_or_insert(i);
        } else {
            run_start = None;
        }
    }
    line[..cut].trim_end_matches(space).to_string()
}

/// One statement (`parse_binding`): `None` for a blank or comment line, the
/// end, or a statement python-dotenv could not parse.
fn binding(reader: &mut Reader<'_>) -> Option<Binding> {
    reader.take_while(space);
    if reader.rest().is_empty() {
        return None;
    }
    match statement(reader) {
        Ok(found) => found,
        Err(Unparsed) => {
            reader.skip_line();
            None
        }
    }
}

fn statement(reader: &mut Reader<'_>) -> Result<Option<Binding>, Unparsed> {
    // `(?:export[^\S\r\n]+)?`
    if let Some(after) = reader.rest().strip_prefix("export")
        && after.starts_with(blank)
    {
        reader.pos += "export".len();
        reader.take_while(blank);
    }
    let key = match reader.peek() {
        Some('#') => None,
        Some('\'') => {
            let key = reader.quoted('\'', false)?;
            if key.is_empty() {
                return Err(Unparsed);
            }
            Some(key)
        }
        _ => {
            let key = reader.take_while(|c| c != '=' && c != '#' && !space(c));
            if key.is_empty() {
                return Err(Unparsed);
            }
            Some(key.to_string())
        }
    };
    reader.take_while(blank);
    let value = if reader.peek() == Some('=') {
        reader.pos += 1;
        reader.take_while(blank);
        Some(match reader.peek() {
            Some(q @ ('\'' | '"')) => {
                let raw = reader.quoted(q, true)?;
                decode_escapes(&raw, q == '"')
            }
            None | Some('\r' | '\n') => String::new(),
            Some(_) => unquoted(reader.take_while(|c| c != '\r' && c != '\n')),
        })
    } else {
        None
    };
    // `(?:[^\S\r\n]*#[^\r\n]*)?`
    let before = reader.pos;
    reader.take_while(blank);
    if reader.peek() == Some('#') {
        reader.take_while(|c| c != '\r' && c != '\n');
    } else {
        reader.pos = before;
    }
    reader.end_of_line()?;
    Ok(key.map(|k| (k, value)))
}

/// The statements of a dotenv file's text in order, as python-dotenv
/// parses them (`parse_stream`), after Python's text mode has made every
/// `\r\n` and `\r` a `\n`.
pub fn parse(text: &str) -> Vec<Binding> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut reader = Reader {
        text: &text,
        pos: 0,
    };
    let mut out = Vec::new();
    while !reader.rest().is_empty() {
        if let Some(found) = binding(&mut reader) {
            out.push(found);
        }
    }
    out
}

/// `${NAME}` and `${NAME:-default}` in `value` replaced, each looked up
/// with `lookup` (`parse_variables`, `Variable.resolve`): a name that is
/// not there takes the default, else nothing. Linear in the value: where
/// the next `}`, and the next `}` or `:`, is from each byte is found in
/// one pass from the end, so a `${` that starts no variable costs no scan
/// to the end of a value the sandbox can write.
fn interpolate(value: &str, lookup: impl Fn(&str) -> Option<Option<String>>) -> String {
    let bytes = value.as_bytes();
    let len = bytes.len();
    // `next_close[i]`: the first `}` at or after `i`; `next_stop[i]`: the
    // first `}` or `:`; `len` for none. Both are ASCII, so every index is
    // a char boundary.
    let mut next_close = vec![len; len + 1];
    let mut next_stop = vec![len; len + 1];
    for i in (0..len).rev() {
        next_close[i] = if bytes[i] == b'}' {
            i
        } else {
            next_close[i + 1]
        };
        next_stop[i] = if matches!(bytes[i], b'}' | b':') {
            i
        } else {
            next_stop[i + 1]
        };
    }
    let mut out = String::new();
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = value[from..].find("${") {
        let at = from + found;
        let name_start = at + 2;
        let stop = next_stop[name_start];
        let (default, end) = if stop < len && bytes[stop] == b'}' {
            (None, stop + 1)
        } else if stop + 1 < len
            && bytes[stop] == b':'
            && bytes[stop + 1] == b'-'
            && next_close[stop + 2] < len
        {
            let close = next_close[stop + 2];
            (Some(&value[stop + 2..close]), close + 1)
        } else {
            // No variable here: the `$` is text, and the search goes on
            // after it.
            from = at + 1;
            continue;
        };
        out.push_str(&value[copied..at]);
        let resolved = match lookup(&value[name_start..stop]) {
            Some(found) => found.unwrap_or_default(),
            None => default.unwrap_or_default().to_string(),
        };
        out.push_str(&resolved);
        copied = end;
        from = end;
    }
    out.push_str(&value[copied..]);
    out
}

/// What `dotenv_values` gives for the text: each key once, at its first
/// place, with its last value, every value's `${…}` taken from the
/// bindings before it, else from `environ`.
pub fn values(text: &str, environ: &HashMap<String, String>) -> Vec<Binding> {
    let mut out: Vec<Binding> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (key, value) in parse(text) {
        let value = value.map(|v| {
            interpolate(&v, |name| match at.get(name) {
                Some(&i) => Some(out[i].1.clone()),
                None => environ.get(name).map(|v| Some(v.clone())),
            })
        });
        match at.get(&key) {
            Some(&i) => out[i].1 = value,
            None => {
                at.insert(key.clone(), out.len());
                out.push((key, value));
            }
        }
    }
    out
}

/// What vibe adds to its environment `environ` from the text
/// (`load_dotenv_values`): each value that is not empty, for a key
/// `environ` does not give a value that is not empty.
pub fn loaded(text: &str, environ: &HashMap<String, String>) -> Vec<(String, String)> {
    values(text, environ)
        .into_iter()
        .filter_map(|(key, value)| Some((key, value.filter(|v| !v.is_empty())?)))
        .filter(|(key, _)| environ.get(key).is_none_or(String::is_empty))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn b(key: &str, value: Option<&str>) -> Binding {
        (key.to_string(), value.map(str::to_string))
    }

    /// Each case as python-dotenv 1.2.2 parsed it (`parse_stream`, run on
    /// the same text).
    #[test]
    fn statements_parse_as_python_dotenv_parses_them() {
        let cases: &[(&str, Vec<Binding>)] = &[
            ("A=1", vec![b("A", Some("1"))]),
            ("export A=1", vec![b("A", Some("1"))]),
            ("exportA=1", vec![b("exportA", Some("1"))]),
            ("export=1", vec![b("export", Some("1"))]),
            ("  A = 1  ", vec![b("A", Some("1"))]),
            ("A", vec![b("A", None)]),
            ("A=", vec![b("A", Some(""))]),
            ("A= # nothing", vec![b("A", Some("# nothing"))]),
            ("A=b # c", vec![b("A", Some("b"))]),
            ("A=b#c", vec![b("A", Some("b#c"))]),
            ("A=b c\t#d #e", vec![b("A", Some("b c"))]),
            ("A='b' # c", vec![b("A", Some("b"))]),
            ("A='b' c", vec![]),
            ("A='b' c\nB=2", vec![b("B", Some("2"))]),
            (r#"A="x\"y\n""#, vec![b("A", Some("x\"y\n"))]),
            (r"A='x\'y\n'", vec![b("A", Some("x'y\\n"))]),
            (r"A='x\'", vec![b("A", Some("x\\"))]),
            ("A='x", vec![]),
            (
                "A=\"multi\nline\"\nB=2",
                vec![b("A", Some("multi\nline")), b("B", Some("2"))],
            ),
            ("'A B'=1", vec![b("A B", Some("1"))]),
            ("# A=1\n\n  #B=2", vec![]),
            ("A#x=1", vec![b("A", None)]),
            ("=1\nB=2", vec![b("B", Some("2"))]),
            (
                "A=1\r\nB=2\rC=3",
                vec![b("A", Some("1")), b("B", Some("2")), b("C", Some("3"))],
            ),
            (
                "VIBE_TOOLS='{\"web_fetch\": {\"permission\": \"always\"}}'",
                vec![b(
                    "VIBE_TOOLS",
                    Some("{\"web_fetch\": {\"permission\": \"always\"}}"),
                )],
            ),
        ];
        for (text, want) in cases {
            assert_eq!(&parse(text), want, "{text:?}");
        }
    }

    /// `dotenv_values`: the last value of a key, `${…}` from the bindings
    /// before, else the environment, else the default.
    #[test]
    fn values_interpolate_as_python_dotenv_does() {
        let environ = env(&[("HOME_DIR", "/h"), ("EMPTY", "")]);
        let text = "A=1\nB=${A}-${HOME_DIR}\nC='${NONE:-d}${EMPTY:-e}'\nA=2\nD=$A ${ \
                    ${X:y} ${A\nE\nF=${E:-f}";
        assert_eq!(
            values(text, &environ),
            vec![
                b("A", Some("2")),
                b("B", Some("1-/h")),
                b("C", Some("d")),
                b("D", Some("$A ${ ${X:y} ${A")),
                b("E", None),
                b("F", Some("")),
            ]
        );
    }

    /// A value or file the sandbox can write cannot make a read slow: a
    /// MiB of `${` that starts no variable, in the shapes that would have
    /// each scan to the end, is read at once (#212). Each took about 0.1 s
    /// here in a debug build; with a scan per `${` one took 196 s in a
    /// release build. The bound leaves room for a loaded machine.
    #[test]
    fn a_mib_of_dollar_braces_is_read_at_once() {
        const BOUND: std::time::Duration = std::time::Duration::from_secs(5);
        let mib = 1 << 20;
        for unit in ["${", "${a:", "${a:-", "${${a:x"] {
            let value = unit.repeat(mib / unit.len());
            let started = std::time::Instant::now();
            let got = interpolate(&value, |_| None);
            assert!(
                started.elapsed() < BOUND,
                "{unit:?}: {:?}",
                started.elapsed()
            );
            assert_eq!(got, value, "{unit:?}");
        }
        let text = "A='${\\'\n".repeat(mib / 8) + &"B=\"${:x\n".repeat(mib / 8);
        let started = std::time::Instant::now();
        let _ = values(&text, &HashMap::new());
        assert!(started.elapsed() < BOUND, "{:?}", started.elapsed());
    }

    /// `load_dotenv_values`: what the environment gives a value keeps it,
    /// and nothing empty is added.
    #[test]
    fn loading_leaves_what_the_environment_sets() {
        let environ = env(&[("SET", "env"), ("BLANK", "")]);
        let text = "SET=file\nBLANK=file\nNEW=file\nEMPTY=\nNONE\nset=lower";
        assert_eq!(
            loaded(text, &environ),
            vec![
                ("BLANK".to_string(), "file".to_string()),
                ("NEW".to_string(), "file".to_string()),
                ("set".to_string(), "lower".to_string()),
            ]
        );
    }
}
