use lodestore_policy::jiff::Timestamp;
use lodestore_policy::jiff::tz::TimeZone;
use lodestore_policy::retention::{Candidate, Decision, FLOOR, Rule, evaluate, timezone_of};
use lodestore_policy::{Retention, parse_yaml};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

fn ms(text: &str) -> i64 {
    text.parse::<Timestamp>().unwrap().as_millisecond()
}

fn snap(id: &str, received_at_ms: i64) -> Candidate {
    Candidate {
        id: id.to_string(),
        received_at_ms,
        suspicious: false,
        incomplete: false,
    }
}

fn hourly(count: i64) -> Vec<Candidate> {
    let base = ms("2026-06-01T00:00:00Z");
    (0..count)
        .map(|i| snap(&format!("s{i:02}"), base + i * HOUR))
        .collect()
}

fn kept(decisions: &[Decision]) -> Vec<&str> {
    decisions
        .iter()
        .filter(|d| d.keep)
        .map(|d| d.id.as_str())
        .collect()
}

fn held_by(decisions: &[Decision], rule: Rule) -> Vec<&str> {
    decisions
        .iter()
        .filter(|d| d.reasons.contains(&rule))
        .map(|d| d.id.as_str())
        .collect()
}

fn reasons<'a>(decisions: &'a [Decision], id: &str) -> &'a [Rule] {
    &decisions.iter().find(|d| d.id == id).unwrap().reasons
}

fn utc() -> TimeZone {
    TimeZone::UTC
}

fn kyiv() -> TimeZone {
    TimeZone::get("Europe/Kyiv").unwrap()
}

#[test]
fn retention_keep_last() {
    let rules = Retention {
        keep_last: Some(3),
        ..Retention::default()
    };
    let out = evaluate(&rules, &utc(), &hourly(10));
    assert_eq!(kept(&out), ["s09", "s08", "s07"]);
    assert_eq!(held_by(&out, Rule::Last), ["s09", "s08", "s07"]);
    assert_eq!(out.len(), 10);
    for d in &out[3..] {
        assert!(!d.keep && d.reasons.is_empty(), "{d:?}");
    }
}

#[test]
fn retention_daily_buckets_in_zone() {
    let mut snaps = Vec::new();
    for day in 1..=10 {
        for (label, hour) in [
            ("0300", "03:00:00"),
            ("1200", "12:00:00"),
            ("2330", "23:30:00"),
        ] {
            let at = ms(&format!("2026-06-{day:02}T{hour}Z"));
            snaps.push(snap(&format!("d{day:02}-{label}"), at));
        }
    }
    let rules = Retention {
        keep_daily: Some(7),
        ..Retention::default()
    };

    let in_kyiv = evaluate(&rules, &kyiv(), &snaps);
    let mut daily = held_by(&in_kyiv, Rule::Daily);
    daily.sort_unstable();
    assert_eq!(
        daily,
        [
            "d05-1200", "d06-1200", "d07-1200", "d08-1200", "d09-1200", "d10-1200", "d10-2330"
        ]
    );

    let in_utc = evaluate(&rules, &utc(), &snaps);
    let mut daily = held_by(&in_utc, Rule::Daily);
    daily.sort_unstable();
    assert_eq!(
        daily,
        [
            "d04-2330", "d05-2330", "d06-2330", "d07-2330", "d08-2330", "d09-2330", "d10-2330"
        ]
    );
}

#[test]
fn retention_iso_week_year_boundary() {
    let base = ms("2026-12-28T12:00:00Z");
    let snaps: Vec<Candidate> = (0..14)
        .map(|i| {
            let at = base + i * DAY;
            let date = Timestamp::from_millisecond(at).unwrap().to_string();
            snap(&date[..10], at)
        })
        .collect();
    let one = Retention {
        keep_weekly: Some(1),
        ..Retention::default()
    };
    assert_eq!(
        held_by(&evaluate(&one, &utc(), &snaps), Rule::Weekly),
        ["2027-01-10"]
    );
    let two = Retention {
        keep_weekly: Some(2),
        ..Retention::default()
    };
    assert_eq!(
        held_by(&evaluate(&two, &utc(), &snaps), Rule::Weekly),
        ["2027-01-10", "2027-01-03"]
    );
    let many = Retention {
        keep_weekly: Some(9),
        ..Retention::default()
    };
    assert_eq!(
        held_by(&evaluate(&many, &utc(), &snaps), Rule::Weekly).len(),
        2
    );
}

