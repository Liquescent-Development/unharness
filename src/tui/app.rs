use crate::config::Config;
use crate::harness::{HarnessKind, ModelInfo, get_adapter};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    Thought,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub sender: String,
    pub duration: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // replaced by a generic ListPicker in the v2 TUI
pub enum ActivePopup {
    HarnessPicker,
    ModelPicker,
    EffortPicker,
}

#[derive(Debug, Clone)]
pub struct HarnessOption {
    pub kind: HarnessKind,
    pub installed: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortOption {
    pub id: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
}

pub const EFFORT_LEVELS: &[EffortOption] = &[
    EffortOption {
        id: "high",
        display_name: "high",
        description: "Deep reasoning for complex coding & architecture (recommended)",
    },
    EffortOption {
        id: "xhigh",
        display_name: "xhigh",
        description: "Extended thinking for difficult bug diagnosis & refactors",
    },
    EffortOption {
        id: "max",
        display_name: "max",
        description: "Maximum thinking depth & exhaustive reasoning budget",
    },
    EffortOption {
        id: "medium",
        display_name: "medium",
        description: "Balanced reasoning depth and latency",
    },
    EffortOption {
        id: "low",
        display_name: "low",
        description: "Minimal thinking, fastest responses & lowest token cost",
    },
];

pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub cursor_pos: usize,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub is_generating: bool,
    pub is_thinking: bool,
    pub generation_start: Option<Instant>,
    pub generation_duration: Option<Duration>,
    pub thought_start: Option<Instant>,
    pub thought_duration: Option<Duration>,
    pub active_harness: HarnessKind,
    pub last_used_harness: Option<HarnessKind>,
    pub harness_turn_count: usize,
    pub auto_approve: bool,
    pub selected_models: HashMap<HarnessKind, String>,
    pub selected_efforts: HashMap<HarnessKind, String>,
    pub cwd: PathBuf,
    pub spinner_frame: usize,
    pub should_quit: bool,

    // Modal state
    pub popup: Option<ActivePopup>,
    pub picker_selected: usize,
    pub harness_options: Vec<HarnessOption>,

    pub model_options: Vec<ModelInfo>,
    pub model_picker_selected: usize,

    pub effort_picker_selected: usize,

    // Autocomplete / Suggestions state
    pub suggestions: Vec<(String, String)>,
    pub selected_suggestion: usize,
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const BASE_COMMAND_CATALOG: &[(&str, &str)] = &[
    ("/switch", "Open harness switcher picker modal"),
    ("/switch agy", "Switch active harness to Antigravity (agy)"),
    (
        "/switch claude",
        "Switch active harness to Claude Code (claude)",
    ),
    ("/switch codex", "Switch active harness to Codex (codex)"),
    ("/model", "Open model picker modal for active harness"),
    ("/effort", "Open reasoning effort / think level modal"),
    ("/think", "Alias for /effort (change reasoning level)"),
    ("/skills", "List loaded workspace and global skills"),
    ("/auto", "Toggle auto-approving tool permissions"),
    ("/clear", "Clear conversation history from screen"),
    ("/help", "Show help, commands, and keybindings"),
    ("/quit", "Exit unharness"),
];

impl App {
    pub fn new(
        cwd: PathBuf,
        initial_harness: HarnessKind,
        auto_approve: bool,
        config: &Config,
    ) -> Self {
        let mut selected_models = HashMap::new();
        let mut selected_efforts = HashMap::new();
        for &kind in HarnessKind::default_priority() {
            let id = kind.as_str();
            if let Some(m) = config.default_model(id) {
                selected_models.insert(kind, m.to_string());
            }
            selected_efforts.insert(
                kind,
                config.default_effort(id).unwrap_or("high").to_string(),
            );
        }

        let mut harness_options = Vec::new();
        for &kind in HarnessKind::default_priority() {
            let adapter = get_adapter(kind);
            let custom_bin = config.binary_override(kind.as_str());
            let bin = adapter.resolve_binary(custom_bin);
            let installed = bin.is_some();
            let version = bin.and_then(|p| adapter.version(&p));
            harness_options.push(HarnessOption {
                kind,
                installed,
                version,
            });
        }

        let mut app = Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll: 0,
            auto_scroll: true,
            is_generating: false,
            is_thinking: false,
            generation_start: None,
            generation_duration: None,
            thought_start: None,
            thought_duration: None,
            active_harness: initial_harness,
            last_used_harness: None,
            harness_turn_count: 0,
            auto_approve,
            selected_models,
            selected_efforts,
            cwd,
            spinner_frame: 0,
            should_quit: false,
            popup: None,
            picker_selected: 0,
            harness_options,
            model_options: Vec::new(),
            model_picker_selected: 0,
            effort_picker_selected: 0,
            suggestions: Vec::new(),
            selected_suggestion: 0,
        };

