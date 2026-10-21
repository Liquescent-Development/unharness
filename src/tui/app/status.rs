//! `/status` (and `/usage`): the session, the account it runs on, what it
//! has used, how full its context is and its rate limits. Built afresh
//! from the state each time it is drawn, so the overlay follows the
//! session while it is open.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{App, ListRequest};
use crate::core::{AccountInfo, HarnessId, RateLimitInfo, RateLimitWindow, SessionStats, Usage};
use crate::tui::modal::Modal;

/// What `/status` shows, section by section.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReport {
    pub sections: Vec<StatusSection>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusSection {
    pub title: &'static str,
    pub rows: Vec<StatusRow>,
}

/// One row: a label and what it says. A row with no label goes on from
/// the one before it.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRow {
    pub label: String,
    pub value: String,
    pub tone: Tone,
}

/// How a row is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    /// Not known, or not reported.
    Dim,
    /// Close to a limit, or not what was asked for.
    Warn,
    /// At a limit.
    Alert,
}

impl StatusRow {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        StatusRow {
            label: label.into(),
            value: value.into(),
            tone: Tone::Normal,
        }
    }

    fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
}

impl StatusReport {
    /// The report as plain text: a heading per section, a row per line.
    pub fn to_text(&self) -> String {
        let mut out = Vec::new();
        for s in &self.sections {
            out.push(s.title.to_string());
            for r in &s.rows {
                out.push(format!("  {}{}", padded(&r.label), r.value));
            }
        }
        out.join("\n")
    }
}

/// Columns of a row's label.
pub const LABEL_WIDTH: usize = 16;

/// `label` in its column, with a space after one too long for it.
pub fn padded(label: &str) -> String {
    match label.chars().count() {
        n if n < LABEL_WIDTH => format!("{label:<LABEL_WIDTH$}"),
        _ => format!("{label} "),
    }
}

/// Percentage of a rate-limit window drawn as a warning.
const WARN_PERCENT: f32 = 80.0;

/// Width of a rate-limit window's bar.
const BAR_WIDTH: usize = 10;

/// `n` with its digits in groups of three: `157,702,965`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A vendor's decimal string without trailing zeros: `62500.0000000000`
/// as `62500`.
fn decimal(s: &str) -> String {
    match s.contains('.') {
        true => s.trim_end_matches('0').trim_end_matches('.').to_string(),
        false => s.to_string(),
    }
}

/// The billed tokens `u` adds up to (what the status row's `session N`
/// counts for the session), what they are made of, and their cost.
fn billed(u: &Usage) -> String {
    format!(
        "{} (uncached input {}, output {}, cache reads {}, cache writes {}){}",
        grouped(u.total_tokens()),
        grouped(u.input),
        grouped(u.output),
        grouped(u.cache_read),
        grouped(u.cache_write),
        u.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default(),
    )
}

/// A length of time, to the two largest units: `2h 13m`, `3d 4h`, `45s`.
fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

/// When a window starts again, `resets_at` Unix seconds, from `now`.
fn resets(resets_at: i64, now: i64) -> String {
    match resets_at - now {
        left if left > 0 => format!("resets in {}", span(Duration::from_secs(left as u64))),
        _ => "reset since reported".to_string(),
    }
}