#[test]
fn retention_monthly() {
    let snaps: Vec<Candidate> = [
        ("a", "2026-03-05T10:00:00Z"),
        ("b", "2026-04-02T10:00:00Z"),
        ("c", "2026-04-28T10:00:00Z"),
        ("d", "2026-05-03T10:00:00Z"),
        ("e", "2026-05-30T10:00:00Z"),
        ("f", "2026-06-04T10:00:00Z"),
        ("g", "2026-06-20T10:00:00Z"),
    ]
    .into_iter()
    .map(|(id, at)| snap(id, ms(at)))
    .collect();
    let rules = Retention {
        keep_monthly: Some(2),
        ..Retention::default()
    };
    let out = evaluate(&rules, &utc(), &snaps);
    assert_eq!(held_by(&out, Rule::Monthly), ["g", "e"]);
    let none_older = Retention {
        keep_monthly: Some(12),
        ..Retention::default()
    };
    let out = evaluate(&none_older, &utc(), &snaps);
    assert_eq!(held_by(&out, Rule::Monthly), ["g", "e", "c", "a"]);
}

#[test]
fn retention_within_relative_to_newest() {
    let newest = ms("2020-01-20T00:00:00Z");
    let snaps = vec![
        snap("newest", newest),
        snap("three-days", newest - 3 * DAY),
        snap("edge", newest - 7 * DAY),
        snap("just-outside", newest - 7 * DAY - 1),
        snap("old", newest - 30 * DAY),
    ];
    let rules = Retention {
        keep_within: Some("7d".to_string()),
        ..Retention::default()
    };
    let out = evaluate(&rules, &utc(), &snaps);
    assert_eq!(
        held_by(&out, Rule::Within),
        ["newest", "three-days", "edge"]
    );
    assert_eq!(kept(&out), ["newest", "three-days", "edge"]);
}

#[test]
fn retention_suspicious_not_counted() {
    let mut snaps = hourly(4);
    snaps[3].suspicious = true;
    let rules = Retention {
        keep_last: Some(1),
        keep_daily: Some(1),
        ..Retention::default()
    };
    let out = evaluate(&rules, &utc(), &snaps);
    assert_eq!(reasons(&out, "s03"), [Rule::Suspicious]);
    assert_eq!(held_by(&out, Rule::Last), ["s02"]);
    assert_eq!(held_by(&out, Rule::Daily), ["s02"]);
    assert_eq!(held_by(&out, Rule::Floor), ["s02", "s01"]);
    assert_eq!(kept(&out), ["s03", "s02", "s01"]);
    assert!(!out.iter().find(|d| d.id == "s00").unwrap().keep);
}

#[test]
fn retention_floor_two_good() {
    assert_eq!(FLOOR, 2);
    let rules = Retention {
        keep_last: Some(1),
        ..Retention::default()
    };
    let out = evaluate(&rules, &utc(), &hourly(5));
    assert_eq!(kept(&out), ["s04", "s03"]);
    assert_eq!(reasons(&out, "s04"), [Rule::Last, Rule::Floor]);
    assert_eq!(reasons(&out, "s03"), [Rule::Floor]);
    let single = evaluate(&rules, &utc(), &hourly(1));
    assert_eq!(kept(&single), ["s00"]);
}

#[test]
fn retention_incomplete_not_floor() {
    let mut snaps = hourly(5);
    snaps[4].incomplete = true;
    let rules = Retention {
        keep_last: Some(1),
        ..Retention::default()
    };
    let out = evaluate(&rules, &utc(), &snaps);
    assert_eq!(reasons(&out, "s04"), [Rule::Incomplete]);
    assert_eq!(held_by(&out, Rule::Last), ["s03"]);
    assert_eq!(held_by(&out, Rule::Floor), ["s03", "s02"]);
    assert_eq!(kept(&out), ["s04", "s03", "s02"]);
}

