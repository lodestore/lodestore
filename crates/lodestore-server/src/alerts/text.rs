use lodestore_policy::jiff::Timestamp;

use super::evaluate::Candidate;
use super::model::{AlertType, CertKind};

const DAY_MS: i64 = 86_400_000;

pub fn alertname(kind: AlertType) -> &'static str {
    match kind {
        AlertType::BackupFailed => "LodestoreBackupFailed",
        AlertType::NoRecentBackup => "LodestoreNoRecentBackup",
        AlertType::AgentOffline => "LodestoreAgentOffline",
        AlertType::CheckFailed => "LodestoreCheckFailed",
        AlertType::CertificateExpiring => "LodestoreCertificateExpiring",
        AlertType::AgentUpdateFailed => "LodestoreAgentUpdateFailed",
        AlertType::SnapshotQuarantined => "LodestoreSnapshotQuarantined",
        AlertType::RepositoryUnavailable => "LodestoreRepositoryUnavailable",
    }
}

pub fn title(c: &Candidate, now_ms: i64) -> String {
    let host = c.hostname.as_deref().unwrap_or("unknown host");
    match c.kind {
        AlertType::BackupFailed => format!("Backup failed on {host}"),
        AlertType::NoRecentBackup => format!("No recent backup on {host}"),
        AlertType::AgentOffline => format!("{host} is offline"),
        AlertType::AgentUpdateFailed => format!("Agent update failed on {host}"),
        AlertType::CheckFailed => format!(
            "{} check failed on {}",
            level_label(c.check_level.as_deref()),
            c.repository_name.as_deref().unwrap_or("unknown repository"),
        ),
        AlertType::SnapshotQuarantined => {
            let n = i64::try_from(c.facts.findings.unwrap_or(0)).unwrap_or(i64::MAX);
            format!(
                "{n} {} in {} cannot be read; cleanup is stopped",
                plural(n, "snapshot"),
                c.repository_name.as_deref().unwrap_or("unknown repository"),
            )
        }
        AlertType::RepositoryUnavailable => format!(
            "Repository {} is unavailable",
            c.repository_name.as_deref().unwrap_or("unknown repository"),
        ),
        AlertType::CertificateExpiring => {
            let name = match c.cert {
                Some(CertKind::Grpc) => "Server gRPC certificate".to_string(),
                _ => format!("Agent certificate on {host}"),
            };
            let not_after = c.facts.not_after.unwrap_or(now_ms);
            if not_after <= now_ms {
                format!("{name} has expired")
            } else {
                let days = (not_after - now_ms + DAY_MS - 1) / DAY_MS;
                format!("{name} expires in {days} {}", plural(days, "day"))
            }
        }
    }
}

pub fn detail(c: &Candidate) -> String {
    match c.kind {
        AlertType::BackupFailed => format!(
            "Policy {}: {}.",
            c.policy.as_deref().unwrap_or("unknown"),
            c.facts
                .error
                .as_deref()
                .unwrap_or("The backup failed")
                .trim_end_matches('.'),
        ),
        AlertType::NoRecentBackup => {
            let last = match c.facts.last_success_at {
                Some(ms) => format!("Last success {}.", utc(ms)),
                None => "No successful backup yet.".to_string(),
            };
            match c.facts.limit_secs {
                Some(secs) => format!(
                    "{last} Expected at least every {}.",
                    duration(i64::try_from(secs).unwrap_or(i64::MAX))
                ),
                None => last,
            }
        }
        AlertType::AgentOffline => match c.facts.last_seen_at {
            Some(ms) => format!("Last contact {}.", utc(ms)),
            None => "The agent has not connected yet.".to_string(),
        },
        AlertType::AgentUpdateFailed => c
            .facts
            .error
            .clone()
            .unwrap_or_else(|| "The update was rolled back.".to_string()),
        AlertType::CheckFailed => {
            let n = i64::try_from(c.facts.findings.unwrap_or(0)).unwrap_or(i64::MAX);
            if let (0, Some(error)) = (n, c.facts.error.as_deref()) {
                return format!("{error} Open the report in Integrity checks.");
            }
            format!(
                "{n} {} found. Open the report in Integrity checks.",
                plural(n, "problem")
            )
        }
        AlertType::SnapshotQuarantined => format!(
            "Review them under Free up space on the repository page: exclude a snapshot the cleanup should skip, or repair or delete it. First: {}.",
            c.facts
                .error
                .as_deref()
                .unwrap_or("unknown")
                .trim_end_matches('.'),
        ),
        AlertType::RepositoryUnavailable => format!(
            "The server does not open it: {}. Cleanup, checks, restores and browsing are stopped for this repository.",
            c.facts
                .error
                .as_deref()
                .unwrap_or("unknown")
                .trim_end_matches('.'),
        ),
        AlertType::CertificateExpiring => match (c.cert, c.facts.external) {
            (Some(CertKind::Grpc), Some(true)) => {
                "The certificate comes from grpc.tls_cert_file. Replace it before it expires."
                    .to_string()
            }
            (Some(CertKind::Grpc), _) => {
                "The built-in certificate is renewed when the server restarts.".to_string()
            }
            _ if c.agent_online => "Renews automatically while the agent is online.".to_string(),
            _ => "The agent is offline and cannot renew it.".to_string(),
        },
    }
}

fn level_label(level: Option<&str>) -> &'static str {
    match level {
        Some("structure") => "Structure",
        Some("packs") => "Pack reading",
        Some("restore") => "Test restore",
        _ => "Integrity",
    }
}

