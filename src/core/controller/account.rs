//! What each harness reported of the account it runs on: who it is, its
//! plan and its rate limits. Kept per harness, for this run only: it is
//! the account's, not the conversation's, soon out of date, and names
//! the user.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use super::{Action, Controller};
use crate::core::rate_limits::LimitWatch;
use crate::core::{AccountInfo, AgentEvent, HarnessId, RateLimitInfo, SessionCommand};

/// What one harness reported of its account.
#[derive(Debug, Clone, Default)]
pub struct AccountStatus {
    pub account: AccountInfo,
    pub rate_limit: Option<RateLimitInfo>,
    /// When `rate_limit` was last reported.
    pub rate_limit_at: Option<SystemTime>,
    /// When the CLI was last asked (`Controller::query_status`).
    asked_at: Option<Instant>,
    /// It was asked, and has not answered yet.
    awaiting: bool,
    /// What the user was told of its limits.
    watch: LimitWatch,
    /// Why the CLI run to ask it said nothing, the last time.
    pub failed: Option<String>,
}

impl AccountStatus {
    /// Whether the CLI was asked and its answer is still to come; one
    /// that has not come in `ANSWER_WAIT` is not waited for.
    pub fn asking(&self) -> bool {
        self.awaiting && self.asked_at.is_some_and(|t| t.elapsed() < ANSWER_WAIT)
    }
}

/// The least time between two status queries to one harness.
const QUERY_EVERY: Duration = Duration::from_secs(10);

/// How long an answer to a status query is shown as coming.
const ANSWER_WAIT: Duration = Duration::from_secs(10);

/// Each harness's [`AccountStatus`], for those that reported any.
pub type Accounts = HashMap<HarnessId, AccountStatus>;

impl Controller {
    /// Ask the active harness's CLI for its account's limits and plan, if
    /// it answers that and was not asked a moment ago: `/status` opened,
    /// or asked to refresh. Its session is asked (`status_query`), not
    /// before it runs. Returns whether a CLI run for that alone is to be
    /// asked instead (`status_probe`), which the front end does
    /// ([`Controller::status_probed`] takes the answer).
    pub fn query_status(&mut self) -> bool {
        let caps = self.caps();
        let session = caps.status_query && self.session_alive && !self.start_held;
        let probe = !caps.status_query && caps.status_probe;
        if !session && !probe {
            return false;
        }
        let status = self.accounts.entry(self.active).or_default();
        if status.asked_at.is_some_and(|t| t.elapsed() < QUERY_EVERY) {
            return false;
        }
        status.asked_at = Some(Instant::now());
        status.awaiting = true;
        if session {
            self.actions
                .push_back(Action::Command(SessionCommand::QueryStatus));
        }
        probe
    }

    /// The answer of a CLI run apart to say `harness`'s limits
    /// ([`Controller::query_status`]): what it said, or why it said
    /// nothing.
    pub fn status_probed(&mut self, harness: HarnessId, answer: Result<Vec<AgentEvent>, String>) {
        let events = match answer {
            Ok(events) => events,
            Err(why) => {
                let status = self.accounts.entry(harness).or_default();
                status.awaiting = false;
                status.failed = Some(why);
                return;
            }
        };
        for ev in events {
            match ev {
                AgentEvent::RateLimit(r) => self.rate_limit_reported(harness, r),
                AgentEvent::Account(a) => self.account_reported(harness, a),
                _ => {}
            }
        }
        let status = self.accounts.entry(harness).or_default();
        status.awaiting = false;
        status.failed = None;
    }

    /// What `harness` reported of its account in this run.
    pub fn account_status(&self, harness: HarnessId) -> Option<&AccountStatus> {
        self.accounts.get(&harness)
    }

    /// `harness` reported its rate limits, merged into what it reported
    /// before ([`RateLimitInfo::merge`]): the user is told when a window
    /// nears its limit or a limit is reached ([`LimitWatch`]).
    pub(super) fn rate_limit_reported(&mut self, harness: HarnessId, r: RateLimitInfo) {
        let name = self.display_name_of(harness);
        let now = SystemTime::now();
        let status = self.accounts.entry(harness).or_default();
        let limit = status.rate_limit.get_or_insert_default();
        limit.merge(r);
        let unix = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        for notice in status.watch.observe(limit, unix) {
            let text = format!("{name}: {}", notice.text);
            if notice.stops {
                self.conversation.transcript.push_error(text);
            } else {
                self.conversation.transcript.push_notice(text);
            }
        }
        status.rate_limit_at = Some(now);
        status.awaiting = false;
    }

    /// `harness` said what account it runs on.
    pub(super) fn account_reported(&mut self, harness: HarnessId, a: AccountInfo) {
        let status = self.accounts.entry(harness).or_default();
        status.account.merge(a);
        status.awaiting = false;
    }
}
