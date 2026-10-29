use std::collections::{HashMap, HashSet};

use anyhow::Context;
use sqlx::{Any, Row, Transaction};

use super::evaluate::Candidate;
use super::model::REMINDER_EVERY;
use super::{silences, text};
use crate::state::AppState;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub fired: usize,
    pub resolved: usize,
    pub reminded: usize,
}

struct Burning {
    id: String,
    notified_at: Option<i64>,
    policy: Option<String>,
}

/// What could not be read this tick, so its alerts are neither confirmed nor resolved.
#[derive(Debug, Default)]
pub struct Unknown {
    pub server_cert: bool,
    pub policies: HashSet<String>,
}

impl Unknown {
    pub fn covers(&self, key: &str, policy: Option<&str>) -> bool {
        (self.server_cert && key == GRPC_CERT_KEY)
            || policy.is_some_and(|p| self.policies.contains(p))
    }
}

pub const GRPC_CERT_KEY: &str = "certificate_expiring:grpc";

pub async fn reconcile(
    state: &AppState,
    candidates: Vec<Candidate>,
    unknown: &Unknown,
    now_ms: i64,
) -> anyhow::Result<Reconciled> {
    let silenced = silences(state, now_ms).await?;
    let mut tx = state
        .db
        .pool
        .begin()
        .await
        .context("starting a transaction")?;
    let rows =
        sqlx::query("SELECT id, key, notified_at, policy_name FROM alerts WHERE state = 'firing'")
            .fetch_all(&mut *tx)
            .await
            .context("reading the firing alerts")?;
    let mut burning: HashMap<String, Burning> = HashMap::new();
    for row in &rows {
        burning.insert(
            row.try_get("key")?,
            Burning {
                id: row.try_get("id")?,
                notified_at: row.try_get("notified_at")?,
                policy: row.try_get("policy_name")?,
            },
        );
    }

    let mut out = Reconciled::default();
    for c in &candidates {
        let quiet = c.host_id.as_ref().is_some_and(|h| silenced.contains_key(h));
        let title = text::title(c, now_ms);
        let detail = text::detail(c);
        let facts = serde_json::to_string(&c.facts).context("encoding alert facts")?;
        match burning.remove(&c.key) {
            Some(alert) => {
                let due = alert
                    .notified_at
                    .is_none_or(|at| now_ms - at >= REMINDER_EVERY.as_millis() as i64);
                let notified_at = if due && !quiet {
                    let kind = if alert.notified_at.is_none() {
                        out.fired += 1;
                        "firing"
                    } else {
                        out.reminded += 1;
                        "reminder"
                    };
                    event(&mut tx, &alert.id, kind, now_ms).await?;
                    Some(now_ms)
                } else {
                    alert.notified_at
                };
                sqlx::query(
                    "UPDATE alerts SET severity = $1, facts_json = $2, title = $3, detail = $4, \
                     updated_at = $5, notified_at = $6 WHERE id = $7",
                )
                .bind(c.severity.as_str())
                .bind(&facts)
                .bind(&title)
                .bind(&detail)
                .bind(now_ms)
                .bind(notified_at)
                .bind(&alert.id)
                .execute(&mut *tx)
                .await
                .context("updating an alert")?;
            }
            None => {
                let id = uuid::Uuid::now_v7().to_string();
                let notified_at = (!quiet).then_some(now_ms);
                let inserted = sqlx::query(
                    "INSERT INTO alerts (id, type, key, severity, state, host_id, repository_id, \
                     policy_name, check_level, cert, title, detail, facts_json, started_at, \
                     updated_at, notified_at) \
                     VALUES ($1, $2, $3, $4, 'firing', $5, $6, $7, $8, $9, $10, $11, $12, $13, \
                     $13, $14) ON CONFLICT DO NOTHING",
                )
                .bind(&id)
                .bind(c.kind.as_str())
                .bind(&c.key)
                .bind(c.severity.as_str())
                .bind(&c.host_id)
                .bind(&c.repository_id)
                .bind(&c.policy)
                .bind(&c.check_level)
                .bind(c.cert.map(|k| k.as_str()))
                .bind(&title)
                .bind(&detail)
                .bind(&facts)
                .bind(now_ms)
                .bind(notified_at)
                .execute(&mut *tx)
                .await
                .context("inserting an alert")?
                .rows_affected();
                if inserted > 0 && !quiet {
                    event(&mut tx, &id, "firing", now_ms).await?;
                    out.fired += 1;
                }
            }
        }
    }

    for (key, alert) in burning {
        if unknown.covers(&key, alert.policy.as_deref()) {
            continue;
        }
        let resolved_notified_at = alert.notified_at.map(|_| now_ms);
        sqlx::query(
            "UPDATE alerts SET state = 'resolved', resolved_at = $1, updated_at = $1, \
             resolved_notified_at = $2 WHERE id = $3",
        )
        .bind(now_ms)
        .bind(resolved_notified_at)
        .bind(&alert.id)
        .execute(&mut *tx)
        .await
        .context("resolving an alert")?;
        if alert.notified_at.is_some() {
            event(&mut tx, &alert.id, "resolved", now_ms).await?;
            out.resolved += 1;
        }
    }
    tx.commit().await.context("committing alerts")?;
    Ok(out)
}

async fn event(
    tx: &mut Transaction<'_, Any>,
    alert_id: &str,
    kind: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO alert_events (id, alert_id, kind, at) VALUES ($1, $2, $3, $4)")
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(alert_id)
        .bind(kind)
        .bind(now_ms)
        .execute(&mut **tx)
        .await
        .context("recording an alert event")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_inputs_hold_only_their_alerts() {
        let unknown = Unknown {
            server_cert: true,
            policies: HashSet::from(["daily".to_string()]),
        };
        assert!(unknown.covers(GRPC_CERT_KEY, None));
        assert!(!unknown.covers("certificate_expiring:agent:h1", None));
        assert!(unknown.covers("backup_failed:h1:daily", Some("daily")));
        assert!(!unknown.covers("backup_failed:h1:weekly", Some("weekly")));
        assert!(!Unknown::default().covers(GRPC_CERT_KEY, Some("daily")));
    }
}
