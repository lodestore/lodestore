use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use indicatif::{HumanBytes, ProgressBar};
use lodestore_format::attest::{put_attestation, seal_provenance};
use lodestore_format::check::is_data_error;
use lodestore_format::crypto::{BlobId, Entropy, RepoId};
use lodestore_format::fs;
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::raw::FileSource;
use lodestore_format::fs::walk::WalkOptions;
use lodestore_format::image::{
    BitmapNames, ChunkRef, DecideInput, FullReason, IMAGE_NAME_RULE, ImageParent, ImageSource,
    ImageState, MAX_IMAGE_SIZE, ReadMode, ReadPlan, SEGMENT_CHUNKS, StateRead, decide,
    instance_snapshots, load_image, source_identity, valid_image_name,
};
use lodestore_format::index::hex32;
use lodestore_format::locks::UNMARKED_WRITER_LIMIT;
use lodestore_format::pb;
use lodestore_format::provenance;
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::{ParentTree, SnapshotEntry, find_parent};
use lodestore_format::source::SourceKey;
use lodestore_format::store::Store;
use lodestore_format::verify_snapshot::{
    check_snapshot_resolves, snapshot_index_refs, verify_shard,
};
use lodestore_format::writer::{
    BackupOptions, BackupOutcome, BackupProgress, Deadline, ImageOptions, Writer, canonical_roots,
};
use lodestore_nbd::{Client, Limits, NbdSource, NbdUri, is_nbd_uri};
use serde::Serialize;

use super::resolve_spec;
use crate::cli::{BackupFlags, SecretArgs, UsageError};
use crate::image_state;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<ImageJson>,
}

#[derive(Serialize)]
struct ImageJson {
    key: String,
    size: u64,
    read_mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitmap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_bitmap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitmap_to_create: Option<String>,
    #[serde(skip)]
    mode_text: String,
    #[serde(skip)]
    next_hint: Option<String>,
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
    if flags.image.is_some() {
        return run_image(url, args, out, flags).await;
    }
    if flags.name.is_some() {
        return Err(UsageError("--name needs --stdin or --image".into()).into());
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
                Some(spec) => Some(explicit_parent(&reader, strict.as_ref(), spec).await?),
                None => {
                    let entries = parent_candidates(&reader, strict.as_ref(), &hostname).await?;
                    find_parent(&entries, &hostname, &roots).map(|e| e.id)
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
        check_and_sign(strict, &writer, &outcome, &hostname, None).await?;
    }
    finish(out, outcome, parent_id, None)
}

/// The snapshot `--parent` names; `latest` follows the same rules as restore.
async fn explicit_parent(
    reader: &Reader<Store>,
    strict: Option<&Strict>,
    spec: &str,
) -> Result<BlobId> {
    let id = resolve_spec(reader, spec, None).await?.id;
    if strict.is_some_and(|s| !s.provenance.contains(&id)) {
        eprintln!(
            "warning: parent snapshot {} is not signed by the server or by the password holder",
            hex32(&id)
        );
    }
    Ok(id)
}

/// The snapshots a parent is looked for in, oldest first. In an upgraded
/// repository only those signed for this hostname count, in the order the
/// store wrote them: the manifest's own hostname and time are whatever the
/// host that wrote it chose.
async fn parent_candidates(
    reader: &Reader<Store>,
    strict: Option<&Strict>,
    hostname: &[u8],
) -> Result<Vec<SnapshotEntry>> {
    let list = reader.list_snapshots().await?;
    warn_skipped(&list.skipped);
    let mut entries = list.entries;
    if let Some(strict) = strict {
        let hostname = String::from_utf8_lossy(hostname);
        entries.retain(|e| {
            strict
                .provenance
                .get(&e.id)
                .is_some_and(|r| r.hostname == hostname)
        });
        entries.sort_by_key(|e| strict.provenance.stored_order(e));
    }
    Ok(entries)
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
                Some(spec) => Some(explicit_parent(&reader, strict.as_ref(), spec).await?),
                None => parent_candidates(&reader, strict.as_ref(), &hostname)
                    .await?
                    .iter()
                    .rev()
                    .find(|e| e.snapshot.hostname == hostname && e.snapshot.tags.contains(&tag))
                    .map(|e| e.id),
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
        check_and_sign(strict, &writer, &outcome, &hostname, None).await?;
    }
    finish(out, outcome, parent_id, None)
}

enum ImageInput {
    File(FileSource),
    Nbd(NbdUri),
}

