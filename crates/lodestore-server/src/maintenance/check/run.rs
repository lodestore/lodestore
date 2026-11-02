//! Driving one check run: the structure check and pack reading, with the
//! server's key.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use lodestore_format::check::packs::check_packs;
use lodestore_format::check::structure::{StructureStep, check_structure};
use lodestore_format::check::{Finding, MAX_FINDINGS, Note};
use lodestore_format::crypto::BlobId;
use lodestore_format::index::{hex32, listed_packs};
use lodestore_format::pack::pack_object_key;
use lodestore_policy::format_duration;

use super::api::{
    CheckFinding, CheckNote, CheckPhase, CheckProgress, CheckSummary, PacksSummary, StoredReport,
    StructureSummary,
};
use super::db::{self, PackRow, RunRow};
use super::restore::restore;
use super::{CheckLevel, CheckOutcome};
use crate::catalog::{OpenRepository, open_repository};
use crate::db::now_ms;
use crate::state::AppState;

pub(super) const PROGRESS_PERIOD: Duration = Duration::from_secs(1);

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(12 * 3600);

/// Runs a queued check to its end and stores the result, once one of the
/// parallel slots is free. A check that can't finish ends as `failed` with
/// the reason; problems found in the repository are part of a finished
/// report instead.
pub async fn run_check(state: &AppState, run_id: &str) {
    let Ok(_slot) = state.checks.slots.acquire().await else {
        return;
    };
    let work = {
        let (state, run_id) = (state.clone(), run_id.to_string());
        async move { drive(&state, &run_id).await }
    };
    supervise(state, run_id, CHECK_TIMEOUT, work).await;
}

/// Drives `work` in a task of its own so that a panic or a hang in it still
/// ends the run as `failed`; it would otherwise stay active and keep every
/// other check and cleanup of the repository waiting.
pub async fn supervise<F>(state: &AppState, run_id: &str, limit: Duration, work: F)
where
    F: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let message = match tokio::spawn(tokio::time::timeout(limit, work)).await {
        Ok(Ok(Ok(()))) => return,
        Ok(Ok(Err(error))) => format!("{error:#}"),
        Ok(Err(_)) => format!(
            "The check ran longer than {} and was stopped.",
            format_duration(limit)
        ),
        Err(_) => "The check stopped unexpectedly.".to_string(),
    };
    tracing::warn!(run_id, error = %message, "check failed");
    if let Err(error) = db::fail_run(state, run_id, &message).await {
        tracing::warn!(run_id, %error, "recording a failed check failed");
    }
}

async fn drive(state: &AppState, run_id: &str) -> anyhow::Result<()> {
    if !db::start(state, run_id).await? {
        return Ok(());
    }
    let run = db::load_run(state, run_id)
        .await?
        .context("the run is missing")?;
    let source = open_repository(state, &run.repository_id)
        .await
        .context("opening the repository")?;
    match run.level {
        CheckLevel::Structure => structure(state, &run, &source).await,
        CheckLevel::Packs => packs(state, &run, &source).await,
        CheckLevel::Restore => restore(state, &run, source).await,
    }
}

/// Polls `work` while writing its progress once a second. `None` when the
/// run was cancelled; `work` is dropped then.
pub(super) async fn until_done<F: Future>(
    state: &AppState,
    run_id: &str,
    progress: &Mutex<CheckProgress>,
    work: F,
) -> anyhow::Result<Option<F::Output>> {
    if db::cancel_requested(state, run_id).await? {
        return Ok(None);
    }
    tokio::pin!(work);
    let mut tick = tokio::time::interval(PROGRESS_PERIOD);
    loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                if db::cancel_requested(state, run_id).await? {
                    return Ok(None);
                }
                let shown = progress.lock().unwrap_or_else(|p| p.into_inner()).clone();
                db::set_progress(state, run_id, &shown).await?;
            }
            output = &mut work => return Ok(Some(output)),
        }
    }
}

pub(super) fn findings_of(list: &[Finding]) -> Vec<CheckFinding> {
    list.iter()
        .take(MAX_FINDINGS)
        .map(|f| CheckFinding {
            kind: f.kind.as_str().into(),
            key: f.key.clone(),
            detail: f.detail.clone(),
        })
        .collect()
}

fn notes_of(list: &[Note]) -> Vec<CheckNote> {
    list.iter()
        .map(|n| CheckNote {
            kind: n.kind.as_str().into(),
            key: n.key.clone(),
            detail: n.detail.clone(),
        })
        .collect()
}

fn verdict(findings_total: u64) -> CheckOutcome {
    if findings_total == 0 {
        CheckOutcome::Passed
    } else {
        CheckOutcome::Problems
    }
}