#[test]
fn retention_incomplete_takes_no_slot() {
    let base = ms("2026-06-01T10:00:00Z");
    let mut snaps = Vec::new();
    for day in 0..3 {
        snaps.push(snap(&format!("full{day}"), base + day * DAY));
        let mut late = snap(&format!("part{day}"), base + day * DAY + 2 * HOUR);
        late.incomplete = true;
        snaps.push(late);
    }
    let rules = Retention {
        keep_daily: Some(3),
        ..Retention::default()
    };
    let out = evaluate(&rules, &utc(), &snaps);
    assert_eq!(held_by(&out, Rule::Daily), ["full2", "full1", "full0"]);
    assert_eq!(held_by(&out, Rule::Incomplete).len(), 3);
    assert_eq!(kept(&out).len(), 6);
}

#[test]
fn retention_empty_rules_keep_all() {
    let mut snaps = hourly(6);
    snaps[2].suspicious = true;
    let out = evaluate(&Retention::default(), &utc(), &snaps);
    assert_eq!(out.len(), 6);
    assert!(out.iter().all(|d| d.keep));
    assert_eq!(reasons(&out, "s02"), [Rule::Suspicious]);
    assert!(evaluate(&Retention::default(), &utc(), &[]).is_empty());
}

#[test]
fn retention_tie_break_by_id() {
    let at = ms("2026-06-01T00:00:00Z");
    let snaps = vec![snap("c", at), snap("a", at), snap("b", at)];
    let rules = Retention {
        keep_last: Some(1),
        ..Retention::default()
    };
    let forward = evaluate(&rules, &utc(), &snaps);
    let mut reversed = snaps.clone();
    reversed.reverse();
    assert_eq!(forward, evaluate(&rules, &utc(), &reversed));
    let ids: Vec<_> = forward.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert_eq!(held_by(&forward, Rule::Last), ["a"]);
    assert_eq!(kept(&forward), ["a", "b"]);
}

#[test]
fn retention_never_forgets_suspicious_or_floor() {
    let rule_sets = [
        Retention {
            keep_last: Some(1),
            ..Retention::default()
        },
        Retention {
            keep_within: Some("1m".to_string()),
            ..Retention::default()
        },
        Retention {
            keep_daily: Some(1),
            keep_weekly: Some(1),
            keep_monthly: Some(1),
            ..Retention::default()
        },
    ];
    for mask in 0u32..(1 << 8) {
        let mut snaps: Vec<Candidate> = (0..8)
            .map(|i| snap(&format!("s{i}"), ms("2026-06-01T00:00:00Z") + i * 3 * DAY))
            .collect();
        for (i, s) in snaps.iter_mut().enumerate() {
            s.suspicious = mask & (1 << i) != 0;
            s.incomplete = (mask >> 3) & (1 << (i % 3)) != 0 && !s.suspicious;
        }
        for rules in &rule_sets {
            let out = evaluate(rules, &kyiv(), &snaps);
            for d in &out {
                let src = snaps.iter().find(|s| s.id == d.id).unwrap();
                if src.suspicious {
                    assert!(d.keep && d.reasons == [Rule::Suspicious], "{mask} {d:?}");
                }
            }
            let mut complete: Vec<&Candidate> = snaps
                .iter()
                .filter(|s| !s.suspicious && !s.incomplete)
                .collect();
            complete.sort_by_key(|s| std::cmp::Reverse(s.received_at_ms));
            for s in complete.iter().take(FLOOR) {
                let d = out.iter().find(|d| d.id == s.id).unwrap();
                assert!(d.keep, "{mask} {s:?}");
            }
        }
    }
}

#[test]
fn retention_timezone_follows_schedule() {
    let with_zone = "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: p\nspec:\n  repository: main\n  paths: [/etc]\n  schedule:\n    cron: '@daily'\n    timezone: Europe/Kyiv\n";
    let policy = parse_yaml(with_zone).unwrap();
    assert_eq!(timezone_of(&policy.spec).iana_name(), Some("Europe/Kyiv"));
    let without = with_zone.replace(
        "  schedule:\n    cron: '@daily'\n    timezone: Europe/Kyiv\n",
        "",
    );
    let policy = parse_yaml(&without).unwrap();
    assert_eq!(timezone_of(&policy.spec), TimeZone::UTC);
    let default_zone = with_zone.replace("    timezone: Europe/Kyiv\n", "");
    let policy = parse_yaml(&default_zone).unwrap();
    assert_eq!(timezone_of(&policy.spec), TimeZone::UTC);
}
