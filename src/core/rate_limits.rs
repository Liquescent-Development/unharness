//! What to tell the user of an account's rate limits (#344): once when a
//! window nears its limit, again only at a higher step or after the
//! window started over, and once when a limit is reached. The same for
//! every harness that reports limits, in the TUI (the controller) and in
//! `--print`; parsers only report them (`AgentEvent::RateLimit`).

use std::collections::HashMap;

use super::{RateLimitInfo, RateLimitWindow};

/// Use (percent) at which a window is warned of, and the steps above it
/// at which it is again.
const WARN_STEPS: [u8; 3] = [80, 90, 95];

/// A window whose reset moved later by more than this (seconds) started
/// over.
const RESET_SLACK: i64 = 600;

/// How far (percent) below the first step use has to fall for a window
/// warned of to be warned of again in the same period, so that use
/// hovering at a step is not.
const REARM_MARGIN: u8 = 10;

/// One thing to tell the user.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitNotice {
    pub text: String,
    /// A limit stops use, and no extra usage goes on past it.
    pub stops: bool,
}

/// What was told of one harness's limits.
#[derive(Debug, Clone, Default)]
pub struct LimitWatch {
    /// Each window warned of: the step it was warned at, and when it
    /// reset then.
    warned: HashMap<String, (u8, Option<i64>)>,
    reached: bool,
}

impl LimitWatch {
    /// What to tell the user of `r`, all that the harness reported of its
    /// limits so far ([`RateLimitInfo::merge`]), at `now` (Unix seconds).
    pub fn observe(&mut self, r: &RateLimitInfo, now: i64) -> Vec<LimitNotice> {
        let r = &current(r, now);
        let mut out = Vec::new();
        for w in &r.windows {
            if let Some((_, Some(then))) = self.warned.get(&w.label)
                && w.resets_at.is_some_and(|at| at > then + RESET_SLACK)
            {
                self.warned.remove(&w.label);
            }
        }
        let reached = r.reached();
        if reached && !self.reached {
            out.push(LimitNotice {
                text: reached_text(r, now),
                stops: !r.overage_covers(),
            });
        }
        self.reached = reached;
        for w in &r.windows {
            let step = step(w, r);
            let warned = self.warned.get(&w.label).map_or(0, |(s, _)| *s);
            if step == 0 {
                // One the vendor alone warned of (Claude names one window
                // per event) is warned of again only once it started over.
                if warned >= WARN_STEPS[0]
                    && w.used_percent.unwrap_or(0.0) < f32::from(WARN_STEPS[0] - REARM_MARGIN)
                {
                    self.warned.remove(&w.label);
                }
                continue;
            }
            if step > warned {
                if !reached {
                    out.push(LimitNotice {
                        text: near_text(w, now),
                        stops: false,
                    });
                }
                self.warned.insert(w.label.clone(), (step, w.resets_at));
            }
        }
        out
    }
}

/// How near its limit `w` is: 100 used up, a step of [`WARN_STEPS`], 1
/// below them where the vendor warns of it (Claude's `allowed_warning`
/// names the window), 0 not near.
fn step(w: &RateLimitWindow, r: &RateLimitInfo) -> u8 {
    let used = w.used_percent.unwrap_or(0.0);
    if used >= 100.0 {
        return 100;
    }
    if let Some(s) = WARN_STEPS.iter().rev().find(|s| used >= f32::from(**s)) {
        return *s;
    }
    let vendor_warns = r.status.as_deref() == Some("allowed_warning")
        && r.window.as_deref() == Some(w.label.as_str());
    u8::from(vendor_warns)
}

fn near_text(w: &RateLimitWindow, now: i64) -> String {
    let mut text = match w.used_percent {
        Some(p) => format!("{p:.0}% of the {} limit used", w.name()),
        None => format!("nearing the {} limit", w.name()),
    };
    if let Some(at) = w.resets_at {
        text.push_str(&format!("; it resets {}", clock(at, now)));
    }
    text
}