async fn run_image(url: &str, args: &SecretArgs, out: Output, flags: BackupFlags) -> Result<u8> {
    let spec = flags.image.clone().unwrap_or_default();
    let (nbd, default_name) = if is_nbd_uri(&spec) {
        let uri = NbdUri::parse(&spec).map_err(|e| UsageError(format!("{spec:?}: {e}")))?;
        let export = Some(uri.export.clone()).filter(|e| !e.is_empty());
        (Some(uri), export)
    } else if spec.contains("://") {
        return Err(UsageError(
            "--image reads a file, a block device or an nbd+unix:// export".into(),
        )
        .into());
    } else {
        let file_name = std::path::Path::new(&spec)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned);
        (None, file_name)
    };
    let explicit_name = flags.name.is_some();
    let name = match (flags.name.clone(), default_name) {
        (Some(name), _) | (None, Some(name)) => name,
        (None, None) if nbd.is_some() => {
            return Err(
                UsageError("--name is needed for an NBD export without a name".into()).into(),
            );
        }
        (None, None) => {
            return Err(UsageError(format!("{spec:?} has no usable file name; use --name")).into());
        }
    };
    if !valid_image_name(&name) {
        return Err(UsageError(format!("{name:?} is not an image name: {IMAGE_NAME_RULE}")).into());
    }
    if args.writer_keys.is_some() && flags.parent.is_some() {
        return Err(UsageError(
            "--parent cannot be used with --writer-keys: writer keys cannot read snapshots".into(),
        )
        .into());
    }
    if flags.bitmap.is_some() && nbd.is_none() {
        return Err(UsageError("--bitmap needs an nbd+unix:// --image".into()).into());
    }
    if flags.bitmap.is_some() && args.writer_keys.is_some() {
        return Err(UsageError(
            "--bitmap cannot be used with --writer-keys: writer keys cannot read snapshots".into(),
        )
        .into());
    }
    let input = match nbd {
        Some(uri) => ImageInput::Nbd(uri),
        None => {
            let source =
                FileSource::open(spec.as_ref()).with_context(|| format!("cannot open {spec}"))?;
            super::device::warn_if_in_use(spec.as_ref());
            ImageInput::File(source)
        }
    };
    let hostname = flags
        .host
        .clone()
        .map_or_else(fs::hostname, String::into_bytes);
    let opened = open_for_write(url, args, &hostname, flags.no_gc_check).await?;
    let deadline = unmarked_deadline(&opened.marker);
    let key = SourceKey::image(&name)?;
    let run = ImageRun {
        names: BitmapNames::new(&opened.repo_id, &hostname, &key),
        writer: opened.writer,
        reader: opened.reader,
        strict: opened.strict,
        repo_id: opened.repo_id,
        hostname,
        name,
        key,
        tags: flags.tag,
        deadline,
        out,
    };
    let result = run.go(input, flags.bitmap.as_deref(), explicit_name).await;
    opened.marker.close().await;
    result
}

struct ImageRun {
    writer: Writer<Store>,
    reader: Option<Reader<Store>>,
    strict: Option<Strict>,
    repo_id: RepoId,
    hostname: Vec<u8>,
    name: String,
    key: SourceKey,
    names: BitmapNames,
    tags: Vec<String>,
    deadline: Option<Deadline>,
    out: Output,
}

/// What the run knows about the dirty bitmap of an NBD export; a file has none.
struct Binding {
    nbd: bool,
    bitmap: Option<u64>,
    has_next: bool,
    state: StateRead,
    state_path: Option<PathBuf>,
    source_id: [u8; 32],
}

