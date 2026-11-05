use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use indicatif::{HumanBytes, ProgressBar};
use lodestore_format::attest::{put_attestation, seal_provenance};
use lodestore_format::check::is_data_error;
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::fs;
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::walk::WalkOptions;
use lodestore_format::index::hex32;
use lodestore_format::locks::UNMARKED_WRITER_LIMIT;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::{ParentTree, find_parent};
use lodestore_format::store::Store;
use lodestore_format::verify_snapshot::{
    check_snapshot_resolves, snapshot_index_refs, verify_shard,
};
use lodestore_format::writer::{
    BackupOptions, BackupOutcome, BackupProgress, Deadline, Writer, canonical_roots,
};
use serde::Serialize;

use crate::cli::{BackupFlags, SecretArgs, UsageError};
use crate::open::{Marker, Strict, open_for_write};
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
    let hostname = flags
        .host
        .clone()
        .map_or_else(fs::hostname, String::into_bytes);
    let opened = open_for_write(url, args, &hostname, flags.no_gc_check).await?;
    let deadline = unmarked_deadline(&opened.marker);
    let result = backup_paths(
        opened.writer,
        opened.reader,
        opened.strict,
        hostname,
        deadline,
        out,
        paths,
        flags,
    )
    .await;
    opened.marker.close().await;
    result
}

fn unmarked_deadline(marker: &Marker) -> Option<Deadline> {
    let deadline = marker.unmarked_deadline()?;
    eprintln!(
        "warning: cannot write the cleanup marker in locks/ (access denied); this backup must finish within {}h or no snapshot is saved",
        UNMARKED_WRITER_LIMIT.as_secs() / 3600
    );
    Some(deadline)
}

