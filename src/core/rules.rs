//! Allow rules: what the user has said never needs asking again.
//!
//! A rule names a tool in terms that are the same on every harness (`shell`,
//! `edit`, `read`, `mcp`) plus a pattern on its input, so a rule granted while
//! one harness was active answers the next one's requests too. Each parser
//! says what a request does (`ToolAction`); matching happens here.
//!
//! Rules are kept under the user's config directory, globally and per
//! workspace, never in the workspace and never in a vendor's settings: an
//! agent that could write them would be granting itself permissions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::core::checkpoints::project_key;

/// What a tool call does, as far as rules are concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAction {
    /// A shell command line, without the `sh -c` a harness wraps it in.
    Shell {
        command: String,
    },
    /// Files created or changed.
    Edit {
        paths: Vec<PathBuf>,
    },
    Read {
        path: PathBuf,
    },
    Mcp {
        server: String,
        tool: String,
    },
    /// Anything else the harness has a name of its own for; matched by that
    /// name, which another harness will not share.
    Other,
    /// Nothing a rule could hold on to: the request is always shown.
    Opaque,
}

impl ToolAction {
    /// For fixture summaries.
    pub fn summary(&self) -> String {
        match self {
            ToolAction::Shell { command } => format!("shell {command:?}"),
            ToolAction::Edit { paths } => format!(
                "edit {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            ToolAction::Read { path } => format!("read {}", path.display()),
            ToolAction::Mcp { server, tool } => format!("mcp {server}/{tool}"),
            ToolAction::Other => "other".to_string(),
            ToolAction::Opaque => "opaque".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// `shell`, `edit`, `read`, `mcp`, or a harness's own name for a tool.
    pub tool: String,
    /// `shell`: the words the command starts with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `edit` and `read`: a glob (`*`, `?`, `**`); relative to the workspace
    /// unless it starts with `/` or `~/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `mcp`: `server/tool`, or `server/*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    #[serde(default)]
    allow: Vec<Rule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Workspace,
    Global,
}

impl Rule {
    fn new(tool: &str) -> Self {
        Rule {
            tool: tool.to_string(),
            command: None,
            path: None,
            name: None,
        }
    }

    pub fn shell(command: impl Into<String>) -> Self {
        Rule {
            command: Some(command.into()),
            ..Self::new("shell")
        }
    }

    pub fn edit(path: impl Into<String>) -> Self {
        Rule {
            path: Some(path.into()),
            ..Self::new("edit")
        }
    }

    pub fn read(path: impl Into<String>) -> Self {
        Rule {
            path: Some(path.into()),
            ..Self::new("read")
        }
    }

    pub fn mcp(name: impl Into<String>) -> Self {
        Rule {
            name: Some(name.into()),
            ..Self::new("mcp")
        }
    }

    /// Every use of a tool, by the harness's own name for it.
    pub fn tool(name: &str) -> Self {
        Self::new(name)
    }

    /// The part of the rule a user would narrow or widen, if it has one.
    pub fn pattern(&self) -> Option<&str> {
        match self.tool.as_str() {
            "shell" => self.command.as_deref(),
            "edit" | "read" => self.path.as_deref(),
            _ => None,
        }
    }

    pub fn with_pattern(&self, pattern: &str) -> Self {
        let mut rule = self.clone();
        match self.tool.as_str() {
            "shell" => rule.command = Some(pattern.to_string()),
            "edit" | "read" => rule.path = Some(pattern.to_string()),
            _ => {}
        }
        rule
    }

    /// Each tool takes exactly its own pattern. A rule that is missing one
    /// would allow everything, and one with a stray field would look
    /// narrower than it is.
    pub fn validate(&self) -> Result<()> {
        let fields = (
            self.command.as_deref(),
            self.path.as_deref(),
            self.name.as_deref(),
        );
        match (self.tool.as_str(), fields) {
            ("", _) => bail!("a rule needs a `tool`"),
            ("shell", (Some(command), None, None)) => match shell_segments(command).as_deref() {
                Some([words]) if !words.is_empty() => Ok(()),
                _ => bail!("`command = {command:?}` must be the first words of one plain command"),
            },
            ("shell", _) => bail!("a `shell` rule takes a `command` and nothing else"),
            ("edit" | "read", (None, Some(path), None)) if !path.is_empty() => Ok(()),
            (tool @ ("edit" | "read"), _) => {
                bail!("a `{tool}` rule takes a `path` and nothing else")
            }
            ("mcp", (None, None, Some(name))) => match name.split_once('/') {
                Some((server, tool)) if !server.is_empty() && !tool.is_empty() => Ok(()),
                _ => bail!("`name = {name:?}` must be `server/tool` or `server/*`"),
            },
            ("mcp", _) => bail!("an `mcp` rule takes a `name` and nothing else"),
            (_, (None, None, None)) => Ok(()),
            (tool, _) => bail!("a rule for `{tool}` takes no pattern"),
        }
    }

    /// What the rule covers, to finish "always allow …".
    pub fn describe(&self) -> String {
        let pattern = self
            .command
            .as_deref()
            .or(self.path.as_deref())
            .or(self.name.as_deref())
            .unwrap_or_default();
        match self.tool.as_str() {
            "shell" => format!("shell commands starting with `{pattern}`"),
            "edit" => format!("edits to `{pattern}`"),
            "read" => format!("reads of `{pattern}`"),
            "mcp" => match pattern.strip_suffix("/*") {
                Some(server) => format!("every tool of the MCP server `{server}`"),
                None => format!("the MCP tool `{pattern}`"),
            },
            tool => format!("every use of `{tool}`"),
        }
    }
}

/// The directories patterns and paths are relative to.
#[derive(Debug, Clone, Copy)]
struct Dirs<'a> {
    /// Where a relative pattern starts: the workspace, else the session's
    /// directory.
    base: &'a Path,
    cwd: &'a Path,
}

/// The rule that allows a request, if the rules do. A command line of
/// several commands, or an edit of several files, may take a rule for each:
/// the first one is returned.
fn find_allowing<'r>(
    rules: &[&'r Rule],
    tool: &str,
    action: &ToolAction,
    dirs: Dirs,
) -> Option<&'r Rule> {
    let of = |name: &'static str| rules.iter().copied().filter(move |r| r.tool == name);
    fn each<'r, T>(
        parts: &[T],
        mut rule_for: impl FnMut(&T) -> Option<&'r Rule>,
    ) -> Option<&'r Rule> {
        let mut first = None;
        for part in parts {
            let rule = rule_for(part)?;
            first.get_or_insert(rule);
        }
        first
    }
    match action {
        ToolAction::Shell { command } => each(&shell_segments(command)?, |segment| {
            of("shell").find(|r| shell_rule_covers(r, segment))
        }),
        ToolAction::Edit { paths } => each(paths, |path| {
            of("edit").find(|r| path_rule_covers(r, path, dirs))
        }),
        ToolAction::Read { path } => of("read").find(|r| path_rule_covers(r, path, dirs)),
        ToolAction::Mcp { server, tool } => of("mcp").find(|r| {
            r.name
                .as_deref()
                .and_then(|name| name.split_once('/'))
                .is_some_and(|(s, t)| s == server && (t == "*" || t == tool))
        }),
        ToolAction::Other => rules.iter().copied().find(|r| {
            r.tool == tool
                && (r.command.as_ref(), r.path.as_ref(), r.name.as_ref()) == (None, None, None)
        }),
        ToolAction::Opaque => None,
    }
}

