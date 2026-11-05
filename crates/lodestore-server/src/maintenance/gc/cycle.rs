//! The steps of a cleanup cycle. The database is advice and the bucket is
//! the truth: every step can be repeated after a restart and reaches the
//! same result from the saved state, the published marker and a listing.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use lodestore_format::attest::{consolidate_attestations, put_attestation, read_attestations};
use lodestore_format::check::is_data_error;
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::gc::compact::{compact_plan, write_compacted};
use lodestore_format::gc::mark::ShardSet;
use lodestore_format::gc::plan::{MAX_CANDIDATES, PlanOptions};
use lodestore_format::gc::repack::repack;
use lodestore_format::gc::sweep::{
    SweepOutcome, confirm_unreferenced, purge_object, sweep_object, sweep_packs, sweep_shards,
};
use lodestore_format::gc::verify::verify;
use lodestore_format::gc::{SurveyProgress, candidates, survey};
use lodestore_format::index::{hex32, index_object_key, parse_hex32, rebuild_index};
use lodestore_format::locks::{
    GcLock, LOCKS_PREFIX, WriterLock, close_writer_marker, gc_manifest_key, lock_id_hex,
    new_lock_id, publish_gc_manifest, read_gc_locks, read_writer_locks, writer_done_key,
};
use lodestore_format::pack::pack_object_key;
use lodestore_format::store::{BlobDelete, BlobPurge, BlobStore, Store, put_named};
use lodestore_format::verify_snapshot::verify_shard;
use lodestore_policy::{format_duration, format_size};
use sqlx::Row;

use super::api::{GcFullReport, GcLockedItem, WriterMarkerDto};
use super::db::{self, CycleRow};
use super::preflight::{old_agents, own_gc_ids};
use super::{GcState, StoredProgress, backups_end, bucket_retention, build_report, held_until};
use crate::catalog::{OpenRepository, open_repository};
use crate::db::now_ms;
use crate::jobs::ACTIVE_STATES;
use crate::jobs::restore::RestoreRecord;
use crate::maintenance::attest::verify::SWEEP;
use crate::maintenance::attest::{ensure_upgraded, load_verified, record_verified};
use crate::state::AppState;

const PACKS_PREFIX: &str = "packs/";
const PROGRESS_PERIOD: Duration = Duration::from_secs(1);
const CLOCK_TOLERANCE_MS: i64 = 5 * 60 * 1000;
const INDEX_PREFIX: &str = "index/";
const SWEPT_PREFIXES: [&str; 4] = [PACKS_PREFIX, INDEX_PREFIX, "snapshots/", LOCKS_PREFIX];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The cycle moved to this state; the next step can run at once.
    Advanced(GcState),
    /// Nothing to do before `until_ms`.
    Waiting { until_ms: i64 },
    /// Something outside the cycle has to finish first; try again later.
    Postponed,
    /// The cycle is over, in this state.
    Finished(GcState),
}

type Seen = Arc<Mutex<StoredProgress>>;

fn record(seen: &Seen) -> impl Fn(u64, u64) + Sync + use<> {
    let seen = Arc::clone(seen);
    move |done, total| {
        let mut seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.done = done;
        seen.total = total;
    }
}

/// Runs `work` and writes the progress it reports to the cycle every second.
async fn tracked<T>(
    state: &AppState,
    cycle_id: &str,
    seen: &Seen,
    work: impl Future<Output = T>,
) -> anyhow::Result<T> {
    tokio::pin!(work);
    let mut tick = tokio::time::interval(PROGRESS_PERIOD);
    loop {
        tokio::select! {
            out = &mut work => return Ok(out),
            _ = tick.tick() => {
                let progress = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
                db::set_progress(state, cycle_id, &progress).await?;
            }
        }
    }
}

fn missing_marker(gc_id: &[u8; 16]) -> anyhow::Error {
    anyhow::anyhow!(
        "The cleanup marker {} is missing from the bucket. Nothing was deleted.",
        gc_manifest_key(gc_id)
    )
}

fn cycle_gc_id(cycle: &CycleRow) -> anyhow::Result<[u8; 16]> {
    let text = cycle.gc_id.as_deref().context("the cycle has no marker")?;
    let bytes = data_encoding::HEXLOWER.decode(text.as_bytes())?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("the stored marker id is not 16 bytes"))
}

async fn own_marker(source: &OpenRepository, gc_id: &[u8; 16]) -> anyhow::Result<Option<GcLock>> {
    let locks = read_gc_locks(&source.store, source.keys.k_idx(), source.keys.repo_id()).await?;
    if locks.denied {
        bail!("The locks folder can't be read, so nothing was deleted.");
    }
    let wanted = lock_id_hex(gc_id);
    Ok(locks
        .manifests
        .into_iter()
        .find(|m| lock_id_hex(&m.gc_id) == wanted))
}