        app.messages.push(Message {
            role: MessageRole::System,
            content: format!(
                "Welcome to unharness! Active harness: {}. Shortcuts: Ctrl+H (harness), Ctrl+M (model), Ctrl+E (effort), /help.",
                initial_harness.display_name()
            ),
            sender: "unharness".to_string(),
            duration: None,
        });

        app
    }

    pub fn current_model(&self) -> Option<String> {
        self.selected_models.get(&self.active_harness).cloned()
    }

    pub fn set_model_for_active_harness(&mut self, model_id: String) {
        self.selected_models
            .insert(self.active_harness, model_id.clone());
        self.add_system_message(format!(
            "Model set to '{}' for {}",
            model_id,
            self.active_harness.display_name()
        ));
    }

    pub fn current_effort(&self) -> String {
        self.selected_efforts
            .get(&self.active_harness)
            .cloned()
            .unwrap_or_else(|| "high".to_string())
    }

    pub fn set_effort_for_active_harness(&mut self, effort_id: String) {
        let normalized = effort_id.to_lowercase();
        self.selected_efforts
            .insert(self.active_harness, normalized.clone());
        self.add_system_message(format!(
            "Reasoning effort set to '{}' for {}",
            normalized,
            self.active_harness.display_name()
        ));
    }

    pub fn current_elapsed_secs(&self) -> f32 {
        if self.is_generating {
            self.generation_start
                .map(|s| s.elapsed().as_secs_f32())
                .unwrap_or(0.0)
        } else {
            self.generation_duration
                .map(|d| d.as_secs_f32())
                .unwrap_or(0.0)
        }
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
    }

