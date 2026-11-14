//! Scheduled and event-started runs: `AgentCronJob` (a workflow on a cron schedule) and
//! `EventTrigger` (a workflow per message published to a topic).

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApiError, invalid};

/// Runs `workflow` with `input` on `schedule` (5-field cron, UTC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CronJobSpec {
    pub schedule: String,
    pub workflow: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub concurrency_policy: ConcurrencyPolicy,
    #[serde(default)]
    pub suspend: bool,
}

/// What happens when a run is due while the previous one is still going.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConcurrencyPolicy {
    /// Skip this tick.
    #[default]
    Forbid,
    /// Start another run anyway.
    Allow,
}

/// Starts `workflow` for every message published to `topic`, with input
/// `{"event": payload, "topic": topic}` merged over `input`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventTriggerSpec {
    pub topic: String,
    pub workflow: String,
    #[serde(default)]
    pub input: Value,
}

pub fn admit_cron_job(spec: &Value) -> Result<CronJobSpec, ApiError> {
    let s: CronJobSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("cron job spec: {e}")))?;
    Cron::parse(&s.schedule).map_err(|m| invalid("spec.schedule", m))?;
    if s.workflow.is_empty() {
        return Err(invalid("spec.workflow", "workflow is required"));
    }
    Ok(s)
}

pub fn admit_event_trigger(spec: &Value) -> Result<EventTriggerSpec, ApiError> {
    let s: EventTriggerSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("trigger spec: {e}")))?;
    if !crate::agent::valid_topic(&s.topic) {
        return Err(invalid("spec.topic", "topic: lowercase letters, digits, '.', '-', '_'"));
    }
    if s.workflow.is_empty() {
        return Err(invalid("spec.workflow", "workflow is required"));
    }
    Ok(s)
}

/// A 5-field cron expression: minute, hour, day of month, month, day of week (0 or 7 =
/// Sunday). Each field is `*`, a number, a range `a-b`, a list `a,b`, with an optional step
/// `/n`. When both day fields are restricted, a day matching either counts (as in cron).
// ponytail: no names (MON, JAN) and no @hourly shorthands; UTC only
#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    fields: [Vec<bool>; 5],
    dom_any: bool,
    dow_any: bool,
}

const RANGES: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];

impl Cron {
    pub fn parse(expr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!("cron schedule needs 5 fields (minute hour day month weekday), got {}", parts.len()));
        }
        let mut fields: [Vec<bool>; 5] = Default::default();
        for (i, part) in parts.iter().enumerate() {
            let (lo, hi) = RANGES[i];
            let mut set = vec![false; hi as usize + 1];
            for item in part.split(',') {
                let (range, step) = match item.split_once('/') {
                    Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(|| format!("bad step in {item:?}"))?),
                    None => (item, 1),
                };
                let (a, b) = match range {
                    "*" => (lo, hi),
                    r => match r.split_once('-') {
                        Some((a, b)) => (num(a, lo, hi)?, num(b, lo, hi)?),
                        None => {
                            let n = num(r, lo, hi)?;
                            (n, if item.contains('/') { hi } else { n })
                        }
                    },
                };
                if a > b {
                    return Err(format!("range {range:?} runs backwards"));
                }
                for v in (a..=b).step_by(step as usize) {
                    set[v as usize] = true;
                }
            }
            fields[i] = set;
        }
        // 7 is Sunday too
        if fields[4][7] {
            fields[4][0] = true;
        }
        Ok(Self { fields, dom_any: parts[2] == "*", dow_any: parts[4] == "*" })
    }

    /// Whether the schedule fires in the minute containing `t`.
    pub fn matches(&self, t: DateTime<Utc>) -> bool {
        let [min, hour, dom, month, dow] = &self.fields;
        let day = match (self.dom_any, self.dow_any) {
            (false, false) => dom[t.day() as usize] || dow[t.weekday().num_days_from_sunday() as usize],
            _ => dom[t.day() as usize] && dow[t.weekday().num_days_from_sunday() as usize],
        };
        min[t.minute() as usize] && hour[t.hour() as usize] && month[t.month() as usize] && day
    }
}

fn num(s: &str, lo: u32, hi: u32) -> Result<u32, String> {
    s.parse::<u32>().ok().filter(|n| (lo..=hi).contains(n)).ok_or_else(|| format!("{s:?} isn't in {lo}..={hi}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn cron_fields() {
        let every5 = Cron::parse("*/5 * * * *").unwrap();
        assert!(every5.matches(at(2026, 11, 3, 10, 15)) && !every5.matches(at(2026, 11, 3, 10, 16)));
        let weekdays9 = Cron::parse("0 9 * * 1-5").unwrap();
        assert!(weekdays9.matches(at(2026, 11, 3, 9, 0)), "Tuesday");
        assert!(!weekdays9.matches(at(2026, 11, 8, 9, 0)), "Sunday");
        let sunday = Cron::parse("30 2 * * 7").unwrap();
        assert!(sunday.matches(at(2026, 11, 8, 2, 30)), "7 is Sunday");
        // both day fields restricted: either matches
        let either = Cron::parse("0 0 1 * 1").unwrap();
        assert!(either.matches(at(2026, 12, 1, 0, 0)) && either.matches(at(2026, 11, 2, 0, 0)) && !either.matches(at(2026, 11, 3, 0, 0)));
        let list = Cron::parse("0,30 8-10/2 * * *").unwrap();
        assert!(list.matches(at(2026, 1, 1, 10, 30)) && !list.matches(at(2026, 1, 1, 9, 30)));
        for bad in ["* * * *", "60 * * * *", "*/0 * * * *", "5-1 * * * *", "x * * * *"] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }
}
