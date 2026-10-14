use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use indicatif::HumanBytes;
use lodestore_format::fs;
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::walk::WalkOptions;
use lodestore_format::index::hex32;
use lodestore_format::snapshot::{ParentTree, find_parent};
use lodestore_format::writer::{BackupOptions, BackupProgress, canonical_roots};
use serde::Serialize;

use crate::cli::{BackupFlags, SecretArgs, UsageError};
use crate::open::open_for_write;
use crate::output::{JsonPath, Output, StatsJson, Throttle, spinner, warn_skipped};

const EXIT_INCOMPLETE: u8 = 3;
const EXIT_INTERRUPTED: i32 = 130;

#[derive(Serialize)]
struct ProgressJson<'a> {
    files_done: u64,
    bytes_done: u64,
    bytes_added: u64,
    current_path: JsonPath<'a>,
}

#[derive(Serialize)]
struct SummaryJson {
    snapshot_id: String,
    incomplete: bool,
    parent: Option<String>,
    stats: StatsJson,
    errors_total: u64,
    warnings_total: u64,
}

/// The first Ctrl+C asks the backup to wind down (open packs are uploaded and
/// indexed, no snapshot is written); the second ends the process at once. A
/// thread of its own handles the signal, so it is seen while the backup is
/// busy reading and hashing, not only when it next waits on the store.
fn cancel_on_ctrl_c() -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                flag.store(true, Ordering::Relaxed);
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(EXIT_INTERRUPTED);
                }
            }
        });
    });
    cancel
}

pub async fn run(
    url: &str,
    args: &SecretArgs,
    out: Output,
    paths: &[PathBuf],
    flags: BackupFlags,
) -> Result<u8> {
    if args.writer_keys.is_some() && flags.parent.is_some() {
        return Err(UsageError(
            "--parent cannot be used with --writer-keys: writer keys cannot read snapshots".into(),
        )
        .into());
    }
    let (mut writer, reader) = open_for_write(url, args).await?;
    let hostname = flags.host.map_or_else(fs::hostname, String::into_bytes);
    let roots: Vec<Vec<u8>> = canonical_roots(paths)?
        .iter()
        .map(|r| r.as_os_str().as_bytes().to_vec())
        .collect();
    let parent = match reader {
        None => None,
        Some(reader) => {
            let found = match &flags.parent {
                Some(spec) => Some(reader.resolve_snapshot(spec).await?.id),
                None => {
                    let list = reader.list_snapshots().await?;
                    warn_skipped(&list.skipped);
                    find_parent(&list.entries, &hostname, &roots).map(|e| e.id)
                }
            };
            match found {
                Some(id) => match ParentTree::open(reader, id).await {
                    Ok(parent) => Some(parent),
                    Err(e) if flags.parent.is_none() => {
                        eprintln!(
                            "warning: parent snapshot {} is unreadable ({e}); backing up without a parent",
                            hex32(&id)
                        );
                        None
                    }
                    Err(e) => return Err(e.into()),
                },
                None => None,
            }
        }
    };
    let parent_id = parent.as_ref().map(|p| hex32(&p.id()));
    let exclude = if flags.exclude.is_empty() && flags.exclude_file.is_empty() {
        None
    } else {
        let mut matcher = PathMatcher::new(&flags.exclude)?;
        for file in &flags.exclude_file {
            matcher.add_file(file)?;
        }
        Some(matcher)
    };
    let throttle = Throttle::new();
    let bar = if out.json { None } else { spinner() };
    let progress = |p: &BackupProgress| {
        if p.starting || !throttle.ready() {
            return;
        }
        if out.json {
            out.emit(
                "backup_progress",
                ProgressJson {
                    files_done: p.files_done,
                    bytes_done: p.bytes_done,
                    bytes_added: p.bytes_added,
                    current_path: JsonPath(&p.current),
                },
            );
        } else if let Some(bar) = &bar {
            bar.set_message(format!(
                "{} files, {} read, {} added  {}",
                p.files_done,
                HumanBytes(p.bytes_done),
                HumanBytes(p.bytes_added),
                String::from_utf8_lossy(&p.current)
            ));
        }
    };
    let outcome = writer
        .backup(
            paths,
            BackupOptions {
                hostname: Some(hostname),
                tags: flags.tag,
                parent,
                force_rescan: flags.force_rescan,
                walk: WalkOptions {
                    exclude,
                    one_file_system: flags.one_file_system,
                    exclude_caches: flags.exclude_caches,
                },
                cancel: cancel_on_ctrl_c(),
                ..Default::default()
            },
            &progress,
        )
        .await;
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    let outcome = outcome?;

    let id = hex32(&outcome.snapshot_id);
    let stats = &outcome.stats;
    if out.json {
        out.emit(
            "backup_summary",
            SummaryJson {
                snapshot_id: id,
                incomplete: outcome.incomplete,
                parent: parent_id,
                stats: Some(stats).into(),
                errors_total: outcome.errors_total,
                warnings_total: outcome.warnings_total,
            },
        );
    } else {
        println!("snapshot {id} saved");
        println!(
            "files: {} new, {} changed, {} unchanged; {} read, {} added to the repository",
            stats.files_new,
            stats.files_changed,
            stats.files_unchanged,
            HumanBytes(stats.bytes_read),
            HumanBytes(stats.bytes_added)
        );
        if outcome.incomplete {
            eprintln!(
                "warning: {} errors; some files could not be read and the snapshot is incomplete",
                outcome.errors_total
            );
        }
    }
    Ok(if outcome.incomplete {
        EXIT_INCOMPLETE
    } else {
        0
    })
}