/// Stops the cycle while another server's cleanup marker is in the bucket.
/// Two servers with their own catalogs can pass the preflight at the same
/// moment. Each looks again once its own marker is up, so at least one of
/// them sees the other and stops before anything is deleted.
async fn refuse_foreign_marker(
    state: &AppState,
    cycle: &CycleRow,
    source: &OpenRepository,
) -> anyhow::Result<()> {
    let locks = read_gc_locks(&source.store, source.keys.k_idx(), source.keys.repo_id()).await?;
    if locks.denied {
        bail!("The locks folder can't be read, so nothing was deleted.");
    }
    let ours = own_gc_ids(state, &cycle.repository_id).await?;
    if let Some(foreign) = locks
        .manifests
        .iter()
        .find(|m| !ours.contains(&lock_id_hex(&m.gc_id)))
    {
        bail!(
            "Another cleanup marker ({}) is in this bucket and this server did not publish it, \
             so this cleanup stopped before deleting anything. If another server is cleaning \
             this bucket, run this one again after it finishes.",
            gc_manifest_key(&foreign.gc_id)
        );
    }
    Ok(())
}

/// Open writer markers stored before the cleanup marker. Nothing ties a
/// marker to a time but the bucket's own clock, which listings report to the
/// second, so a marker in the same second as the cleanup marker and one
/// without a time count as early.
async fn blocking_writers(
    source: &OpenRepository,
    marker: &GcLock,
) -> anyhow::Result<Vec<WriterLock>> {
    let locks =
        read_writer_locks(&source.store, source.keys.k_idx(), source.keys.repo_id()).await?;
    Ok(locks
        .into_iter()
        .filter(|w| {
            !w.done
                && match (w.modified_ms, marker.modified_ms) {
                    (Some(written), Some(published)) => written <= published,
                    _ => true,
                }
        })
        .collect())
}

fn writer_dto(lock: &WriterLock) -> WriterMarkerDto {
    WriterMarkerDto {
        key: lock.key.clone(),
        host: lock
            .marker
            .as_ref()
            .map(|m| String::from_utf8_lossy(&m.hostname).into_owned()),
        started_at: lock.modified_ms,
    }
}

/// Closes the markers that hold a waiting cycle back; returns how many.
pub async fn unlock_writers(state: &AppState, cycle: &CycleRow) -> anyhow::Result<usize> {
    let gc_id = cycle_gc_id(cycle)?;
    let source = open_repository(state, &cycle.repository_id).await?;
    let marker = own_marker(&source, &gc_id)
        .await?
        .ok_or_else(|| missing_marker(&gc_id))?;
    let blocking = blocking_writers(&source, &marker).await?;
    for lock in &blocking {
        close_writer_marker(&source.store, &lock.id).await?;
    }
    Ok(blocking.len())
}

/// Whether the marker of a failed cycle is still in the bucket.
pub async fn marker_in_bucket(state: &AppState, cycle: &CycleRow) -> anyhow::Result<bool> {
    let Ok(gc_id) = cycle_gc_id(cycle) else {
        return Ok(false);
    };
    let source = open_repository(state, &cycle.repository_id).await?;
    Ok(own_marker(&source, &gc_id).await?.is_some())
}

async fn finished(state: &AppState, cycle_id: &str) -> anyhow::Result<StepOutcome> {
    let cycle = db::load_cycle(state, cycle_id)
        .await?
        .context("the cycle is missing")?;
    Ok(StepOutcome::Finished(cycle.state))
}

/// Runs one step of the cycle from the state it is in.
pub async fn step(state: &AppState, cycle_id: &str, now_ms: i64) -> anyhow::Result<StepOutcome> {
    let cycle = db::load_cycle(state, cycle_id)
        .await?
        .context("the cycle is missing")?;
    if !cycle.state.is_active() {
        return Ok(StepOutcome::Finished(cycle.state));
    }
    let result = match cycle.state {
        GcState::Marking | GcState::Repacking => mark_and_repack(state, &cycle, now_ms).await,
        GcState::Waiting => waiting(state, &cycle, now_ms).await,
        GcState::Verifying => verifying(state, &cycle).await,
        GcState::Sweeping => sweeping(state, &cycle, now_ms).await,
        GcState::Done | GcState::Failed | GcState::Aborted => unreachable!("checked above"),
    };
    match result {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            fail(state, &cycle, &format!("{error:#}")).await?;
            Ok(StepOutcome::Finished(GcState::Failed))
        }
    }
}

/// Before the first delete a failed cycle takes its marker down: nothing
/// was removed, so the marker protects nothing. After it, the marker stays
/// and the next run continues.
async fn fail(state: &AppState, cycle: &CycleRow, message: &str) -> anyhow::Result<()> {
    tracing::warn!(cycle_id = %cycle.id, error = %message, "cleanup cycle failed");
    take_down_marker(state, &cycle.id).await;
    db::fail_cycle(state, &cycle.id, message).await
}

