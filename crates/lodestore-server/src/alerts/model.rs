use std::time::Duration;

use anyhow::Context;
use lodestore_policy::jiff::Timestamp;
use lodestore_policy::{PolicySpec, parse_duration};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

use crate::hosts::labels::Label;
use crate::state::AppState;

pub const OFFLINE_AFTER_DEFAULT: Duration = Duration::from_secs(15 * 60);
pub const OFFLINE_AFTER_FLOOR: Duration = Duration::from_secs(5 * 60);
pub const OFFLINE_AFTER_CEILING: Duration = Duration::from_secs(30 * 24 * 3600);
pub const CERT_DAYS_DEFAULT: u32 = 14;
pub const CERT_DAYS_MAX: u32 = 60;
pub const REMINDER_EVERY: Duration = Duration::from_secs(24 * 3600);

pub const OFFLINE_AFTER_KEY: &str = "alerts.offline_after_s";
pub const CERT_DAYS_KEY: &str = "alerts.cert_days";

const LIMIT_SLOTS: usize = 9;

pub fn overdue_limit(spec: &PolicySpec, now: Timestamp) -> Option<Duration> {
    if let Some(limit) = spec
        .alerts
        .as_ref()
        .and_then(|a| a.max_age.as_deref())
        .and_then(parse_duration)
    {
        return Some(limit);
    }
    let compiled = spec.schedule.as_ref()?.compile().ok()?;
    let slots = compiled.next_slots(now, LIMIT_SLOTS);
    let gap = slots
        .windows(2)
        .map(|w| w[1].as_second() - w[0].as_second())
        .max()?;
    Some(Duration::from_secs(2 * u64::try_from(gap).ok()?) + compiled.jitter())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    Failed,
    Overdue,
    Offline,
    NotProtected,
    Healthy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairFacts {
    pub policy: String,
    pub limit: Option<Duration>,
    pub last_success_ms: Option<i64>,
    pub baseline_ms: i64,
    pub last_failure: Option<(i64, String)>,
}

impl PairFacts {
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        now_ms - self.last_success_ms.unwrap_or(self.baseline_ms)
    }

    pub fn pair_overdue(&self, now_ms: i64) -> bool {
        self.limit
            .is_some_and(|limit| self.age_ms(now_ms) > limit.as_millis() as i64)
    }

    pub fn pair_failed(&self) -> bool {
        self.last_failure.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotFacts {
    pub time_ms: i64,
    pub bytes_total: u64,
    pub bytes_added: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostFacts {
    pub host_id: String,
    pub hostname: String,
    pub labels: Vec<Label>,
    pub platform: String,
    pub pairs: Vec<PairFacts>,
    pub online: bool,
    pub last_seen_ms: Option<i64>,
    pub cert_not_after_ms: i64,
    pub last_snapshot: Option<SnapshotFacts>,
}

pub fn offline(
    h: &HostFacts,
    now_ms: i64,
    process_started_ms: i64,
    offline_after: Duration,
) -> bool {
    let since = h.last_seen_ms.unwrap_or(0).max(process_started_ms);
    let limit = i64::try_from(offline_after.as_millis()).unwrap_or(i64::MAX);
    !h.online && now_ms - since > limit
}

pub fn host_state(
    h: &HostFacts,
    now_ms: i64,
    process_started_ms: i64,
    offline_after: Duration,
) -> HostState {
    if h.pairs.iter().any(PairFacts::pair_failed) {
        HostState::Failed
    } else if h.pairs.iter().any(|p| p.pair_overdue(now_ms)) {
        HostState::Overdue
    } else if offline(h, now_ms, process_started_ms, offline_after) {
        HostState::Offline
    } else if h
        .pairs
        .iter()
        .any(|p| p.limit.is_some() && p.last_success_ms.is_some())
    {
        HostState::Healthy
    } else {
        HostState::NotProtected
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertType {
    BackupFailed,
    NoRecentBackup,
    AgentOffline,
    CheckFailed,
    CertificateExpiring,
}

impl AlertType {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertType::BackupFailed => "backup_failed",
            AlertType::NoRecentBackup => "no_recent_backup",
            AlertType::AgentOffline => "agent_offline",
            AlertType::CheckFailed => "check_failed",
            AlertType::CertificateExpiring => "certificate_expiring",
        }
    }

    pub fn parse(text: &str) -> Option<AlertType> {
        Some(match text {
            "backup_failed" => AlertType::BackupFailed,
            "no_recent_backup" => AlertType::NoRecentBackup,
            "agent_offline" => AlertType::AgentOffline,
            "check_failed" => AlertType::CheckFailed,
            "certificate_expiring" => AlertType::CertificateExpiring,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::Warning => "warning",
        }
    }

    pub fn parse(text: &str) -> Option<Severity> {
        match text {
            "critical" => Some(Severity::Critical),
            "warning" => Some(Severity::Warning),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertState {
    Firing,
    Resolved,
}

impl AlertState {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertState::Firing => "firing",
            AlertState::Resolved => "resolved",
        }
    }

    pub fn parse(text: &str) -> Option<AlertState> {
        match text {
            "firing" => Some(AlertState::Firing),
            "resolved" => Some(AlertState::Resolved),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CertKind {
    Agent,
    Grpc,
}

impl CertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CertKind::Agent => "agent",
            CertKind::Grpc => "grpc",
        }
    }

    pub fn parse(text: &str) -> Option<CertKind> {
        match text {
            "agent" => Some(CertKind::Agent),
            "grpc" => Some(CertKind::Grpc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AlertFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub offline_after: Duration,
    pub cert_days: u32,
}

pub async fn load_thresholds(state: &AppState) -> anyhow::Result<Thresholds> {
    let rows = sqlx::query("SELECT key, value FROM settings WHERE key IN ($1, $2)")
        .bind(OFFLINE_AFTER_KEY)
        .bind(CERT_DAYS_KEY)
        .fetch_all(&state.db.pool)
        .await
        .context("reading alert thresholds")?;
    let mut out = Thresholds {
        offline_after: OFFLINE_AFTER_DEFAULT,
        cert_days: CERT_DAYS_DEFAULT,
    };
    for row in &rows {
        let key: String = row.try_get("key")?;
        let value: String = row.try_get("value")?;
        match (key.as_str(), value.trim().parse::<u64>()) {
            (OFFLINE_AFTER_KEY, Ok(secs)) => {
                out.offline_after =
                    Duration::from_secs(secs).clamp(OFFLINE_AFTER_FLOOR, OFFLINE_AFTER_CEILING);
            }
            (CERT_DAYS_KEY, Ok(days)) => {
                out.cert_days = u32::try_from(days.clamp(1, u64::from(CERT_DAYS_MAX)))?;
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use lodestore_policy::parse_yaml;

    use super::*;

    const HOUR: i64 = 3_600_000;
    const NOW_MS: i64 = 1_800_000_000_000;

    fn spec(extra: &str) -> PolicySpec {
        parse_yaml(&format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: p\n\
             spec:\n  repository: main\n  paths: [/etc]\n{extra}"
        ))
        .unwrap()
        .spec
    }

    fn now() -> Timestamp {
        Timestamp::from_millisecond(NOW_MS).unwrap()
    }

    fn pair(limit: Option<u64>, success_ago_h: Option<i64>) -> PairFacts {
        PairFacts {
            policy: "p".into(),
            limit: limit.map(|h| Duration::from_secs(h * 3600)),
            last_success_ms: success_ago_h.map(|h| NOW_MS - h * HOUR),
            baseline_ms: NOW_MS - HOUR,
            last_failure: None,
        }
    }

    fn host(pairs: Vec<PairFacts>, online: bool, seen_ago_h: i64) -> HostFacts {
        HostFacts {
            host_id: "h".into(),
            hostname: "h".into(),
            labels: vec![],
            platform: "linux / x86_64".into(),
            pairs,
            online,
            last_seen_ms: Some(NOW_MS - seen_ago_h * HOUR),
            cert_not_after_ms: NOW_MS + 100 * 24 * HOUR,
            last_snapshot: None,
        }
    }

    fn state(h: &HostFacts) -> HostState {
        host_state(h, NOW_MS, 0, OFFLINE_AFTER_DEFAULT)
    }

    #[test]
    fn daily_limit_is_two_days() {
        let limit = overdue_limit(&spec("  schedule:\n    cron: 0 2 * * *\n"), now()).unwrap();
        assert_eq!(limit, Duration::from_secs(48 * 3600 + 15 * 60));
        let tight = overdue_limit(
            &spec("  schedule:\n    cron: 0 2 * * *\n    jitter: 0m\n"),
            now(),
        )
        .unwrap();
        assert_eq!(tight, Duration::from_secs(48 * 3600));
    }

    #[test]
    fn weekdays_limit_covers_weekend() {
        let limit = overdue_limit(
            &spec("  schedule:\n    cron: 0 2 * * 1-5\n    jitter: 0m\n"),
            now(),
        )
        .unwrap();
        assert_eq!(limit, Duration::from_secs(6 * 86_400));
    }

    #[test]
    fn max_age_overrides() {
        let limit = overdue_limit(
            &spec("  schedule:\n    cron: 0 2 * * *\n  alerts:\n    maxAge: 36h\n"),
            now(),
        )
        .unwrap();
        assert_eq!(limit, Duration::from_secs(36 * 3600));
        let manual = overdue_limit(&spec("  alerts:\n    maxAge: 3d\n"), now()).unwrap();
        assert_eq!(manual, Duration::from_secs(3 * 86_400));
    }

    #[test]
    fn manual_policy_has_no_limit() {
        assert_eq!(overdue_limit(&spec(""), now()), None);
    }

    #[test]
    fn state_priority() {
        let mut failed = pair(Some(48), Some(100));
        failed.last_failure = Some((NOW_MS, "boom".into()));
        let overdue = pair(Some(48), Some(100));
        let fresh = pair(Some(48), Some(1));

        let all = host(vec![failed.clone(), overdue.clone()], false, 100);
        assert_eq!(state(&all), HostState::Failed);
        let late = host(vec![overdue, fresh.clone()], false, 100);
        assert_eq!(state(&late), HostState::Overdue);
        let gone = host(vec![fresh.clone()], false, 100);
        assert_eq!(state(&gone), HostState::Offline);
        let none = host(vec![], true, 0);
        assert_eq!(state(&none), HostState::NotProtected);
        let manual = host(vec![pair(None, Some(1))], true, 0);
        assert_eq!(state(&manual), HostState::NotProtected);
        let first_run = host(vec![pair(Some(48), None)], true, 0);
        assert_eq!(state(&first_run), HostState::NotProtected);
        let ok = host(vec![fresh], true, 0);
        assert_eq!(state(&ok), HostState::Healthy);
    }

    #[test]
    fn restart_grace() {
        let h = host(vec![pair(Some(48), Some(1))], false, 3);
        let minute = 60_000;
        assert!(!offline(&h, NOW_MS, NOW_MS - minute, OFFLINE_AFTER_DEFAULT));
        assert!(offline(
            &h,
            NOW_MS,
            NOW_MS - 20 * minute,
            OFFLINE_AFTER_DEFAULT
        ));
        let online = host(vec![], true, 3);
        assert!(!offline(&online, NOW_MS, 0, OFFLINE_AFTER_DEFAULT));
    }
}