fn window_row(w: &RateLimitWindow, now: i64) -> StatusRow {
    let mut value = String::new();
    let mut tone = Tone::Normal;
    if let Some(p) = w.used_percent {
        let filled = ((p.clamp(0.0, 100.0) / 100.0) * BAR_WIDTH as f32).round() as usize;
        value.push_str(&format!(
            "{}{} {p:>3.0}% used",
            "█".repeat(filled),
            "░".repeat(BAR_WIDTH - filled)
        ));
        if p >= 100.0 {
            tone = Tone::Alert;
        } else if p >= WARN_PERCENT {
            tone = Tone::Warn;
        }
    } else {
        value.push_str("use not reported");
    }
    if let Some(at) = w.resets_at {
        value.push_str(" · ");
        value.push_str(&resets(at, now));
    }
    StatusRow::new(w.name(), value).tone(tone)
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl App {
    /// Open `/status`, and ask the CLI for its account's figures where it
    /// answers that.
    pub(super) fn open_status(&mut self) {
        self.modal = Some(Modal::Status(Default::default()));
        self.ask_status();
    }

    /// Ask the active harness's CLI for its account's figures: its session,
    /// or a run of it for that alone (`Capabilities::status_probe`).
    pub(super) fn ask_status(&mut self) {
        if self.controller.query_status() {
            self.request_list(ListRequest::Status(self.controller.active));
        }
    }

    /// What `/status` shows at `now`.
    pub fn status_report(&self, now: SystemTime) -> StatusReport {
        StatusReport {
            sections: vec![
                StatusSection {
                    title: "Session",
                    rows: self.session_rows(),
                },
                StatusSection {
                    title: "Account",
                    rows: self.account_rows(),
                },
                StatusSection {
                    title: "Usage",
                    rows: self.usage_rows(),
                },
                StatusSection {
                    title: "Context",
                    rows: vec![self.context_row()],
                },
                StatusSection {
                    title: "Rate limits",
                    rows: self.rate_limit_rows(now),
                },
            ],
        }
    }

    fn session_rows(&self) -> Vec<StatusRow> {
        let c = &self.controller;
        let active = c.active;
        let option = self.harness_options.iter().find(|o| o.id == active);
        let mut harness = self.display_name().to_string();
        // `codex-cli 0.162.1`, `2.1.305 (Claude Code)`: the number.
        let version = option.and_then(|o| o.version.as_deref()).and_then(|v| {
            v.split_whitespace()
                .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        });
        if let Some(v) = version {
            harness.push_str(&format!(" {v}"));
        }
        if let Some(b) = self.harness_binary() {
            harness.push_str(&format!(" ({})", b.display()));
        }
        let mut rows = vec![StatusRow::new("Harness", harness)];
        if let Some(p) = self.current_provider() {
            let row = match c.runs_on(active) {
                Some(runs_on) => StatusRow::new(
                    "Provider",
                    format!("{p}, but the session runs on {runs_on}"),
                )
                .tone(Tone::Warn),
                None => StatusRow::new("Provider", p.as_str()),
            };
            rows.push(row);
        }
        rows.push(StatusRow::new("Model", self.model_label()));
        if let Some(effort) = self.effort_label() {
            rows.push(StatusRow::new("Effort", effort));
        }
        let wanted = self.wanted_policy();
        rows.push(match self.effective_policy() {
            Some(e) if e == wanted => StatusRow::new("Policy", e.to_string()),
            Some(e) => {
                StatusRow::new("Policy", format!("{e} (asked for {wanted})")).tone(Tone::Warn)
            }
            None => StatusRow::new("Policy", format!("{wanted}, not available")).tone(Tone::Alert),
        });
        if let Some(w) = self.policy_warning() {
            rows.push(StatusRow::new("", w).tone(Tone::Warn));
        }
        let (level, why) = self.sandbox_level();
        rows.push(match why {
            Some(why) => {
                StatusRow::new("Sandbox", format!("{} ({why})", level.as_str())).tone(Tone::Warn)
            }
            None => StatusRow::new("Sandbox", level.as_str()),
        });
        if let Some(remote) = self.remote_status() {
            rows.push(StatusRow::new("Remote control", remote));
        }
        let conv = &c.conversation;
        rows.push(match conv.session_ids.get(&active) {
            Some(id) => StatusRow::new("Session", id.as_str()),
            None if c.session_alive && c.session_blank => StatusRow::new(
                "Session",
                "started; part of the conversation from the first prompt",
            )
            .tone(Tone::Dim),
            None if c.session_alive => StatusRow::new("Session", "starting").tone(Tone::Dim),
            None => StatusRow::new("Session", "not started").tone(Tone::Dim),
        });
        let title = conv.record.title.trim();
        rows.push(StatusRow::new(
            "Conversation",
            if title.is_empty() {
                conv.record.id.clone()
            } else {
                format!("{} ({title})", conv.record.id)
            },
        ));
        let mut dir = c.cwd.display().to_string();
        if let Some(b) = &self.git_branch {
            dir.push_str(&format!(" ({b})"));
        }
        rows.push(StatusRow::new("Directory", dir));
        if let Some(root) = c.workspace_root.as_ref().filter(|r| **r != c.cwd) {
            rows.push(StatusRow::new("Workspace", root.display().to_string()));
        }
        let turn = if c.is_generating {
            format!(
                "running for {}",
                span(Duration::from_secs_f32(self.elapsed_secs()))
            )
        } else {
            match conv.generation_duration {
                Some(d) => format!("idle; the last took {}", span(d)),
                None => "idle".to_string(),
            }
        };
        rows.push(StatusRow::new("Turn", turn));
        rows
    }

    fn account_rows(&self) -> Vec<StatusRow> {
        let active = self.controller.active;
        let reported = self
            .controller
            .account_status(active)
            .map(|s| &s.account)
            .filter(|a| **a != AccountInfo::default());
        if let Some(a) = reported {
            let mut rows = Vec::new();
            let mut field = |label: &str, value: &Option<String>| {
                if let Some(v) = value {
                    rows.push(StatusRow::new(label, v.as_str()));
                }
            };
            field("Plan", &a.plan);
            field("Signed in with", &a.auth);
            field("Email", &a.email);
            field("Organization", &a.organization);
            if let Some(credits) = &a.credits {
                let value = match (credits.unlimited, &credits.balance) {
                    (true, _) => "unlimited".to_string(),
                    (false, Some(b)) => decimal(b),
                    (false, None) => "balance not reported".to_string(),
                };
                let row = StatusRow::new("Credits", value);
                rows.push(if credits.has_credits || credits.unlimited {
                    row
                } else {
                    row.tone(Tone::Warn)
                });
            }
            return rows;
        }
        // What the CLI said when unharness started. Not signed in may mean
        // not known until a session starts (ACP), which its details say.
        match self.harness_options.iter().find(|o| o.id == active) {
            Some(o) if o.auth.authenticated => {
                let mut rows = vec![StatusRow::new("Signed in", "yes, at startup")];
                if let Some(d) = &o.auth.details {
                    rows.push(StatusRow::new("", d.as_str()));
                }
                rows
            }
            Some(o) if o.auth.details.is_some() => vec![StatusRow::new(
                "At startup",
                o.auth.details.as_deref().unwrap_or_default(),
            )],
            _ => vec![
                StatusRow::new("", format!("not reported by {}", self.display_name()))
                    .tone(Tone::Dim),
            ],
        }
    }

    fn usage_rows(&self) -> Vec<StatusRow> {
        let c = &self.controller.conversation;
        let mut rows = vec![
            StatusRow::new("Last turn", billed(&c.turn_usage)),
            StatusRow::new("Session", billed(&c.session_usage)),
        ];
        if let Some(st) = self.controller.session_stats(self.controller.active) {
            rows.extend(session_stats_rows(self.short_name(), st));
        }
        // The others' in this conversation, as last saved.
        let mut others: Vec<(&HarnessId, &Usage)> = c
            .record
            .usage
            .iter()
            .filter(|(id, u)| **id != self.controller.active && u.total_tokens() > 0)
            .collect();
        others.sort_by_key(|(id, _)| id.as_str());
        for (id, u) in others {
            rows.push(StatusRow::new(
                self.registry()
                    .get(*id)
                    .map(|h| h.descriptor().short_name)
                    .unwrap_or(id.as_str()),
                format!(
                    "{} tokens{}",
                    grouped(u.total_tokens()),
                    u.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default()
                ),
            ));
        }
        rows
    }

    /// How full the context is, told apart from a harness that never
    /// says (as the status row does).
    fn context_row(&self) -> StatusRow {
        let context = &self.controller.conversation.context;
        match (context.percent(), context.used, context.window) {
            (Some(pct), Some(used), Some(window)) => StatusRow::new(
                "Filled",
                format!("{} of {} tokens ({pct}%)", grouped(used), grouped(window)),
            ),
            _ if !self.caps().context_usage => {
                StatusRow::new("", format!("not reported by {}", self.display_name()))
                    .tone(Tone::Dim)
            }
            (_, Some(used), _) => StatusRow::new(
                "Filled",
                format!("{} tokens, of a window not known yet", grouped(used)),
            ),
            (_, None, Some(window)) if window > 0 => StatusRow::new(
                "Filled",
                format!("not known yet, of a {}-token window", grouped(window)),
            )
            .tone(Tone::Dim),
            _ => StatusRow::new("Filled", "not known yet").tone(Tone::Dim),
        }
    }

    fn rate_limit_rows(&self, now: SystemTime) -> Vec<StatusRow> {
        let status = self.controller.account_status(self.controller.active);
        let asking = status.is_some_and(|s| s.asking());
        let Some((r, at)) = status.and_then(|s| s.rate_limit.as_ref().zip(s.rate_limit_at)) else {
            if asking {
                return vec![
                    StatusRow::new("", format!("asking {}…", self.display_name())).tone(Tone::Dim),
                ];
            }
            // A run of the CLI to ask it said nothing.
            if let Some(why) = status.and_then(|s| s.failed.as_ref()) {
                return vec![
                    StatusRow::new("", format!("could not be asked: {why}")).tone(Tone::Warn),
                ];
            }
            // What a harness that reports none sends shows all the same.
            let why = if self.caps().rate_limits {
                format!("none yet: {} reports them with a turn", self.display_name())
            } else if self.caps().status_probe {
                format!("none yet: r asks {}", self.display_name())
            } else {
                format!("not reported by {}", self.display_name())
            };
            return vec![StatusRow::new("", why).tone(Tone::Dim)];
        };
        let mut rows = rate_limit_rows(r, at, now);
        if asking {
            rows.push(StatusRow::new("", "refreshing…").tone(Tone::Dim));
        } else if let Some(why) = status.and_then(|s| s.failed.as_ref()) {
            rows.push(
                StatusRow::new("", format!("could not be asked again: {why}")).tone(Tone::Warn),
            );
        }
        rows
    }
}

/// The CLI's own count of its session (pi's): beside unharness's, not in
/// it, since it covers the turns of the session it was forked from too.
fn session_stats_rows(name: &str, st: &SessionStats) -> Vec<StatusRow> {
    // A session that has done nothing yet (a turn that failed) says
    // nothing worth a row.
    if [
        st.user_messages,
        st.assistant_messages,
        st.tool_calls,
        st.tokens,
    ]
    .iter()
    .all(|n| n.unwrap_or(0) == 0)
    {
        return Vec::new();
    }
    let plural = |n: u64, one: &str, many: &str| {
        format!("{} {}", grouped(n), if n == 1 { one } else { many })
    };
    let mut parts = Vec::new();
    if let Some(t) = st.tokens {
        parts.push(format!(
            "{} tokens{}",
            grouped(t),
            st.cost_usd
                .map(|c| format!(", ${c:.4}"))
                .unwrap_or_default()
        ));
    }
    let counts: Vec<String> = [
        st.user_messages.map(|n| plural(n, "prompt", "prompts")),
        st.assistant_messages.map(|n| plural(n, "reply", "replies")),
        st.tool_calls.map(|n| plural(n, "tool call", "tool calls")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !counts.is_empty() {
        parts.push(counts.join(", "));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    vec![
        StatusRow::new(format!("{name}'s count"), parts.join(" · ")),
        StatusRow::new("", "its whole session, with any it was forked from").tone(Tone::Dim),
    ]
}

/// The rows of a harness's rate limits `r`, reported at `at`.
fn rate_limit_rows(r: &RateLimitInfo, at: SystemTime, now: SystemTime) -> Vec<StatusRow> {
    let now_unix = unix(now);
    let mut rows = Vec::new();
    if let Some(s) = &r.status {
        let about = r
            .windows
            .iter()
            .find(|w| r.window.as_deref() == Some(w.label.as_str()));
        let value = match about {
            Some(w) => format!("{s} ({})", w.name()),
            None => s.clone(),
        };
        let tone = if r.reached() {
            Tone::Alert
        } else if s == "allowed" {
            Tone::Normal
        } else {
            Tone::Warn
        };
        rows.push(StatusRow::new("Status", value).tone(tone));
    }
    if let Some(limit) = &r.limit {
        rows.push(StatusRow::new("Limit", limit.as_str()));
    }
    rows.extend(r.windows.iter().map(|w| window_row(w, now_unix)));
    if let Some(o) = &r.overage {
        let mut value = o.status.clone().unwrap_or_else(|| "not reported".into());
        if let Some(why) = &o.disabled_reason {
            value.push_str(&format!(" ({})", why.replace('_', " ")));
        }
        if o.in_use {
            value.push_str(", in use");
        }
        if let Some(at) = o.resets_at {
            value.push_str(&format!(" · {}", resets(at, now_unix)));
        }
        rows.push(StatusRow::new("Extra usage", value).tone(if o.in_use {
            Tone::Warn
        } else {
            Tone::Normal
        }));
    }
    let age = now.duration_since(at).unwrap_or_default();
    rows.push(StatusRow::new("", format!("as of {} ago", span(age))).tone(Tone::Dim));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Overage;

    #[test]
    fn grouped_puts_digits_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(100_000), "100,000");
        assert_eq!(grouped(u64::MAX), "18,446,744,073,709,551,615");
        assert_eq!(decimal("62500.0000000000"), "62500");
        assert_eq!(decimal("12.50"), "12.5");
        assert_eq!(decimal("700"), "700");
    }

    #[test]
    fn a_label_too_long_for_its_column_is_kept_apart() {
        assert_eq!(padded("Plan"), format!("Plan{}", " ".repeat(12)));
        assert_eq!(padded("seven day fable"), "seven day fable ");
        assert_eq!(padded("Claude Code (claude)"), "Claude Code (claude) ");
    }

    #[test]
    fn spans_keep_the_two_largest_units() {
        assert_eq!(span(Duration::from_secs(45)), "45s");
        assert_eq!(span(Duration::from_secs(600)), "10m");
        assert_eq!(span(Duration::from_secs(2 * 3600 + 13 * 60 + 5)), "2h 13m");
        assert_eq!(span(Duration::from_secs(3 * 86_400 + 4 * 3600)), "3d 4h");
        assert_eq!(resets(1_000, 1_000), "reset since reported");
        assert_eq!(resets(1_000 + 90 * 60, 1_000), "resets in 1h 30m");
    }

    #[test]
    fn rate_limit_rows_show_windows_resets_and_overage() {
        let at = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let now = at + Duration::from_secs(40);
        let r = RateLimitInfo {
            status: Some("allowed_warning".into()),
            window: Some("seven_day".into()),
            windows: vec![
                RateLimitWindow {
                    label: "five_hour".into(),
                    used_percent: Some(27.0),
                    resets_at: Some(1_000_040 + 2 * 3600 + 13 * 60),
                },
                RateLimitWindow {
                    label: "seven_day".into(),
                    used_percent: Some(85.0),
                    resets_at: None,
                },
            ],
            limit: None,
            overage: Some(Overage {
                status: Some("rejected".into()),
                in_use: false,
                disabled_reason: Some("org_level_disabled".into()),
                resets_at: None,
            }),
        };
        let rows = rate_limit_rows(&r, at, now);
        let text: Vec<String> = rows
            .iter()
            .map(|r| format!("{}: {}", r.label, r.value))
            .collect();
        assert_eq!(
            text,
            [
                "Status: allowed_warning (weekly)",
                "5-hour: ███░░░░░░░  27% used · resets in 2h 13m",
                "weekly: █████████░  85% used",
                "Extra usage: rejected (org level disabled)",
                ": as of 40s ago",
            ]
        );
        assert_eq!(rows[0].tone, Tone::Warn);
        assert_eq!(rows[2].tone, Tone::Warn);
    }
}