    pub fn current_spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame]
    }

    pub fn start_generation(&mut self) {
        self.is_generating = true;
        self.is_thinking = false;
        self.generation_start = Some(Instant::now());
        self.generation_duration = None;
        self.thought_start = None;
        self.thought_duration = None;
    }

    pub fn finish_generation(&mut self) {
        self.is_generating = false;
        self.is_thinking = false;
        if let Some(start) = self.generation_start.take() {
            let dur = start.elapsed();
            self.generation_duration = Some(dur);
            if let Some(last) = self.messages.last_mut()
                && last.role == MessageRole::Assistant
                && last.duration.is_none()
            {
                last.duration = Some(dur);
            }
        }
    }

    pub fn add_user_message(&mut self, text: String) {
        self.messages.push(Message {
            role: MessageRole::User,
            content: text,
            sender: "You".to_string(),
            duration: None,
        });
        self.auto_scroll = true;
    }

    pub fn append_assistant_text(&mut self, delta: &str) {
        // If coming from thinking, mark thought finished
        if self.is_thinking {
            self.is_thinking = false;
            if let Some(t_start) = self.thought_start.take() {
                let dur = t_start.elapsed();
                self.thought_duration = Some(dur);
                if let Some(last) = self.messages.last_mut()
                    && last.role == MessageRole::Thought
                    && last.duration.is_none()
                {
                    last.duration = Some(dur);
                }
            }
        }

        let sender = self.active_harness.display_name().to_string();
        if let Some(last) = self.messages.last_mut()
            && last.role == MessageRole::Assistant
            && last.sender == sender
        {
            last.content.push_str(delta);
            return;
        }
        self.messages.push(Message {
            role: MessageRole::Assistant,
            content: delta.to_string(),
            sender,
            duration: None,
        });
    }

    pub fn append_thought_text(&mut self, delta: &str) {
        if !self.is_thinking {
            self.is_thinking = true;
            self.thought_start = Some(Instant::now());
        }

        let sender = self.active_harness.display_name().to_string();
        if let Some(last) = self.messages.last_mut()
            && last.role == MessageRole::Thought
            && last.sender == sender
        {
            last.content.push_str(delta);
            return;
        }
        self.messages.push(Message {
            role: MessageRole::Thought,
            content: delta.to_string(),
            sender,
            duration: None,
        });
    }

    pub fn add_tool_message(&mut self, name: &str, summary: &str) {
        let content = if summary.is_empty() {
            format!("⚡ Executing tool: {}", name)
        } else {
            format!("⚡ Tool {}: {}", name, summary)
        };
        self.messages.push(Message {
            role: MessageRole::Tool,
            content,
            sender: self.active_harness.display_name().to_string(),
            duration: None,
        });
    }

    pub fn add_error_message(&mut self, err: String) {
        self.messages.push(Message {
            role: MessageRole::Error,
            content: err,
            sender: "unharness".to_string(),
            duration: None,
        });
        self.auto_scroll = true;
    }

    pub fn add_system_message(&mut self, text: String) {
        self.messages.push(Message {
            role: MessageRole::System,
            content: text,
            sender: "unharness".to_string(),
            duration: None,
        });
        self.auto_scroll = true;
    }

    pub fn switch_harness(&mut self, next: HarnessKind) {
        if self.active_harness != next {
            self.active_harness = next;
            let model_display = self
                .current_model()
                .unwrap_or_else(|| "default".to_string());
            let effort_display = self.current_effort();
            self.add_system_message(format!(
                "Switched active harness to {} (Model: {}, Effort: {})",
                next.display_name(),
                model_display,
                effort_display
            ));
        }
    }

    pub fn toggle_auto_approve(&mut self) {
        self.auto_approve = !self.auto_approve;
        let status = if self.auto_approve { "ON" } else { "OFF" };
        self.add_system_message(format!("Auto-approve permissions toggled: {}", status));
    }

    pub fn prepare_prompt_for_dispatch(&mut self, new_prompt: &str) -> (String, bool) {
        let is_same_harness = self.last_used_harness == Some(self.active_harness);

        if is_same_harness && self.harness_turn_count > 0 {
            self.harness_turn_count += 1;
            (new_prompt.to_string(), true)
        } else {
            self.last_used_harness = Some(self.active_harness);
            self.harness_turn_count = 1;

            let mut history_chunks = Vec::new();
            for msg in &self.messages {
                match msg.role {
                    MessageRole::User => {
                        history_chunks.push(format!("User: {}", msg.content));
                    }
                    MessageRole::Assistant => {
                        history_chunks.push(format!("{}: {}", msg.sender, msg.content));
                    }
                    _ => {}
                }
            }

            if history_chunks.is_empty() {
                (new_prompt.to_string(), false)
            } else {
                let history_text = history_chunks.join("\n\n");
                let bridged_prompt = format!(
                    "[Context: Previous conversation in unharness session]\n{}\n\n[Current Task for {}]:\n{}",
                    history_text,
                    self.active_harness.display_name(),
                    new_prompt
                );
                (bridged_prompt, false)
            }
        }
    }

    // Modal picker helpers (Harness)
    pub fn open_harness_picker(&mut self) {
        for (i, opt) in self.harness_options.iter().enumerate() {
            if opt.kind == self.active_harness {
                self.picker_selected = i;
                break;
            }
        }
        self.popup = Some(ActivePopup::HarnessPicker);
    }

    // Modal picker helpers (Model)
    pub fn open_model_picker(&mut self) {
        let adapter = get_adapter(self.active_harness);
        let bin = adapter.resolve_binary(None);
        self.model_options = adapter.available_models(bin.as_deref());

        self.model_picker_selected = 0;
        if let Some(cur) = self.current_model() {
            for (i, m) in self.model_options.iter().enumerate() {
                if m.id == cur {
                    self.model_picker_selected = i;
                    break;
                }
            }
        }
        self.popup = Some(ActivePopup::ModelPicker);
    }

    // Modal picker helpers (Effort / Think)
    pub fn open_effort_picker(&mut self) {
        let current = self.current_effort();
        self.effort_picker_selected = 0;
        for (i, eff) in EFFORT_LEVELS.iter().enumerate() {
            if eff.id == current {
                self.effort_picker_selected = i;
                break;
            }
        }
        self.popup = Some(ActivePopup::EffortPicker);
    }

    pub fn close_popup(&mut self) {
        self.popup = None;
    }

    pub fn picker_up(&mut self) {
        match self.popup {
            Some(ActivePopup::HarnessPicker) => {
                if self.picker_selected > 0 {
                    self.picker_selected -= 1;
                } else if !self.harness_options.is_empty() {
                    self.picker_selected = self.harness_options.len() - 1;
                }
            }
            Some(ActivePopup::ModelPicker) => {
                if self.model_picker_selected > 0 {
                    self.model_picker_selected -= 1;
                } else if !self.model_options.is_empty() {
                    self.model_picker_selected = self.model_options.len() - 1;
                }
            }
            Some(ActivePopup::EffortPicker) => {
                if self.effort_picker_selected > 0 {
                    self.effort_picker_selected -= 1;
                } else {
                    self.effort_picker_selected = EFFORT_LEVELS.len() - 1;
                }
            }
            None => {}
        }
    }

    pub fn picker_down(&mut self) {
        match self.popup {
            Some(ActivePopup::HarnessPicker) => {
                if !self.harness_options.is_empty() {
                    self.picker_selected = (self.picker_selected + 1) % self.harness_options.len();
                }
            }
            Some(ActivePopup::ModelPicker) => {
                if !self.model_options.is_empty() {
                    self.model_picker_selected =
                        (self.model_picker_selected + 1) % self.model_options.len();
                }
            }
            Some(ActivePopup::EffortPicker) => {
                self.effort_picker_selected =
                    (self.effort_picker_selected + 1) % EFFORT_LEVELS.len();
            }
            None => {}
        }
    }

    pub fn confirm_picker(&mut self) {
        match self.popup {
            Some(ActivePopup::HarnessPicker) => {
                if let Some(opt) = self.harness_options.get(self.picker_selected) {
                    let kind = opt.kind;
                    self.switch_harness(kind);
                }
            }
            Some(ActivePopup::ModelPicker) => {
                if let Some(opt) = self.model_options.get(self.model_picker_selected) {
                    let model_id = opt.id.clone();
                    self.set_model_for_active_harness(model_id);
                }
            }
            Some(ActivePopup::EffortPicker) => {
                if let Some(eff) = EFFORT_LEVELS.get(self.effort_picker_selected) {
                    self.set_effort_for_active_harness(eff.id.to_string());
                }
            }
            None => {}
        }
        self.close_popup();
    }

    // Suggestions helpers
    pub fn update_suggestions(&mut self) {
        if self.input.starts_with('/') {
            let query = self.input.to_lowercase();
            let mut matches = Vec::new();

            // 1. /model <subquery>
            if query.starts_with("/model ") || query == "/model" {
                let subquery = query.strip_prefix("/model ").unwrap_or("").trim();
                let adapter = get_adapter(self.active_harness);
                let bin = adapter.resolve_binary(None);
                let models = adapter.available_models(bin.as_deref());

                if query == "/model" {
                    matches.push(("/model".to_string(), "Open model picker modal".to_string()));
                }

                for m in models {
                    if subquery.is_empty()
                        || m.id.to_lowercase().contains(subquery)
                        || m.display_name.to_lowercase().contains(subquery)
                    {
                        let desc = m.description.unwrap_or(m.display_name);
                        matches.push((format!("/model {}", m.id), desc));
                    }
                }
            }
            // 2. /effort or /think <subquery>
            else if query.starts_with("/effort ")
                || query == "/effort"
                || query.starts_with("/think ")
                || query == "/think"
            {
                let (prefix, subquery) = if query.starts_with("/think") {
                    ("/think", query.strip_prefix("/think ").unwrap_or("").trim())
                } else {
                    (
                        "/effort",
                        query.strip_prefix("/effort ").unwrap_or("").trim(),
                    )
                };

                if query == prefix {
                    matches.push((
                        prefix.to_string(),
                        "Open reasoning effort picker modal".to_string(),
                    ));
                }

                for eff in EFFORT_LEVELS {
                    if subquery.is_empty() || eff.id.starts_with(subquery) {
                        matches.push((
                            format!("{} {}", prefix, eff.id),
                            eff.description.to_string(),
                        ));
                    }
                }
            }
            // 3. Base commands
            else {
                for (cmd, desc) in BASE_COMMAND_CATALOG {
                    if cmd.to_lowercase().starts_with(&query) || (query == "/" && !cmd.is_empty()) {
                        matches.push((cmd.to_string(), desc.to_string()));
                    }
                }
            }

            self.suggestions = matches;
            if self.selected_suggestion >= self.suggestions.len() {
                self.selected_suggestion = 0;
            }
        } else {
            self.suggestions.clear();
            self.selected_suggestion = 0;
        }
    }

    pub fn suggestion_up(&mut self) {
        if !self.suggestions.is_empty() {
            if self.selected_suggestion > 0 {
                self.selected_suggestion -= 1;
            } else {
                self.selected_suggestion = self.suggestions.len() - 1;
            }
        }
    }

    pub fn suggestion_down(&mut self) {
        if !self.suggestions.is_empty() {
            self.selected_suggestion = (self.selected_suggestion + 1) % self.suggestions.len();
        }
    }

    pub fn accept_suggestion(&mut self) {
        if let Some((cmd, _)) = self.suggestions.get(self.selected_suggestion) {
            self.input = cmd.clone();
            self.cursor_pos = self.input.len();
            self.update_suggestions();
        }
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.auto_scroll = false;
        self.scroll = self.scroll.saturating_sub(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
    }

    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += 1;
        self.update_suggestions();
    }

    pub fn delete_backwards(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            self.input.remove(self.cursor_pos);
            self.update_suggestions();
        }
    }

    pub fn delete_forwards(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.input.remove(self.cursor_pos);
            self.update_suggestions();
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.cursor_pos += 1;
        }
    }

    pub fn move_cursor_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn move_cursor_end(&mut self) {
        self.cursor_pos = self.input.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_suggestions() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            HarnessKind::Agy,
            false,
            &Config::default(),
        );
        assert!(app.suggestions.is_empty());

        app.insert_char('/');
        assert_eq!(app.suggestions.len(), BASE_COMMAND_CATALOG.len());

        app.insert_char('s');
        app.insert_char('w');
        assert_eq!(app.suggestions.len(), 4);

        app.accept_suggestion();
        assert_eq!(app.input, "/switch");
    }

    #[test]
    fn test_model_suggestions_for_claude() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            HarnessKind::Claude,
            false,
            &Config::default(),
        );
        for c in "/model ".chars() {
            app.insert_char(c);
        }

        assert!(app.suggestions.iter().any(|(cmd, _)| cmd.contains("opus")));
        assert!(
            app.suggestions
                .iter()
                .any(|(cmd, _)| cmd.contains("sonnet"))
        );
        assert!(app.suggestions.iter().any(|(cmd, _)| cmd.contains("fable")));
    }

    #[test]
    fn test_effort_suggestions() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            HarnessKind::Claude,
            false,
            &Config::default(),
        );
        for c in "/effort ".chars() {
            app.insert_char(c);
        }

        assert!(app.suggestions.iter().any(|(cmd, _)| cmd == "/effort high"));
        assert!(app.suggestions.iter().any(|(cmd, _)| cmd == "/effort low"));
        assert!(app.suggestions.iter().any(|(cmd, _)| cmd == "/effort max"));
    }

    #[test]
    fn test_modal_picker_navigation() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            HarnessKind::Agy,
            false,
            &Config::default(),
        );
        assert_eq!(app.active_harness, HarnessKind::Agy);
        assert!(app.popup.is_none());

        app.open_harness_picker();
        assert_eq!(app.popup, Some(ActivePopup::HarnessPicker));
        assert_eq!(app.picker_selected, 0);

        app.picker_down();
        assert_eq!(app.picker_selected, 1);

        app.confirm_picker();
        assert_eq!(app.active_harness, HarnessKind::Claude);
        assert!(app.popup.is_none());
    }

    #[test]
    fn test_effort_picker_modal() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            HarnessKind::Claude,
            false,
            &Config::default(),
        );
        assert_eq!(app.current_effort(), "high");

        app.open_effort_picker();
        assert_eq!(app.popup, Some(ActivePopup::EffortPicker));

        app.picker_down();
        app.confirm_picker();

        assert_ne!(app.current_effort(), "");
        assert!(app.popup.is_none());
    }

    #[test]
    fn test_thinking_timer_flow() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            HarnessKind::Claude,
            false,
            &Config::default(),
        );
        assert!(!app.is_generating);
        assert!(!app.is_thinking);

        app.start_generation();
        assert!(app.is_generating);

        app.append_thought_text("pondering...");
        assert!(app.is_thinking);

        app.append_assistant_text("answer");
        assert!(!app.is_thinking);

        app.finish_generation();
        assert!(!app.is_generating);
        assert!(app.generation_duration.is_some());
    }
}
