use super::inputs::Inputs;
use super::model::{AlertFacts, AlertType, CertKind, HostFacts, Severity, Thresholds, offline};

const DAY_MS: i64 = 86_400_000;

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
    out
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
}