#[allow(clippy::too_many_arguments)]
async fn backup_paths(
    mut writer: Writer<Store>,
    reader: Option<Reader<Store>>,
    mut strict: Option<Strict>,
    hostname: Vec<u8>,
    deadline: Option<Deadline>,
    out: Output,
    paths: &[PathBuf],
    flags: BackupFlags,
) -> Result<u8> {
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
                    let mut list = reader.list_snapshots().await?;
                    warn_skipped(&list.skipped);
                    if let Some(strict) = &strict {
                        list.entries.retain(|e| strict.provenance.contains(&e.id));
                    }
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
                hostname: Some(hostname.clone()),
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
                deadline,
                ..Default::default()
            },
            &progress,
        )
        .await;
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    let outcome = outcome?;
    if let Some(strict) = &mut strict {
        check_and_sign(strict, &writer, &outcome, &hostname).await?;
    }
    finish(out, outcome, parent_id)
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
    let name = flags.name.clone().unwrap_or_default();
    if name.is_empty() {
        return Err(UsageError("--name must not be empty".into()).into());
    }
    if args.writer_keys.is_some() && flags.parent.is_some() {
        return Err(UsageError(
            "--parent cannot be used with --writer-keys: writer keys cannot read snapshots".into(),
        )
        .into());
    }
    let hostname = flags
        .host
        .clone()
        .map_or_else(fs::hostname, String::into_bytes);
    let opened = open_for_write(url, args, &hostname, flags.no_gc_check).await?;
    let deadline = unmarked_deadline(&opened.marker);
    let result = backup_stdin(
        opened.writer,
        opened.reader,
        opened.strict,
        hostname,
        deadline,
        out,
        name,
        flags,
    )
    .await;
    opened.marker.close().await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn backup_stdin(
    mut writer: Writer<Store>,
    reader: Option<Reader<Store>>,
    mut strict: Option<Strict>,
    hostname: Vec<u8>,
    deadline: Option<Deadline>,
    out: Output,
    name: String,
    flags: BackupFlags,
) -> Result<u8> {
    let tag = format!("stdin={name}");
    let parent = match reader {
        None => None,
        Some(reader) => {
            let found = match &flags.parent {
                Some(spec) => Some(reader.resolve_snapshot(spec).await?.id),
                None => {
                    let mut list = reader.list_snapshots().await?;
                    warn_skipped(&list.skipped);
                    if let Some(strict) = &strict {
                        list.entries.retain(|e| strict.provenance.contains(&e.id));
                    }
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
                hostname: Some(hostname.clone()),
                tags,
                parent,
                cancel: cancel_on_ctrl_c(),
                deadline,
                ..Default::default()
            },
            &progress,
        )
        .await;
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    let outcome = outcome?;
    if let Some(strict) = &mut strict {
        check_and_sign(strict, &writer, &outcome, &hostname).await?;
    }
    finish(out, outcome, parent_id)
}

/// Checks the snapshot the way the server does before it counts as verified,
/// then signs it as the repository owner. Any failure leaves it unsigned.
async fn check_and_sign(
    strict: &mut Strict,
    writer: &Writer<Store>,
    outcome: &BackupOutcome,
    hostname: &[u8],
) -> Result<()> {
    let short = hex32(&outcome.snapshot_id)[..8].to_string();
    let result = check_own_snapshot(strict, writer, outcome, &short).await;
    let signed = match result {
        Ok(newly) => sign_as_owner(strict, writer, outcome, hostname, newly).await,
        Err(e) => Err(e),
    };
    if let Err(e) = strict.cache.save() {
        eprintln!("warning: {e:#}");
    }
    signed
}

async fn check_own_snapshot(
    strict: &mut Strict,
    writer: &Writer<Store>,
    outcome: &BackupOutcome,
    short: &str,
) -> Result<Vec<BlobId>> {
    let (store, keys) = (&strict.store, &strict.keys);
    let id = outcome.snapshot_id;
    let fail = |e: &lodestore_format::Error| {
        if is_data_error(e) {
            anyhow::anyhow!(
                "the backup was written but its data does not check out: {e}. Snapshot {short} is left unsigned and `latest` skips it; the failing shard will not be used again, run the backup again."
            )
        } else {
            anyhow::anyhow!("could not check the backup: {e}; snapshot {short} is left unsigned")
        }
    };
    let refs = snapshot_index_refs(store, keys, &id)
        .await
        .map_err(|e| fail(&e))?;
    let mut verified: HashSet<BlobId> = writer.written_shards().iter().copied().collect();
    verified.extend(&strict.cache.verified);
    let mut newly = Vec::new();
    for shard in refs {
        if verified.contains(&shard) {
            continue;
        }
        match verify_shard(store, keys, &shard).await {
            Ok(Some(_)) => {
                verified.insert(shard);
                newly.push(shard);
            }
            Ok(None) => {}
            Err(e) => {
                if is_data_error(&e) {
                    strict.cache.add_distrusted(shard);
                }
                return Err(fail(&e));
            }
        }
    }
    let check = check_snapshot_resolves(store, keys, &id, &verified, &strict.attested)
        .await
        .map_err(|e| fail(&e))?;
    for lying in check.lying {
        strict.cache.add_distrusted(lying);
    }
    newly.extend(check.newly_verified);
    Ok(newly)
}

async fn sign_as_owner(
    strict: &mut Strict,
    writer: &Writer<Store>,
    outcome: &BackupOutcome,
    hostname: &[u8],
    newly: Vec<BlobId>,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let record = pb::ProvenanceRecord {
        snapshot_id: outcome.snapshot_id.to_vec(),
        host_id: String::new(),
        hostname: String::from_utf8_lossy(hostname).into_owned(),
        job_id: String::new(),
        verified_at: Some(pb::Timestamp {
            seconds: i64::try_from(now.as_secs()).unwrap_or(i64::MAX),
            nanos: now.subsec_nanos(),
        }),
    };
    let mut entropy = Entropy::os();
    let sealed = seal_provenance(&strict.keys, &record, &mut entropy)?;
    put_attestation(
        &strict.store,
        &strict.keys,
        writer.written_shards(),
        &[],
        vec![sealed],
        &mut entropy,
    )
    .await
    .context("cannot sign the snapshot")?;
    strict
        .cache
        .add_verified(writer.written_shards().iter().copied().chain(newly));
    Ok(())
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
