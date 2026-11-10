use super::inputs::Inputs;
use super::model::{AlertFacts, AlertType, CertKind, HostFacts, Severity, Thresholds, offline};

const DAY_MS: i64 = 86_400_000;
pub const SECURITY_ALERT_WINDOW_MS: i64 = DAY_MS;

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
            if pair.pair_overdue(now_ms) {
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
    out
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
}
