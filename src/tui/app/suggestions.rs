//! The suggestion list under the prompt: slash commands and `@` files.

use std::path::PathBuf;

use super::App;
use super::commands::BASE_COMMANDS;
use crate::core::HarnessCommand;
use crate::tui::files;

/// A harness's command as the `/` list shows it: what it takes, then the
/// first line of what it does.
fn describe_command(harness: &str, c: &HarnessCommand) -> String {
    let what = c.description.lines().next().unwrap_or_default();
    match &c.hint {
        Some(hint) => format!("{harness}: {hint} · {what}"),
        None => format!("{harness}: {what}"),
    }
}

impl App {
    pub fn update_suggestions(&mut self) {
        self.refresh_suggestions(true);
    }

    /// `files` is whether an `@` word may open the list of files: one that
    /// was typed does, one that came in a paste or from the editor does not.
    pub(super) fn refresh_suggestions(&mut self, files: bool) {
        self.suggestions.clear();
        self.suggestions_closed = false;
        let was_completing_file = self.completing_file.take().is_some();
        // `\/name` is the harness's command even where unharness has one.
        let escaped = self.input.starts_with("\\/");
        if !(self.input.starts_with('/') || escaped) || self.input.contains('\n') {
            self.selected_suggestion = 0;
            if files {
                self.suggest_files(was_completing_file);
            }
            return;
        }
        let query = self.input.to_lowercase();
        let query = query.strip_prefix('\\').unwrap_or(&query);
        let (cmd, sub) = match query.split_once(' ') {
            Some((c, s)) => (c.to_string(), Some(s.trim().to_string())),
            None => (query.to_string(), None),
        };
        let mut out = Vec::new();
        if escaped {
            if sub.is_none() {
                out = self.suggest_harness_commands(&cmd, true);
            }
        } else {
            match (cmd.as_str(), sub.as_deref()) {
                ("/switch" | "/harness", Some(s)) => {
                    for o in self.harness_options.iter().filter(|o| o.installed) {
                        let id = o.id.as_str();
                        if id.starts_with(s) {
                            out.push((format!("{cmd} {id}"), o.display_name.to_string()));
                        }
                    }
                }
                ("/policy", Some(s)) => {
                    for p in self.offered_policies() {
                        if p.as_str().starts_with(s) {
                            out.push((format!("/policy {p}"), p.description().to_string()));
                        }
                    }
                }
                ("/sandbox", Some(s)) => {
                    for l in self.available_sandbox_levels() {
                        if l.as_str().starts_with(s) {
                            out.push((format!("/sandbox {l}"), l.description().to_string()));
                        }
                    }
                }
                ("/effort" | "/think", Some(s)) => {
                    for l in self.caps().effort_levels {
                        if l.starts_with(s) {
                            out.push((format!("{cmd} {l}"), "Reasoning effort".to_string()));
                        }
                    }
                }
                ("/model", Some(s)) => {
                    if let Some(p) = self.current_provider().cloned()
                        && let Some(models) = self.cached_models(&p)
                    {
                        for m in models {
                            let id = m.model_ref.model.to_lowercase();
                            if id.contains(s) || m.display_name.to_lowercase().contains(s) {
                                out.push((
                                    format!("/model {}", m.model_ref.model),
                                    m.description.unwrap_or(m.display_name),
                                ));
                            }
                        }
                    }
                }
                ("/resume", Some(s)) => {
                    for r in self.controller.store.list() {
                        if r.id.starts_with(s) || r.title.to_lowercase().contains(s) {
                            out.push((
                                format!("/resume {}", &r.id[..8.min(r.id.len())]),
                                format!("{}  {}", r.updated_at, r.title),
                            ));
                        }
                    }
                }
                (_, None) => {
                    for (c, d) in BASE_COMMANDS {
                        if c.starts_with(cmd.as_str()) && self.offers_command(c) {
                            out.push((c.to_string(), d.to_string()));
                        }
                    }
                    out.extend(self.suggest_harness_commands(&cmd, false));
                }
                _ => {}
            }
        }
        self.suggestions = out;
        if self.selected_suggestion >= self.suggestions.len() {
            self.selected_suggestion = 0;
        }
    }

    /// The active harness's commands that start with `typed` (`/` and
    /// lowercase), where a prompt runs them. Unless `escaped`, those that
    /// unharness has a command of the same name for are left out.
    fn suggest_harness_commands(&self, typed: &str, escaped: bool) -> Vec<(String, String)> {
        if !self.caps().slash_commands {
            return Vec::new();
        }
        let harness = self.short_name();
        self.harness_commands()
            .iter()
            .map(|c| (format!("/{}", c.name), c))
            .filter(|(name, _)| name.to_lowercase().starts_with(typed))
            .filter(|(name, _)| escaped || !self.owns_command(name))
            .map(|(name, c)| {
                let shown = if escaped { format!("\\{name}") } else { name };
                (shown, describe_command(harness, c))
            })
            .collect()
    }

