//! The prompt box: editing, history, paste, the clipboard, the editor and attachments.

use std::path::{Path, PathBuf};

use super::App;
use crate::core::Attachment;
use crate::tui::clipboard::{self, Pasted};
use crate::tui::{drop, prompt};

impl App {
    /// The attachment `path` makes, or why the active harness gets none.
    fn attachable(&self, path: &Path) -> Result<Attachment, String> {
        match Attachment::from_path(path) {
            Some(a) if !self.caps().accepts(&a) => Err(self.controller.refusal(&a)),
            Some(a) => Ok(a),
            None => {
                Err("only images (png, jpg, gif, webp), PDFs and text files can be attached".into())
            }
        }
    }

    fn push_attachment(&mut self, a: Attachment) {
        self.controller
            .conversation
            .transcript
            .push_system(format!("Attached {} to the next prompt.", a.label()));
        self.attachments.push(a);
    }

    /// Queue an image or a document for the next prompt.
    pub fn attach(&mut self, path: &str) {
        let path = path.trim().trim_matches(['"', '\'']);
        let path = match path.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => self.controller.cwd.join(path),
        };
        if !path.is_file() {
            self.controller
                .conversation
                .transcript
                .push_error(format!("no such file: {}", path.display()));
            return;
        }
        match self.attachable(&path) {
            Ok(a) => self.push_attachment(a),
            Err(why) => self.controller.conversation.transcript.push_error(why),
        }
    }

    /// Files dropped onto the terminal: attach those the harness takes.
    /// The others stay what they were, paths in the prompt.
    fn attach_dropped(&mut self, pasted: &str, paths: Vec<PathBuf>) {
        let mut refused = Vec::new();
        for path in &paths {
            match self.attachable(path) {
                Ok(a) => self.push_attachment(a),
                Err(why) => {
                    self.controller
                        .conversation
                        .transcript
                        .push_notice(format!("{} not attached: {why}", path.display()));
                    refused.push(path.display().to_string());
                }
            }
        }
        if refused.len() == paths.len() {
            self.insert_str(pasted);
        } else if !refused.is_empty() {
            self.insert_str(&refused.join(" "));
        }
    }

    pub(super) fn byte_index(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    pub fn insert_char(&mut self, c: char) {
        let idx = self.byte_index(self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
        self.update_suggestions();
    }

    pub fn delete_backwards(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let idx = self.byte_index(self.cursor);
            self.input.remove(idx);
            self.update_suggestions();
        }
    }

    pub fn delete_forwards(&mut self) {
        if self.cursor < self.input.chars().count() {
            let idx = self.byte_index(self.cursor);
            self.input.remove(idx);
            self.update_suggestions();
        }
    }

    pub fn move_cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.close_file_suggestions();
    }

    pub fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
        self.close_file_suggestions();
    }

    /// The input's visual rows at the current prompt width.
    pub fn prompt_rows(&self) -> Vec<prompt::Row> {
        prompt::rows(&self.input, self.prompt_width)
    }

    /// Insert text at the cursor, newlines included (a paste).
    pub fn insert_str(&mut self, text: &str) {
        let text = prompt::clean(text);
        let idx = self.byte_index(self.cursor);
        self.input.insert_str(idx, &text);
        self.cursor += text.chars().count();
        self.refresh_suggestions(false);
    }

    /// Show the next older sent prompt, keeping what was typed as a draft.
    pub fn history_older(&mut self) {
        if let Some(text) = self.history.older(&self.input) {
            self.input = text.to_string();
            self.show_recalled();
        }
    }

    /// Show the next newer sent prompt, or the draft after the newest.
    /// False when no earlier prompt was on show: there is nothing newer.
    pub fn history_newer(&mut self) -> bool {
        match self.history.newer() {
            Some(text) => {
                self.input = text;
                self.show_recalled();
                true
            }
            None => false,
        }
    }

    fn show_recalled(&mut self) {
        self.cursor = self.input.chars().count();
        // No suggestion list for a recalled command: it would take over
        // the arrows that are stepping through history.
        self.close_suggestions();
    }

    /// A paste goes in whole, into whichever text field has the keyboard;
    /// one that is the paths of dropped files attaches them instead.
    /// It never acts as keystrokes: a modal without a text field ignores
    /// it rather than treat its letters as answers.
    pub fn paste(&mut self, text: &str) {
        let Some(modal) = self.modal.as_mut() else {
            match drop::files(text) {
                Some(paths) => self.attach_dropped(text, paths),
                None => self.insert_str(text),
            }
            return;
        };
        if let Some((field, multiline)) = modal.text_field() {
            let text = prompt::clean(text);
            if multiline {
                field.push_str(&text);
            } else {
                field.push_str(&text.replace('\n', " "));
            }
        }
    }

    pub fn request_clipboard(&mut self) {
        self.clipboard_requested = true;
    }

    pub fn take_clipboard_request(&mut self) -> bool {
        std::mem::take(&mut self.clipboard_requested)
    }

    /// Attach what was read off the clipboard: an image, saved into `dir`
    /// first, or the files copied to it.
    pub fn attach_pasted(&mut self, pasted: anyhow::Result<Pasted>, dir: &Path) {
        match pasted {
            Ok(Pasted::Image { extension, bytes }) => {
                if !self.caps().image_input {
                    let why = format!(
                        "{} does not accept images with the current model",
                        self.short_name()
                    );
                    return self.controller.conversation.transcript.push_error(why);
                }
                match clipboard::save_image(dir, extension, &bytes) {
                    Ok(path) => match self.attachable(&path) {
                        Ok(a) => self.push_attachment(a),
                        Err(why) => self.controller.conversation.transcript.push_error(why),
                    },
                    Err(e) => self
                        .controller
                        .conversation
                        .transcript
                        .push_error(format!("not pasted: {e:#}")),
                }
            }
            Ok(Pasted::Files(paths)) => {
                for path in paths {
                    match self.attachable(&path) {
                        Ok(a) => self.push_attachment(a),
                        Err(why) => self
                            .controller
                            .conversation
                            .transcript
                            .push_error(format!("{} not attached: {why}", path.display())),
                    }
                }
            }
            Err(e) => self
                .controller
                .conversation
                .transcript
                .push_error(format!("not pasted: {e:#}")),
        }
    }

    pub fn request_edit(&mut self) {
        self.edit_requested = true;
    }

    pub fn take_edit_request(&mut self) -> bool {
        std::mem::take(&mut self.edit_requested)
    }

    /// Replace the prompt's text, leaving the cursor at its end.
    pub fn set_input(&mut self, text: &str) {
        self.input = prompt::clean(text);
        self.cursor = self.input.chars().count();
        self.refresh_suggestions(false);
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Move to the row above, keeping the column. False on the first row.
    pub fn move_cursor_up(&mut self) -> bool {
        self.move_cursor_rows(-1)
    }

    /// Move to the row below, keeping the column. False on the last row.
    pub fn move_cursor_down(&mut self) -> bool {
        self.move_cursor_rows(1)
    }

    fn move_cursor_rows(&mut self, delta: isize) -> bool {
        let rows = self.prompt_rows();
        let row = prompt::cursor_row(&rows, self.cursor);
        let Some(target) = row
            .checked_add_signed(delta)
            .and_then(|r| rows.get(r).copied())
        else {
            return false;
        };
        let col = prompt::width_between(&self.input, rows[row].start, self.cursor);
        self.cursor = prompt::index_at_column(&self.input, target, col);
        self.close_file_suggestions();
        true
    }

    /// Start of the line the cursor is on.
    pub fn move_cursor_home(&mut self) {
        let before: Vec<char> = self.input.chars().take(self.cursor).collect();
        self.cursor = before.iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
        self.close_file_suggestions();
    }

    /// End of the line the cursor is on.
    pub fn move_cursor_end(&mut self) {
        self.cursor += self
            .input
            .chars()
            .skip(self.cursor)
            .take_while(|c| *c != '\n')
            .count();
        self.close_file_suggestions();
    }

    pub fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.prompt_scroll = 0;
        self.history.reset();
        self.close_suggestions();
        text
    }
}
