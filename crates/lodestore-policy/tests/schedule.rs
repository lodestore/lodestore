use lodestore_policy::jiff::{SignedDuration, Timestamp};
use lodestore_policy::schedule::{CompiledSchedule, DstNote, DstNoteKind};
use lodestore_policy::{Schedule, parse_duration, parse_rate};

fn compile(cron: &str, timezone: &str) -> CompiledSchedule {
    Schedule {
        cron: cron.to_string(),
        timezone: Some(timezone.to_string()),
        jitter: None,
        catch_up_window: None,
    }
    .compile()
    .unwrap_or_else(|p| panic!("{cron}: {}", p.message))
}

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn show(compiled: &CompiledSchedule, slots: &[Timestamp]) -> Vec<String> {
    slots
        .iter()
        .map(|s| {
            s.to_zoned(compiled.timezone().clone())
                .strftime("%Y-%m-%dT%H:%M%:z")
                .to_string()
        })
        .collect()
}

#[test]
fn schedule_next_runs_daily_kyiv() {
    let kyiv = compile("30 2 * * *", "Europe/Kyiv");
    let slots = kyiv.next_slots(ts("2026-05-31T12:00:00Z"), 3);
    assert_eq!(
        show(&kyiv, &slots),
        [
            "2026-06-01T02:30+03:00",
            "2026-06-02T02:30+03:00",
            "2026-06-03T02:30+03:00"
        ]
    );
    assert_eq!(slots[0], ts("2026-05-31T23:30:00Z"));
    assert!(kyiv.next_slots(ts("2026-05-31T12:00:00Z"), 0).is_empty());
    assert!(!kyiv.describe().is_empty());
}

#[test]
fn schedule_dst_new_york_hourly_fall_back() {
    let ny = compile("0 * * * *", "America/New_York");
    let slots = ny.next_slots(ts("2026-11-01T00:30:00-04:00"), 3);
    assert_eq!(
        show(&ny, &slots),
        [
            "2026-11-01T01:00-04:00",
            "2026-11-01T02:00-05:00",
            "2026-11-01T03:00-05:00"
        ]
    );
    let from = ts("2026-11-01T00:30:00-04:00");
    let until = ts("2026-11-01T04:00:00-05:00");
    assert_eq!(ny.slots_between(from, until, 100), 4);
    let second_pass = ts("2026-11-01T01:30:00-05:00");
    assert_eq!(
        ny.previous_slot(second_pass),
        Some(ts("2026-11-01T01:00:00-04:00"))
    );
}

#[test]
fn schedule_dst_new_york_fixed_fall_back() {
    let ny = compile("30 1 * * *", "America/New_York");
    let slots = ny.next_slots(ts("2026-11-01T00:00:00-04:00"), 2);
    assert_eq!(
        show(&ny, &slots),
        ["2026-11-01T01:30-04:00", "2026-11-02T01:30-05:00"]
    );
    let prev = ny.previous_slot(ts("2026-11-01T01:45:00-05:00")).unwrap();
    assert_eq!(show(&ny, &[prev]), ["2026-11-01T01:30-04:00"]);
    let prev = ny.previous_slot(ts("2026-11-01T01:30:00-04:00")).unwrap();
    assert_eq!(show(&ny, &[prev]), ["2026-11-01T01:30-04:00"]);
}

#[test]
fn schedule_dst_kyiv_spring_and_fall() {
    let kyiv = compile("30 3 * * *", "Europe/Kyiv");
    let spring = kyiv.next_slots(ts("2026-03-28T12:00:00Z"), 3);
    assert_eq!(
        show(&kyiv, &spring),
        [
            "2026-03-29T04:00+03:00",
            "2026-03-30T03:30+03:00",
            "2026-03-31T03:30+03:00"
        ]
    );
    let fall = kyiv.next_slots(ts("2026-10-24T12:00:00Z"), 3);
    assert_eq!(
        show(&kyiv, &fall),
        [
            "2026-10-25T03:30+03:00",
            "2026-10-26T03:30+02:00",
            "2026-10-27T03:30+02:00"
        ]
    );
    let second_pass = ts("2026-10-25T01:45:00Z");
    assert_eq!(
        kyiv.previous_slot(second_pass),
        Some(ts("2026-10-25T00:30:00Z"))
    );
    let after_gap = ts("2026-03-29T01:00:00Z");
    assert_eq!(kyiv.previous_slot(after_gap), Some(after_gap));
    let before_gap = ts("2026-03-29T00:59:00Z");
    assert_eq!(
        kyiv.previous_slot(before_gap),
        Some(ts("2026-03-28T01:30:00Z"))
    );
}