async fn structure(state: &AppState, run: &RunRow, source: &OpenRepository) -> anyhow::Result<()> {
    let seen = Mutex::new(CheckProgress {
        phase: CheckPhase::Listing,
        done: 0,
        total: 0,
        bytes: 0,
    });
    let record = |step: StructureStep, done: u64, total: u64| {
        let phase = match step {
            StructureStep::Listing => CheckPhase::Listing,
            StructureStep::Indexes => CheckPhase::Indexes,
            StructureStep::Snapshots => CheckPhase::Snapshots,
        };
        *seen.lock().unwrap_or_else(|p| p.into_inner()) = CheckProgress {
            phase,
            done,
            total,
            bytes: 0,
        };
    };
    let checking = check_structure(&source.store, &source.keys, &source.config, &record);
    let Some(checked) = until_done(state, &run.id, &seen, checking).await? else {
        db::abort_run(state, &run.id).await?;
        return Ok(());
    };
    let report = checked.context("checking the structure")?;

    let logical = db::logical_bytes(state, &run.repository_id).await?;
    db::save_usage(
        state,
        &run.repository_id,
        report.physical_bytes,
        logical,
        report.packs_listed,
        report.objects,
    )
    .await?;

    let stored = StoredReport {
        summary: CheckSummary {
            structure: Some(StructureSummary {
                snapshots_checked: report.snapshots_checked,
                trees_checked: report.trees_checked,
                packs_listed: report.packs_listed,
                missing_packs: report.missing_packs,
            }),
            packs: None,
            restore: None,
        },
        findings: findings_of(&report.findings),
        findings_total: report.findings_total,
        notes: notes_of(&report.notes),
        notes_total: report.notes_total,
    };
    db::finish_run(state, &run.id, verdict(report.findings_total), &stored).await
}

/// The order packs are read in: those that failed last time, then those
/// never read (the newest first), then the longest unread.
pub(crate) fn rotation(
    listed: &HashMap<BlobId, u64>,
    rows: &HashMap<String, PackRow>,
) -> Vec<(BlobId, u64)> {
    let mut order: Vec<((u8, i64), BlobId, u64)> = listed
        .iter()
        .map(|(id, size)| {
            let key = match rows.get(&hex32(id)) {
                Some(row) if row.bad => (0, row.checked_at.unwrap_or(0)),
                Some(PackRow {
                    checked_at: None,
                    first_seen_at,
                    ..
                }) => (1, -first_seen_at),
                Some(PackRow {
                    checked_at: Some(at),
                    ..
                }) => (2, *at),
                None => (1, 0),
            };
            (key, *id, *size)
        })
        .collect();
    order.sort_unstable_by_key(|(key, id, _)| (*key, *id));
    order.into_iter().map(|(_, id, size)| (id, size)).collect()
}

async fn packs(state: &AppState, run: &RunRow, source: &OpenRepository) -> anyhow::Result<()> {
    let now = now_ms();
    let settings = db::load_level_settings(state, &run.repository_id, CheckLevel::Packs).await?;
    let seen = Mutex::new(CheckProgress {
        phase: CheckPhase::Listing,
        done: 0,
        total: 0,
        bytes: 0,
    });

    // The work polled beside the progress writes never holds a database
    // transaction: SQLite would make the writes wait for it and it would wait
    // for them.
    let listing = listed_packs(&source.store);
    let Some(listed) = until_done(state, &run.id, &seen, listing).await? else {
        db::abort_run(state, &run.id).await?;
        return Ok(());
    };
    let listed = listed.context("listing the packs")?;
    let names: HashSet<String> = listed.keys().map(hex32).collect();
    let rows = db::sync_packs(state, &run.repository_id, &names, now).await?;
    let order = rotation(&listed, &rows);

    let record = |done: u64, total: u64, bytes: u64| {
        *seen.lock().unwrap_or_else(|p| p.into_inner()) = CheckProgress {
            phase: CheckPhase::Packs,
            done,
            total,
            bytes,
        };
    };
    let reading = check_packs(
        &source.store,
        &source.keys,
        &order,
        settings.read_per_run,
        &record,
    );
    let Some(read) = until_done(state, &run.id, &seen, reading).await? else {
        db::abort_run(state, &run.id).await?;
        return Ok(());
    };
    let report = read.context("reading the packs")?;

    // ponytail: a pack is marked bad from the findings kept in the report; past
    // MAX_FINDINGS only `damaged_blobs` marks it, so a pack with a mismatch alone
    // can read as good until the next run.
    let named: HashSet<&str> = report.findings.iter().map(|f| f.key.as_str()).collect();
    let results: Vec<(String, Option<bool>)> = report
        .outcomes
        .iter()
        .map(|o| {
            let bad = o.damaged > 0 || named.contains(pack_object_key(&o.pack_id).as_str());
            (hex32(&o.pack_id), (!o.missing).then_some(bad))
        })
        .collect();
    db::record_packs(state, &run.repository_id, &results, now).await?;
    let unread: Vec<CheckNote> = report
        .read_errors
        .iter()
        .map(|(key, detail)| CheckNote {
            kind: "read_failed".into(),
            key: key.clone(),
            detail: detail.clone(),
        })
        .collect();

    let stored = StoredReport {
        summary: CheckSummary {
            structure: None,
            restore: None,
            packs: Some(PacksSummary {
                packs_read: report.packs_read,
                bytes_read: report.bytes_read,
                blobs_verified: report.blobs_verified,
                damaged_blobs: report.damaged_blobs,
            }),
        },
        findings: findings_of(&report.findings),
        findings_total: report.findings_total,
        notes_total: unread.len() as u64,
        notes: unread,
    };
    if report.findings_total == 0 && !report.read_errors.is_empty() {
        let first = &report.read_errors[0];
        let message = format!(
            "Some packs could not be read, so the run has no result: {}: {}",
            first.0, first.1
        );
        return db::fail_run(state, &run.id, &message).await;
    }
    db::finish_run(state, &run.id, verdict(report.findings_total), &stored).await
}