fn shell_rule_covers(rule: &Rule, segment: &[String]) -> bool {
    let prefix = rule.command.as_deref().and_then(shell_segments);
    match prefix.as_deref() {
        Some([words]) => !words.is_empty() && segment.starts_with(words),
        _ => false,
    }
}

fn path_rule_covers(rule: &Rule, path: &Path, dirs: Dirs) -> bool {
    let (Some(pattern), Some(path)) = (rule.path.as_deref(), real_path(path, dirs.cwd)) else {
        return false;
    };
    let pattern = if pattern.starts_with('/') {
        PathBuf::from(pattern)
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => return false,
        }
    } else {
        dirs.base
            .canonicalize()
            .unwrap_or_else(|_| dirs.base.to_path_buf())
            .join(pattern)
    };
    let pattern = pattern.to_string_lossy().into_owned();
    let path = path.to_string_lossy().into_owned();
    let segments = |s: &str| -> Vec<Vec<char>> {
        s.split('/')
            .filter(|p| !p.is_empty())
            .map(|p| p.chars().collect())
            .collect()
    };
    glob_match(&segments(&pattern), &segments(&path))
}

/// The file a path names once symbolic links are followed, so that a link
/// inside an allowed directory does not carry the rule to wherever it
/// points. The path need not exist yet: what does exist of it is resolved.
/// `None` when that cannot be told (`..` after a missing directory).
fn real_path(path: &Path, cwd: &Path) -> Option<PathBuf> {
    let mut head = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut tail = Vec::new();
    loop {
        if let Ok(real) = head.canonicalize() {
            return Some(tail.iter().rev().fold(real, |p, name| p.join(name)));
        }
        tail.push(head.file_name()?.to_owned());
        if !head.pop() {
            return None;
        }
    }
}