fn plural(n: i64, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn utc(ms: i64) -> String {
    Timestamp::from_millisecond(ms)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|_| "at an unknown time".to_string())
}

fn duration(secs: i64) -> String {
    let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    let parts: Vec<String> = [(days, "day"), (hours, "hour"), (minutes, "minute")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n} {}", plural(n, unit)))
        .collect();
    if parts.is_empty() {
        "minute".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::model::{AlertFacts, Severity};

    const NOW: i64 = 1_800_000_000_000;

    fn candidate(kind: AlertType, facts: AlertFacts) -> Candidate {
        Candidate {
            kind,
            key: "k".into(),
            severity: Severity::Warning,
            host_id: Some("h".into()),
            hostname: Some("web-1".into()),
            agent_online: true,
            repository_id: Some("r".into()),
            repository_name: Some("main".into()),
            policy: Some("daily".into()),
            check_level: Some("structure".into()),
            cert: Some(CertKind::Agent),
            facts,
        }
    }

    fn text(kind: AlertType, facts: AlertFacts) -> (String, String) {
        let c = candidate(kind, facts);
        (title(&c, NOW), detail(&c))
    }

    #[test]
    fn alert_names() {
        assert_eq!(alertname(AlertType::BackupFailed), "LodestoreBackupFailed");
        assert_eq!(
            alertname(AlertType::NoRecentBackup),
            "LodestoreNoRecentBackup"
        );
        assert_eq!(alertname(AlertType::AgentOffline), "LodestoreAgentOffline");
        assert_eq!(alertname(AlertType::CheckFailed), "LodestoreCheckFailed");
        assert_eq!(
            alertname(AlertType::CertificateExpiring),
            "LodestoreCertificateExpiring"
        );
        assert_eq!(
            alertname(AlertType::AgentUpdateFailed),
            "LodestoreAgentUpdateFailed"
        );
        assert_eq!(
            alertname(AlertType::SnapshotQuarantined),
            "LodestoreSnapshotQuarantined"
        );
        assert_eq!(
            alertname(AlertType::RepositoryUnavailable),
            "LodestoreRepositoryUnavailable"
        );
    }

    #[test]
    fn titles_and_details() {
        let failed = AlertFacts {
            error: Some("disk full.".into()),
            ..AlertFacts::default()
        };
        assert_eq!(
            text(AlertType::BackupFailed, failed),
            (
                "Backup failed on web-1".into(),
                "Policy daily: disk full.".into()
            )
        );
        let overdue = AlertFacts {
            last_success_at: Some(NOW - 3 * DAY_MS),
            limit_secs: Some(2 * 86_400),
            ..AlertFacts::default()
        };
        let (title, detail) = text(AlertType::NoRecentBackup, overdue);
        assert_eq!(title, "No recent backup on web-1");
        assert!(
            detail.ends_with("Expected at least every 2 days."),
            "{detail}"
        );
        let (title, detail) = text(
            AlertType::AgentOffline,
            AlertFacts {
                last_seen_at: Some(NOW),
                ..AlertFacts::default()
            },
        );
        assert_eq!(title, "web-1 is offline");
        assert!(detail.starts_with("Last contact 2027-"), "{detail}");
        let findings = AlertFacts {
            findings: Some(3),
            ..AlertFacts::default()
        };
        assert_eq!(
            text(AlertType::CheckFailed, findings),
            (
                "Structure check failed on main".into(),
                "3 problems found. Open the report in Integrity checks.".into()
            )
        );
    }

    #[test]
    fn repository_alert_texts() {
        let quarantined = AlertFacts {
            findings: Some(2),
            error: Some("snapshots/ab: decryption failed".into()),
            ..AlertFacts::default()
        };
        let (title, detail) = text(AlertType::SnapshotQuarantined, quarantined);
        assert_eq!(
            title,
            "2 snapshots in main cannot be read; cleanup is stopped"
        );
        assert!(
            detail.ends_with("First: snapshots/ab: decryption failed."),
            "{detail}"
        );
        let unavailable = AlertFacts {
            error: Some("config hash mismatch".into()),
            ..AlertFacts::default()
        };
        let (title, detail) = text(AlertType::RepositoryUnavailable, unavailable);
        assert_eq!(title, "Repository main is unavailable");
        assert!(detail.contains("config hash mismatch."), "{detail}");
    }

    #[test]
    fn certificate_texts() {
        let soon = AlertFacts {
            not_after: Some(NOW + 10 * DAY_MS),
            ..AlertFacts::default()
        };
        assert_eq!(
            text(AlertType::CertificateExpiring, soon),
            (
                "Agent certificate on web-1 expires in 10 days".into(),
                "Renews automatically while the agent is online.".into()
            )
        );
        let mut grpc = candidate(
            AlertType::CertificateExpiring,
            AlertFacts {
                not_after: Some(NOW - 1),
                external: Some(false),
                ..AlertFacts::default()
            },
        );
        grpc.cert = Some(CertKind::Grpc);
        assert_eq!(title(&grpc, NOW), "Server gRPC certificate has expired");
        grpc.facts.not_after = Some(NOW + 3 * DAY_MS);
        assert_eq!(
            title(&grpc, NOW),
            "Server gRPC certificate expires in 3 days"
        );
        grpc.facts.not_after = Some(NOW + DAY_MS / 2);
        assert_eq!(
            title(&grpc, NOW),
            "Server gRPC certificate expires in 1 day"
        );
    }
}
