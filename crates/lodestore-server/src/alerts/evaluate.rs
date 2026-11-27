use lodestore_format::store::probe::GuaranteeLevel;

use super::inputs::{Inputs, RepositoryGuarantee};
use super::model::{AlertFacts, AlertType, CertKind, HostFacts, Severity, Thresholds, offline};

const DAY_MS: i64 = 86_400_000;
pub const SECURITY_ALERT_WINDOW_MS: i64 = DAY_MS;
pub const GONE_AFTER_MS: i64 = 3_600_000;
pub const GONE_WINDOW_MS: i64 = DAY_MS;
pub const PLUGIN_KEY: &str = "pve_plugin_missing:";

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub kind: AlertType,
    pub key: String,
    pub severity: Severity,
    pub host_id: Option<String>,
    pub hostname: Option<String>,
    pub agent_online: bool,
    pub repository_id: Option<String>,
    pub repository_name: Option<String>,
    pub policy: Option<String>,
    pub check_level: Option<String>,
    pub cert: Option<CertKind>,
    pub facts: AlertFacts,
}

impl Candidate {
    fn for_host(h: &HostFacts, kind: AlertType, key: String, severity: Severity) -> Candidate {
        Candidate {
            kind,
            key,
            severity,
            host_id: Some(h.host_id.clone()),
            hostname: Some(h.hostname.clone()),
            agent_online: h.online,
            repository_id: None,
            repository_name: None,
            policy: None,
            check_level: None,
            cert: None,
            facts: AlertFacts::default(),
        }
    }
}