/// Takes the cycle's marker out of the bucket if it is there.
async fn remove_marker(state: &AppState, cycle: &CycleRow) -> anyhow::Result<()> {
    let Ok(gc_id) = cycle_gc_id(cycle) else {
        return Ok(());
    };
    let source = open_repository(state, &cycle.repository_id).await?;
    if own_marker(&source, &gc_id).await?.is_none() {
        return Ok(());
    }
    check(&sweep_object(&source.store, &gc_manifest_key(&gc_id), 0).await)
}

/// Takes down the marker of a cycle that has deleted nothing, as stored now
/// rather than as a step loaded it: the step may have published the marker
/// since. A marker that stays is reported on the cycle with its key, since
/// writers keep avoiding its packs until it goes.
pub async fn take_down_marker(state: &AppState, cycle_id: &str) {
    let cycle = match db::load_cycle(state, cycle_id).await {
        Ok(Some(cycle)) if !cycle.sweep_started => cycle,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(cycle_id, %error, "reading the cycle to take its marker down failed");
            return;
        }
    };
    if let Err(error) = remove_marker(state, &cycle).await {
        let key =
            cycle_gc_id(&cycle).map_or_else(|_| "locks/gc-…".into(), |id| gc_manifest_key(&id));
        tracing::warn!(cycle_id, %error, key, "the cleanup marker could not be removed");
        event(
            state,
            cycle_id,
            "error",
            "marker_left",
            &format!(
                "The cleanup marker {key} could not be removed: {error:#}. Backups keep avoiding \
                 its packs until it is deleted; the next cleanup run removes it."
            ),
        )
        .await;
    }
}

/// Records an event; a failure to record one never fails the cycle.
pub(crate) async fn event(
    state: &AppState,
    cycle_id: &str,
    level: &str,
    kind: &str,
    message: &str,
) {
    if let Err(error) = db::append_event(state, cycle_id, level, kind, message).await {
        tracing::warn!(cycle_id, %error, kind, "recording a cleanup event failed");
    }
}

/// Steps until the cycle waits, is postponed or ends. A cycle already
/// driven here is left alone. The driver owns the marker while it runs: an
/// abort that comes meanwhile leaves the marker to it, and it takes the
/// marker down once its step has stopped, after any compaction it wrote.
pub async fn drive(state: AppState, cycle_id: String) {
    let Some(_guard) = state.gc.claim(&cycle_id) else {
        return;
    };
    loop {
        match step(&state, &cycle_id, now_ms()).await {
            Ok(StepOutcome::Advanced(_)) => {}
            Ok(_) => break,
            Err(error) => {
                tracing::warn!(%cycle_id, %error, "cleanup step failed");
                break;
            }
        }
    }
    if let Ok(Some(cycle)) = db::load_cycle(&state, &cycle_id).await
        && cycle.state == GcState::Aborted
    {
        take_down_marker(&state, &cycle_id).await;
    }
}

/// Fails the cycle before anything is deleted when an agent that does not
/// follow the cleanup protocol has joined the repository since it started:
/// such an agent neither reads the marker nor keeps to a time limit.
async fn refuse_old_agents(state: &AppState, repository_id: &str) -> anyhow::Result<()> {
    let old = old_agents(state, repository_id).await?;
    if old.is_empty() {
        return Ok(());
    }
    let hosts: Vec<&str> = old.iter().map(|h| h.hostname.as_str()).collect();
    bail!(
        "Agents that don't support cleanup joined this repository ({}). Nothing was deleted. \
         Update or revoke them, then run cleanup again.",
        hosts.join(", ")
    )
}

/// Takes down the markers this server left behind from cycles that will
/// never continue: done, aborted, or failed before deleting anything.
async fn remove_stale_markers(
    state: &AppState,
    cycle: &CycleRow,
    source: &OpenRepository,
) -> anyhow::Result<()> {
    let locks = read_gc_locks(&source.store, source.keys.k_idx(), source.keys.repo_id()).await?;
    if locks.manifests.is_empty() {
        return Ok(());
    }
    let stale = db::stale_gc_ids(state, &cycle.repository_id).await?;
    for manifest in &locks.manifests {
        let id = lock_id_hex(&manifest.gc_id);
        if cycle.gc_id.as_deref() == Some(id.as_str()) || stale.contains(&id) {
            check(&sweep_object(&source.store, &manifest.key, 0).await)?;
        }
    }
    Ok(())
}

/// Signs the shards the rebuild wrote once every frame they name opens, so
/// the next rebuild counts their packs as indexed instead of writing them
/// again. A shard whose frames do not open stays unsigned.
async fn vouch_rebuilt(
    state: &AppState,
    repository_id: &str,
    source: &OpenRepository,
    written: &[BlobId],
) -> anyhow::Result<()> {
    let mut vouched = Vec::new();
    for id in written {
        match verify_shard(&source.store, &source.keys, id).await {
            Ok(Some(_)) => vouched.push(*id),
            Ok(None) => {}
            Err(error) if is_data_error(&error) => {
                tracing::warn!(shard = %hex32(id), %error, "a rebuilt index shard does not open");
            }
            Err(error) => return Err(error.into()),
        }
    }
    if vouched.is_empty() {
        return Ok(());
    }
    record_verified(state, repository_id, &vouched).await?;
    put_attestation(
        &source.store,
        &source.keys,
        &vouched,
        &[],
        Vec::new(),
        &mut Entropy::os(),
    )
    .await?;
    Ok(())
}