impl ImageRun {
    /// The order is the safety: the state file is read into memory and removed
    /// before anything connects to the export, so any failure after this point
    /// leaves no state behind, and it is written back only once the snapshot is
    /// confirmed.
    async fn go(self, input: ImageInput, bitmap: Option<&str>, explicit_name: bool) -> Result<u8> {
        let bitmap = bitmap
            .map(|text| {
                self.names.parse(text).ok_or_else(|| {
                    let zero = self.names.name(0);
                    let prefix = zero.strip_suffix('0').unwrap_or(&zero);
                    UsageError(format!(
                        "the dirty bitmap of {} on {} in this repository is named {prefix}<N>, with N from 1",
                        self.key,
                        String::from_utf8_lossy(&self.hostname)
                    ))
                })
            })
            .transpose()?;
        let uri = match input {
            ImageInput::File(mut source) => {
                let binding = Binding {
                    nbd: false,
                    bitmap: None,
                    has_next: false,
                    state: StateRead::Missing,
                    state_path: None,
                    source_id: source_identity(&self.name),
                };
                return self.write(&mut source, binding).await;
            }
            ImageInput::Nbd(uri) => uri,
        };
        let source_id = source_identity(&uri.canonical());
        let state_path = image_state::state_path(&self.repo_id, &self.hostname, &self.key);
        let (_lock, state) = match &state_path {
            None => {
                eprintln!(
                    "warning: no cache folder (set HOME or XDG_CACHE_HOME); every backup of an NBD export reads the image in full"
                );
                (None, StateRead::Missing)
            }
            Some(path) => {
                let lock = image_state::lock(path, &self.key)?;
                let state = image_state::load(path, &self.repo_id, &self.hostname, &self.key);
                if let StateRead::Found(found) = &state
                    && found.source != source_id
                    && !explicit_name
                {
                    return Err(UsageError(format!(
                        "the export behind {} changed since the last backup on this machine; pass --name to say which instance this export is",
                        self.key
                    ))
                    .into());
                }
                image_state::consume(path)?;
                (Some(lock), state)
            }
        };
        let wanted: Vec<String> = bitmap
            .map(|n| vec![self.names.name(n), self.names.name(n.saturating_add(1))])
            .unwrap_or_default();
        let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
        let limits = Limits {
            max_export_size: MAX_IMAGE_SIZE,
            ..Default::default()
        };
        let client = Client::connect(&uri, &wanted, limits)
            .await
            .map_err(|e| anyhow::anyhow!("cannot open the NBD export {}: {e}", uri.canonical()))?;
        let mut source = NbdSource::new(client);
        let has_next =
            bitmap.is_some_and(|n| source.has_context(&self.names.name(n.saturating_add(1))));
        let binding = Binding {
            nbd: true,
            bitmap,
            has_next,
            state,
            state_path,
            source_id,
        };
        let result = self.write(&mut source, binding).await;
        let _ = source.into_client().disconnect().await;
        result
    }

    async fn write<I: ImageSource>(mut self, source: &mut I, binding: Binding) -> Result<u8> {
        let nbd = binding.nbd;
        let size = source.size();
        let mut instance = Vec::new();
        if let (true, Some(reader)) = (nbd, &self.reader) {
            let mut entries =
                parent_candidates(reader, self.strict.as_ref(), &self.hostname).await?;
            if let Some(strict) = &self.strict {
                entries.retain(|e| strict.provenance.source_of(&e.id) == self.key);
            }
            instance = instance_snapshots(&entries, &self.key, &self.hostname);
        }
        let decision = decide(&DecideInput {
            names: &self.names,
            bitmap: binding.bitmap,
            export_has_bitmap: source.has_dirty(),
            export_has_next: binding.has_next,
            export_size: size,
            source: binding.source_id,
            state: &binding.state,
            instance: &instance,
        });
        let mut plan = decision.plan;
        let mut table = None;
        if let ReadPlan::Incremental { parent } = plan {
            let found = match self.reader.as_mut() {
                Some(reader) => parent_table(reader, &parent, &self.name, size).await,
                None => None,
            };
            match found {
                Some(t) => table = Some((parent, t)),
                None => plan = ReadPlan::Full(FullReason::ParentUnreadable),
            }
        }
        let throttle = Throttle::new();
        let bar = if self.out.json { None } else { spinner() };
        let progress = progress_sink(self.out, &throttle, bar.as_ref());
        let incremental_from = table.as_ref().map(|(id, _)| *id);
        let parent = match (table, self.reader.as_mut()) {
            (Some((snapshot_id, table)), Some(reader)) => Some(ImageParent {
                snapshot_id,
                table,
                chunks: reader,
            }),
            _ => None,
        };
        let full_reason = match plan {
            ReadPlan::Full(reason) if nbd => Some(reason),
            _ => None,
        };
        let result = self
            .writer
            .backup_image(
                &self.name,
                source,
                parent,
                BackupOptions {
                    hostname: Some(self.hostname.clone()),
                    tags: std::mem::take(&mut self.tags),
                    cancel: cancel_on_ctrl_c(),
                    deadline: self.deadline.take(),
                    ..Default::default()
                },
                ImageOptions {
                    segment_chunks: segment_chunks(),
                    bitmap: decision.record.clone(),
                    full_reason,
                },
                &progress,
            )
            .await;
        if let Some(bar) = &bar {
            bar.finish_and_clear();
        }
        let done = result?;
        if let Some(strict) = &mut self.strict {
            check_and_sign(
                strict,
                &self.writer,
                &done.outcome,
                &self.hostname,
                Some(done.source.clone()),
            )
            .await?;
        }
        if let (Some(record), Some(path), false) = (
            &decision.record,
            &binding.state_path,
            done.outcome.incomplete,
        ) {
            let state = ImageState {
                parent: done.outcome.snapshot_id,
                bitmap_gen: record.generation,
                disk_size: size,
                source: binding.source_id,
            };
            if let Err(e) =
                image_state::save(path, &state, &self.repo_id, &self.hostname, &self.key)
            {
                eprintln!(
                    "warning: cannot save the image state {}: {e:#}; the next backup reads the image in full",
                    path.display()
                );
            }
        }
        let mode_text = match (incremental_from, full_reason) {
            (Some(id), _) => format!("incremental read from {}", &hex32(&id)[..8]),
            (None, Some(reason)) => format!("full read ({})", reason.as_str()),
            (None, None) => "full read".to_owned(),
        };
        let mut image = ImageJson {
            key: done.source.key.clone(),
            size,
            read_mode: if incremental_from.is_some() {
                ReadMode::Incremental.as_str()
            } else {
                ReadMode::Full.as_str()
            },
            full_reason: full_reason.map(FullReason::as_str),
            bitmap: None,
            next_bitmap: None,
            bitmap_to_create: None,
            mode_text,
            next_hint: None,
        };
        if nbd {
            let (next, create) = match &decision.record {
                Some(b) => (b.generation, b.generation.saturating_add(1)),
                None => (decision.restart, decision.restart.saturating_add(1)),
            };
            let (next, create) = (self.names.name(next), self.names.name(create));
            image.next_hint = Some(format!(
                "next backup: create {create} before it starts, then pass --bitmap {next}"
            ));
            image.bitmap = binding.bitmap.map(|n| self.names.name(n));
            image.next_bitmap = Some(next);
            image.bitmap_to_create = Some(create);
            match (binding.bitmap, &decision.record) {
                (Some(n), None) => eprintln!(
                    "note: {} was not exported before the read, so the next backup reads the image in full",
                    self.names.name(n.saturating_add(1))
                ),
                (None, _) => eprintln!(
                    "note: incremental backups need dirty bitmaps; see the disk image recipe"
                ),
                _ => {}
            }
        }
        finish(
            self.out,
            done.outcome,
            incremental_from.map(|id| hex32(&id)),
            Some(image),
        )
    }
}

