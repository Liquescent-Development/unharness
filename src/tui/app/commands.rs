//! Slash commands: unharness's own and the ones passed to the harness.

use super::{App, rate_limit_summary};
use crate::core::rules::Scope;
use crate::core::sandbox::SandboxLevel;
use crate::core::{PermissionPolicy, ProviderId};

pub(super) const BASE_COMMANDS: &[(&str, &str)] = &[
    ("/harness", "Open the harness picker"),
    ("/switch", "Alias for /harness"),
    (
        "/provider",
        "Open the provider picker (multi-provider harnesses)",
    ),
    ("/model", "Open the model picker"),
    ("/effort", "Open the reasoning effort picker"),
    ("/think", "Alias for /effort"),
    ("/policy", "Open the permission policy picker"),
    ("/sandbox", "Open the sandbox level picker"),
    (
        "/resume",
        "Resume a saved conversation (all harnesses in it)",
    ),
    (
        "/conversations",
        "List saved conversations in this workspace",
    ),
    ("/usage", "Show token usage and cost"),
    ("/plan", "Show or hide the agent's plan"),
    (
        "/subagents",
        "Open a subagent's own transcript (and stop it from there)",
    ),
    (
        "/steer",
        "Send a message into the running turn: /steer <text>",
    ),
    ("/rewind", "Go back to before an earlier prompt and edit it"),
    (
        "/undo-restore",
        "Reverse the last file restore made by /rewind",
    ),
    (
        "/fork",
        "Continue in a copy of this conversation, leaving the original as it is",
    ),
    (
        "/compact",
        "Summarise the context now: /compact [instructions]",
    ),
    (
        "/attach",
        "Attach an image, PDF or text file to the next prompt: /attach <path>",
    ),
    (
        "/paste",
        "Attach the image on the clipboard to the next prompt (Ctrl+V)",
    ),
    ("/detach", "Drop the pending attachments"),
    ("/skills", "List skills discovered in .agents/skills"),
    ("/allow", "List what is allowed without asking"),
    (
        "/remote-control",
        "Answer this session from claude.ai/code or the Claude app: /remote-control [on [name]|off]",
    ),
    (
        "/clear",
        "Start a new conversation (the previous one stays in /resume)",
    ),
    ("/help", "Show commands and shortcuts"),
    ("/quit", "Exit unharness"),
];

/// Names `handle_slash_command` answers to that the list leaves out.
const HIDDEN_ALIASES: &[&str] = &["/sessions", "/exit", "/rc"];

/// Whether `/name` is one of unharness's own commands on some harness
/// (`App::owns_command` says whether on the active one).
fn is_own_command(name: &str) -> bool {
    BASE_COMMANDS.iter().any(|(c, _)| *c == name) || HIDDEN_ALIASES.contains(&name)
}

impl App {
    /// Whether one of unharness's own commands is offered for the active
    /// harness (in the `/` list and `/help`).
    pub(super) fn offers_command(&self, name: &str) -> bool {
        !matches!(name, "/remote-control" | "/rc") || self.caps().remote_control
    }

    /// Whether `/name` is one of unharness's own commands for the active
    /// harness, which one of the harness's of that name gives way to.
    pub(super) fn owns_command(&self, name: &str) -> bool {
        is_own_command(name) && self.offers_command(name)
    }

