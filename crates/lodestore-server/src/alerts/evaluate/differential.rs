// Frozen copy of the pair loop of evaluate from commit 83884fdf (main after 19.1, before phase 20); never fix it together with the code.
use std::time::Duration;

use proptest::prelude::*;

use super::{Candidate, evaluate};
use crate::alerts::inputs::Inputs;
use crate::alerts::model::{
    AlertFacts, AlertType, HostFacts, OFFLINE_AFTER_DEFAULT, PairFacts, Severity, Thresholds,
};

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
const T: Thresholds = Thresholds {
    offline_after: OFFLINE_AFTER_DEFAULT,
    cert_days: 14,
};

fn age_ms(pair: &PairFacts, now_ms: i64) -> i64 {
    now_ms - pair.last_success_ms.unwrap_or(pair.baseline_ms)
}

fn pair_overdue(pair: &PairFacts, now_ms: i64) -> bool {
    pair.limit
        .is_some_and(|limit| age_ms(pair, now_ms) > limit.as_millis() as i64)
}

fn pair_candidates_before_phase_20(hosts: &[HostFacts], now_ms: i64) -> Vec<Candidate> {
    let mut out = Vec::new();
    for h in hosts {
        for pair in &h.pairs {
            if let Some((_, error)) = &pair.last_failure {
                out.push(Candidate {
                    policy: Some(pair.policy.clone()),
                    facts: AlertFacts {
                        error: Some(error.clone()),
                        ..AlertFacts::default()
                    },
                    ..Candidate::for_host(
                        h,
                        AlertType::BackupFailed,
                        format!("backup_failed:{}:{}", h.host_id, pair.policy),
                        Severity::Critical,
                    )
                });
            }
            if pair_overdue(pair, now_ms) {
                out.push(Candidate {
                    policy: Some(pair.policy.clone()),
                    facts: AlertFacts {
                        last_success_at: pair.last_success_ms,
                        limit_secs: pair.limit.map(|l| l.as_secs()),
                        ..AlertFacts::default()
                    },
                    ..Candidate::for_host(
                        h,
                        AlertType::NoRecentBackup,
                        format!("no_recent_backup:{}:{}", h.host_id, pair.policy),
                        Severity::Warning,
                    )
                });
            }
        }
    }
    out
}

fn files_pair() -> impl Strategy<Value = PairFacts> {
    (
        proptest::option::of(HOUR / 1000..=7 * DAY / 1000),
        proptest::option::of(0..=10 * DAY),
        0..=30 * DAY,
        proptest::option::of((0..=10 * DAY, "[a-z ]{0,12}")),
        proptest::option::of(0..=10 * DAY),
        proptest::option::of(0..=10 * DAY),
    )
        .prop_map(
            |(limit_secs, success_ago, baseline_ago, failure, gone_ago, returned_ago)| PairFacts {
                policy: String::new(),
                limit: limit_secs.map(|s| Duration::from_secs(s as u64)),
                last_success_ms: success_ago.map(|ago| NOW - ago),
                baseline_ms: NOW - baseline_ago,
                last_failure: failure.map(|(ago, text)| (NOW - ago, text)),
                source_ref: String::new(),
                source_label: None,
                expected: true,
                gone_at_ms: gone_ago.map(|ago| NOW - ago),
                returned_at_ms: returned_ago.map(|ago| NOW - ago),
            },
        )
}

fn host(index: usize, pairs: Vec<PairFacts>) -> HostFacts {
    HostFacts {
        host_id: format!("h{index}"),
        hostname: format!("web-{index}"),
        labels: vec![],
        platform: "linux / x86_64".into(),
        pairs: pairs
            .into_iter()
            .enumerate()
            .map(|(n, pair)| PairFacts {
                policy: format!("p{n}"),
                ..pair
            })
            .collect(),
        online: true,
        last_seen_ms: Some(NOW),
        cert_not_after_ms: NOW + 100 * DAY,
        last_snapshot: None,
    }
}

proptest! {
    #[test]
    fn files_pairs_alert_like_before(
        pairs in proptest::collection::vec(proptest::collection::vec(files_pair(), 0..=4), 1..=4),
    ) {
        let hosts: Vec<HostFacts> = pairs
            .into_iter()
            .enumerate()
            .map(|(index, pairs)| host(index, pairs))
            .collect();
        let inputs = Inputs { hosts: hosts.clone(), ..Inputs::default() };
        let now: Vec<Candidate> = evaluate(&inputs, NOW, 0, &T)
            .into_iter()
            .filter(|c| matches!(c.kind, AlertType::BackupFailed | AlertType::NoRecentBackup))
            .collect();
        prop_assert_eq!(now, pair_candidates_before_phase_20(&hosts, NOW));
    }
}
