//! What each harness reported of the account it runs on: who it is, its
//! plan and its rate limits. Kept per harness, for this run only: it is
//! the account's, not the conversation's, soon out of date, and names
//! the user.

use std::collections::HashMap;
use std::time::SystemTime;

use super::Controller;
use crate::core::{AccountInfo, HarnessId, RateLimitInfo};

/// What one harness reported of its account.
#[derive(Debug, Clone, Default)]
pub struct AccountStatus {
    pub account: AccountInfo,
    pub rate_limit: Option<RateLimitInfo>,
    /// When `rate_limit` was last reported.
    pub rate_limit_at: Option<SystemTime>,
}

/// Each harness's [`AccountStatus`], for those that reported any.
pub type Accounts = HashMap<HarnessId, AccountStatus>;

/// Percentage of a rate-limit window at which the user is warned.
const RATE_LIMIT_WARN_PERCENT: f32 = 80.0;

fn rate_limit_high(r: &RateLimitInfo) -> bool {
    r.windows
        .iter()
        .any(|w| w.used_percent.is_some_and(|p| p >= RATE_LIMIT_WARN_PERCENT))
}

pub fn rate_limit_summary(r: &RateLimitInfo) -> String {
    let windows: Vec<String> = r
        .windows
        .iter()
        .map(|w| match w.used_percent {
            Some(p) => format!("{} {p:.0}% used", w.label),
            None => w.label.clone(),
        })
        .collect();
    match (&r.status, windows.is_empty()) {
        (Some(s), true) => s.clone(),
        (Some(s), false) if s != "allowed" => format!("{s} ({})", windows.join(", ")),
        _ => windows.join(", "),
    }
}

impl Controller {
    /// What `harness` reported of its account in this run.
    pub fn account_status(&self, harness: HarnessId) -> Option<&AccountStatus> {
        self.accounts.get(&harness)
    }

    /// The active harness reported its rate limits, merged into what it
    /// reported before ([`RateLimitInfo::merge`]): the user is warned
    /// once each time one of its windows crosses the threshold.
    pub(super) fn rate_limit_reported(&mut self, r: RateLimitInfo) {
        let status = self.accounts.entry(self.active).or_default();
        let was_high = status.rate_limit.as_ref().is_some_and(rate_limit_high);
        let limit = status.rate_limit.get_or_insert_default();
        limit.merge(r);
        if !was_high && rate_limit_high(limit) {
            self.conversation
                .transcript
                .push_notice(format!("rate limit: {}", rate_limit_summary(limit)));
        }
        status.rate_limit_at = Some(SystemTime::now());
    }

    /// The active harness said what account it runs on.
    pub(super) fn account_reported(&mut self, a: AccountInfo) {
        self.accounts
            .entry(self.active)
            .or_default()
            .account
            .merge(a);
    }
}