pub fn evaluate(
    inputs: &Inputs,
    now_ms: i64,
    process_started_ms: i64,
    thresholds: &Thresholds,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for h in &inputs.hosts {
        for pair in h.pairs.iter().filter(|p| !p.expected) {
            if let Some(gone_at) = pair.gone_at_ms
                && pair.returned_at_ms.is_none()
                && gone_at + GONE_AFTER_MS <= now_ms
                && now_ms < gone_at + GONE_AFTER_MS + GONE_WINDOW_MS
            {
                out.push(Candidate {
                    policy: Some(pair.policy.clone()),
                    facts: AlertFacts {
                        last_success_at: pair.last_success_ms,
                        source_label: pair.source_label.clone(),
                        ..AlertFacts::default()
                    },
                    ..Candidate::for_host(
                        h,
                        AlertType::SourceInstanceGone,
                        format!(
                            "source_instance_gone:{}:{}:{}",
                            h.host_id, pair.policy, pair.source_ref
                        ),
                        Severity::Warning,
                    )
                });
            }
        }
        for pair in h.pairs.iter().filter(|p| p.expected) {
            let label = (!pair.source_ref.is_empty())
                .then(|| pair.source_label.clone())
                .flatten();
            let suffix = match pair.source_ref.as_str() {
                "" => String::new(),
                instance => format!(":{instance}"),
            };
            if let Some((_, error)) = &pair.last_failure {
                out.push(Candidate {
                    policy: Some(pair.policy.clone()),
                    facts: AlertFacts {
                        error: Some(error.clone()),
                        source_label: label.clone(),
                        ..AlertFacts::default()
                    },
                    ..Candidate::for_host(
                        h,
                        AlertType::BackupFailed,
                        format!("backup_failed:{}:{}{suffix}", h.host_id, pair.policy),
                        Severity::Critical,
                    )
                });
            }
            if pair.pair_overdue(now_ms) {
                out.push(Candidate {
                    policy: Some(pair.policy.clone()),
                    facts: AlertFacts {
                        last_success_at: pair.last_success_ms,
                        limit_secs: pair.limit.map(|l| l.as_secs()),
                        source_label: label,
                        ..AlertFacts::default()
                    },
                    ..Candidate::for_host(
                        h,
                        AlertType::NoRecentBackup,
                        format!("no_recent_backup:{}:{}{suffix}", h.host_id, pair.policy),
                        Severity::Warning,
                    )
                });
            }
        }
        if offline(h, now_ms, process_started_ms, thresholds.offline_after) {
            out.push(Candidate {
                facts: AlertFacts {
                    last_seen_at: h.last_seen_ms,
                    ..AlertFacts::default()
                },
                ..Candidate::for_host(
                    h,
                    AlertType::AgentOffline,
                    format!("agent_offline:{}", h.host_id),
                    Severity::Warning,
                )
            });
        }
        if expiring(h.cert_not_after_ms, now_ms, thresholds) {
            out.push(Candidate {
                cert: Some(CertKind::Agent),
                facts: AlertFacts {
                    not_after: Some(h.cert_not_after_ms),
                    ..AlertFacts::default()
                },
                ..Candidate::for_host(
                    h,
                    AlertType::CertificateExpiring,
                    format!("certificate_expiring:agent:{}", h.host_id),
                    cert_severity(h.cert_not_after_ms, now_ms),
                )
            });
        }
    }
    if let Some((not_after_ms, external)) = inputs.server_cert
        && expiring(not_after_ms, now_ms, thresholds)
    {
        out.push(Candidate {
            kind: AlertType::CertificateExpiring,
            key: super::reconcile::GRPC_CERT_KEY.into(),
            severity: cert_severity(not_after_ms, now_ms),
            host_id: None,
            hostname: None,
            agent_online: false,
            repository_id: None,
            repository_name: None,
            policy: None,
            check_level: None,
            cert: Some(CertKind::Grpc),
            facts: AlertFacts {
                not_after: Some(not_after_ms),
                external: Some(external),
                ..AlertFacts::default()
            },
        });
    }
    for u in &inputs.unprotected {
        let Some(h) = inputs.hosts.iter().find(|h| h.host_id == u.host_id) else {
            continue;
        };
        if u.since_ms
            .is_some_and(|since| since + GONE_AFTER_MS <= now_ms)
        {
            out.push(Candidate {
                policy: u.policy.clone(),
                facts: AlertFacts {
                    last_success_at: u.last_snapshot_ms,
                    source_label: u.label.clone(),
                    ..AlertFacts::default()
                },
                ..Candidate::for_host(
                    h,
                    AlertType::GuestUnprotected,
                    format!("guest_unprotected:{}:{}", h.host_id, u.source_ref),
                    Severity::Warning,
                )
            });
        }
    }
    for finding in &inputs.plugins {
        let Some(h) = inputs.hosts.iter().find(|h| h.host_id == finding.host_id) else {
            continue;
        };
        out.push(Candidate {
            facts: AlertFacts {
                pve_node: Some(finding.node.clone()),
                pve_cluster: finding.in_cluster.then(|| finding.scope.clone()),
                plugin_outdated: finding.outdated.then_some(true),
                ..AlertFacts::default()
            },
            ..Candidate::for_host(
                h,
                AlertType::PvePluginMissing,
                format!("{PLUGIN_KEY}{}:{}", finding.scope, finding.node),
                Severity::Warning,
            )
        });
    }
    for failure in &inputs.update_failures {
        let Some(h) = inputs.hosts.iter().find(|h| h.host_id == failure.host_id) else {
            continue;
        };
        out.push(Candidate {
            facts: AlertFacts {
                error: Some(failure.reason.clone()),
                ..AlertFacts::default()
            },
            ..Candidate::for_host(
                h,
                AlertType::AgentUpdateFailed,
                format!("agent_update_failed:{}", h.host_id),
                Severity::Critical,
            )
        });
    }
    for s in &inputs.security {
        let Some(h) = inputs.hosts.iter().find(|h| h.host_id == s.host_id) else {
            continue;
        };
        if s.at_ms <= now_ms - SECURITY_ALERT_WINDOW_MS {
            continue;
        }
        let severity = match s.kind {
            AlertType::AgentCertificateRejected => Severity::Critical,
            _ => Severity::Warning,
        };
        out.push(Candidate {
            facts: s.facts.clone(),
            ..Candidate::for_host(
                h,
                s.kind,
                format!("{}:{}", s.kind.as_str(), h.host_id),
                severity,
            )
        });
    }
    for failure in &inputs.check_failures {
        let level = failure.level.as_str();
        out.push(Candidate {
            kind: AlertType::CheckFailed,
            key: format!("check_failed:{}:{level}", failure.repository_id),
            severity: Severity::Critical,
            host_id: None,
            hostname: None,
            agent_online: false,
            repository_id: Some(failure.repository_id.clone()),
            repository_name: Some(failure.repository.clone()),
            policy: None,
            check_level: Some(level.into()),
            cert: None,
            facts: AlertFacts {
                findings: Some(failure.findings),
                error: failure.error.clone(),
                ..AlertFacts::default()
            },
        });
    }
    for q in &inputs.quarantines {
        out.push(Candidate {
            facts: AlertFacts {
                findings: Some(q.count),
                error: Some(format!("{}: {}", q.first_key, q.first_reason)),
                ..AlertFacts::default()
            },
            ..for_repository(
                &q.repository_id,
                &q.repository,
                AlertType::SnapshotQuarantined,
                format!("snapshot_quarantined:{}", q.repository_id),
            )
        });
    }
    for r in &inputs.unavailable {
        out.push(Candidate {
            facts: AlertFacts {
                error: Some(r.error.clone()),
                ..AlertFacts::default()
            },
            ..for_repository(
                &r.repository_id,
                &r.repository,
                AlertType::RepositoryUnavailable,
                format!("repository_unavailable:{}", r.repository_id),
            )
        });
    }
    for g in &inputs.guarantees {
        if let Some((pinned, offered)) = &g.host_key_mismatch {
            out.push(Candidate {
                facts: AlertFacts {
                    pinned_fingerprint: Some(pinned.clone()),
                    offered_fingerprint: Some(offered.clone()),
                    ..AlertFacts::default()
                },
                ..for_repository(
                    &g.repository_id,
                    &g.repository,
                    AlertType::HostKeyChanged,
                    format!("host_key_changed:{}", g.repository_id),
                )
            });
        }
        let Some(reason) = guarantee_reason(g, now_ms, process_started_ms, inputs.daily_recheck)
        else {
            continue;
        };
        out.push(Candidate {
            severity: Severity::Warning,
            facts: AlertFacts {
                guarantee_level: Some(g.level.clone()),
                guarantee_accepted: Some(g.accepted.clone()),
                guarantee_reason: Some(reason.into()),
                agent_cannot_write: g.agent_cannot_write.then_some(true),
                unchecked_runs: Some(g.unchecked_runs),
                failed_runs: Some(g.failed_runs),
                ..AlertFacts::default()
            },
            ..for_repository(
                &g.repository_id,
                &g.repository,
                AlertType::GuaranteeDowngraded,
                format!("guarantee_downgraded:{}", g.repository_id),
            )
        });
    }
    out
}

