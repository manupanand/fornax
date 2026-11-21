// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Cron schedules for `on.schedule` (unit U1), evaluated in **UTC**.
//!
//! Format: five fields — `minute hour day-of-month month day-of-week` — or one of the
//! descriptors `@yearly`, `@annually`, `@monthly`, `@weekly`, `@daily`, `@midnight`,
//! `@hourly`. Each field is a comma-separated list of `*`, `N`, `A-B`, with an optional
//! `/STEP` (`*/15`, `1-5/2`, `3/10` = from 3 to the maximum every 10). Month names
//! (`JAN`–`DEC`) and day names (`SUN`–`SAT`) are accepted case-insensitively; in
//! day-of-week both `0` and `7` mean Sunday. `?` is accepted as `*` in the day fields.
//!
//! **Day-of-month / day-of-week:** when both fields are restricted (neither starts with
//! `*`/`?`), a day matches if *either* matches (Vixie cron / robfig behaviour, used by
//! Gitea and GitHub). If one of them is `*`, only the other one restricts. As in robfig,
//! `*/N` with `N > 1` counts as restricted.
//!
//! Not supported (rejected with a message): seconds fields, `@every`, `@reboot`, `TZ=` /
//! `CRON_TZ=` prefixes, Jenkins `H`, Quartz `L`/`W`/`#`.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};

/// Maximum length of a cron expression.
pub const MAX_CRON_LEN: usize = 256;
/// How far ahead [`CronSchedule::next_after`] searches (covers the 8-year gap between
/// leap days around 2100).
const SEARCH_YEARS: i32 = 10;

/// An invalid cron expression.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CronError {
    /// The expression is empty.
    #[error("cron expression is empty")]
    Empty,
    /// The expression is too long.
    #[error("cron expression is longer than {MAX_CRON_LEN} characters")]
    TooLong,
    /// Wrong number of fields.
    #[error("expected 5 fields (minute hour day-of-month month day-of-week), found {found}{hint}")]
    FieldCount {
        /// Number of fields found.
        found: usize,
        /// Extra hint (for example about seconds fields).
        hint: &'static str,
    },
    /// A time-zone prefix was used.
    #[error(
        "time zone prefixes (`TZ=`, `CRON_TZ=`) are not supported; schedules are evaluated in UTC"
    )]
    TimeZone,
    /// An unknown or unsupported `@` descriptor.
    #[error(
        "unsupported descriptor `{0}`; use @yearly, @annually, @monthly, @weekly, @daily, @midnight or @hourly"
    )]
    Descriptor(String),
    /// A field is invalid.
    #[error("{field} field `{text}`: {message}")]
    Field {
        /// Field name (`minute`, `hour`, …).
        field: &'static str,
        /// The field text.
        text: String,
        /// What is wrong.
        message: String,
    },
}

struct FieldSpec {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
    names_start: u32,
    allow_question: bool,
}

const MINUTE: FieldSpec = FieldSpec {
    name: "minute",
    min: 0,
    max: 59,
    names: &[],
    names_start: 0,
    allow_question: false,
};
const HOUR: FieldSpec = FieldSpec {
    name: "hour",
    min: 0,
    max: 23,
    names: &[],
    names_start: 0,
    allow_question: false,
};
const DOM: FieldSpec = FieldSpec {
    name: "day-of-month",
    min: 1,
    max: 31,
    names: &[],
    names_start: 0,
    allow_question: true,
};
const MONTH: FieldSpec = FieldSpec {
    name: "month",
    min: 1,
    max: 12,
    names: &[
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ],
    names_start: 1,
    allow_question: false,
};
const DOW: FieldSpec = FieldSpec {
    name: "day-of-week",
    min: 0,
    max: 7,
    names: &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"],
    names_start: 0,
    allow_question: true,
};

/// A parsed cron schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    source: String,
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    dom_star: bool,
    dow_star: bool,
}

