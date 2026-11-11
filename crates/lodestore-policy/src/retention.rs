use std::collections::HashSet;
use std::hash::Hash;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;

use crate::{PolicySpec, Retention, parse_duration};

pub const FLOOR: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub received_at_ms: i64,
    pub suspicious: bool,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Rule {
    Last,
    Within,
    Daily,
    Weekly,
    Monthly,
    Floor,
    Suspicious,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: String,
    pub keep: bool,
    pub reasons: Vec<Rule>,
}

pub fn timezone_of(spec: &PolicySpec) -> TimeZone {
    spec.schedule
        .as_ref()
        .and_then(|s| s.timezone.as_deref())
        .and_then(|name| TimeZone::get(name).ok())
        .unwrap_or(TimeZone::UTC)
}

pub fn evaluate(rules: &Retention, tz: &TimeZone, candidates: &[Candidate]) -> Vec<Decision> {
    let mut order: Vec<&Candidate> = candidates.iter().collect();
    order.sort_by(|a, b| {
        b.received_at_ms
            .cmp(&a.received_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut reasons: Vec<Vec<Rule>> = vec![Vec::new(); order.len()];
    let good: Vec<usize> = (0..order.len())
        .filter(|&i| !order[i].suspicious && !order[i].incomplete)
        .collect();

    for (i, c) in order.iter().enumerate() {
        if c.suspicious {
            reasons[i].push(Rule::Suspicious);
        } else if c.incomplete {
            reasons[i].push(Rule::Incomplete);
        }
    }

    if rules.is_empty() {
        return decisions(&order, reasons, true);
    }

    if let Some(n) = count(rules.keep_last) {
        for &i in good.iter().take(n) {
            reasons[i].push(Rule::Last);
        }
    }
    if let (Some(&newest), Some(within)) = (good.first(), rules.keep_within.as_deref())
        && let Some(within) = parse_duration(within)
    {
        let window = i64::try_from(within.as_millis()).unwrap_or(i64::MAX);
        let cutoff = order[newest].received_at_ms.saturating_sub(window);
        for &i in &good {
            if order[i].received_at_ms >= cutoff {
                reasons[i].push(Rule::Within);
            }
        }
    }
    bucket_rule(
        rules.keep_daily,
        Rule::Daily,
        tz,
        &order,
        &good,
        &mut reasons,
        |d| (d.year(), d.month(), d.day()),
    );
    bucket_rule(
        rules.keep_weekly,
        Rule::Weekly,
        tz,
        &order,
        &good,
        &mut reasons,
        |d| {
            let w = d.iso_week_date();
            (w.year(), w.week())
        },
    );
    bucket_rule(
        rules.keep_monthly,
        Rule::Monthly,
        tz,
        &order,
        &good,
        &mut reasons,
        |d| (d.year(), d.month()),
    );

    for &i in good.iter().take(FLOOR) {
        reasons[i].push(Rule::Floor);
    }

    decisions(&order, reasons, false)
}

fn count(value: Option<i64>) -> Option<usize> {
    value.and_then(|n| usize::try_from(n).ok())
}

fn bucket_rule<K: Hash + Eq>(
    keep: Option<i64>,
    rule: Rule,
    tz: &TimeZone,
    order: &[&Candidate],
    good: &[usize],
    reasons: &mut [Vec<Rule>],
    key: impl Fn(jiff::civil::Date) -> K,
) {
    let Some(limit) = count(keep) else { return };
    let mut seen = HashSet::new();
    for &i in good {
        let ts = Timestamp::from_millisecond(order[i].received_at_ms).unwrap_or(
            if order[i].received_at_ms.is_negative() {
                Timestamp::MIN
            } else {
                Timestamp::MAX
            },
        );
        let bucket = key(ts.to_zoned(tz.clone()).date());
        if seen.contains(&bucket) {
            continue;
        }
        if seen.len() == limit {
            break;
        }
        seen.insert(bucket);
        reasons[i].push(rule);
    }
}

fn decisions(order: &[&Candidate], reasons: Vec<Vec<Rule>>, keep_all: bool) -> Vec<Decision> {
    order
        .iter()
        .zip(reasons)
        .map(|(c, reasons)| Decision {
            id: c.id.clone(),
            keep: keep_all || !reasons.is_empty(),
            reasons,
        })
        .collect()
}