    pub fn handle_slash_command(&mut self, cmd: &str) {
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("");
        let arg = parts.next().map(str::to_string);
        // Everything after the command, for commands that take free text
        // (a path or a message may contain spaces).
        let rest = cmd.trim_start().strip_prefix(name).unwrap_or("").trim();
        match name {
            "/switch" | "/harness" => match arg {
                Some(a) => match self.registry().parse(&a).map(|h| h.descriptor().id) {
                    // Refused as the picker greys it out.
                    Some(id) if !self.harness_installed(id) => self
                        .controller
                        .conversation
                        .transcript
                        .push_error(format!("{id} is not installed (not found on PATH)")),
                    Some(id) => self.switch_harness(id),
                    None => self
                        .controller
                        .conversation
                        .transcript
                        .push_error(format!("unknown harness '{a}'")),
                },
                None => self.open_harness_picker(),
            },
            "/provider" => match arg {
                Some(a) => self.set_provider(ProviderId::new(a)),
                None => self.open_provider_picker(),
            },
            "/model" => match arg {
                Some(a) => self.set_model(a),
                None => self.open_model_picker(),
            },
            "/effort" | "/think" => match arg {
                Some(a) => self.set_effort(a),
                None => self.open_effort_picker(),
            },
            "/policy" => match arg {
                Some(a) => match PermissionPolicy::parse(&a) {
                    Some(p) => {
                        self.set_policy(p);
                    }
                    None => {
                        let offered: Vec<&str> =
                            self.offered_policies().iter().map(|p| p.as_str()).collect();
                        self.controller
                            .conversation
                            .transcript
                            .push_error(format!("unknown policy '{a}' ({})", offered.join(", ")));
                    }
                },
                None => self.open_policy_picker(),
            },
            "/sandbox" => match arg {
                Some(a) => match SandboxLevel::parse(&a) {
                    Some(l) => {
                        self.set_sandbox(l);
                    }
                    None => {
                        let levels: Vec<&str> = self
                            .controller
                            .available_sandbox_levels()
                            .iter()
                            .map(|l| l.as_str())
                            .collect();
                        self.controller.conversation.transcript.push_error(format!(
                            "unknown sandbox level '{a}' ({})",
                            levels.join(", ")
                        ));
                    }
                },
                None => self.open_sandbox_picker(),
            },
            "/resume" => match arg {
                Some(a) => self.resume_conversation(a),
                None => self.open_resume_picker(),
            },
            "/conversations" | "/sessions" => {
                let rows = self.controller.store.list();
                if rows.is_empty() {
                    self.controller
                        .conversation
                        .transcript
                        .push_notice("no saved conversations");
                } else {
                    let lines: Vec<String> = rows
                        .iter()
                        .map(|r| {
                            format!(
                                "• {}  {}  [{}]  {}",
                                &r.id[..8.min(r.id.len())],
                                r.updated_at,
                                r.harnesses
                                    .iter()
                                    .map(|h| h.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                r.title
                            )
                        })
                        .collect();
                    self.controller.conversation.transcript.push_system(format!(
                        "Saved conversations ({}):\n{}",
                        rows.len(),
                        lines.join("\n")
                    ));
                }
            }
            "/usage" => {
                let t = &self.controller.conversation.turn_usage;
                let s = &self.controller.conversation.session_usage;
                self.controller.conversation.transcript.push_system(format!(
                    "Last turn: {}↑ {}↓ (cache read {}, write {}){}\nSession: {} tokens{}",
                    t.input,
                    t.output,
                    t.cache_read,
                    t.cache_write,
                    t.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default(),
                    s.total_tokens(),
                    s.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default(),
                ));
                if let (Some(used), Some(window)) = (
                    self.controller.conversation.context.used,
                    self.controller.conversation.context.window,
                ) {
                    self.controller.conversation.transcript.push_system(format!(
                        "Context: {used} of {window} tokens ({}%)",
                        self.controller.conversation.context.percent().unwrap_or(0)
                    ));
                }
                if let Some(r) = &self.controller.rate_limit {
                    self.controller
                        .conversation
                        .transcript
                        .push_system(format!("Rate limits: {}", rate_limit_summary(r)));
                }
            }
            "/steer" => match rest {
                "" => self
                    .controller
                    .conversation
                    .transcript
                    .push_notice("usage: /steer <message>"),
                text => self.steer(text.to_string()),
            },
            "/rewind" => self.open_rewind_picker(),
            "/undo-restore" => self.controller.undo_restore(),
            "/fork" => self.fork_conversation(),
            "/compact" => self.compact((!rest.is_empty()).then(|| rest.to_string())),
            "/remote-control" | "/rc" => self.remote_control_command(cmd, rest),
            "/attach" => match rest {
                path if !path.is_empty() => self.attach(path),
                _ if self.attachments.is_empty() => self
                    .controller
                    .conversation
                    .transcript
                    .push_notice("usage: /attach <path>"),
                _ => {
                    let names: Vec<String> = self.attachments.iter().map(|a| a.label()).collect();
                    self.controller
                        .conversation
                        .transcript
                        .push_system(format!("Attached: {}", names.join(", ")));
                }
            },
            "/paste" => self.request_clipboard(),
            "/detach" => {
                self.attachments.clear();
                self.controller
                    .conversation
                    .transcript
                    .push_system("Attachments cleared.");
            }
            "/subagents" => self.open_subagent_picker(),
            "/plan" => {
                self.show_plan = !self.show_plan;
                if self.controller.conversation.plan.is_empty() {
                    self.controller
                        .conversation
                        .transcript
                        .push_notice("the agent has not shared a plan");
                } else if !self.show_plan {
                    self.controller
                        .conversation
                        .transcript
                        .push_system("Plan hidden (/plan shows it again).");
                }
            }
            "/allow" => {
                let list: Vec<String> = self
                    .controller
                    .rules
                    .iter()
                    .map(|(scope, rule)| {
                        let scope = match scope {
                            Scope::Workspace => "workspace",
                            Scope::Global => "global",
                        };
                        format!("• [{scope}] {}", rule.describe())
                    })
                    .collect();
                let files: Vec<String> = [Scope::Workspace, Scope::Global]
                    .into_iter()
                    .filter_map(|scope| self.controller.rules.path(scope))
                    .map(|path| format!("  {}", path.display()))
                    .collect();
                let files = format!("Kept in (edit to change or remove):\n{}", files.join("\n"));
                if list.is_empty() {
                    self.controller.conversation.transcript.push_system(format!(
                        "Nothing is allowed without asking yet. Answering a \
                         permission request with `a` adds a rule.\n{files}"
                    ));
                } else {
                    self.controller.conversation.transcript.push_system(format!(
                        "Allowed without asking ({}):\n{}\n{files}",
                        list.len(),
                        list.join("\n")
                    ));
                }
            }
            "/skills" => {
                let mut list = Vec::new();
                if let Some(root) = &self.controller.workspace_root {
                    for s in crate::skills::discover_skills_in_dir(
                        &crate::skills::workspace_skills_dir(root),
                    ) {
                        list.push(format!("• [workspace] {}", s.name));
                    }
                }
                if let Some(dir) = crate::skills::global_skills_dir() {
                    for s in crate::skills::discover_skills_in_dir(&dir) {
                        list.push(format!("• [global] {}", s.name));
                    }
                }
                if list.is_empty() {
                    self.controller
                        .conversation
                        .transcript
                        .push_notice("no skills in .agents/skills (try `unharness skills add`)");
                } else {
                    self.controller.conversation.transcript.push_system(format!(
                        "Skills ({}):\n{}",
                        list.len(),
                        list.join("\n")
                    ));
                }
            }
            "/clear" => self.new_conversation(),
            "/help" => {
                let mut help = String::from("Commands:\n");
                for (c, d) in BASE_COMMANDS.iter().filter(|(c, _)| self.offers_command(c)) {
                    help.push_str(&format!("  {c:<14} {d}\n"));
                }
                help.push_str(
                    "Shortcuts: Ctrl+H harness · Ctrl+M model · Ctrl+E effort · Ctrl+P policy · Ctrl+R resume · Ctrl+O expand the last tool call (click any call to expand that one, Ctrl+T for all) · Esc/Ctrl+C interrupt or quit\nSubagents: listed above the prompt while they work, also after their turn has ended · listed under the prompt while they work · what each one does is in a transcript of its own · Down from the prompt goes into the list, Enter opens the one chosen, Delete takes a finished one off the list (so does Ctrl+S, /subagents, or a click on the call that spawned it) · there: s stops it where the agent can, Tab goes to the next, Esc comes back · a prompt sent meanwhile goes straight to the agent\nPrompt: Ctrl+J newline (Shift+Enter too where the terminal can tell it from Enter) · Up/Down move between lines, then through earlier prompts · Home/End (Ctrl+A) line start/end · Ctrl+U clear · Ctrl+G edit in $EDITOR · Ctrl+V attach the clipboard's image (/paste)\nAgent commands: a /command unharness does not have goes to the agent where it runs commands of its own (Claude Code, pi, ACP agents), and the / list offers the ones it reports · \\/command sends one unharness also has (\\/clear, \\/model) or sends a prompt that starts with / to any agent\nShell: !command runs it yourself, in the session's directory and sandbox, with no input; its output is shown here and goes to the agent in front of your next prompt · Esc stops it · not during a turn · \\!text sends a prompt that starts with !\nTranscript: PageUp/PageDown, Shift+Up/Down, the mouse wheel or the scrollbar scroll · click \"Jump to bottom\" or press End (empty prompt) to go back to the end · drag to select and copy the text (double click a word, triple a line) · right click a block to copy all of it, or a code block its code\nDuring a turn: Enter queues the prompt · Alt+Enter steers the running turn · Alt+Up edits the last queued prompt",
                );
                self.controller.conversation.transcript.push_system(help);
            }
            "/quit" | "/exit" => self.quit(),
            _ => self.pass_command(cmd, false),
        }
    }

    /// A `/command` unharness does not have, or one sent past unharness's
    /// own with `\/` (`forced`): the active harness runs it, where it runs
    /// commands of its own from a prompt.
    pub fn pass_command(&mut self, text: &str, forced: bool) {
        let text = text.trim();
        let name = text.split_whitespace().next().unwrap_or(text);
        let harness = self.short_name();
        if !self.caps().slash_commands {
            if forced {
                self.queue_prompt(text.to_string());
            } else {
                self.controller.conversation.transcript.push_error(format!(
                    "unknown command '{name}'; /help lists commands ({harness} has not \
                     said it runs commands from a prompt; \\{name} sends it as text)"
                ));
            }
            return;
        }
        if name == "/" {
            self.controller
                .conversation
                .transcript
                .push_error("unknown command '/'; /help lists commands");
            return;
        }
        let listed = self
            .harness_commands()
            .iter()
            .find(|c| c.answers_to(name))
            .map(|c| format!("/{}", c.name));
        self.controller
            .conversation
            .transcript
            .push_notice(match &listed {
                Some(_) => format!("{name} is {harness}'s own command: passed on"),
                None => {
                    format!("'{name}' is not an unharness command: passed to {harness} as typed")
                }
            });
        // One that does what one of unharness's does changes the session
        // behind unharness's back.
        if self.owns_command(name) || listed.as_deref().is_some_and(|l| self.owns_command(l)) {
            let clears = name == "/clear" || listed.as_deref() == Some("/clear");
            self.controller.conversation.transcript.push_notice(format!(
                "unharness does not see what {harness}'s {name} changes: the model, \
                 transcript and context shown may no longer match its session{}",
                if clears {
                    " (unharness's /clear starts a new conversation)"
                } else {
                    ""
                }
            ));
        }
        self.queue_prompt(text.to_string());
    }
}