impl CronSchedule {
    /// Parse a cron expression (see the module docs for the syntax).
    pub fn parse(source: &str) -> Result<Self, CronError> {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return Err(CronError::Empty);
        }
        if trimmed.len() > MAX_CRON_LEN {
            return Err(CronError::TooLong);
        }
        if trimmed.starts_with("TZ=") || trimmed.starts_with("CRON_TZ=") {
            return Err(CronError::TimeZone);
        }
        let expanded = if let Some(desc) = trimmed.strip_prefix('@') {
            match desc.to_ascii_lowercase().as_str() {
                "yearly" | "annually" => "0 0 1 1 *",
                "monthly" => "0 0 1 * *",
                "weekly" => "0 0 * * 0",
                "daily" | "midnight" => "0 0 * * *",
                "hourly" => "0 * * * *",
                _ => return Err(CronError::Descriptor(trimmed.to_string())),
            }
        } else {
            trimmed
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            let hint = if fields.len() == 6 {
                "; a seconds field is not supported"
            } else {
                ""
            };
            return Err(CronError::FieldCount {
                found: fields.len(),
                hint,
            });
        }
        let (minute, _) = parse_field(fields[0], &MINUTE)?;
        let (hour, _) = parse_field(fields[1], &HOUR)?;
        let (dom, dom_star) = parse_field(fields[2], &DOM)?;
        let (month, _) = parse_field(fields[3], &MONTH)?;
        let (mut dow, dow_star) = parse_field(fields[4], &DOW)?;
        if dow & (1 << 7) != 0 {
            dow = (dow & !(1 << 7)) | 1;
        }
        Ok(Self {
            source: trimmed.to_string(),
            minute,
            hour,
            dom,
            month,
            dow,
            dom_star,
            dow_star,
        })
    }

    /// The expression as written (trimmed).
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The first fire time strictly after `after` (UTC, whole minutes), or `None` when the
    /// schedule never fires within the next ten years (e.g. `0 0 30 2 *`).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = after
            .timestamp()
            .div_euclid(60)
            .checked_add(1)?
            .checked_mul(60)?;
        let mut t: NaiveDateTime = DateTime::from_timestamp(start, 0)?.naive_utc();
        let limit = t.year().checked_add(SEARCH_YEARS)?;
        loop {
            if t.year() > limit {
                return None;
            }
            if !bit(self.month, t.month()) {
                let (y, m) = if t.month() == 12 {
                    (t.year() + 1, 1)
                } else {
                    (t.year(), t.month() + 1)
                };
                t = NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.day_matches(t.date()) {
                t = t.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !bit(self.hour, t.hour()) {
                t = t.with_minute(0)?.checked_add_signed(Duration::hours(1))?;
                continue;
            }
            if !bit(self.minute, t.minute()) {
                t = t.checked_add_signed(Duration::minutes(1))?;
                continue;
            }
            return Some(t.and_utc());
        }
    }

    /// Whether the schedule fires at `at` (seconds are ignored).
    pub fn matches(&self, at: DateTime<Utc>) -> bool {
        let t = at.naive_utc();
        bit(self.minute, t.minute())
            && bit(self.hour, t.hour())
            && bit(self.month, t.month())
            && self.day_matches(t.date())
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        let dom_ok = bit(self.dom, d.day());
        let dow_ok = bit(self.dow, d.weekday().num_days_from_sunday());
        if self.dom_star || self.dow_star {
            dom_ok && dow_ok
        } else {
            dom_ok || dow_ok
        }
    }
}

impl FromStr for CronSchedule {
    type Err = CronError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for CronSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

fn bit(set: u64, n: u32) -> bool {
    n < 64 && set & (1u64 << n) != 0
}

/// Parse one field into a bit set; the flag is robfig's "star bit".
fn parse_field(text: &str, spec: &FieldSpec) -> Result<(u64, bool), CronError> {
    let err = |message: String| CronError::Field {
        field: spec.name,
        text: text.to_string(),
        message,
    };
    let mut bits = 0u64;
    let mut star = false;
    for part in text.split(',') {
        if part.is_empty() {
            return Err(err("empty list element".into()));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s.parse().map_err(|_| err(format!("invalid step `{s}`")))?;
                if step == 0 {
                    return Err(err("step must be at least 1".into()));
                }
                (r, Some(step))
            }
            None => (part, None),
        };
        let (start, end, part_star) = if range == "*" || (spec.allow_question && range == "?") {
            (spec.min, spec.max, true)
        } else if let Some((a, b)) = range.split_once('-') {
            let a = value(a, spec).map_err(err)?;
            let b = value(b, spec).map_err(err)?;
            if a > b {
                return Err(err(format!("range start {a} is after range end {b}")));
            }
            (a, b, false)
        } else {
            let v = value(range, spec).map_err(err)?;
            // `N/STEP` means `N-max/STEP`.
            (v, if step.is_some() { spec.max } else { v }, false)
        };
        let step = step.unwrap_or(1);
        if part_star && step == 1 {
            star = true;
        }
        let mut v = start;
        while v <= end {
            bits |= 1u64 << v;
            v = match v.checked_add(step) {
                Some(n) => n,
                None => break,
            };
        }
    }
    Ok((bits, star))
}