fn rank(level: &str) -> Option<u8> {
    GuaranteeLevel::parse(level)
        .unwrap_or(GuaranteeLevel::Unchecked)
        .rank()
}

/// Daily rechecks in a row that measured nothing before the alert opens.
pub const INCONCLUSIVE_RUNS: u64 = 3;
/// Daily rechecks in a row that failed before storing a result before the alert opens.
pub const FAILED_RUNS: u64 = 3;
pub const NOT_RUNNING_AFTER_MS: i64 = 2 * DAY_MS;

/// Why a repository's storage protection deserves an alert, if it does. The
/// first match wins: `lower`, `weak_unaccepted`, `inconclusive`,
/// `recheck_failing`, `recheck_not_running`. A host key change explains a
/// missing measurement, so it silences `inconclusive` and `recheck_failing`.
pub fn guarantee_reason(
    g: &RepositoryGuarantee,
    now_ms: i64,
    process_started_ms: i64,
    daily_recheck: bool,
) -> Option<&'static str> {
    let (measured, accepted) = (rank(&g.level), rank(&g.accepted));
    if matches!((measured, accepted), (Some(m), Some(a)) if m < a) {
        Some("lower")
    } else if accepted.is_none() && measured == Some(0) {
        Some("weak_unaccepted")
    } else if g.host_key_mismatch.is_none() && g.unchecked_runs >= INCONCLUSIVE_RUNS {
        Some("inconclusive")
    } else if g.host_key_mismatch.is_none() && g.failed_runs >= FAILED_RUNS {
        Some("recheck_failing")
    } else {
        // The process start stands in only until the daily recheck first ran,
        // so frequent restarts cannot hide one that stopped running. Marks from
        // a clock that ran ahead count as missing for the same reason.
        let past = |at: i64| (at <= now_ms).then_some(at);
        let alive_since = g
            .tick_at_ms
            .and_then(past)
            .unwrap_or(process_started_ms)
            .max(past(g.measured_at_ms).unwrap_or(i64::MIN));
        (daily_recheck && now_ms - alive_since > NOT_RUNNING_AFTER_MS)
            .then_some("recheck_not_running")
    }
}