#[test]
fn schedule_dst_lord_howe_half_hour() {
    let tz = lodestore_policy::jiff::tz::TimeZone::get("Australia/Lord_Howe").unwrap();
    let spring = tz.following(ts("2026-10-01T00:00:00Z")).next().unwrap();
    assert_eq!(spring.timestamp(), ts("2026-10-03T15:30:00Z"));
    let before = tz.to_offset(ts("2026-10-03T15:29:59Z")).seconds();
    assert_eq!(spring.offset().seconds() - before, 1800);

    let gap = compile("15 2 * * *", "Australia/Lord_Howe");
    let slots = gap.next_slots(ts("2026-10-03T00:00:00Z"), 2);
    assert_eq!(slots[0], ts("2026-10-03T15:30:00Z"));
    assert_eq!(
        show(&gap, &slots),
        ["2026-10-04T02:30+11:00", "2026-10-05T02:15+11:00"]
    );

    let fall = tz.following(ts("2026-04-01T00:00:00Z")).next().unwrap();
    assert_eq!(fall.timestamp(), ts("2026-04-04T15:00:00Z"));
    assert_eq!(fall.offset().seconds(), 10 * 3600 + 1800);

    let fold = compile("45 1 * * *", "Australia/Lord_Howe");
    let slots = fold.next_slots(ts("2026-04-04T00:00:00Z"), 2);
    assert_eq!(
        show(&fold, &slots),
        ["2026-04-05T01:45+11:00", "2026-04-06T01:45+10:30"]
    );
    let second_pass = ts("2026-04-04T15:20:00Z");
    assert_eq!(
        show(&fold, &[fold.previous_slot(second_pass).unwrap()]),
        ["2026-04-05T01:45+11:00"]
    );
}

#[test]
fn schedule_slots_between_capped() {
    let every = compile("*/1 * * * *", "UTC");
    let from = ts("2026-06-01T00:00:00Z");
    assert_eq!(
        every.slots_between(from, ts("2026-06-02T00:00:00Z"), 100),
        100
    );
    assert_eq!(
        every.slots_between(from, ts("2026-06-01T00:10:00Z"), 100),
        10
    );
    assert_eq!(every.slots_between(from, from, 100), 0);
}

#[test]
fn schedule_dst_notes_kyiv() {
    let kyiv = compile("30 3 * * *", "Europe/Kyiv");
    let notes = kyiv.dst_notes(ts("2026-01-01T00:00:00Z"), 366);
    assert_eq!(
        notes,
        [
            DstNote {
                date: "2026-03-29".into(),
                time: "03:30".into(),
                kind: DstNoteKind::Skipped,
                runs_at: Some("04:00".into()),
            },
            DstNote {
                date: "2026-10-25".into(),
                time: "03:30".into(),
                kind: DstNoteKind::Repeated,
                runs_at: None,
            },
        ]
    );
    let json = serde_json::to_value(&notes[0]).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "date": "2026-03-29", "time": "03:30", "kind": "skipped", "runs_at": "04:00"
        })
    );

    let wildcard = compile("*/30 * * * *", "Europe/Kyiv");
    let notes = wildcard.dst_notes(ts("2026-01-01T00:00:00Z"), 366);
    assert_eq!(notes[0].kind, DstNoteKind::Skipped);
    assert_eq!(notes[0].time, "03:00");
    assert_eq!(notes[0].runs_at, None);
    assert_eq!(notes[1].kind, DstNoteKind::Repeated);

    let lord_howe = compile("15 2 * * *", "Australia/Lord_Howe");
    let notes = lord_howe.dst_notes(ts("2026-01-01T00:00:00Z"), 366);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].runs_at.as_deref(), Some("02:30"));
}