fn glob_match(pattern: &[Vec<char>], path: &[Vec<char>]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first.iter().collect::<String>() == "**" => {
            (0..=path.len()).any(|skip| glob_match(rest, &path[skip..]))
        }
        Some((first, rest)) => path
            .split_first()
            .is_some_and(|(name, tail)| segment_match(first, name) && glob_match(rest, tail)),
    }
}

/// `*` and `?` within one path component.
fn segment_match(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|skip| segment_match(rest, &name[skip..])),
        Some((c, rest)) => name
            .split_first()
            .is_some_and(|(n, tail)| (*c == '?' || c == n) && segment_match(rest, tail)),
    }
}

/// The simple commands of a command line, each as its words.
///
/// `None` when the line uses anything whose effect cannot be read off its
/// words: command or process substitution, redirection, subshells, comments,
/// `$'…'`, an open quote. Such a command is never covered by a rule. This
/// errs on the side of asking; it is not a shell parser.
pub fn shell_segments(command: &str) -> Option<Vec<Vec<String>>> {
    let mut segments = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '`' => return None,
                        '$' if chars.peek() == Some(&'(') => return None,
                        '\\' => match chars.next()? {
                            '\n' => {}
                            c @ ('"' | '\\' | '$' | '`') => w.push(c),
                            c => {
                                w.push('\\');
                                w.push(c);
                            }
                        },
                        c => w.push(c),
                    }
                }
            }
            '\\' => match chars.next()? {
                '\n' => {}
                c => word.get_or_insert_with(String::new).push(c),
            },
            '$' if matches!(chars.peek(), Some('(' | '\'' | '"')) => return None,
            '`' | '<' | '>' | '(' | ')' => return None,
            '#' if word.is_none() => return None,
            ';' | '&' | '|' | '\n' => {
                words.extend(word.take());
                if !words.is_empty() {
                    segments.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => words.extend(word.take()),
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word.take());
    if !words.is_empty() {
        segments.push(words);
    }
    Some(segments)
}

/// A command line without the shell a harness runs it through
/// (`/bin/zsh -lc '<command>'`): rules are about the command.
pub fn unwrap_shell(command: &str) -> String {
    if let Some([words]) = shell_segments(command).as_deref()
        && let [shell, flag, inner] = words.as_slice()
        && matches!(
            shell.rsplit('/').next(),
            Some("sh" | "bash" | "zsh" | "dash" | "ksh" | "fish")
        )
        && matches!(flag.as_str(), "-c" | "-lc" | "-ic" | "-lic")
    {
        return inner.clone();
    }
    command.to_string()
}