fn reached_text(r: &RateLimitInfo, now: i64) -> String {
    // The window the vendor names, else the one used most.
    let window = r
        .windows
        .iter()
        .find(|w| r.window.as_deref() == Some(w.label.as_str()))
        .or_else(|| {
            r.windows
                .iter()
                .filter(|w| w.used_percent.is_some())
                .max_by(|a, b| a.used_percent.partial_cmp(&b.used_percent).unwrap())
        });
    let what = match r.status.as_deref() {
        // Codex's workspace credits and limits, in its words.
        Some(s) if s.starts_with("workspace_") => s.replace('_', " "),
        _ => match window {
            Some(w) => format!("{} limit reached", w.name()),
            None => "rate limit reached".to_string(),
        },
    };
    let mut text = what;
    if let Some(at) = window.and_then(|w| w.resets_at) {
        text.push_str(&format!("; it resets {}", clock(at, now)));
    }
    if r.overage_covers() {
        text.push_str("; extra usage covers what follows");
    }
    text
}

/// What the status line shows while a window nears or reached its limit:
/// the nearest one, as `5h 82%`, and whether a limit stops use, at `now`
/// (Unix seconds).
pub fn marker(r: &RateLimitInfo, now: i64) -> Option<(String, bool)> {
    let r = &current(r, now);
    let w = r.windows.iter().filter(|w| step(w, r) > 0).max_by(|a, b| {
        (step(a, r), a.used_percent.unwrap_or(0.0))
            .partial_cmp(&(step(b, r), b.used_percent.unwrap_or(0.0)))
            .unwrap()
    });
    let stops = r.reached() && !r.overage_covers();
    let text = match w {
        Some(w) => match w.used_percent {
            Some(p) => format!("{} {p:.0}%", w.short_name()),
            None => format!("{} limit", w.short_name()),
        },
        None if r.reached() => "limit reached".to_string(),
        None => return None,
    };
    Some((text, stops))
}

/// What of `r` still holds at `now`: a window that reset since it was
/// reported is left out, and with it a status that named it.
fn current(r: &RateLimitInfo, now: i64) -> RateLimitInfo {
    let past = |w: &RateLimitWindow| w.resets_at.is_some_and(|at| at <= now);
    let mut r = r.clone();
    if r.windows
        .iter()
        .any(|w| past(w) && r.window.as_deref() == Some(w.label.as_str()))
    {
        r.status = None;
        r.window = None;
    }
    r.windows.retain(|w| !past(w));
    r
}

/// Unix seconds `at` on the local clock, from `now`: `at 15:00` the same
/// day, `Tue 15:00` within the week, `Oct 28 15:00` after.
pub fn clock(at: i64, now: i64) -> String {
    match (local(at), local(now)) {
        (Some(a), Some(n)) => clock_of(&a, &n, at - now),
        _ => "later".to_string(),
    }
}

/// Unix seconds of an ISO 8601 time with a date, a time and an offset
/// (`2026-10-21T03:40:00.099609+00:00`, `...Z`); the fraction is dropped.
pub fn iso8601_to_unix(s: &str) -> Option<i64> {
    let num = |s: &str| -> Option<i64> {
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse().ok())
            .flatten()
    };
    let (date, rest) = s.split_once('T')?;
    let mut d = date.splitn(3, '-');
    let (y, m, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    let time_len = rest.find(['Z', '+', '-']).unwrap_or(rest.len());
    let (time, offset) = rest.split_at(time_len);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':');
    let (hh, mm, ss) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let offset = match offset {
        "Z" | "" => 0,
        o => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            // `+05:30`, `+0530` or `+05`.
            let o = &o[1..];
            let (oh, om) = match o.split_once(':') {
                Some(parts) => parts,
                None if o.len() == 4 => o.split_at(2),
                None => (o, "0"),
            };
            let (oh, om) = (num(oh)?, num(om)?);
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    // Days from 1970-01-01 to the date (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

/// A local time's parts, as `localtime_r` gives them.
#[derive(Debug, Clone, Copy)]
struct LocalTime {
    year: i32,
    yday: i32,
    wday: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
}

fn local(t: i64) -> Option<LocalTime> {
    let t = t as libc::time_t;
    // SAFETY: `localtime_r` writes only into `tm`, and reads `t`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(LocalTime {
        year: tm.tm_year,
        yday: tm.tm_yday,
        wday: tm.tm_wday,
        mon: tm.tm_mon,
        mday: tm.tm_mday,
        hour: tm.tm_hour,
        min: tm.tm_min,
    })
}