#[test]
fn schedule_dst_notes_none() {
    let noon = compile("0 12 * * *", "Europe/Kyiv");
    assert!(noon.dst_notes(ts("2026-01-01T00:00:00Z"), 366).is_empty());
    let utc = compile("30 3 * * *", "UTC");
    assert!(utc.dst_notes(ts("2026-01-01T00:00:00Z"), 366).is_empty());
    let kyiv = compile("30 3 * * *", "Europe/Kyiv");
    assert!(kyiv.dst_notes(ts("2026-04-01T00:00:00Z"), 100).is_empty());
    assert!(kyiv.dst_notes(ts("2026-01-01T00:00:00Z"), 0).is_empty());
}

#[test]
fn schedule_aliases() {
    let after = ts("2026-06-03T10:20:00Z");
    for (alias, first) in [
        ("@hourly", "2026-06-03T11:00:00Z"),
        ("@daily", "2026-06-04T00:00:00Z"),
        ("@weekly", "2026-06-07T00:00:00Z"),
        ("@monthly", "2026-07-01T00:00:00Z"),
    ] {
        let compiled = compile(alias, "UTC");
        assert_eq!(compiled.next_slots(after, 1), [ts(first)], "{alias}");
    }
}

#[test]
fn schedule_parse_rate_and_duration() {
    for (text, secs) in [
        ("0s", 0),
        ("90s", 90),
        ("15m", 900),
        ("24h", 86_400),
        ("1d", 86_400),
        ("2w", 14 * 86_400),
        ("1y", 365 * 86_400),
    ] {
        assert_eq!(
            parse_duration(text).map(|d| d.as_secs()),
            Some(secs),
            "{text}"
        );
    }
    for bad in [
        "",
        "m",
        "1",
        "1.5h",
        "-1h",
        "+1h",
        "1 h",
        "1x",
        "99999999999999999999y",
        "18446744073709551615y",
    ] {
        assert_eq!(parse_duration(bad), None, "{bad}");
    }
    for (text, rate) in [
        ("1024B/s", 1024),
        ("1KiB/s", 1024),
        ("800KiB/s", 800 * 1024),
        ("50MiB/s", 52_428_800),
        ("1GiB/s", 1 << 30),
    ] {
        assert_eq!(parse_rate(text), Some(rate), "{text}");
    }
    for bad in [
        "",
        "1023B/s",
        "0B/s",
        "1KiB",
        "KiB/s",
        "50MB/s",
        "50mib/s",
        "1.5MiB/s",
        "-1KiB/s",
        "99999999999GiB/s",
        "1 KiB/s",
    ] {
        assert_eq!(parse_rate(bad), None, "{bad}");
    }
}

fn min_gap_year(cron: &str) -> Option<i64> {
    compile(cron, "UTC")
        .min_gap(
            ts("2026-11-06T00:00:00Z"),
            SignedDuration::from_hours(24 * 366),
            SignedDuration::from_secs(60),
            usize::MAX,
        )
        .smallest
        .map(|gap| gap.as_secs())
}

#[test]
fn schedule_min_gap_every_minute() {
    assert_eq!(min_gap_year("* * * * *"), Some(60));
}

#[test]
fn schedule_min_gap_daily() {
    assert_eq!(min_gap_year("0 3 * * *"), Some(24 * 3600));
}

#[test]
fn schedule_min_gap_weekly() {
    assert_eq!(min_gap_year("0 3 * * 1"), Some(7 * 24 * 3600));
}

#[test]
fn schedule_min_gap_month_restricted() {
    assert_eq!(min_gap_year("0 3 * 12 *"), Some(24 * 3600));
}

#[test]
fn schedule_min_gap_yearly_none() {
    assert_eq!(min_gap_year("0 3 1 1 *"), None);
}

#[test]
fn schedule_min_gap_dst() {
    let kyiv = compile("30 3 * * *", "Europe/Kyiv");
    let gap = kyiv.min_gap(
        ts("2026-10-24T00:00:00Z"),
        SignedDuration::from_hours(72),
        SignedDuration::from_secs(60),
        usize::MAX,
    );
    assert_eq!(gap.smallest, Some(SignedDuration::from_hours(24)));
}

#[test]
fn schedule_min_gap_stops_at_the_run_cap() {
    let found = compile("*/2 * * * *", "UTC").min_gap(
        ts("2026-11-06T00:00:00Z"),
        SignedDuration::from_hours(24 * 366),
        SignedDuration::from_secs(60),
        1000,
    );
    assert!(found.capped);
    assert_eq!(found.smallest, Some(SignedDuration::from_secs(120)));
}