/// A word as it has to be written for `shell_segments` to read it back.
fn shell_quote(word: &str) -> String {
    let plain = |c: char| c.is_alphanumeric() || "_-./=:,@%+~".contains(c);
    if !word.is_empty() && word.chars().all(plain) && !word.starts_with('#') {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The rules "allow always" would write for a request: enough to cover it,
/// and not much more. Empty when no rule can cover it.
pub fn propose(tool: &str, action: &ToolAction, root: Option<&Path>, cwd: &Path) -> Vec<Rule> {
    let base = root.unwrap_or(cwd);
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let path_pattern = |path: &Path| -> Option<String> {
        let real = real_path(path, cwd)?;
        let (inside, rel) = match real.strip_prefix(&base) {
            Ok(rel) => (true, rel.to_path_buf()),
            Err(_) => (false, real),
        };
        Some(match rel.parent() {
            // A file at the top of the workspace: a rule for its directory
            // would be a rule for everything.
            Some(dir) if inside && dir.as_os_str().is_empty() => rel.display().to_string(),
            Some(dir) => format!("{}/**", dir.display()).replace("//", "/"),
            None => rel.display().to_string(),
        })
    };
    let mut rules: Vec<Rule> = match action {
        ToolAction::Shell { command } => shell_segments(command)
            .unwrap_or_default()
            .iter()
            .map(|words| {
                // `cargo test`, `git status`: the second word is the command
                // that matters. Otherwise the whole command, to be shortened
                // by whoever knows what it does.
                let subcommand = words.get(1).is_some_and(|w| {
                    w.starts_with(|c: char| c.is_ascii_lowercase())
                        && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                });
                let keep = if subcommand { 2 } else { words.len() };
                let quoted: Vec<String> = words[..keep].iter().map(|w| shell_quote(w)).collect();
                Rule::shell(quoted.join(" "))
            })
            .collect(),
        ToolAction::Edit { paths } => {
            let patterns: Option<Vec<String>> = paths.iter().map(|p| path_pattern(p)).collect();
            patterns
                .unwrap_or_default()
                .into_iter()
                .map(Rule::edit)
                .collect()
        }
        ToolAction::Read { path } => path_pattern(path).map(Rule::read).into_iter().collect(),
        ToolAction::Mcp { server, tool } => vec![Rule::mcp(format!("{server}/{tool}"))],
        ToolAction::Other => vec![Rule::tool(tool)],
        ToolAction::Opaque => Vec::new(),
    };
    let mut seen = Vec::new();
    rules.retain(|r| {
        let new = !seen.contains(r);
        seen.push(r.clone());
        new
    });
    rules.retain(|r| r.validate().is_ok());
    rules
}

/// The user's allow rules, global and for one workspace.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    /// `<config dir>/unharness`; `None` when there is nowhere to keep rules.
    dir: Option<PathBuf>,
    root: Option<PathBuf>,
    global: Vec<Rule>,
    workspace: Vec<Rule>,
}

const HEADER: &str = "# Allow rules: permission requests unharness answers without asking.\n\
# \"Allow always\" appends to this file; it is yours to edit.\n";

impl Rules {
    /// Where rules are kept: `<config dir>/unharness`.
    pub fn default_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("unharness"))
    }

    pub fn load(root: Option<&Path>) -> Result<Self> {
        match Self::default_dir() {
            Some(dir) => Self::load_in(&dir, root),
            None => Ok(Rules {
                root: root.map(Path::to_path_buf),
                ..Rules::default()
            }),
        }
    }

    /// A rules file that does not parse is an error, like a config file:
    /// carrying on without it would silently change what gets asked.
    pub fn load_in(dir: &Path, root: Option<&Path>) -> Result<Self> {
        let mut rules = Rules {
            dir: Some(dir.to_path_buf()),
            root: root.map(Path::to_path_buf),
            ..Rules::default()
        };
        rules.global = read_file(&rules.path(Scope::Global).expect("has a directory"))?;
        if let Some(path) = rules.path(Scope::Workspace) {
            rules.workspace = read_file(&path)?;
        }
        Ok(rules)
    }

    /// The file a scope's rules are in; `None` for the workspace's when
    /// there is no workspace.
    pub fn path(&self, scope: Scope) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        match scope {
            Scope::Global => Some(dir.join("allow.toml")),
            Scope::Workspace => {
                let root = self.root.as_ref()?;
                let root = root.canonicalize().unwrap_or_else(|_| root.clone());
                Some(
                    dir.join("workspaces")
                        .join(format!("{}.allow.toml", project_key(&root))),
                )
            }
        }
    }

    pub fn has_workspace(&self) -> bool {
        self.root.is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (Scope, &Rule)> {
        let workspace = self.workspace.iter().map(|r| (Scope::Workspace, r));
        workspace.chain(self.global.iter().map(|r| (Scope::Global, r)))
    }

    fn dirs<'a>(&'a self, cwd: &'a Path) -> Dirs<'a> {
        Dirs {
            base: self.root.as_deref().unwrap_or(cwd),
            cwd,
        }
    }

    /// The rule that allows a request, if one does. `cwd` is the session's
    /// directory, which relative paths in a request are relative to.
    pub fn allows(&self, tool: &str, action: &ToolAction, cwd: &Path) -> Option<&Rule> {
        let rules: Vec<&Rule> = self.iter().map(|(_, r)| r).collect();
        find_allowing(&rules, tool, action, self.dirs(cwd))
    }

    /// The rules "allow always" would write for a request.
    pub fn propose(&self, tool: &str, action: &ToolAction, cwd: &Path) -> Vec<Rule> {
        propose(tool, action, self.root.as_deref(), cwd)
    }

    /// Whether these rules, once added, would cover the request.
    pub fn would_allow(&self, new: &[Rule], tool: &str, action: &ToolAction, cwd: &Path) -> bool {
        let mut with = self.clone();
        with.workspace.extend(new.iter().cloned());
        with.allows(tool, action, cwd).is_some()
    }

    /// Add rules to a scope's file and to the rules in force. The file is
    /// read again first (another unharness may have written to it) and
    /// appended to as text, so comments in it survive.
    pub fn append(&mut self, scope: Scope, new: &[Rule]) -> Result<PathBuf> {
        let path = match (self.path(scope), scope) {
            (Some(path), _) => path,
            (None, Scope::Workspace) => bail!("there is no workspace to keep rules for"),
            (None, Scope::Global) => bail!("could not determine the user's config directory"),
        };
        for rule in new {
            rule.validate()?;
        }
        let existing = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        let mut rules = parse(&existing, &path)?;
        let added: Vec<Rule> = new.iter().fold(Vec::new(), |mut added, rule| {
            if !rules.contains(rule) && !added.contains(rule) {
                added.push(rule.clone());
            }
            added
        });
        if !added.is_empty() {
            let mut content = if existing.is_empty() {
                HEADER.to_string()
            } else {
                existing
            };
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push('\n');
            content.push_str(&toml::to_string(&RuleFile {
                allow: added.clone(),
            })?);
            // What is about to be written has to read back.
            parse(&content, &path)?;
            write_atomic(&path, &content)
                .with_context(|| format!("could not write {}", path.display()))?;
            rules.extend(added);
        }
        match scope {
            Scope::Global => self.global = rules,
            Scope::Workspace => self.workspace = rules,
        }
        Ok(path)
    }
}