async fn mark_and_repack(
    state: &AppState,
    cycle: &CycleRow,
    now_ms: i64,
) -> anyhow::Result<StepOutcome> {
    ensure_upgraded(state, &cycle.repository_id)
        .await
        .context("signing the repository's existing index before garbage collection")?;
    let source = open_repository(state, &cycle.repository_id)
        .await
        .context("opening the repository")?;
    let seen: Seen = Arc::default();
    remove_stale_markers(state, cycle, &source)
        .await
        .context("removing old cleanup markers")?;

    let rebuilt = tracked(
        state,
        &cycle.id,
        &seen,
        rebuild_index(&source.store, &source.keys, &record(&seen)),
    )
    .await?
    .context("rebuilding the index")?;
    vouch_rebuilt(state, &cycle.repository_id, &source, &rebuilt.written)
        .await
        .context("signing the rebuilt index")?;

    let opts = PlanOptions {
        now_ms,
        min_age_ms: cycle.settings.pause_ms,
        repack_below_pct: cycle.settings.repack_below_pct,
        max_repack_bytes: cycle.settings.max_repack_bytes,
        max_candidates: MAX_CANDIDATES,
    };
    let progress = {
        let set = record(&seen);
        move |event: SurveyProgress| {
            let SurveyProgress::Snapshots { done, total } = event;
            set(done, total);
        }
    };
    let surveyed = tracked(
        state,
        &cycle.id,
        &seen,
        survey(
            &source.store,
            &source.keys,
            &source.config,
            &opts,
            &progress,
        ),
    )
    .await?
    .context("surveying the repository")?;

    let started = cycle.started_at.unwrap_or(now_ms);
    let report = build_report(cycle, &surveyed, now_ms, now_ms - started, None);
    let plan = &surveyed.plan;
    if plan.delete.is_empty() && plan.repack.is_empty() {
        let older_than = now_ms.saturating_sub(cycle.settings.pause_ms);
        let swept = sweep_deleted(&source.store, older_than).await?;
        let mut report = report;
        report.summary.bytes_freed = swept.bytes_freed;
        apply_locked(state, cycle, &mut report, &swept).await;
        let message = if swept.deleted == 0 && swept.retained.is_empty() {
            "Nothing to free.".to_string()
        } else {
            format!(
                "Cleanup finished: {} freed.",
                format_size(report.summary.bytes_freed)
            )
        };
        event(state, &cycle.id, "info", "finished", &message).await;
        db::finish_cycle(state, &cycle.id, &report).await?;
        return finished(state, &cycle.id).await;
    }

    refuse_foreign_marker(state, cycle, &source).await?;
    db::transition(state, &cycle.id, GcState::Marking, GcState::Repacking).await?;
    let mut entropy = Entropy::os();
    let repacked = tracked(
        state,
        &cycle.id,
        &seen,
        repack(
            &source.store,
            &source.keys,
            &surveyed,
            &mut entropy,
            &record(&seen),
        ),
    )
    .await?
    .context("repacking sparse packs")?;
    if !repacked.skipped.is_empty() {
        let packs: Vec<String> = repacked
            .skipped
            .iter()
            .map(|(id, reason)| format!("{} ({reason})", pack_object_key(id)))
            .collect();
        let message = format!(
            "{} pack(s) could not be repacked and stay in place: {}. Run a full check of the \
             repository.",
            packs.len(),
            packs.join("; ")
        );
        event(state, &cycle.id, "warning", "repack_skipped", &message).await;
    }
    if let Some(shard) = repacked.shard {
        record_verified(state, &cycle.repository_id, &[shard])
            .await
            .context("recording the repacked shard as verified")?;
    }

    let marked = candidates(plan, &repacked);
    let gc_id = new_lock_id(&mut entropy);
    db::set_gc_id(state, &cycle.id, &lock_id_hex(&gc_id)).await?;
    publish_gc_manifest(
        &source.store,
        source.keys.k_idx(),
        source.keys.repo_id(),
        &gc_id,
        &marked,
        &mut entropy,
    )
    .await
    .context("publishing the cleanup marker")?;

    let progress = StoredProgress {
        marked_packs: marked.len() as u64,
        marked_bytes: plan.delete.iter().chain(&plan.repack).map(|p| p.size).sum(),
        repack_read: repacked.bytes_read,
        repack_written: repacked.bytes_written,
        ..StoredProgress::default()
    };
    let marker = own_marker(&source, &gc_id)
        .await?
        .ok_or_else(|| missing_marker(&gc_id))?;
    refuse_foreign_marker(state, cycle, &source).await?;
    let published_ms = published_at(now_ms, &marker);
    let delete_after = published_ms.saturating_add(cycle.settings.pause_ms).max(
        backups_end(state, &cycle.repository_id, published_ms)
            .await?
            .unwrap_or(0),
    );
    let published = db::publish_cycle(
        state,
        &cycle.id,
        &lock_id_hex(&gc_id),
        published_ms,
        delete_after,
        &progress,
        &report,
    )
    .await?;
    if !published {
        take_down_marker(state, &cycle.id).await;
        return finished(state, &cycle.id).await;
    }
    event(
        state,
        &cycle.id,
        "info",
        "marked",
        &format!(
            "Marked {} packs ({}) for deletion. Nothing is deleted before the pause is over.",
            progress.marked_packs,
            format_size(progress.marked_bytes)
        ),
    )
    .await;
    Ok(StepOutcome::Advanced(GcState::Waiting))
}