fn value(text: &str, spec: &FieldSpec) -> Result<u32, String> {
    if text.is_empty() {
        return Err("missing value".into());
    }
    if let Some(pos) = spec.names.iter().position(|n| n.eq_ignore_ascii_case(text)) {
        return Ok(spec.names_start + pos as u32);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(if spec.names.is_empty() {
            format!("`{text}` is not a number")
        } else {
            format!(
                "`{text}` is not a number or a name ({})",
                spec.names.join(", ")
            )
        });
    }
    let v: u32 = text
        .parse()
        .map_err(|_| format!("`{text}` is out of range {}-{}", spec.min, spec.max))?;
    if v < spec.min || v > spec.max {
        return Err(format!("{v} is out of range {}-{}", spec.min, spec.max));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid time")
    }

    fn next(spec: &str, after: DateTime<Utc>) -> DateTime<Utc> {
        CronSchedule::parse(spec)
            .expect("valid spec")
            .next_after(after)
            .expect("fires")
    }

    fn next_n(spec: &str, after: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
        let s = CronSchedule::parse(spec).expect("valid spec");
        let mut out = Vec::new();
        let mut t = after;
        for _ in 0..n {
            t = s.next_after(t).expect("fires");
            out.push(t);
        }
        out
    }

    #[test]
    fn every_fifteen_minutes() {
        assert_eq!(
            next_n("*/15 * * * *", at(2026, 1, 1, 10, 7), 3),
            vec![
                at(2026, 1, 1, 10, 15),
                at(2026, 1, 1, 10, 30),
                at(2026, 1, 1, 10, 45)
            ]
        );
    }

    #[test]
    fn strictly_after_and_seconds_ignored() {
        let t = at(2026, 1, 1, 10, 15);
        assert_eq!(next("*/15 * * * *", t), at(2026, 1, 1, 10, 30));
        let with_secs = t + Duration::seconds(30);
        assert_eq!(next("*/15 * * * *", with_secs), at(2026, 1, 1, 10, 30));
    }

    #[test]
    fn weekdays_range() {
        // 2026-11-20 is a Friday.
        assert_eq!(
            next("0 3 * * 1-5", at(2026, 11, 20, 4, 0)),
            at(2026, 11, 23, 3, 0)
        );
        assert_eq!(
            next("0 3 * * MON-FRI", at(2026, 11, 20, 2, 0)),
            at(2026, 11, 20, 3, 0)
        );
    }

    #[test]
    fn names_are_case_insensitive() {
        assert_eq!(
            next("0 0 1 jan *", at(2026, 3, 1, 0, 0)),
            at(2027, 1, 1, 0, 0)
        );
        assert_eq!(
            next("0 12 * * sun", at(2026, 11, 20, 0, 0)),
            at(2026, 11, 22, 12, 0)
        );
    }

    #[test]
    fn seven_is_sunday() {
        let a = CronSchedule::parse("0 0 * * 7").expect("valid");
        let b = CronSchedule::parse("0 0 * * 0").expect("valid");
        assert_eq!(a.dow, b.dow);
        assert_eq!(
            next("0 0 * * 5-7", at(2026, 11, 20, 1, 0)),
            at(2026, 11, 21, 0, 0)
        );
    }

    #[test]
    fn feb_29() {
        assert_eq!(
            next("0 0 29 2 *", at(2026, 1, 1, 0, 0)),
            at(2028, 2, 29, 0, 0)
        );
        // 2100 is not a leap year: 2096 -> 2104.
        assert_eq!(
            next("0 0 29 2 *", at(2096, 3, 1, 0, 0)),
            at(2104, 2, 29, 0, 0)
        );
    }

    #[test]
    fn never_fires() {
        let s = CronSchedule::parse("0 0 30 2 *").expect("valid");
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), None);
        let s = CronSchedule::parse("0 0 31 4,6,9,11 *").expect("valid");
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn month_ends() {
        assert_eq!(
            next("0 0 31 * *", at(2026, 4, 1, 0, 0)),
            at(2026, 5, 31, 0, 0)
        );
        assert_eq!(
            next("59 23 * * *", at(2026, 12, 31, 23, 59)),
            at(2027, 1, 1, 23, 59)
        );
        assert_eq!(
            next("0 0 1 * *", at(2026, 12, 15, 0, 0)),
            at(2027, 1, 1, 0, 0)
        );
    }

    #[test]
    fn dom_dow_or_rule() {
        // Both restricted: the 13th OR any Friday. 2026-11-20 is Friday.
        let after = at(2026, 11, 1, 0, 0);
        assert_eq!(
            next_n("0 0 13 * 5", after, 4),
            vec![
                at(2026, 11, 6, 0, 0),
                at(2026, 11, 13, 0, 0),
                at(2026, 11, 20, 0, 0),
                at(2026, 11, 27, 0, 0)
            ]
        );
        assert_eq!(
            next("0 0 1 * 1", at(2026, 11, 20, 0, 0)),
            at(2026, 11, 23, 0, 0)
        );
        // DOW star: only DOM restricts.
        assert_eq!(next("0 0 13 * *", after), at(2026, 11, 13, 0, 0));
        // DOM star: only DOW restricts.
        assert_eq!(next("0 0 * * 5", after), at(2026, 11, 6, 0, 0));
        // `*/2` in DOM counts as restricted (robfig): OR with Friday.
        assert_eq!(
            next("0 0 */10 * 5", at(2026, 11, 1, 0, 0)),
            at(2026, 11, 6, 0, 0)
        );
    }

    #[test]
    fn lists_ranges_steps() {
        assert_eq!(
            next_n("5,10-12/2 8 * * *", at(2026, 1, 1, 0, 0), 3),
            vec![
                at(2026, 1, 1, 8, 5),
                at(2026, 1, 1, 8, 10),
                at(2026, 1, 1, 8, 12)
            ]
        );
        assert_eq!(
            next_n("50/5 * * * *", at(2026, 1, 1, 0, 0), 3),
            vec![
                at(2026, 1, 1, 0, 50),
                at(2026, 1, 1, 0, 55),
                at(2026, 1, 1, 1, 50)
            ]
        );
    }

    #[test]
    fn descriptors() {
        let t = at(2026, 11, 20, 10, 30);
        assert_eq!(next("@hourly", t), at(2026, 11, 20, 11, 0));
        assert_eq!(next("@daily", t), at(2026, 11, 21, 0, 0));
        assert_eq!(next("@midnight", t), at(2026, 11, 21, 0, 0));
        assert_eq!(next("@weekly", t), at(2026, 11, 22, 0, 0));
        assert_eq!(next("@monthly", t), at(2026, 12, 1, 0, 0));
        assert_eq!(next("@yearly", t), at(2027, 1, 1, 0, 0));
        assert_eq!(next("@annually", t), at(2027, 1, 1, 0, 0));
    }

    #[test]
    fn matches_instant() {
        let s = CronSchedule::parse("30 2 * * *").expect("valid");
        assert!(s.matches(at(2026, 5, 5, 2, 30)));
        assert!(!s.matches(at(2026, 5, 5, 2, 31)));
    }

    #[test]
    fn invalid_specs_have_messages() {
        let cases = [
            ("", "empty"),
            ("* * * *", "expected 5 fields"),
            ("0 * * * * *", "seconds field"),
            ("60 * * * *", "minute field `60`: 60 is out of range 0-59"),
            ("* 24 * * *", "hour field"),
            ("* * 0 * *", "day-of-month field"),
            ("* * * 13 *", "month field"),
            ("* * * * 8", "day-of-week field"),
            ("*/0 * * * *", "step must be at least 1"),
            ("5-1 * * * *", "range start 5 is after range end 1"),
            ("a * * * *", "is not a number"),
            ("* * * FOO *", "is not a number or a name"),
            ("1,,2 * * * *", "empty list element"),
            ("@every 1h", "unsupported descriptor"),
            ("@reboot", "unsupported descriptor"),
            ("TZ=UTC 0 * * * *", "time zone"),
            ("? * * * *", "minute field"),
            ("H * * * *", "minute field"),
            ("99999999999 * * * *", "out of range"),
        ];
        for (spec, want) in cases {
            let err = CronSchedule::parse(spec).expect_err(spec).to_string();
            assert!(err.contains(want), "{spec:?}: {err}");
        }
    }

    #[test]
    fn question_mark_in_day_fields() {
        let a = CronSchedule::parse("0 0 ? * MON").expect("valid");
        assert!(a.dom_star);
    }
}
