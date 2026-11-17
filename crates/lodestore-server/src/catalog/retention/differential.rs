// Frozen copy of decide from commit 83884fdf (main after 19.1, before phase 20); never fix it together with the code.
use std::collections::BTreeMap;

use lodestore_policy::retention::{Candidate, Decision, evaluate, timezone_of};
use lodestore_policy::{BackupPolicy, Retention, parse_yaml};
use proptest::collection::vec;
use proptest::prelude::*;

use super::{PairRow, Pool, decide};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PoolBefore {
    Scheduled,
    Manual,
    Stdin,
}

struct RowBefore {
    pool: PoolBefore,
    exact: bool,
    candidate: Candidate,
}

fn times_unconfirmed_before(rows: &[&RowBefore]) -> bool {
    rows.iter().any(|r| !r.exact)
}

fn decide_before_phase_20(
    policy: &BackupPolicy,
    rules: &Retention,
    rows: &[&RowBefore],
) -> Vec<Decision> {
    if times_unconfirmed_before(rows) {
        return rows
            .iter()
            .map(|r| Decision {
                id: r.candidate.id.clone(),
                keep: true,
                reasons: Vec::new(),
            })
            .collect();
    }
    let timezone = timezone_of(&policy.spec);
    let mut decisions: Vec<Decision> =
        [PoolBefore::Scheduled, PoolBefore::Manual, PoolBefore::Stdin]
            .into_iter()
            .flat_map(|pool| {
                let candidates: Vec<Candidate> = rows
                    .iter()
                    .filter(|r| r.pool == pool)
                    .map(|r| r.candidate.clone())
                    .collect();
                evaluate(rules, &timezone, &candidates)
            })
            .collect();
    let received: BTreeMap<&str, i64> = rows
        .iter()
        .map(|r| (r.candidate.id.as_str(), r.candidate.received_at_ms))
        .collect();
    decisions.sort_by(|a, b| {
        received[b.id.as_str()]
            .cmp(&received[a.id.as_str()])
            .then_with(|| a.id.cmp(&b.id))
    });
    decisions
}

const BASE_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;
const STREAM: &str = "etcd.snap";
const GUESTS: [&str; 3] = [
    "proxmox-vm/pve1/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    "proxmox-vm/pve1/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    "postgres/7300000000000000001/16384",
];

#[derive(Debug, Clone)]
struct Spec {
    pool: u8,
    hours: i64,
    exact: bool,
    suspicious: bool,
    incomplete: bool,
}

fn flag(rare: u32) -> impl Strategy<Value = bool> {
    prop_oneof![rare => Just(false), 1 => Just(true)]
}

fn spec() -> impl Strategy<Value = Spec> {
    (0u8..3, 0i64..6000, any::<bool>(), flag(7), flag(7)).prop_map(
        |(pool, hours, exact, suspicious, incomplete)| Spec {
            pool,
            hours,
            exact,
            suspicious,
            incomplete,
        },
    )
}

fn count() -> impl Strategy<Value = Option<i64>> {
    prop_oneof![Just(None), (1i64..=5).prop_map(Some)]
}

fn retention() -> impl Strategy<Value = Retention> {
    let within = prop_oneof![
        Just(None),
        Just(Some("1h".to_string())),
        Just(Some("2d".to_string())),
        Just(Some("30d".to_string())),
    ];
    (count(), within, count(), count(), count()).prop_map(
        |(keep_last, keep_within, keep_daily, keep_weekly, keep_monthly)| Retention {
            keep_last,
            keep_within,
            keep_daily,
            keep_weekly,
            keep_monthly,
            include_stdin: true,
        },
    )
}

fn policy(zone: &str) -> BackupPolicy {
    parse_yaml(&format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: diff\n\
         spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: test\n  \
         paths:\n    - /data\n  schedule:\n    cron: \"0 * * * *\"\n    timezone: {zone}\n"
    ))
    .unwrap()
}

fn zone() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just("UTC"), Just("Europe/Kyiv"), Just("America/New_York")]
}

fn candidate(index: usize, spec: &Spec) -> Candidate {
    Candidate {
        id: format!("{index:064x}"),
        received_at_ms: BASE_MS + spec.hours * HOUR_MS,
        suspicious: spec.suspicious,
        incomplete: spec.incomplete,
    }
}

fn stream(name: Option<&str>) -> Pool {
    Pool::Stdin(name.unwrap_or_default().to_string())
}

fn row(candidate: Candidate, pool: Pool, exact: bool, source_ref: &str) -> PairRow {
    PairRow {
        pool,
        host_id: "h".into(),
        hostname: "h".into(),
        time_ms: candidate.received_at_ms,
        exact,
        source_type: source_ref.split('/').next().unwrap_or_default().into(),
        source_ref: source_ref.into(),
        source_label: None,
        candidate,
    }
}

fn files_rows(
    specs: &[Spec],
    force_exact: bool,
    name: Option<&str>,
) -> (Vec<RowBefore>, Vec<PairRow>) {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let exact = force_exact || spec.exact;
        let (old, new) = match spec.pool {
            0 => (PoolBefore::Scheduled, Pool::Scheduled),
            1 => (PoolBefore::Manual, Pool::Manual),
            _ => (PoolBefore::Stdin, stream(name)),
        };
        before.push(RowBefore {
            pool: old,
            exact,
            candidate: candidate(index, spec),
        });
        let mut files = row(candidate(index, spec), new, exact, "");
        files.source_type = "files".into();
        after.push(files);
    }
    (before, after)
}

fn exactness() -> impl Strategy<Value = bool> {
    prop_oneof![4 => Just(true), 1 => Just(false)]
}

fn stream_name() -> impl Strategy<Value = Option<&'static str>> {
    prop_oneof![Just(None), Just(Some(STREAM))]
}

proptest! {
    #[test]
    fn files_only_catalogs_decide_like_before(
        specs in vec(spec(), 0..=60),
        force_exact in exactness(),
        name in stream_name(),
        rules in retention(),
        zone in zone(),
    ) {
        let policy = policy(zone);
        let (before, after) = files_rows(&specs, force_exact, name);
        let old_refs: Vec<&RowBefore> = before.iter().collect();
        let new_refs: Vec<&PairRow> = after.iter().collect();

        prop_assert_eq!(
            decide(&policy, &rules, &new_refs),
            decide_before_phase_20(&policy, &rules, &old_refs)
        );
    }

    #[test]
    fn other_instances_do_not_change_files_decisions(
        specs in vec(spec(), 0..=60),
        others in vec((spec(), 0usize..3), 0..=30),
        force_exact in exactness(),
        name in stream_name(),
        rules in retention(),
        zone in zone(),
    ) {
        let policy = policy(zone);
        let (_, mut mixed) = files_rows(&specs, force_exact, name);
        let alone: Vec<Decision> = {
            let refs: Vec<&PairRow> = mixed.iter().collect();
            decide(&policy, &rules, &refs)
        };
        for (offset, (spec, guest)) in others.iter().enumerate() {
            let pool = match spec.pool {
                0 => Pool::Scheduled,
                1 => Pool::Manual,
                _ => stream(name),
            };
            mixed.push(row(
                candidate(specs.len() + offset, spec),
                pool,
                spec.exact,
                GUESTS[*guest],
            ));
        }
        let refs: Vec<&PairRow> = mixed.iter().collect();

        let actual: Vec<Decision> = decide(&policy, &rules, &refs)
            .into_iter()
            .filter(|d| d.id < format!("{:064x}", specs.len()))
            .collect();

        prop_assert_eq!(actual, alone);
    }
}