/// T0: when writers can first have seen the marker. The walk and the repack
/// before it take hours on a large bucket, so this is taken after the PUT,
/// and never before the bucket's own time for the object.
fn published_at(step_started_ms: i64, marker: &GcLock) -> i64 {
    step_started_ms
        .max(crate::db::now_ms())
        .max(marker.modified_ms.unwrap_or(i64::MIN))
}

/// The storage's own time, read from an object written for the purpose and
/// removed again; `None` for a local folder, whose clock is the server's.
async fn bucket_now(store: &Store) -> anyhow::Result<Option<i64>> {
    if matches!(store, Store::Local(_)) {
        return Ok(None);
    }
    let key = format!(
        "{LOCKS_PREFIX}clock-{}",
        lock_id_hex(&new_lock_id(&mut Entropy::os()))
    );
    put_named(store, &key, Vec::new()).await?;
    let at = store.modified_ms(&key).await;
    let removed = sweep_object(store, &key, 0).await;
    if let Some((_, reason)) = removed.failed.first() {
        tracing::warn!(key, reason, "removing a clock probe failed");
    }
    Ok(at?)
}

/// How much longer the pause has to run when the server's clock has run
/// ahead of the storage's since the marker was published: an NTP step or a
/// VM with the wrong time would otherwise cut the pause short without a
/// sign. The pause is then measured on the storage's clock.
async fn pause_short_by_bucket_clock(
    source: &OpenRepository,
    cycle: &CycleRow,
    marker: &GcLock,
) -> anyhow::Result<Option<i64>> {
    let (Some(published), Some(marked)) = (cycle.published_at, marker.modified_ms) else {
        return Ok(None);
    };
    let Some(bucket) = bucket_now(&source.store).await? else {
        return Ok(None);
    };
    let server_elapsed = crate::db::now_ms().saturating_sub(published);
    let bucket_elapsed = bucket.saturating_sub(marked);
    if server_elapsed.saturating_sub(bucket_elapsed) <= CLOCK_TOLERANCE_MS {
        return Ok(None);
    }
    tracing::warn!(
        cycle_id = %cycle.id,
        server_elapsed,
        bucket_elapsed,
        "the server's clock ran ahead of the storage's during the pause"
    );
    let short = cycle.settings.pause_ms.saturating_sub(bucket_elapsed);
    Ok((short > 0).then_some(short))
}

async fn waiting(state: &AppState, cycle: &CycleRow, now_ms: i64) -> anyhow::Result<StepOutcome> {
    let until_ms = cycle.delete_after.context("the cycle has no deadline")?;
    if now_ms < until_ms {
        return Ok(StepOutcome::Waiting { until_ms });
    }
    let published_ms = cycle
        .published_at
        .context("the cycle was never published")?;
    if let Some(end) = backups_end(state, &cycle.repository_id, published_ms).await?
        && now_ms < end
    {
        db::set_delete_after(state, &cycle.id, end).await?;
        return Ok(StepOutcome::Waiting { until_ms: end });
    }
    let gc_id = cycle_gc_id(cycle)?;
    let source = open_repository(state, &cycle.repository_id).await?;
    let marker = own_marker(&source, &gc_id)
        .await?
        .ok_or_else(|| missing_marker(&gc_id))?;
    let blocking = blocking_writers(&source, &marker).await?;
    let shown: Vec<WriterMarkerDto> = blocking.iter().map(writer_dto).collect();
    if shown != cycle.progress.blocked_by_writers {
        let mut progress = cycle.progress.clone();
        progress.blocked_by_writers = shown.clone();
        db::set_progress(state, &cycle.id, &progress).await?;
        if !shown.is_empty() {
            let message = format!(
                "Deletion is postponed: {} backup(s) that started before the marking are still open.",
                shown.len()
            );
            event(state, &cycle.id, "warning", "postponed", &message).await;
        }
    }
    if !blocking.is_empty() {
        return Ok(StepOutcome::Postponed);
    }
    refuse_old_agents(state, &cycle.repository_id).await?;
    if let Some(short) = pause_short_by_bucket_clock(&source, cycle, &marker).await? {
        let until_ms = now_ms.saturating_add(short);
        db::set_delete_after(state, &cycle.id, until_ms).await?;
        let message = format!(
            "The server's clock ran {} ahead of the storage's since the marking, so deletion \
             waits until the storage's clock has seen the whole pause.",
            format_duration(Duration::from_millis(short.unsigned_abs()))
        );
        event(state, &cycle.id, "warning", "clock", &message).await;
        return Ok(StepOutcome::Waiting { until_ms });
    }
    if !db::transition(state, &cycle.id, GcState::Waiting, GcState::Verifying).await? {
        return finished(state, &cycle.id).await;
    }
    Ok(StepOutcome::Advanced(GcState::Verifying))
}

