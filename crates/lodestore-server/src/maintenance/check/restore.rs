//! The `restore` level: the server restores a random sample of files into a
//! scratch folder with its own key, compares them with the snapshots and
//! deletes the folder again.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use lodestore_format::check::sample::{MAX_SAMPLE_NODES, SampleOptions, sample_restore};
use lodestore_format::crypto::Entropy;
use lodestore_format::reader::Reader;
use tokio::runtime::Handle;

use super::api::{CheckPhase, CheckProgress, CheckSummary, RestoreSummary, StoredReport};
use super::db::{self, RunRow};
use super::run::{PROGRESS_PERIOD, findings_of, until_done};
use super::{CheckLevel, CheckOutcome};
use crate::catalog::OpenRepository;
use crate::state::AppState;

pub const SIZE_LIMIT: &str = "Stopped at the size limit.";
pub const NO_FILES: &str = "There were no files to restore.";

/// The scratch folder of one run, removed with the guard whatever way the run ends.
struct Scratch(PathBuf);

impl Scratch {
    fn create(path: PathBuf) -> anyhow::Result<Scratch> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .context("creating the scratch folder")?;
        Ok(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.0.display(), %error, "removing the scratch folder failed");
        }
    }
}

/// Makes the restoring thread stop at its next file when the run is left.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub async fn restore(state: &AppState, run: &RunRow, source: OpenRepository) -> anyhow::Result<()> {
    let settings = db::load_level_settings(state, &run.repository_id, CheckLevel::Restore).await?;
    let scratch = Scratch::create(state.config.data_dir.join("check-restore").join(&run.id))?;
    let mut reader = Reader::open(source.store, source.keys, &source.config)
        .context("opening the repository")?;

    let seen = Arc::new(Mutex::new(CheckProgress {
        phase: CheckPhase::Listing,
        done: 0,
        total: 0,
        bytes: 0,
    }));
    let listing = reader.list_snapshots();
    let Some(listed) = until_done(state, &run.id, &seen, listing).await? else {
        db::abort_run(state, &run.id).await?;
        return Ok(());
    };
    let snapshots: Vec<_> = listed
        .context("listing the snapshots")?
        .entries
        .iter()
        .map(|entry| entry.id)
        .collect();

    let stop = Arc::new(AtomicBool::new(false));
    let _stop_on_exit = StopOnDrop(stop.clone());
    let options = SampleOptions {
        files: settings.files_per_run,
        max_bytes: settings.max_data_per_run,
        scratch: scratch.0.clone(),
        max_nodes: MAX_SAMPLE_NODES,
    };
    let handle = Handle::current();
    let mut sampling = tokio::task::spawn_blocking({
        let (stop, seen) = (stop.clone(), seen.clone());
        move || {
            let progress = |done: u64, total: u64| {
                *seen.lock().unwrap_or_else(|p| p.into_inner()) = CheckProgress {
                    phase: CheckPhase::Restoring,
                    done,
                    total,
                    bytes: 0,
                };
            };
            let should_stop = || stop.load(Ordering::Relaxed);
            handle.block_on(sample_restore(
                &mut reader,
                &snapshots,
                &options,
                &mut Entropy::os(),
                &progress,
                &should_stop,
            ))
        }
    });

    let mut tick = tokio::time::interval(PROGRESS_PERIOD);
    let mut cancelled = false;
    let sampled = loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                if db::cancel_requested(state, &run.id).await? {
                    cancelled = true;
                    stop.store(true, Ordering::Relaxed);
                } else {
                    let shown = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    db::set_progress(state, &run.id, &shown).await?;
                }
            }
            joined = &mut sampling => break joined.context("the test restore stopped unexpectedly")?,
        }
    };
    drop(scratch);
    if cancelled {
        db::abort_run(state, &run.id).await?;
        return Ok(());
    }
    let sampled = sampled.context("restoring the sample")?;

    let findings = findings_of(&sampled.findings);
    let stored = StoredReport {
        summary: CheckSummary {
            restore: Some(RestoreSummary {
                snapshots_sampled: sampled.snapshots_sampled,
                files_restored: sampled.files_restored,
                bytes_restored: sampled.bytes_restored,
                hash_mismatches: sampled.hash_mismatches,
                skipped: sampled.skipped,
                stopped_at_limit: sampled.stopped_at_limit,
            }),
            ..Default::default()
        },
        findings_total: findings.len() as u64,
        findings,
        notes: Vec::new(),
        notes_total: 0,
    };
    if sampled.bytes_restored == 0 && sampled.stopped_at_limit {
        return db::stop_run(state, &run.id, SIZE_LIMIT, &stored).await;
    }
    if sampled.files_restored == 0 && stored.findings_total == 0 {
        return db::fail_run(state, &run.id, NO_FILES).await;
    }
    let outcome = if stored.findings_total == 0 {
        CheckOutcome::Passed
    } else {
        CheckOutcome::Problems
    };
    db::finish_run(state, &run.id, outcome, &stored).await
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::Scratch;

    #[test]
    fn scratch_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("check-restore").join("run-1");
        let scratch = Scratch::create(run.clone()).unwrap();
        for path in [run.parent().unwrap(), run.as_path()] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", path.display());
        }
        drop(scratch);
        assert!(!run.exists());
    }
}