fn for_repository(id: &str, name: &str, kind: AlertType, key: String) -> Candidate {
    Candidate {
        kind,
        key,
        severity: Severity::Critical,
        host_id: None,
        hostname: None,
        agent_online: false,
        repository_id: Some(id.into()),
        repository_name: Some(name.into()),
        policy: None,
        check_level: None,
        cert: None,
        facts: AlertFacts::default(),
    }
}

fn expiring(not_after_ms: i64, now_ms: i64, thresholds: &Thresholds) -> bool {
    not_after_ms - now_ms < i64::from(thresholds.cert_days) * DAY_MS
}

fn cert_severity(not_after_ms: i64, now_ms: i64) -> Severity {
    if not_after_ms <= now_ms {
        Severity::Critical
    } else {
        Severity::Warning
    }
}

#[cfg(test)]
mod differential;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::alerts::model::{OFFLINE_AFTER_DEFAULT, PairFacts};

    const NOW: i64 = 1_800_000_000_000;
    const HOUR: i64 = 3_600_000;
    const T: Thresholds = Thresholds {
        offline_after: OFFLINE_AFTER_DEFAULT,
        cert_days: 14,
    };

    fn pair(failure: Option<&str>, success_ago_h: Option<i64>) -> PairFacts {
        PairFacts {
            policy: "daily".into(),
            limit: Some(Duration::from_secs(48 * 3600)),
            last_success_ms: success_ago_h.map(|h| NOW - h * HOUR),
            baseline_ms: NOW - 1000 * HOUR,
            last_failure: failure.map(|e| (NOW, e.into())),
            source_ref: String::new(),
            source_label: None,
            expected: true,
            gone_at_ms: None,
            returned_at_ms: None,
        }
    }

    fn instance(source_ref: &str, success_ago_h: i64) -> PairFacts {
        PairFacts {
            source_ref: source_ref.into(),
            ..pair(None, Some(success_ago_h))
        }
    }

    fn host(pairs: Vec<PairFacts>) -> HostFacts {
        HostFacts {
            host_id: "h1".into(),
            hostname: "web-1".into(),
            labels: vec![],
            platform: "linux / x86_64".into(),
            pairs,
            online: true,
            last_seen_ms: Some(NOW),
            cert_not_after_ms: NOW + 100 * DAY_MS,
            last_snapshot: None,
        }
    }

    fn run(h: HostFacts, server_cert: Option<(i64, bool)>) -> Vec<Candidate> {
        let inputs = Inputs {
            hosts: vec![h],
            server_cert,
            ..Inputs::default()
        };
        evaluate(&inputs, NOW, 0, &T)
    }

    fn keys(c: &[Candidate]) -> Vec<&str> {
        c.iter().map(|c| c.key.as_str()).collect()
    }

    #[test]
    fn failed_and_overdue_pairs_raise_separate_alerts() {
        let out = run(host(vec![pair(Some("boom"), Some(100))]), None);
        assert_eq!(
            keys(&out),
            ["backup_failed:h1:daily", "no_recent_backup:h1:daily"]
        );
        assert_eq!(out[0].severity, Severity::Critical);
        assert_eq!(out[0].facts.error.as_deref(), Some("boom"));
        assert_eq!(out[1].severity, Severity::Warning);
        assert_eq!(out[1].facts.limit_secs, Some(48 * 3600));
        assert!(run(host(vec![pair(None, Some(1))]), None).is_empty());
    }

    #[test]
    fn files_alert_keys_are_unchanged() {
        let out = run(host(vec![pair(Some("boom"), Some(100))]), None);
        assert_eq!(
            keys(&out),
            ["backup_failed:h1:daily", "no_recent_backup:h1:daily"]
        );
    }

    #[test]
    fn one_of_40_instances_overdue_raises_one_alert() {
        let pairs = (0..40)
            .map(|n| {
                instance(
                    &format!("proxmox-vm/pve1/{n:02}"),
                    if n == 17 { 100 } else { 1 },
                )
            })
            .collect();
        let out = run(host(pairs), None);
        assert_eq!(keys(&out), ["no_recent_backup:h1:daily:proxmox-vm/pve1/17"]);
    }

    fn proxmox_policy() -> lodestore_policy::BackupPolicy {
        lodestore_policy::parse_yaml(
            "apiVersion: lodestore.io/v1alpha2\nkind: BackupPolicy\nmetadata:\n  name: p\n\
             spec:\n  repository: main\n  source:\n    type: proxmox-vm\n    vmids: [101]\n",
        )
        .unwrap()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn expected_instances_union_failures_and_stub() {
        use crate::alerts::inputs::{Discovered, expected_instances};
        let guests = proxmox_policy();
        let files = lodestore_policy::parse_yaml(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: p\n\
             spec:\n  repository: main\n  paths: [/etc]\n",
        )
        .unwrap();
        let found = |keys: &[&str], complete| Discovered {
            keys: strings(keys),
            complete,
        };
        assert_eq!(
            expected_instances(
                &guests,
                Some(&found(&["B", "A"], true)),
                &strings(&["C"]),
                &strings(&[""])
            ),
            strings(&["", "A", "B"])
        );
        assert_eq!(
            expected_instances(&guests, Some(&found(&["A"], false)), &strings(&["C"]), &[]),
            strings(&["A", "C"])
        );
        assert_eq!(
            expected_instances(&guests, Some(&found(&[], true)), &[], &[]),
            strings(&[""])
        );
        assert_eq!(
            expected_instances(&guests, None, &strings(&["C"]), &[]),
            strings(&["C"])
        );
        assert_eq!(
            expected_instances(&files, Some(&found(&["A"], true)), &strings(&["C"]), &[]),
            strings(&[""])
        );
    }

    #[test]
    fn unknown_is_not_empty() {
        use crate::alerts::inputs::{Discovered, expected_instances};
        let guests = proxmox_policy();
        let known = strings(&["C"]);
        let unknown = expected_instances(&guests, None, &known, &[]);
        let nothing = expected_instances(
            &guests,
            Some(&Discovered {
                keys: vec![],
                complete: true,
            }),
            &known,
            &[],
        );
        assert_eq!(unknown, strings(&["C"]));
        assert_eq!(nothing, strings(&[""]));
    }

    #[test]
    fn instances_no_longer_expected_stay_quiet() {
        use crate::alerts::inputs::instance_facts;
        use crate::alerts::model::{HostState, OFFLINE_AFTER_DEFAULT};
        let known = [
            ("A".to_string(), NOW - HOUR, None),
            ("B".to_string(), NOW - 30 * 24 * HOUR, None),
        ];
        let pairs = instance_facts(
            "daily",
            Some(Duration::from_secs(48 * 3600)),
            NOW - 1000 * HOUR,
            &["A".to_string()],
            &known,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(pairs.len(), 2);
        assert!(!pairs[1].expected);
        for later in [NOW, NOW + 365 * 24 * HOUR] {
            let mut h = host(pairs.clone());
            h.last_seen_ms = Some(later);
            let inputs = Inputs {
                hosts: vec![h.clone()],
                ..Inputs::default()
            };
            let out = evaluate(&inputs, later, 0, &T);
            let backups: Vec<_> = out
                .iter()
                .filter(|c| c.key.contains(":B") || c.kind == AlertType::BackupFailed)
                .collect();
            assert!(backups.is_empty(), "{backups:?}");
            if later == NOW {
                assert_ne!(
                    crate::alerts::model::host_state(&h, later, 0, OFFLINE_AFTER_DEFAULT),
                    HostState::Overdue
                );
            }
        }
    }

    fn gone_pair(
        limit: Option<u64>,
        success_ago_h: Option<i64>,
        gone_at: Option<i64>,
        returned_at: Option<i64>,
    ) -> PairFacts {
        let mut p = pair(None, success_ago_h);
        p.source_ref = "proxmox-vm/pve1/7".into();
        p.source_label = Some("VM 101 \u{b7} web".into());
        p.limit = limit.map(|h| Duration::from_secs(h * 3600));
        p.expected = false;
        p.gone_at_ms = gone_at;
        p.returned_at_ms = returned_at;
        p
    }

    #[test]
    fn gone_instance_raises_one_event_for_a_day() {
        let g = NOW - 30 * DAY_MS;
        for limit in [None, Some(48)] {
            for success in [None, Some(30 * 24), Some(1)] {
                let p = gone_pair(limit, success, Some(g), None);
                for (at, raised) in [
                    (g + HOUR, true),
                    (g + 24 * HOUR, true),
                    (g + 25 * HOUR - 1, true),
                    (g + 25 * HOUR, false),
                    (g + 26 * HOUR, false),
                    (g, false),
                ] {
                    let inputs = Inputs {
                        hosts: vec![host(vec![p.clone()])],
                        ..Inputs::default()
                    };
                    let out = evaluate(&inputs, at, at, &T);
                    if raised {
                        assert_eq!(
                            keys(&out),
                            ["source_instance_gone:h1:daily:proxmox-vm/pve1/7"],
                            "{limit:?} {success:?} {at}"
                        );
                        assert_eq!(out[0].severity, Severity::Warning);
                        assert_eq!(
                            out[0].facts.source_label.as_deref(),
                            Some("VM 101 \u{b7} web")
                        );
                    } else {
                        assert!(out.is_empty(), "{out:?}");
                    }
                }
            }
        }
        let inputs = Inputs {
            hosts: vec![host(vec![gone_pair(Some(48), Some(1), None, None)])],
            ..Inputs::default()
        };
        assert!(evaluate(&inputs, NOW, NOW, &T).is_empty());
    }

    #[test]
    fn gone_event_waits_an_hour_and_is_quiet_after_a_return() {
        let t = NOW - 10 * DAY_MS;
        let raised = |returned: Option<i64>, at: i64| {
            let inputs = Inputs {
                hosts: vec![host(vec![gone_pair(Some(48), Some(1), Some(t), returned)])],
                ..Inputs::default()
            };
            !evaluate(&inputs, at, at, &T).is_empty()
        };
        assert!(!raised(None, t + 59 * 60_000));
        assert!(raised(None, t + 61 * 60_000));
        assert!(!raised(Some(t + 2 * HOUR), t + 61 * 60_000));
        assert!(!raised(None, t + HOUR + 24 * HOUR));
    }

    #[test]
    fn offline_and_certificates() {
        let mut h = host(vec![]);
        h.online = false;
        h.last_seen_ms = Some(NOW - 16 * 60_000);
        h.cert_not_after_ms = NOW - HOUR;
        let out = run(h, Some((NOW + 3 * DAY_MS, true)));
        assert_eq!(
            keys(&out),
            [
                "agent_offline:h1",
                "certificate_expiring:agent:h1",
                "certificate_expiring:grpc"
            ]
        );
        assert_eq!(out[1].severity, Severity::Critical);
        assert_eq!(out[2].severity, Severity::Warning);
        assert_eq!(out[2].host_id, None);
        assert_eq!(out[2].facts.external, Some(true));
    }

    fn security(
        kind: AlertType,
        host_id: &str,
        at_ms: i64,
    ) -> crate::alerts::inputs::SecurityAlert {
        crate::alerts::inputs::SecurityAlert {
            host_id: host_id.into(),
            kind,
            at_ms,
            facts: AlertFacts {
                serial: Some("abc".into()),
                ip: Some("203.0.113.9".into()),
                ..AlertFacts::default()
            },
        }
    }

    #[test]
    fn security_events_raise_alerts_for_a_day_on_known_hosts_only() {
        let inputs = Inputs {
            hosts: vec![host(vec![])],
            security: vec![
                security(AlertType::AgentSessionConflict, "h1", NOW - HOUR),
                security(AlertType::AgentCertificateRejected, "h1", NOW - 23 * HOUR),
                security(AlertType::AgentCertificateRejected, "gone", NOW),
            ],
            ..Inputs::default()
        };
        let out = evaluate(&inputs, NOW, NOW, &T);
        assert_eq!(
            keys(&out),
            ["agent_session_conflict:h1", "agent_certificate_rejected:h1"]
        );
        assert_eq!(out[0].severity, Severity::Warning);
        assert_eq!(out[1].severity, Severity::Critical);
        assert_eq!(out[1].facts.serial.as_deref(), Some("abc"));
        let later = evaluate(&inputs, NOW + 2 * HOUR, 0, &T);
        assert_eq!(keys(&later), ["agent_session_conflict:h1"]);
        let much_later = evaluate(&inputs, NOW + 25 * HOUR, 0, &T);
        assert!(much_later.is_empty());
    }

    const LEVELS: [&str; 4] = [
        "create_only_enforced",
        "overwrite_keeps_old_version",
        "host_can_overwrite_or_delete",
        "unchecked",
    ];

    fn guarantee(level: &str, accepted: &str, unchecked: u64, failed: u64) -> RepositoryGuarantee {
        RepositoryGuarantee {
            repository_id: "r1".into(),
            repository: "main".into(),
            level: level.into(),
            accepted: accepted.into(),
            unchecked_runs: unchecked,
            failed_runs: failed,
            measured_at_ms: NOW,
            tick_at_ms: Some(NOW),
            host_key_mismatch: None,
            agent_cannot_write: false,
        }
    }

    fn mismatched(g: RepositoryGuarantee) -> RepositoryGuarantee {
        RepositoryGuarantee {
            host_key_mismatch: Some(("SHA256:old".into(), "SHA256:new".into())),
            ..g
        }
    }

    #[test]
    fn guarantee_reason_follows_the_table() {
        // rows: measured level, columns: accepted level, both in `LEVELS` order
        let base: [[Option<&str>; 4]; 4] = [
            [None, None, None, None],
            [Some("lower"), None, None, None],
            [Some("lower"), Some("lower"), None, Some("weak_unaccepted")],
            [None, None, None, None],
        ];
        // [unchecked 0, 2, 3][failed 0, 2, 3] without and with a host key change
        let counts: [[[Option<&str>; 3]; 3]; 2] = [
            [
                [None, None, Some("recheck_failing")],
                [None, None, Some("recheck_failing")],
                [
                    Some("inconclusive"),
                    Some("inconclusive"),
                    Some("inconclusive"),
                ],
            ],
            [[None; 3], [None; 3], [None; 3]],
        ];
        let mut rows = 0;
        for (m, level) in LEVELS.iter().enumerate() {
            for (a, accepted) in LEVELS.iter().enumerate() {
                for (u, unchecked) in [0, 2, 3].into_iter().enumerate() {
                    for (f, failed) in [0, 2, 3].into_iter().enumerate() {
                        for (key_changed, counts) in counts.iter().enumerate() {
                            let expected = base[m][a].or(counts[u][f]);
                            let g = guarantee(level, accepted, unchecked, failed);
                            let g = if key_changed == 1 { mismatched(g) } else { g };
                            assert_eq!(
                                guarantee_reason(&g, NOW, 0, true),
                                expected,
                                "{level} {accepted} {unchecked} {failed} {key_changed}"
                            );
                            rows += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(rows, 288);
    }

    #[test]
    fn recheck_not_running_follows_the_clock() {
        let level = "overwrite_keeps_old_version";
        let g = |measured: i64, tick: Option<i64>| RepositoryGuarantee {
            measured_at_ms: measured,
            tick_at_ms: tick,
            ..guarantee(level, level, 0, 0)
        };
        let reason = |g: &RepositoryGuarantee, now: i64, started: i64, daily: bool| {
            guarantee_reason(g, now, started, daily)
        };
        let ran = g(NOW, Some(NOW));
        assert_eq!(reason(&ran, NOW + 48 * HOUR, 0, false), None);
        assert_eq!(reason(&ran, NOW + 48 * HOUR, 0, true), None);
        assert_eq!(
            reason(&ran, NOW + 48 * HOUR + 1, 0, true),
            Some("recheck_not_running")
        );
        assert_eq!(reason(&ran, NOW + 48 * HOUR + 1, 0, false), None);

        let old = g(NOW - 8760 * HOUR, None);
        assert_eq!(reason(&old, NOW, NOW - HOUR, true), None);
        assert_eq!(
            reason(&old, NOW + 48 * HOUR, NOW - HOUR, true),
            Some("recheck_not_running")
        );
        assert_eq!(reason(&old, NOW + 47 * HOUR, NOW - HOUR, true), None);

        let fresh_tick = g(NOW - 100 * HOUR, Some(NOW - HOUR));
        assert_eq!(reason(&fresh_tick, NOW, 0, true), None);

        let restarted = g(NOW - 100 * HOUR, Some(NOW - 49 * HOUR));
        assert_eq!(
            reason(&restarted, NOW, NOW - HOUR, true),
            Some("recheck_not_running"),
            "a restart does not hide a tick that stopped running"
        );

        let future = g(NOW + 720 * HOUR, Some(NOW + 720 * HOUR));
        assert_eq!(
            reason(&future, NOW + 48 * HOUR + 1, NOW, true),
            Some("recheck_not_running"),
            "marks from a clock that ran ahead do not hide a tick that stopped running"
        );
        assert_eq!(reason(&future, NOW + 47 * HOUR, NOW, true), None);

        let lowered = RepositoryGuarantee {
            level: "host_can_overwrite_or_delete".into(),
            ..g(NOW - 100 * HOUR, None)
        };
        assert_eq!(reason(&lowered, NOW, 0, true), Some("lower"));
    }

    #[test]
    fn host_key_change_suppresses_missing_measurements() {
        let ok = "overwrite_keeps_old_version";
        let keys_of = |g: RepositoryGuarantee| {
            let inputs = Inputs {
                guarantees: vec![mismatched(g)],
                daily_recheck: true,
                ..Inputs::default()
            };
            evaluate(&inputs, NOW, 0, &T)
        };
        let out = keys_of(guarantee("host_can_overwrite_or_delete", ok, 0, 0));
        assert_eq!(
            keys(&out),
            ["host_key_changed:r1", "guarantee_downgraded:r1"]
        );
        assert_eq!(out[0].severity, Severity::Critical);
        assert_eq!(
            out[0].facts.pinned_fingerprint.as_deref(),
            Some("SHA256:old")
        );
        assert_eq!(
            out[0].facts.offered_fingerprint.as_deref(),
            Some("SHA256:new")
        );
        assert_eq!(out[0].facts.error, None);
        assert_eq!(
            keys(&keys_of(guarantee(ok, ok, 3, 0))),
            ["host_key_changed:r1"]
        );
        assert_eq!(
            keys(&keys_of(guarantee(ok, ok, 0, 3))),
            ["host_key_changed:r1"]
        );
        let stale = RepositoryGuarantee {
            tick_at_ms: Some(NOW - 49 * HOUR),
            measured_at_ms: NOW - 49 * HOUR,
            ..guarantee(ok, ok, 0, 0)
        };
        let out = keys_of(stale);
        assert_eq!(
            keys(&out),
            ["host_key_changed:r1", "guarantee_downgraded:r1"]
        );
        assert_eq!(
            out[1].facts.guarantee_reason.as_deref(),
            Some("recheck_not_running")
        );
    }

    #[test]
    fn guarantee_alerts_are_warnings_keyed_by_repository() {
        let inputs = Inputs {
            guarantees: vec![guarantee(
                "host_can_overwrite_or_delete",
                "overwrite_keeps_old_version",
                0,
                0,
            )],
            ..Inputs::default()
        };
        let out = evaluate(&inputs, NOW, 0, &T);
        assert_eq!(keys(&out), ["guarantee_downgraded:r1"]);
        assert_eq!(out[0].severity, Severity::Warning);
        assert_eq!(out[0].facts.guarantee_reason.as_deref(), Some("lower"));
        assert_eq!(out[0].facts.error, None);
        assert_eq!(out[0].facts.agent_cannot_write, None);

        let inputs = Inputs {
            guarantees: vec![RepositoryGuarantee {
                agent_cannot_write: true,
                ..inputs.guarantees[0].clone()
            }],
            ..Inputs::default()
        };
        let out = evaluate(&inputs, NOW, 0, &T);
        assert_eq!(out[0].facts.agent_cannot_write, Some(true));
    }
}