async fn verifying(state: &AppState, cycle: &CycleRow) -> anyhow::Result<StepOutcome> {
    ensure_upgraded(state, &cycle.repository_id)
        .await
        .context("signing the repository's existing index before compacting it")?;
    let gc_id = cycle_gc_id(cycle)?;
    let source = open_repository(state, &cycle.repository_id).await?;
    let marker = own_marker(&source, &gc_id)
        .await?
        .ok_or_else(|| missing_marker(&gc_id))?;
    let marked: HashSet<BlobId> = marker.candidate_packs.iter().copied().collect();
    let seen: Seen = Arc::new(Mutex::new(cycle.progress.clone()));
    if hold_for_restores(state, cycle, &seen).await? {
        return Ok(StepOutcome::Postponed);
    }

    let mut verified = tracked(
        state,
        &cycle.id,
        &seen,
        verify(
            &source.store,
            &source.keys,
            &source.config,
            &marked,
            &record(&seen),
        ),
    )
    .await?
    .context("checking the snapshots again")?;
    let current = db::load_cycle(state, &cycle.id).await?;
    if current.is_none_or(|c| c.state != GcState::Verifying) {
        return finished(state, &cycle.id).await;
    }

    verified.shards.verified = load_verified(state, &cycle.repository_id).await?;
    leave_unvouched_shards(&mut verified.shards, &verified.candidates);
    let plan = compact_plan(&verified.shards, &verified.candidates);
    let mut entropy = Entropy::os();
    let compacted = write_compacted(
        &source.store,
        &source.keys,
        &plan,
        &verified.live,
        &verified.candidates,
        &mut entropy,
    )
    .await
    .context("compacting the index and checking that every live blob can still be found")?;
    record_verified(state, &cycle.repository_id, &compacted.verified)
        .await
        .context("recording the compacted shards as verified")?;
    let consolidated = consolidate_attestations(
        &source.store,
        &source.keys,
        false,
        &HashSet::new(),
        &mut entropy,
    )
    .await
    .context("consolidating the server attestations")?;
    let message = format!(
        "Attestations consolidated: {} read, {} written, {} deleted.",
        consolidated.read,
        consolidated.written.len(),
        consolidated.deleted
    );
    event(state, &cycle.id, "info", "attestations", &message).await;
    remove_rejected_attestations(state, cycle, &source).await?;

    refuse_old_agents(state, &cycle.repository_id).await?;
    refuse_foreign_marker(state, cycle, &source).await?;
    let resurrected: Vec<String> = verified.resurrected.iter().map(hex32).collect();
    let rewrite: Vec<String> = plan.rewrite.iter().map(hex32).collect();
    let admit = state.checks.admit.lock().await;
    if hold_for_restores(state, cycle, &seen).await? {
        return Ok(StepOutcome::Postponed);
    }
    if !db::begin_sweep(state, &cycle.id, &resurrected, &rewrite).await? {
        return finished(state, &cycle.id).await;
    }
    drop(admit);
    event(
        state,
        &cycle.id,
        "info",
        "sweeping",
        "Checks passed. Deleting the marked packs.",
    )
    .await;
    Ok(StepOutcome::Advanced(GcState::Sweeping))
}

/// Reports and deletes the `locks/attest-*` objects whose MAC does not verify
/// under the server key: only a writer key planted them, and every reader
/// ignores them. A valid attestation written meanwhile is never among them.
/// A storage that refuses the delete (Object Lock) keeps the object; it stays
/// ignored and is reported again next time.
async fn remove_rejected_attestations(
    state: &AppState,
    cycle: &CycleRow,
    source: &OpenRepository,
) -> anyhow::Result<()> {
    let set = read_attestations(&source.store, &source.keys)
        .await
        .context("reading the server attestations")?;
    if set.rejected.is_empty() {
        return Ok(());
    }
    let mut removed = Vec::new();
    for (key, reason) in &set.rejected {
        tracing::warn!(cycle_id = %cycle.id, key, reason, "attestation does not verify");
        let swept = sweep_object(&source.store, key, 0).await;
        match swept.failed.first() {
            None => removed.push(key.as_str()),
            Some((_, error)) => tracing::warn!(key, error, "removing a forged attestation failed"),
        }
    }
    let message = format!(
        "{} attestation object(s) did not verify under the server key and were ignored; \
         removed: {}.",
        set.rejected.len(),
        if removed.is_empty() {
            "none".to_string()
        } else {
            removed.join(", ")
        }
    );
    event(
        state,
        &cycle.id,
        "warning",
        "attestations_rejected",
        &message,
    )
    .await;
    Ok(())
}