fn clock_of(at: &LocalTime, now: &LocalTime, ahead: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let time = format!("{:02}:{:02}", at.hour, at.min);
    if (at.year, at.yday) == (now.year, now.yday) {
        format!("at {time}")
    } else if (0..6 * 86_400).contains(&ahead) {
        format!("{} {time}", DAYS[at.wday.rem_euclid(7) as usize])
    } else {
        format!(
            "{} {} {time}",
            MONTHS[at.mon.rem_euclid(12) as usize],
            at.mday
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(label: &str, pct: f32, resets_at: i64) -> RateLimitWindow {
        RateLimitWindow {
            label: label.into(),
            used_percent: Some(pct),
            resets_at: Some(resets_at),
        }
    }

    fn limits(status: &str, windows: Vec<RateLimitWindow>) -> RateLimitInfo {
        RateLimitInfo {
            status: Some(status.into()),
            windows,
            ..Default::default()
        }
    }

    fn texts(watch: &mut LimitWatch, r: &RateLimitInfo) -> Vec<String> {
        watch
            .observe(r, 1_000)
            .into_iter()
            .map(|n| n.text.split("; it resets").next().unwrap().to_string())
            .collect()
    }

    /// Claude sends `allowed_warning` with every event once a window is
    /// past its threshold: the user hears of it once, then at 90 and 95.
    #[test]
    fn a_window_is_warned_of_once_per_step() {
        let mut watch = LimitWatch::default();
        let at = |pct| {
            let mut r = limits("allowed_warning", vec![window("five_hour", pct, 9_000)]);
            r.window = Some("five_hour".into());
            r
        };
        assert_eq!(
            texts(&mut watch, &at(82.0)),
            ["82% of the 5-hour limit used"]
        );
        assert!(texts(&mut watch, &at(82.0)).is_empty());
        assert!(texts(&mut watch, &at(86.0)).is_empty());
        assert_eq!(
            texts(&mut watch, &at(91.0)),
            ["91% of the 5-hour limit used"]
        );
        assert_eq!(
            texts(&mut watch, &at(96.0)),
            ["96% of the 5-hour limit used"]
        );
        assert!(texts(&mut watch, &at(97.0)).is_empty());
    }

    #[test]
    fn a_window_that_started_over_is_warned_of_again() {
        let mut watch = LimitWatch::default();
        let r = limits("allowed", vec![window("seven_day", 85.0, 9_000)]);
        assert_eq!(texts(&mut watch, &r).len(), 1);
        // Down and up again in the same period.
        texts(
            &mut watch,
            &limits("allowed", vec![window("seven_day", 10.0, 9_000)]),
        );
        assert_eq!(texts(&mut watch, &r).len(), 1);
        // The next period, still high.
        let next = limits(
            "allowed",
            vec![window("seven_day", 85.0, 9_000 + 7 * 86_400)],
        );
        assert_eq!(texts(&mut watch, &next), ["85% of the weekly limit used"]);
    }

    /// A window Claude warns of below 80% is warned of once, in its words.
    #[test]
    fn the_vendor_s_own_warning_is_heard_once() {
        let mut watch = LimitWatch::default();
        let mut r = limits(
            "allowed_warning",
            vec![
                window("five_hour", 20.0, 9_000),
                window("seven_day", 76.0, 90_000),
            ],
        );
        r.window = Some("seven_day".into());
        assert_eq!(texts(&mut watch, &r), ["76% of the weekly limit used"]);
        assert!(texts(&mut watch, &r).is_empty());
    }

    #[test]
    fn a_limit_reached_is_told_once_and_whether_extra_usage_covers_it() {
        let mut watch = LimitWatch::default();
        let mut r = limits("rejected", vec![window("five_hour", 100.0, 9_000)]);
        r.window = Some("five_hour".into());
        let told = watch.observe(&r, 1_000);
        assert_eq!(told.len(), 1);
        assert!(told[0].text.starts_with("5-hour limit reached; it resets "));
        assert!(told[0].stops);
        assert!(watch.observe(&r, 1_000).is_empty());

        let mut watch = LimitWatch::default();
        r.overage = Some(crate::core::Overage {
            status: Some("allowed".into()),
            in_use: true,
            ..Default::default()
        });
        let told = watch.observe(&r, 1_000);
        assert!(told[0].text.ends_with("; extra usage covers what follows"));
        assert!(!told[0].stops);
    }

    /// Codex says why in `rateLimitReachedType`, and names no window.
    #[test]
    fn codex_s_reached_types_are_said_in_words() {
        let mut watch = LimitWatch::default();
        let r = limits(
            "workspace_member_credits_depleted",
            vec![window("7d", 40.0, 9_000)],
        );
        assert_eq!(texts(&mut watch, &r), ["workspace member credits depleted"]);
        let r = limits("rate_limit_reached", vec![window("5h", 100.0, 9_000)]);
        let mut watch = LimitWatch::default();
        assert_eq!(texts(&mut watch, &r), ["5-hour limit reached"]);
    }

    #[test]
    fn the_marker_shows_the_nearest_window() {
        assert_eq!(
            marker(
                &limits("allowed", vec![window("five_hour", 50.0, 9_000)]),
                1_000
            ),
            None
        );
        let r = limits(
            "allowed",
            vec![
                window("five_hour", 82.0, 9_000),
                window("seven_day", 91.0, 9_000),
            ],
        );
        assert_eq!(marker(&r, 1_000), Some(("7d 91%".into(), false)));
        let mut r = limits("rejected", vec![window("five_hour", 100.0, 9_000)]);
        r.window = Some("five_hour".into());
        assert_eq!(marker(&r, 1_000), Some(("5h 100%".into(), true)));
        // Once the window reset, until the CLI next reports.
        assert_eq!(marker(&r, 9_000), None);
    }

    /// Use hovering at a step, or Claude naming another window between
    /// the events that name one, is not warned of again.
    #[test]
    fn a_window_is_not_warned_of_again_until_well_below_its_step() {
        let mut watch = LimitWatch::default();
        let at = |pct| limits("allowed", vec![window("five_hour", pct, 9_000)]);
        assert_eq!(texts(&mut watch, &at(80.5)).len(), 1);
        assert!(texts(&mut watch, &at(79.5)).is_empty());
        assert!(texts(&mut watch, &at(80.5)).is_empty());

        let mut watch = LimitWatch::default();
        let naming = |name: &str| {
            let mut r = limits(
                "allowed_warning",
                vec![
                    window("five_hour", 30.0, 9_000),
                    window("seven_day", 76.0, 90_000),
                ],
            );
            r.window = Some(name.into());
            r
        };
        assert_eq!(texts(&mut watch, &naming("seven_day")).len(), 1);
        for _ in 0..3 {
            assert!(texts(&mut watch, &naming("five_hour")).len() <= 1);
            assert!(texts(&mut watch, &naming("seven_day")).is_empty());
        }
    }

    /// A report of a window that reset since is not told as current.
    #[test]
    fn a_window_already_reset_is_not_told() {
        let mut watch = LimitWatch::default();
        let r = limits("allowed", vec![window("five_hour", 92.0, 500)]);
        assert!(texts(&mut watch, &r).is_empty());
    }

    #[test]
    fn iso8601_times_read_as_unix_seconds() {
        assert_eq!(iso8601_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso8601_to_unix("2026-10-21T03:40:00.099609+00:00"),
            Some(1_792_554_000)
        );
        assert_eq!(
            iso8601_to_unix("2026-10-21T05:40:00+02:00"),
            Some(1_792_554_000)
        );
        assert_eq!(
            iso8601_to_unix("2026-10-20T22:40:00-05:00"),
            Some(1_792_554_000)
        );
        assert_eq!(
            iso8601_to_unix("2026-10-21T09:10:00+0530"),
            Some(1_792_554_000)
        );
        assert_eq!(
            iso8601_to_unix("2026-10-21T08:40:00+05"),
            Some(1_792_554_000)
        );
        assert_eq!(iso8601_to_unix("2026-10-21T08:40:00+0x"), None);
        assert_eq!(iso8601_to_unix("2026-10-21T08:40:00+24:00"), None);
        assert_eq!(iso8601_to_unix("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(iso8601_to_unix("2026-13-01T00:00:00Z"), None);
        assert_eq!(iso8601_to_unix("yesterday"), None);
    }

    #[test]
    fn reset_times_read_on_the_local_clock() {
        let t = |yday, wday, mon, mday, hour, min| LocalTime {
            year: 126,
            yday,
            wday,
            mon,
            mday,
            hour,
            min,
        };
        let now = t(293, 3, 9, 21, 10, 5);
        assert_eq!(clock_of(&t(293, 3, 9, 21, 15, 0), &now, 3600), "at 15:00");
        assert_eq!(
            clock_of(&t(295, 5, 9, 23, 9, 30), &now, 2 * 86_400),
            "Fri 09:30"
        );
        assert_eq!(
            clock_of(&t(301, 2, 9, 28, 9, 30), &now, 7 * 86_400),
            "Oct 28 09:30"
        );
    }
}
