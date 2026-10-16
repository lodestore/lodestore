use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use indicatif::{HumanBytes, ProgressBar};
use lodestore_format::crypto::BlobId;
use lodestore_format::fs;
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::walk::WalkOptions;
use lodestore_format::index::hex32;
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::{ParentTree, find_parent};
use lodestore_format::store::Store;
use lodestore_format::writer::{BackupOptions, BackupOutcome, BackupProgress, canonical_roots};
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
    files_total: u64,
    bytes_total: u64,
    scan_done: bool,
    bytes_processed: u64,
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
    if flags.stdin {
        return run_stdin(url, args, out, flags).await;
    }
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
            open_parent(reader, found, flags.parent.is_some()).await?
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
    let progress = progress_sink(out, &throttle, bar.as_ref());
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
                prescan: true,
                ..Default::default()
            },
            &progress,
        )
        .await;
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    finish(out, outcome?, parent_id)
}

async fn open_parent(
    reader: Reader<Store>,
    found: Option<BlobId>,
    explicit: bool,
) -> Result<Option<ParentTree<'static>>> {
    let Some(id) = found else {
        return Ok(None);
    };
    match ParentTree::open(reader, id).await {
        Ok(parent) => Ok(Some(parent)),
        Err(e) if !explicit => {
            eprintln!(
                "warning: parent snapshot {} is unreadable ({e}); backing up without a parent",
                hex32(&id)
            );
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

fn progress_sink<'a>(
    out: Output,
    throttle: &'a Throttle,
    bar: Option<&'a ProgressBar>,
) -> impl Fn(&BackupProgress) + 'a {
    move |p| {
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
                    files_total: p.files_total,
                    bytes_total: p.bytes_total,
                    scan_done: p.scan_done,
                    bytes_processed: p.bytes_processed,
                },
            );
        } else if let Some(bar) = bar {
            let files = if p.scan_done {
                format!("{}/{}", p.files_done, p.files_total)
            } else {
                p.files_done.to_string()
            };
            bar.set_message(format!(
                "{files} files, {} read, {} added  {}",
                HumanBytes(p.bytes_done),
                HumanBytes(p.bytes_added),
                String::from_utf8_lossy(&p.current)
            ));
        }
    }
}

async fn run_stdin(url: &str, args: &SecretArgs, out: Output, flags: BackupFlags) -> Result<u8> {
    let name = flags.name.unwrap_or_default();
    if name.is_empty() {
        return Err(UsageError("--name must not be empty".into()).into());
    }
    if args.writer_keys.is_some() && flags.parent.is_some() {
        return Err(UsageError(
            "--parent cannot be used with --writer-keys: writer keys cannot read snapshots".into(),
        )
        .into());
    }
    let (mut writer, reader) = open_for_write(url, args).await?;
    let hostname = flags.host.map_or_else(fs::hostname, String::into_bytes);
    let tag = format!("stdin={name}");
    let parent = match reader {
        None => None,
        Some(reader) => {
            let found = match &flags.parent {
                Some(spec) => Some(reader.resolve_snapshot(spec).await?.id),
                None => {
                    let list = reader.list_snapshots().await?;
                    warn_skipped(&list.skipped);
                    list.entries
                        .iter()
                        .rev()
                        .find(|e| e.snapshot.hostname == hostname && e.snapshot.tags.contains(&tag))
                        .map(|e| e.id)
                }
            };
            open_parent(reader, found, flags.parent.is_some()).await?
        }
    };
    let parent_id = parent.as_ref().map(|p| hex32(&p.id()));
    let mut tags = flags.tag;
    if let Some(policy) = flags.policy {
        tags.push(format!("policy={policy}"));
    }
    let throttle = Throttle::new();
    let bar = if out.json { None } else { spinner() };
    let progress = progress_sink(out, &throttle, bar.as_ref());
    let outcome = writer
        .backup_stream(
            name.as_bytes(),
            &mut std::io::stdin().lock(),
            BackupOptions {
                hostname: Some(hostname),
                tags,
                parent,
                cancel: cancel_on_ctrl_c(),
                ..Default::default()
            },
            &progress,
        )
        .await;
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    finish(out, outcome?, parent_id)
}

fn finish(out: Output, outcome: BackupOutcome, parent_id: Option<String>) -> Result<u8> {
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
