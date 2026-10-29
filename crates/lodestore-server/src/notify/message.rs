use std::collections::BTreeMap;

use anyhow::Context;
use data_encoding::HEXLOWER;
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::alerts::model::AlertType;
use crate::alerts::text::alertname;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Firing,
    Resolved,
    Reminder,
    Test,
}

impl MessageKind {
    pub fn parse(text: &str) -> Option<MessageKind> {
        match text {
            "firing" => Some(MessageKind::Firing),
            "resolved" => Some(MessageKind::Resolved),
            "reminder" => Some(MessageKind::Reminder),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MessageKind::Firing => "FIRING",
            MessageKind::Resolved => "RESOLVED",
            MessageKind::Reminder => "STILL FIRING",
            MessageKind::Test => "TEST",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub kind: MessageKind,
    pub title: String,
    pub detail: String,
    pub link: String,
    pub labels: BTreeMap<String, String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct AlertRow {
    pub kind: AlertType,
    pub key: String,
    pub severity: String,
    pub host_id: Option<String>,
    pub hostname: Option<String>,
    pub repository_id: Option<String>,
    pub repository: Option<String>,
    pub policy: Option<String>,
    pub check_level: Option<String>,
    pub cert: Option<String>,
    pub title: String,
    pub detail: String,
    pub started_at: i64,
    pub resolved_at: Option<i64>,
}

pub async fn load_alert(state: &AppState, alert_id: &str) -> anyhow::Result<Option<AlertRow>> {
    let row = sqlx::query(
        "SELECT a.type AS kind, a.key, a.severity, a.host_id, ag.hostname, a.repository_id, \
         r.name AS repository, a.policy_name, a.check_level, a.cert, a.title, a.detail, \
         a.started_at, a.resolved_at \
         FROM alerts a \
         LEFT JOIN agents ag ON ag.id = a.host_id \
         LEFT JOIN repositories r ON r.id = a.repository_id \
         WHERE a.id = $1",
    )
    .bind(alert_id)
    .fetch_optional(&state.db.pool)
    .await
    .context("reading an alert")?;
    let Some(row) = row else { return Ok(None) };
    let kind: String = row.try_get("kind")?;
    Ok(Some(AlertRow {
        kind: AlertType::parse(&kind).with_context(|| format!("unknown alert type {kind:?}"))?,
        key: row.try_get("key")?,
        severity: row.try_get("severity")?,
        host_id: row.try_get("host_id")?,
        hostname: row.try_get("hostname")?,
        repository_id: row.try_get("repository_id")?,
        repository: row.try_get("repository")?,
        policy: row.try_get("policy_name")?,
        check_level: row.try_get("check_level")?,
        cert: row.try_get("cert")?,
        title: row.try_get("title")?,
        detail: row.try_get("detail")?,
        started_at: row.try_get("started_at")?,
        resolved_at: row.try_get("resolved_at")?,
    }))
}

fn link(base: &str, alert: &AlertRow) -> String {
    if let Some(host) = &alert.host_id {
        format!("{base}/hosts/{host}")
    } else if let Some(repository) = &alert.repository_id {
        format!("{base}/settings/repositories/{repository}#checks")
    } else {
        format!("{base}/overview")
    }
}

pub fn for_event(public_url: &str, alert: &AlertRow, kind: MessageKind, now_ms: i64) -> Message {
    let mut labels = BTreeMap::new();
    labels.insert("alertname".to_string(), alertname(alert.kind).to_string());
    labels.insert("severity".to_string(), alert.severity.clone());
    let optional = [
        ("host", &alert.hostname),
        ("host_id", &alert.host_id),
        ("policy", &alert.policy),
        ("repository", &alert.repository),
        ("level", &alert.check_level),
        ("cert", &alert.cert),
    ];
    for (name, value) in optional {
        if let Some(value) = value {
            labels.insert(name.to_string(), value.clone());
        }
    }
    let ended_at = (kind == MessageKind::Resolved).then(|| alert.resolved_at.unwrap_or(now_ms));
    Message {
        kind,
        title: alert.title.clone(),
        detail: alert.detail.clone(),
        link: link(public_url.trim_end_matches('/'), alert),
        labels,
        started_at: alert.started_at,
        ended_at,
        fingerprint: HEXLOWER.encode(&Sha256::digest(alert.key.as_bytes())[..8]),
    }
}

pub fn test_message(public_url: &str) -> Message {
    let labels = BTreeMap::from([
        ("alertname".to_string(), "LodestoreTest".to_string()),
        ("severity".to_string(), "info".to_string()),
    ]);
    Message {
        kind: MessageKind::Test,
        title: "Lodestore test alert".to_string(),
        detail: "If you can read this, the channel works.".to_string(),
        link: format!("{}/overview", public_url.trim_end_matches('/')),
        labels,
        started_at: crate::db::now_ms(),
        ended_at: None,
        fingerprint: HEXLOWER.encode(&Sha256::digest(b"lodestore test")[..8]),
    }
}

pub fn text(m: &Message) -> String {
    let head = match m.kind {
        MessageKind::Test => format!("{}. {}", m.title, m.detail),
        kind => format!("{}: {}\n{}", kind.label(), m.title, m.detail),
    };
    format!("{head}\nOpen in Lodestore: {}", m.link)
}