async fn parent_table(
    reader: &mut Reader<Store>,
    parent: &BlobId,
    name: &str,
    size: u64,
) -> Option<Vec<ChunkRef>> {
    let loaded = match load_image(reader, parent, name).await {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!(
                "warning: the parent snapshot {} cannot be used ({e}); reading the image in full",
                hex32(parent)
            );
            return None;
        }
    };
    (loaded.layout.size == size).then(|| loaded.layout.chunk_table())
}

#[cfg(debug_assertions)]
fn segment_chunks() -> u64 {
    std::env::var("LODE_TEST_IMAGE_SEGMENT_CHUNKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SEGMENT_CHUNKS)
}

#[cfg(not(debug_assertions))]
fn segment_chunks() -> u64 {
    SEGMENT_CHUNKS
}

/// Checks the snapshot the way the server does before it counts as verified,
/// then signs it as the repository owner. Any failure leaves it unsigned.
async fn check_and_sign(
    strict: &mut Strict,
    writer: &Writer<Store>,
    outcome: &BackupOutcome,
    hostname: &[u8],
    source: Option<pb::Source>,
) -> Result<()> {
    let short = hex32(&outcome.snapshot_id)[..8].to_string();
    let result = check_own_snapshot(strict, writer, outcome, &short).await;
    let signed = match result {
        Ok(newly) => sign_as_owner(strict, writer, outcome, hostname, source, newly).await,
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
    source: Option<pb::Source>,
    newly: Vec<BlobId>,
) -> Result<()> {
    let record = provenance::owner_record(
        &outcome.snapshot_id,
        hostname,
        source,
        std::time::SystemTime::now(),
    );
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

fn finish(
    out: Output,
    outcome: BackupOutcome,
    parent_id: Option<String>,
    image: Option<ImageJson>,
) -> Result<u8> {
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
                image,
            },
        );
    } else {
        println!("snapshot {id} saved");
        if let Some(image) = &image {
            println!(
                "image {}: {}, {}; {} read, {} added to the repository",
                image.key,
                HumanBytes(image.size),
                image.mode_text,
                HumanBytes(stats.bytes_read),
                HumanBytes(stats.bytes_added)
            );
            if let Some(hint) = &image.next_hint {
                println!("{hint}");
            }
        } else {
            println!(
                "files: {} new, {} changed, {} unchanged; {} read, {} added to the repository",
                stats.files_new,
                stats.files_changed,
                stats.files_unchanged,
                HumanBytes(stats.bytes_read),
                HumanBytes(stats.bytes_added)
            );
        }
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