    /// The files matching the `@` word the cursor ends, if it ends one.
    fn suggest_files(&mut self, already_open: bool) {
        let Some((start, query)) = files::token_at(&self.input, self.cursor) else {
            return;
        };
        self.completing_file = Some(start);
        // Each `@` lists the files anew: agents create and delete them.
        if !already_open {
            self.file_index_requested = true;
        }
        self.suggestions = files::rank(&query, &self.file_index, files::SHOWN)
            .into_iter()
            .map(|path| (path, String::new()))
            .collect();
    }

    /// The directory whose files are to be listed, when a list was asked
    /// for. One asked for while files are being listed waits for that to
    /// end: its list would be older than the `@`.
    pub fn take_file_index_request(&mut self) -> Option<PathBuf> {
        if self.file_walk_pending || !std::mem::take(&mut self.file_index_requested) {
            return None;
        }
        self.file_walk_pending = true;
        Some(self.cwd.clone())
    }

    /// The files under `cwd`, from `files::walk`.
    pub fn set_file_index(&mut self, paths: Vec<String>) {
        self.file_index = paths;
        self.file_walk_pending = false;
        if self.completing_file.is_none() {
            return;
        }
        // An open list takes the new files in, without moving the selection
        // off the file it is on.
        self.refresh_keeping_selection();
    }

    fn refresh_keeping_selection(&mut self) {
        let selected = self.suggestions.get(self.selected_suggestion).cloned();
        self.update_suggestions();
        self.selected_suggestion = selected
            .and_then(|s| self.suggestions.iter().position(|o| *o == s))
            .unwrap_or(0);
    }

    /// The session listed its commands: a `/` being typed has them now
    /// (the session may have started after the `/`).
    pub(super) fn commands_arrived(&mut self) {
        let typing_command = (self.input.starts_with('/') || self.input.starts_with("\\/"))
            && !self.input.contains('\n');
        if typing_command && self.completing_file.is_none() && !self.suggestions_closed {
            self.refresh_keeping_selection();
        }
    }

    /// Listing the files came to nothing: the last list stays.
    pub fn file_walk_failed(&mut self) {
        self.file_walk_pending = false;
    }

    /// The char range of the `@` word starting at `start`: to the end of
    /// the word the cursor is in.
    fn file_word(&self, start: usize) -> (usize, usize) {
        let cursor = self.cursor.max(start);
        let rest = self.input.chars().skip(cursor);
        (
            start,
            cursor + rest.take_while(|c| !c.is_whitespace()).count(),
        )
    }

    pub fn close_suggestions(&mut self) {
        self.suggestions.clear();
        self.completing_file = None;
        self.suggestions_closed = true;
    }

    /// A list of files belongs to the word the cursor ends: it goes when
    /// the cursor leaves, or when text arrives that was not typed.
    pub(super) fn close_file_suggestions(&mut self) {
        if self.completing_file.is_some() {
            self.close_suggestions();
        }
    }

    /// The selected file as the active harness wants it written.
    fn selected_file_reference(&self) -> Option<String> {
        let (path, _) = self.suggestions.get(self.selected_suggestion)?;
        Some(self.harness().file_reference(path))
    }

    pub fn suggestion_up(&mut self) {
        if !self.suggestions.is_empty() {
            self.selected_suggestion = if self.selected_suggestion == 0 {
                self.suggestions.len() - 1
            } else {
                self.selected_suggestion - 1
            };
        }
    }

    pub fn suggestion_down(&mut self) {
        if !self.suggestions.is_empty() {
            self.selected_suggestion = (self.selected_suggestion + 1) % self.suggestions.len();
        }
    }

    /// Enter should complete the highlighted suggestion when the typed text
    /// is only a prefix of a command (e.g. `/pro` → `/provider`).
    ///
    /// With a list of files it should unless the word typed already is the
    /// selected file's reference: completing would change nothing.
    pub fn should_accept_suggestion(&self) -> bool {
        if let Some(start) = self.completing_file {
            let (from, to) = self.file_word(start);
            let typed: String = self.input.chars().skip(from).take(to - from).collect();
            return self
                .selected_file_reference()
                .is_some_and(|reference| reference != typed);
        }
        if !(self.input.starts_with('/') || self.input.starts_with("\\/"))
            || self.suggestions.is_empty()
        {
            return false;
        }
        let Some((cmd, _)) = self.suggestions.get(self.selected_suggestion) else {
            return false;
        };
        // The list matches regardless of case.
        let typed = self.input.trim();
        typed != cmd && cmd.to_lowercase().starts_with(&typed.to_lowercase())
    }

    pub fn accept_suggestion(&mut self) {
        if let Some(start) = self.completing_file {
            let Some(mut text) = self.selected_file_reference() else {
                return;
            };
            // The whole word goes, what is after the cursor included.
            let (from, to) = self.file_word(start);
            let (from, to) = (self.byte_index(from), self.byte_index(to));
            if to == self.input.len() {
                text.push(' ');
            }
            self.input.replace_range(from..to, &text);
            self.cursor = start + text.chars().count();
            self.close_suggestions();
            return;
        }
        if let Some((cmd, _)) = self.suggestions.get(self.selected_suggestion) {
            self.input = cmd.clone();
            self.cursor = self.input.chars().count();
            self.update_suggestions();
        }
    }
}