/// Leaves out of the compaction every shard the server neither verified nor
/// attested, unless it names a pack that is going. Such a shard may belong to
/// a backup whose verification has not run yet; rewritten, its records would
/// sit in a shard nobody vouches for and the verification would fail an
/// honest job. An honest shard only names the young packs its backup wrote,
/// which are never candidates.
fn leave_unvouched_shards(shards: &mut ShardSet, candidates: &HashSet<BlobId>) {
    let (verified, attested) = (&shards.verified, &shards.attested);
    shards.shards.retain(|(id, shard)| {
        verified.contains(id)
            || attested.contains(id)
            || shard.packs.is_empty()
            || shard.packs.iter().any(|p| {
                BlobId::try_from(p.pack_id.as_slice()).is_ok_and(|p| candidates.contains(&p))
            })
    });
}

/// Restore jobs reading this repository, downloads in flight and checks
/// running on it. A restore whose source can't be read counts as reading this
/// one. New restores, downloads and checks wait while the sweep runs.
pub(crate) async fn restores_active(state: &AppState, repository_id: &str) -> anyhow::Result<bool> {
    let jobs = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT restore_json FROM jobs WHERE kind = 'restore' AND state IN ({ACTIVE_STATES})"
    )))
    .fetch_all(&state.db.pool)
    .await?;
    for row in jobs {
        let json: Option<String> = row.try_get("restore_json")?;
        let source = json
            .and_then(|json| serde_json::from_str::<RestoreRecord>(&json).ok())
            .map(|record| record.source_repository_id);
        match source {
            Some(source) if source != repository_id => {}
            _ => return Ok(true),
        }
    }
    let downloads = sqlx::query(
        "SELECT COUNT(*) AS n FROM downloads WHERE repository_id = $1 AND state = 'running'",
    )
    .bind(repository_id)
    .fetch_one(&state.db.pool)
    .await?;
    if downloads.try_get::<i64, _>("n")? > 0 {
        return Ok(true);
    }
    let checks = sqlx::query(
        "SELECT COUNT(*) AS n FROM check_runs \
         WHERE repository_id = $1 AND state IN ('queued', 'running')",
    )
    .bind(repository_id)
    .fetch_one(&state.db.pool)
    .await?;
    Ok(checks.try_get::<i64, _>("n")? > 0)
}

/// Records whether deletion has to wait for restores and downloads.
async fn hold_for_restores(
    state: &AppState,
    cycle: &CycleRow,
    seen: &Seen,
) -> anyhow::Result<bool> {
    let busy = restores_active(state, &cycle.repository_id).await?;
    let progress = {
        let mut seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.waiting_for_restores = busy;
        seen.clone()
    };
    db::set_progress(state, &cycle.id, &progress).await?;
    Ok(busy)
}

fn check(out: &SweepOutcome) -> anyhow::Result<()> {
    let Some((key, reason)) = out.failed.first() else {
        return Ok(());
    };
    let more = out.failed.len() - 1;
    if more == 0 {
        bail!("Deleting {key} failed: {reason}");
    }
    bail!("Deleting {key} failed: {reason} ({more} more failed)");
}

async fn pack_sizes<S: BlobStore>(
    store: &S,
    wanted: &HashSet<BlobId>,
) -> anyhow::Result<Vec<(BlobId, u64)>> {
    let mut out = Vec::new();
    for entry in store.list(PACKS_PREFIX).await? {
        let id = entry.key.rsplit('/').next().and_then(parse_hex32);
        if let Some(id) = id.filter(|id| wanted.contains(id) && pack_object_key(id) == entry.key) {
            out.push((id, entry.size));
        }
    }
    Ok(out)
}

/// Removes the objects a versioned bucket only hides, once their delete
/// marker is older than the pause: all their versions, then the markers.
async fn sweep_deleted<S: BlobStore + BlobDelete + BlobPurge>(
    store: &S,
    older_than: i64,
) -> anyhow::Result<SweepOutcome> {
    let mut swept = SweepOutcome::default();
    for prefix in SWEPT_PREFIXES {
        for key in store.deleted_keys(prefix, older_than).await? {
            let out = purge_object(store, &key, 0).await;
            check(&out)?;
            swept.merge(out);
        }
    }
    Ok(swept)
}

