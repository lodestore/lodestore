use std::time::Duration;

use croner::parser::{CronParser, Seconds, Year};
use croner::{Cron, JobType};
use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{SignedDuration, Timestamp, Zoned};
use serde::Serialize;

use crate::{CRON_RULE, DURATION_RULE, Problem, Schedule, parse_duration, zone_rule};

pub const DEFAULT_JITTER: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_CATCH_UP: Duration = Duration::from_secs(24 * 3600);

const MAX_JITTER: Duration = Duration::from_secs(24 * 3600);
const MIN_CATCH_UP: Duration = Duration::from_secs(60);
const MAX_CATCH_UP: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum DstNoteKind {
    Skipped,
    Repeated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DstNote {
    pub date: String,
    pub time: String,
    pub kind: DstNoteKind,
    pub runs_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompiledSchedule {
    cron: Cron,
    tz: TimeZone,
    tz_name: String,
    jitter: Duration,
    catch_up: Duration,
}

impl Schedule {
    pub fn compile(&self) -> Result<CompiledSchedule, Problem> {
        let mut problems = Vec::new();
        match self.compile_into(&mut problems) {
            Some(compiled) => Ok(compiled),
            None => Err(problems.remove(0)),
        }
    }

    pub(crate) fn compile_into(&self, out: &mut Vec<Problem>) -> Option<CompiledSchedule> {
        let before = out.len();
        let cron = match parser().parse(self.cron.trim()) {
            Ok(cron) => Some(cron),
            Err(_) => {
                out.push(Problem::at("spec.schedule.cron", CRON_RULE));
                None
            }
        };
        let tz_name = self.timezone.as_deref().unwrap_or("UTC");
        let tz = match TimeZone::get(tz_name) {
            Ok(tz) => Some(tz),
            Err(_) => {
                out.push(Problem::at("spec.schedule.timezone", zone_rule(tz_name)));
                None
            }
        };
        let jitter = duration_in(
            self.jitter.as_deref(),
            DEFAULT_JITTER,
            Duration::ZERO..=MAX_JITTER,
        );
        if jitter.is_none() {
            out.push(Problem::at("spec.schedule.jitter", DURATION_RULE));
        }
        let catch_up = duration_in(
            self.catch_up_window.as_deref(),
            DEFAULT_CATCH_UP,
            MIN_CATCH_UP..=MAX_CATCH_UP,
        );
        if catch_up.is_none() {
            out.push(Problem::at("spec.schedule.catchUpWindow", DURATION_RULE));
        }
        if out.len() > before {
            return None;
        }
        Some(CompiledSchedule {
            cron: cron?,
            tz: tz?,
            tz_name: tz_name.to_string(),
            jitter: jitter?,
            catch_up: catch_up?,
        })
    }
}

fn parser() -> CronParser {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
}

fn duration_in(
    text: Option<&str>,
    default: Duration,
    range: std::ops::RangeInclusive<Duration>,
) -> Option<Duration> {
    match text {
        None => Some(default),
        Some(text) => parse_duration(text).filter(|d| range.contains(d)),
    }
}

impl CompiledSchedule {
    pub fn timezone(&self) -> &TimeZone {
        &self.tz
    }

    pub fn timezone_name(&self) -> &str {
        &self.tz_name
    }

    pub fn jitter(&self) -> Duration {
        self.jitter
    }

    pub fn catch_up_window(&self) -> Duration {
        self.catch_up
    }

    pub fn describe(&self) -> String {
        self.cron.describe()
    }

    pub fn previous_slot(&self, at: Timestamp) -> Option<Timestamp> {
        let cursor = at.to_zoned(self.tz.clone());
        let slot = self.cron.find_previous_occurrence(&cursor, true).ok()?;
        if !self.is_repeat(&slot) {
            return Some(slot.timestamp());
        }
        let probe = slot
            .timestamp()
            .checked_add(SignedDuration::from_nanos(1))
            .ok()?;
        let fold = self.tz.preceding(probe).next()?.timestamp();
        let first_pass = fold.checked_sub(SignedDuration::from_secs(1)).ok()?;
        let cursor = first_pass.to_zoned(self.tz.clone());
        let slot = self.cron.find_previous_occurrence(&cursor, true).ok()?;
        Some(slot.timestamp())
    }

    pub fn next_slots(&self, after: Timestamp, n: usize) -> Vec<Timestamp> {
        let mut out = Vec::with_capacity(n);
        let mut cursor = after.to_zoned(self.tz.clone());
        while out.len() < n {
            let Ok(next) = self.cron.find_next_occurrence(&cursor, false) else {
                break;
            };
            if !self.is_repeat(&next) {
                out.push(next.timestamp());
            }
            cursor = next;
        }
        out
    }

    pub fn slots_between(&self, after: Timestamp, until: Timestamp, cap: usize) -> usize {
        let mut count = 0;
        let mut cursor = after.to_zoned(self.tz.clone());
        while count < cap {
            let Ok(next) = self.cron.find_next_occurrence(&cursor, false) else {
                break;
            };
            if next.timestamp() > until {
                break;
            }
            if !self.is_repeat(&next) {
                count += 1;
            }
            cursor = next;
        }
        count
    }

    /// Smallest gap between consecutive runs within the horizon; stops at the first gap no longer than `stop_at`.
    pub fn min_gap(
        &self,
        from: Timestamp,
        horizon: SignedDuration,
        stop_at: SignedDuration,
    ) -> Option<SignedDuration> {
        let end = from.checked_add(horizon).ok()?;
        let mut cursor = from.to_zoned(self.tz.clone());
        let mut previous: Option<Timestamp> = None;
        let mut smallest: Option<SignedDuration> = None;
        loop {
            let Ok(next) = self.cron.find_next_occurrence(&cursor, false) else {
                break;
            };
            let at = next.timestamp();
            if at > end {
                break;
            }
            if !self.is_repeat(&next) {
                if let Some(previous) = previous {
                    let gap = at.duration_since(previous);
                    if smallest.is_none_or(|s| gap < s) {
                        smallest = Some(gap);
                    }
                    if gap <= stop_at {
                        break;
                    }
                }
                previous = Some(at);
            }
            cursor = next;
        }
        smallest
    }

    pub fn dst_notes(&self, from: Timestamp, horizon_days: u32) -> Vec<DstNote> {
        let Ok(end) = from.checked_add(SignedDuration::from_hours(24 * i64::from(horizon_days)))
        else {
            return Vec::new();
        };
        let fixed = self.cron.determine_job_type() == JobType::FixedTime;
        let mut notes = Vec::new();
        for transition in self.tz.following(from) {
            let at = transition.timestamp();
            if at > end {
                break;
            }
            let Ok(just_before) = at.checked_sub(SignedDuration::from_secs(1)) else {
                continue;
            };
            let before = self.tz.to_offset(just_before).seconds();
            let after = transition.offset().seconds();
            if before == after {
                continue;
            }
            let landing = at.to_zoned(self.tz.clone()).datetime();
            let shift = SignedDuration::from_secs(i64::from(before.abs_diff(after)));
            let gap = after > before;
            let start = if gap {
                landing.checked_sub(shift)
            } else {
                Ok(landing)
            };
            let Ok(start) = start else { continue };
            let Some(found) = self.first_match(start, shift) else {
                continue;
            };
            let (kind, runs_at) = match (gap, fixed) {
                (true, true) => (
                    DstNoteKind::Skipped,
                    Some(landing.strftime("%H:%M").to_string()),
                ),
                (true, false) => (DstNoteKind::Skipped, None),
                (false, _) => (DstNoteKind::Repeated, None),
            };
            notes.push(DstNote {
                date: found.date().to_string(),
                time: found.strftime("%H:%M").to_string(),
                kind,
                runs_at,
            });
        }
        notes
    }

    fn first_match(&self, start: DateTime, span: SignedDuration) -> Option<DateTime> {
        let step = SignedDuration::from_secs(60);
        let stop = start.checked_add(span).ok()?;
        let mut minute = start;
        while minute < stop {
            if self.cron.is_time_matching(&minute).unwrap_or(false) {
                return Some(minute);
            }
            minute = minute.checked_add(step).ok()?;
        }
        None
    }

    fn is_repeat(&self, slot: &Zoned) -> bool {
        match self.tz.to_ambiguous_zoned(slot.datetime()).offset() {
            AmbiguousOffset::Fold { after, .. } => slot.offset() == after,
            _ => false,
        }
    }
}