fn parse(content: &str, path: &Path) -> Result<Vec<Rule>> {
    let file: RuleFile = toml::from_str(content)
        .with_context(|| format!("{} is not a valid file of allow rules", path.display()))?;
    for (i, rule) in file.allow.iter().enumerate() {
        rule.validate()
            .with_context(|| format!("{}: rule {}", path.display(), i + 1))?;
    }
    Ok(file.allow)
}

fn read_file(path: &Path) -> Result<Vec<Rule>> {
    match std::fs::read_to_string(path) {
        Ok(content) => parse(&content, path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension(format!("toml.{}.partial", std::process::id()));
    std::fs::write(&partial, content)?;
    std::fs::rename(&partial, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(command: &str) -> ToolAction {
        ToolAction::Shell {
            command: command.to_string(),
        }
    }

    fn rules_with(dir: &Path, root: &Path, rules: &[Rule]) -> Rules {
        let mut r = Rules::load_in(dir, Some(root)).unwrap();
        r.append(Scope::Workspace, rules).unwrap();
        r
    }

    #[test]
    fn shell_commands_are_split_into_their_words() {
        let words = |s: &str| shell_segments(s).unwrap();
        assert_eq!(words("cargo test --all"), [["cargo", "test", "--all"]]);
        assert_eq!(
            words("git commit -m 'a; b' && git  push"),
            [vec!["git", "commit", "-m", "a; b"], vec!["git", "push"]]
        );
        assert_eq!(
            words(r#"echo "a \"b\" $HOME" c\ d"#),
            [["echo", "a \"b\" $HOME", "c d"]]
        );
        assert_eq!(words("a | b; c &\nd"), [["a"], ["b"], ["c"], ["d"]]);
        assert_eq!(words("echo ''"), [["echo", ""]]);
        assert_eq!(words("  "), Vec::<Vec<String>>::new());
    }

    #[test]
    fn the_shell_a_command_is_run_through_is_taken_off() {
        assert_eq!(unwrap_shell("/usr/bin/zsh -lc 'echo ok'"), "echo ok");
        assert_eq!(
            unwrap_shell(r#"bash -c 'echo '\''a b'\'' && ls'"#),
            "echo 'a b' && ls"
        );
        assert_eq!(unwrap_shell("cargo test"), "cargo test");
        // Not just a wrapper: left as it is, and no rule will cover it.
        for command in [
            "zsh -lc 'echo ok' && rm -rf x",
            "zsh -lc 'echo ok' extra",
            "zsh -lc \"echo $(date)\"",
            "env -c x",
        ] {
            assert_eq!(unwrap_shell(command), command);
        }
    }

    #[test]
    fn commands_that_hide_what_they_run_are_not_split() {
        for command in [
            "echo $(rm -rf x)",
            "echo \"$(rm -rf x)\"",
            "echo `rm -rf x`",
            "echo \"`rm -rf x`\"",
            "cargo test > /etc/passwd",
            "cat < secret",
            "diff <(a) b",
            "(cd x; rm y)",
            "cargo test # 'a\nrm -rf x\n# b'",
            "echo $'a\\'; rm -rf x; echo \\''",
            "echo 'open",
            "echo \"open",
            "echo trailing\\",
        ] {
            assert_eq!(shell_segments(command), None, "{command}");
        }
    }

    #[test]
    fn a_shell_rule_is_a_prefix_of_whole_words_for_every_command_on_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let rules = rules_with(
            dir.path(),
            dir.path(),
            &[Rule::shell("cargo test"), Rule::shell("git status")],
        );
        let allowed = |c: &str| rules.allows("Bash", &shell(c), dir.path()).is_some();
        assert!(allowed("cargo test"));
        assert!(allowed("cargo  test --all 'x y'"));
        assert!(allowed("cargo test && git status -s"));
        assert!(!allowed("cargo testing"));
        assert!(!allowed("cargo"));
        assert!(!allowed("cargo build"));
        assert!(!allowed("cargo test && rm -rf x"));
        assert!(!allowed("cargo test; rm -rf x"));
        assert!(!allowed("cargo test | sh"));
        assert!(!allowed("cargo test $(rm -rf x)"));
        assert!(!allowed("cargo test > x"));
        assert!(!allowed("FOO=1 cargo test"));
        assert!(!allowed(""));
        // The tool's name on the harness does not matter, what it does does.
        assert!(
            rules
                .allows("shell", &shell("cargo test"), dir.path())
                .is_some()
        );
        assert!(
            rules
                .allows("Bash", &ToolAction::Other, dir.path())
                .is_none()
        );
    }

    #[test]
    fn path_rules_are_globs_under_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("src/tui")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        let rules = rules_with(
            dir.path(),
            &root,
            &[
                Rule::edit("src/**"),
                Rule::edit("*.md"),
                Rule::read("/etc/host*"),
            ],
        );
        let edit = |paths: &[&str]| ToolAction::Edit {
            paths: paths.iter().map(|p| root.join(p)).collect(),
        };
        let allowed = |a: &ToolAction| rules.allows("Write", a, &root).is_some();
        assert!(allowed(&edit(&["src/main.rs"])));
        assert!(allowed(&edit(&["src/tui/new/deep.rs", "README.md"])));
        assert!(!allowed(&edit(&["docs/a.md"])));
        assert!(!allowed(&edit(&["src/main.rs", "Cargo.toml"])));
        assert!(!allowed(&edit(&[])));
        assert!(!allowed(&edit(&["src/../Cargo.toml"])));
        assert!(!allowed(&edit(&["src/missing/../../Cargo.toml"])));
        // Relative to the session's directory.
        let relative = ToolAction::Edit {
            paths: vec![PathBuf::from("main.rs")],
        };
        assert!(
            rules
                .allows("Write", &relative, &root.join("src"))
                .is_some()
        );
        assert!(
            rules
                .allows("Write", &relative, &root.join("docs"))
                .is_none()
        );
        // An edit rule says nothing about reads, and the other way round.
        let read = |p: &str| ToolAction::Read { path: p.into() };
        assert!(allowed(&read("/etc/hosts")));
        assert!(!allowed(&read("/etc/passwd")));
        assert!(!allowed(&ToolAction::Read {
            path: root.join("src/main.rs")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_does_not_carry_a_path_rule_out_of_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("src/link")).unwrap();
        let rules = rules_with(dir.path(), &root, &[Rule::edit("src/**")]);
        let edit = |p: &str| ToolAction::Edit {
            paths: vec![root.join(p)],
        };
        assert!(rules.allows("Write", &edit("src/a.rs"), &root).is_some());
        assert!(rules.allows("Write", &edit("src/link/a"), &root).is_none());
        assert!(
            rules
                .allows("Write", &edit("src/link/../a"), &root)
                .is_none()
        );
    }

    #[test]
    fn mcp_and_named_tools() {
        let dir = tempfile::tempdir().unwrap();
        let rules = rules_with(
            dir.path(),
            dir.path(),
            &[
                Rule::mcp("probe/magic_word"),
                Rule::mcp("docs/*"),
                Rule::tool("WebFetch"),
            ],
        );
        let mcp = |server: &str, tool: &str| ToolAction::Mcp {
            server: server.into(),
            tool: tool.into(),
        };
        let allowed = |tool: &str, a: &ToolAction| rules.allows(tool, a, dir.path()).is_some();
        assert!(allowed(
            "mcp__probe__magic_word",
            &mcp("probe", "magic_word")
        ));
        assert!(allowed("probe/magic_word", &mcp("probe", "magic_word")));
        assert!(!allowed("x", &mcp("probe", "other")));
        assert!(allowed("x", &mcp("docs", "anything")));
        assert!(allowed("WebFetch", &ToolAction::Other));
        assert!(!allowed("WebSearch", &ToolAction::Other));
        assert!(!allowed("WebFetch", &ToolAction::Opaque));
    }

    #[test]
    fn proposals_cover_the_request_and_little_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let propose = |tool: &str, a: &ToolAction| propose(tool, a, Some(&root), &root);
        assert_eq!(
            propose("Bash", &shell("cargo test --all && ls -la 'my dir'")),
            [Rule::shell("cargo test"), Rule::shell("ls -la 'my dir'")]
        );
        assert_eq!(
            propose("Bash", &shell("git status; git status -s")),
            [Rule::shell("git status")]
        );
        assert_eq!(propose("Bash", &shell("echo $(date)")), []);
        let edit = ToolAction::Edit {
            paths: vec![root.join("src/tui/app.rs"), root.join("Cargo.toml")],
        };
        assert_eq!(
            propose("apply_patch", &edit),
            [Rule::edit("src/tui/**"), Rule::edit("Cargo.toml")]
        );
        let outside = ToolAction::Read {
            path: "/etc/ssl/openssl.cnf".into(),
        };
        assert_eq!(propose("Read", &outside), [Rule::read("/etc/ssl/**")]);
        assert_eq!(
            propose(
                "x",
                &ToolAction::Mcp {
                    server: "probe".into(),
                    tool: "magic_word".into()
                }
            ),
            [Rule::mcp("probe/magic_word")]
        );
        assert_eq!(
            propose("WebFetch", &ToolAction::Other),
            [Rule::tool("WebFetch")]
        );
        assert_eq!(propose("permissions", &ToolAction::Opaque), []);

        // What is proposed allows what it was proposed for.
        let rules = Rules::load_in(dir.path(), Some(&root)).unwrap();
        for (tool, action) in [
            ("Bash", shell("cargo test --all && ls -la 'my dir'")),
            ("apply_patch", edit),
            ("Read", outside),
        ] {
            assert!(rules.allows(tool, &action, &root).is_none());
            let new = propose(tool, &action);
            assert!(rules.would_allow(&new, tool, &action, &root), "{action:?}");
        }
    }

    #[test]
    fn rules_are_appended_per_scope_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let mut rules = Rules::load_in(dir.path(), Some(&root)).unwrap();
        let global = rules
            .append(Scope::Global, &[Rule::shell("git status")])
            .unwrap();
        assert_eq!(global, dir.path().join("allow.toml"));

        // A comment the user wrote, and a rule another unharness added.
        let mut content = std::fs::read_to_string(&global).unwrap();
        content.push_str("\n# mine\n[[allow]]\ntool = \"mcp\"\nname = \"docs/*\"\n");
        std::fs::write(&global, content).unwrap();
        rules
            .append(
                Scope::Global,
                &[Rule::shell("git status"), Rule::edit("docs/**")],
            )
            .unwrap();
        let content = std::fs::read_to_string(&global).unwrap();
        assert!(content.contains("# mine"), "{content}");
        assert_eq!(content.matches("git status").count(), 1, "{content}");

        let workspace = rules
            .append(Scope::Workspace, &[Rule::shell("cargo test")])
            .unwrap();
        assert!(workspace.starts_with(dir.path().join("workspaces")));
        assert!(
            workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".allow.toml")
        );

        let reloaded = Rules::load_in(dir.path(), Some(&root)).unwrap();
        assert_eq!(reloaded, rules);
        assert_eq!(
            reloaded.iter().collect::<Vec<_>>(),
            [
                (Scope::Workspace, &Rule::shell("cargo test")),
                (Scope::Global, &Rule::shell("git status")),
                (Scope::Global, &Rule::mcp("docs/*")),
                (Scope::Global, &Rule::edit("docs/**")),
            ]
        );
        // Another workspace sees only the global ones.
        let other = Rules::load_in(dir.path(), Some(dir.path())).unwrap();
        assert_eq!(other.iter().count(), 3);
        // No partial files are left behind.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("partial"))
            .collect();
        assert_eq!(left, Vec::<String>::new());
    }

    #[test]
    fn without_a_workspace_only_global_rules_can_be_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut rules = Rules::load_in(dir.path(), None).unwrap();
        assert!(!rules.has_workspace());
        assert!(rules.append(Scope::Workspace, &[Rule::tool("x")]).is_err());
        rules.append(Scope::Global, &[Rule::tool("x")]).unwrap();
        assert!(rules.allows("x", &ToolAction::Other, dir.path()).is_some());
    }

    #[test]
    fn a_rules_file_that_does_not_parse_is_an_error_naming_the_file() {
        for content in [
            "allow = 3",
            "[[allow]]\ntool = \"shell\"",
            "[[allow]]\ntool = \"shell\"\ncommand = \"a && b\"",
            "[[allow]]\ntool = \"shell\"\ncommand = \"  \"",
            "[[allow]]\ntool = \"shell\"\ncommand = \"ls\"\npath = \"x\"",
            "[[allow]]\ntool = \"edit\"",
            "[[allow]]\ntool = \"edit\"\ncommand = \"ls\"",
            "[[allow]]\ntool = \"mcp\"\nname = \"probe\"",
            "[[allow]]\ntool = \"WebFetch\"\npath = \"x\"",
            "[[allow]]\ntool = \"shell\"\ncommand = \"ls\"\ncomand = \"x\"",
            "[[deny]]\ntool = \"x\"",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("allow.toml"), content).unwrap();
            let err = Rules::load_in(dir.path(), None).unwrap_err();
            assert!(format!("{err:#}").contains("allow.toml"), "{err:#}");
            // And nothing is appended to a file that cannot be read.
            let mut rules = Rules {
                dir: Some(dir.path().to_path_buf()),
                ..Rules::default()
            };
            assert!(rules.append(Scope::Global, &[Rule::tool("x")]).is_err());
            assert_eq!(
                std::fs::read_to_string(dir.path().join("allow.toml")).unwrap(),
                content
            );
        }
    }

    #[test]
    fn rules_say_what_they_cover() {
        assert_eq!(
            Rule::shell("cargo test").describe(),
            "shell commands starting with `cargo test`"
        );
        assert_eq!(Rule::edit("src/**").describe(), "edits to `src/**`");
        assert_eq!(Rule::read("/etc/**").describe(), "reads of `/etc/**`");
        assert_eq!(
            Rule::mcp("probe/*").describe(),
            "every tool of the MCP server `probe`"
        );
        assert_eq!(Rule::mcp("probe/x").describe(), "the MCP tool `probe/x`");
        assert_eq!(Rule::tool("WebFetch").describe(), "every use of `WebFetch`");
    }
}