async fn sweeping(state: &AppState, cycle: &CycleRow, now_ms: i64) -> anyhow::Result<StepOutcome> {
    let gc_id = cycle_gc_id(cycle)?;
    let source = open_repository(state, &cycle.repository_id).await?;
    let store = &source.store;
    let mut swept = SweepOutcome::default();
    let mut deleted_packs = None;

    let marker = own_marker(&source, &gc_id).await?;
    if let Some(marker) = &marker {
        let _verifications = SWEEP.write().await;
        let resurrected: HashSet<String> = cycle.resurrected.iter().cloned().collect();
        let doomed: HashSet<BlobId> = marker
            .candidate_packs
            .iter()
            .copied()
            .filter(|id| !resurrected.contains(&hex32(id)))
            .collect();

        let listed: HashSet<String> = store
            .list(INDEX_PREFIX)
            .await?
            .into_iter()
            .map(|e| e.key)
            .collect();
        let old: Vec<BlobId> = cycle
            .rewrite
            .iter()
            .filter_map(|hex| parse_hex32(hex))
            .filter(|id| listed.contains(&index_object_key(id)))
            .collect();
        let out = sweep_shards(store, &old).await;
        check(&out)?;
        swept.merge(out);
        state.readers.invalidate(&cycle.repository_id);

        confirm_unreferenced(store, &source.keys, &doomed)
            .await
            .context("checking that no index names a pack that is going")?;
        let packs = pack_sizes(store, &doomed).await?;
        let out = sweep_packs(store, &packs).await;
        check(&out)?;
        deleted_packs = Some(doomed);
        swept.merge(out);
    }

    let older_than = now_ms.saturating_sub(cycle.settings.pause_ms);
    swept.merge(sweep_deleted(store, older_than).await?);
    state.readers.invalidate(&cycle.repository_id);
    if let Some(published) = marker.as_ref().and_then(|m| m.modified_ms) {
        let locks = read_writer_locks(store, source.keys.k_idx(), source.keys.repo_id()).await?;
        for lock in locks
            .iter()
            .filter(|w| w.done && w.modified_ms.is_some_and(|written| written < published))
        {
            for key in [lock.key.clone(), writer_done_key(&lock.id)] {
                let out = sweep_object(store, &key, 0).await;
                check(&out)?;
                swept.merge(out);
            }
        }
    }
    if marker.is_some() {
        let out = sweep_object(store, &gc_manifest_key(&gc_id), 0).await;
        check(&out)?;
        swept.merge(out);
    }

    let report = final_report(state, cycle, now_ms, &swept, deleted_packs.as_ref()).await?;
    event(
        state,
        &cycle.id,
        "info",
        "finished",
        &format!(
            "Cleanup finished: {} freed.",
            format_size(report.summary.bytes_freed)
        ),
    )
    .await;
    db::finish_cycle(state, &cycle.id, &report).await?;
    finished(state, &cycle.id).await
}

/// `deleted_packs` is `None` when the marker was already gone, as on a run
/// that resumes after the marker was taken down: the packs deleted are then
/// those of the stored plan that were not brought back.
async fn final_report(
    state: &AppState,
    cycle: &CycleRow,
    now_ms: i64,
    swept: &SweepOutcome,
    deleted_packs: Option<&HashSet<BlobId>>,
) -> anyhow::Result<GcFullReport> {
    let mut report = db::load_plan_report(state, &cycle.id)
        .await?
        .context("the cleanup plan is missing")?;
    let gone: HashSet<String> = match deleted_packs {
        Some(packs) => packs.iter().map(hex32).collect(),
        None => report
            .delete
            .iter()
            .chain(&report.repack)
            .map(|p| p.id.clone())
            .filter(|id| !cycle.resurrected.contains(id))
            .collect(),
    };
    let count = |items: &[super::api::GcPackItem]| {
        items.iter().filter(|p| gone.contains(&p.id)).count() as u64
    };
    let summary = &mut report.summary;
    summary.finished_at = now_ms;
    summary.duration_ms = now_ms
        .saturating_sub(cycle.started_at.unwrap_or(now_ms))
        .max(0) as u64;
    summary.bytes_freed = swept.bytes_freed;
    summary.packs_deleted = count(&report.delete);
    summary.packs_repacked = count(&report.repack);
    summary.repack_transfer_bytes = cycle.progress.repack_read + cycle.progress.repack_written;
    apply_locked(state, cycle, &mut report, swept).await;
    Ok(report)
}

/// Lists the versions the storage refused to delete. A lock ends at the
/// version's time plus the bucket's `default_retention`; the refusal came
/// from the storage's own clock, so that is judged by the wall clock rather
/// than by the cycle's.
async fn apply_locked(
    state: &AppState,
    cycle: &CycleRow,
    report: &mut GcFullReport,
    swept: &SweepOutcome,
) {
    let retention = if swept.retained.is_empty() {
        None
    } else {
        bucket_retention(state, &cycle.repository_id).await
    };
    let now = now_ms();
    let mut seen = HashSet::new();
    report.locked = swept
        .retained
        .iter()
        .filter(|(key, version)| seen.insert((key.as_str(), version.version.as_str())))
        .map(|(key, version)| GcLockedItem {
            key: key.clone(),
            size: version.size,
            until: held_until(version.modified_ms, retention, now),
            modified_ms: version.modified_ms,
        })
        .collect();
    let summary = &mut report.summary;
    summary.locked_bytes = report.locked.iter().map(|l| l.size).sum();
    summary.locked_until = report.locked.iter().filter_map(|l| l.until).min();
    summary.locked_until_unknown = report.locked.iter().any(|l| l.until.is_none());
}
