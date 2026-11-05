use serde::Deserialize;
use sqlx::Row;

pub use super::actions::{AnomalyKind as ReasonKind, AnomalyReasonDto as Reason};
use crate::db::now_ms;
use crate::state::AppState;

const BASE_SIZE: i64 = 10;
const DROP_RATIO: f64 = 0.5;
const SPIKE_FACTOR: f64 = 5.0;
const CHANGED_FLOOR: f64 = 0.30;
const ADDED_FLOOR: f64 = 0.25;
const SPIKE_MIN_BASE: usize = 3;
const MIN_USUAL_SHARE: f64 = 0.01;
const BASE_SQL: &str = "SELECT stats_json FROM snapshots WHERE repository_id = $1 \
     AND policy_name = $2 AND host_id = $3 AND paths_json = $4 AND id <> $5 \
     AND received_at < $6 AND incomplete = 0 AND suspicious = 0 AND verified_at IS NOT NULL \
     ORDER BY received_at DESC LIMIT $7";

#[derive(Default, Deserialize)]
#[serde(default)]
struct Stats {
    files_new: u64,
    files_changed: u64,
    files_unchanged: u64,
    bytes_total: u64,
    bytes_added: u64,
}

impl Stats {
    fn files(&self) -> f64 {
        (self.files_new + self.files_changed + self.files_unchanged) as f64
    }

    fn changed_share(&self) -> Option<f64> {
        (self.files() > 0.0).then(|| (self.files_new + self.files_changed) as f64 / self.files())
    }
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values: Vec<f64> = values.collect();
    values.sort_by(f64::total_cmp);
    match values.len() {
        0 => 0.0,
        n if n % 2 == 1 => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2.0,
    }
}

fn drop_reason(kind: ReasonKind, to: f64, from: f64) -> Option<Reason> {
    (from >= 1.0 && to < from * DROP_RATIO).then(|| Reason {
        kind,
        pct: ((1.0 - to / from) * 100.0).round() as u32,
        from: from.round() as u64,
        to: to as u64,
        factor: to / from,
    })
}

fn reasons_for(new: &Stats, base: &[Stats]) -> Vec<Reason> {
    let mut reasons = Vec::new();
    let usual_files = median(base.iter().map(Stats::files));
    let usual_bytes = median(base.iter().map(|s| s.bytes_total as f64));
    reasons.extend(drop_reason(ReasonKind::FilesDrop, new.files(), usual_files));
    reasons.extend(drop_reason(
        ReasonKind::BytesDrop,
        new.bytes_total as f64,
        usual_bytes,
    ));
    if base.len() < SPIKE_MIN_BASE {
        return reasons;
    }
    if let Some(share) = new.changed_share() {
        let usual = median(base.iter().filter_map(Stats::changed_share));
        if share >= (SPIKE_FACTOR * usual).max(CHANGED_FLOOR) {
            reasons.push(Reason {
                kind: ReasonKind::ChangedSpike,
                pct: (share * 100.0).round() as u32,
                from: (usual * 100.0).round() as u64,
                to: (share * 100.0).round() as u64,
                factor: share / usual.max(MIN_USUAL_SHARE),
            });
        }
    }
    let added = new.bytes_added as f64;
    let usual = median(base.iter().map(|s| s.bytes_added as f64));
    let total = new.bytes_total as f64;
    if total > 0.0 && added >= (SPIKE_FACTOR * usual).max(ADDED_FLOOR * total) {
        reasons.push(Reason {
            kind: ReasonKind::AddedSpike,
            pct: (added / total * 100.0).round() as u32,
            from: usual.round() as u64,
            to: new.bytes_added,
            factor: added / usual.max(1.0),
        });
    }
    reasons
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl Reason {
    pub fn describe(&self) -> String {
        match self.kind {
            ReasonKind::FilesDrop => format!(
                "{}% fewer files than before ({} to {}).",
                self.pct, self.from, self.to
            ),
            ReasonKind::BytesDrop => format!(
                "{}% less data than before ({} to {}).",
                self.pct,
                size(self.from),
                size(self.to)
            ),
            ReasonKind::ChangedSpike => format!(
                "{}% of files changed, about {:.0} times the usual.",
                self.pct, self.factor
            ),
            ReasonKind::AddedSpike => format!(
                "{} of new data, about {:.0} times the usual.",
                size(self.to),
                self.factor
            ),
        }
    }
}

pub async fn evaluate(state: &AppState, snapshot_id: &str) -> anyhow::Result<Option<Vec<Reason>>> {
    let Some(row) = sqlx::query(
        "SELECT repository_id, host_id, policy_name, paths_json, incomplete, suspicious, \
         stats_json, received_at, suspicious_cleared_at FROM snapshots WHERE id = $1",
    )
    .bind(snapshot_id)
    .fetch_optional(&state.db.pool)
    .await?
    else {
        return Ok(None);
    };
    let (Some(policy), Some(host)) = (
        row.try_get::<Option<String>, _>("policy_name")?,
        row.try_get::<Option<String>, _>("host_id")?,
    ) else {
        return Ok(None);
    };
    if row.try_get::<i64, _>("incomplete")? != 0
        || row.try_get::<i64, _>("suspicious")? != 0
        || row
            .try_get::<Option<i64>, _>("suspicious_cleared_at")?
            .is_some()
    {
        return Ok(None);
    }
    let new: Stats = serde_json::from_str(&row.try_get::<String, _>("stats_json")?)?;
    let lock = state.catalog.pair_lock(&policy, &host);
    let _pair = lock.lock().await;
    let base: Vec<Stats> = sqlx::query(BASE_SQL)
        .bind(row.try_get::<String, _>("repository_id")?)
        .bind(&policy)
        .bind(&host)
        .bind(row.try_get::<String, _>("paths_json")?)
        .bind(snapshot_id)
        .bind(row.try_get::<i64, _>("received_at")?)
        .bind(BASE_SIZE)
        .fetch_all(&state.db.pool)
        .await?
        .iter()
        .filter_map(|r| serde_json::from_str(&r.try_get::<String, _>("stats_json").ok()?).ok())
        .collect();
    if base.is_empty() {
        return Ok(None);
    }
    let reasons = reasons_for(&new, &base);
    if reasons.is_empty() {
        return Ok(None);
    }
    let marked = sqlx::query(
        "UPDATE snapshots SET suspicious = 1, suspicious_by = 'anomaly', anomaly_json = $1, \
         suspicious_at = $2 WHERE id = $3 AND suspicious = 0",
    )
    .bind(serde_json::to_string(&reasons)?)
    .bind(now_ms())
    .bind(snapshot_id)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    Ok((marked > 0).then_some(reasons))
}
