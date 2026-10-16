//! Backup pipeline: walk, chunk, deduplicate, pack, index, snapshot.
//!
//! Write order is packs, then the index shard naming them, then the snapshot
//! last, so a snapshot never refers to something that is not in the store.
//! Every store write is `put_if_absent` under a name derived from the bytes;
//! `AlreadyExists` therefore means the identical object is already there.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hpke::Serializable;
use rustix::fs::{Stat, fstat, readlinkat};
use rustix::io::Errno;

use crate::Fut;
use crate::chunker::{Chunker, blob_id, check_params};
use crate::config;
use crate::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy};
use crate::error::{Error, Result};
use crate::fs::scan::{self, ScanTotals};
use crate::fs::walk::{CACHEDIR_TAG, ExtentReader, WalkOptions, Walker, data_extents};
use crate::fs::{conv, hostname, meta, now};
use crate::index::{Index, index_object_key, seal_shard};
use crate::keys::WriteKeys;
use crate::pack::{MAX_TREE_ULEN, PACK_TARGET_SIZE, PackBuilder, pack_object_key, parse_entries};
use crate::pb;
use crate::snapshot::{ParentTree, seal_snapshot, snapshot_object_key};
use crate::store::{BlobError, BlobStore, put_named};
use crate::tree::{encode_tree, split_nodes, validate_name};

/// Recorded issues beyond this many are only counted.
const MAX_ISSUES: usize = 1000;

/// Index shards are written after this many uploaded packs.
pub const INDEX_FLUSH_PACKS: usize = 8;

/// A tree blob may not pass `MAX_TREE_ULEN`; this many chunk ids in one file
/// node leave room for the rest of the piece it is cut into.
// ponytail: a file whose chunk list exceeds this (about 430 GB at the 1 MiB
// average chunk) is reported as TOO_LARGE and left out; lifting the ceiling
// needs a content-list indirection and so a higher min_reader_version.
const MAX_CONTENT_IDS: usize = (MAX_TREE_ULEN - (2 << 20)) / 34;

pub type IssueFn<'a> = Box<dyn Fn(&IssueEvent<'_>) + 'a>;
pub type TreeSinkFn<'a> = Box<dyn FnMut(&BlobId, &[u8]) + 'a>;

/// Writer settings; none of them is part of the repository format.
pub struct BackupOptions<'a> {
    /// Host name for the manifest; the machine's own when absent.
    pub hostname: Option<Vec<u8>>,
    pub tags: Vec<String>,
    /// Snapshot time, the start of the run; now when absent.
    pub time: Option<pb::Timestamp>,
    /// Snapshot to compare files with; unchanged files take their content
    /// from it instead of being read again.
    pub parent: Option<ParentTree<'a>>,
    /// Read every file even when the parent says it is unchanged.
    pub force_rescan: bool,
    pub walk: WalkOptions,
    /// An open pack is uploaded once it reaches this size.
    pub pack_target_size: usize,
    /// Write an index shard after this many uploaded packs.
    pub index_flush_packs: usize,
    /// Set from anywhere (a signal handler) to stop the run: open packs are
    /// uploaded and indexed, no snapshot is written and `backup` returns
    /// `Error::Interrupted`.
    pub cancel: Arc<AtomicBool>,
    /// Count the files and bytes below the paths on a thread of its own while
    /// the backup runs; the totals show in [`BackupProgress`].
    pub prescan: bool,
    /// Called for every warning and error at the moment it is found, also for
    /// those beyond the number the snapshot keeps.
    pub on_issue: Option<IssueFn<'a>>,
    /// Called with the bytes of every tree the run writes, known to the index
    /// or not.
    pub tree_sink: Option<TreeSinkFn<'a>>,
    /// Recorded in the manifest as `agent_id`.
    pub agent_id: Option<Vec<u8>>,
}

/// A warning or error as it is found.
#[derive(Debug, Clone, Copy)]
pub struct IssueEvent<'e> {
    pub warning: bool,
    pub kind: pb::IssueKind,
    pub path: &'e [u8],
    pub message: &'e str,
}

impl Default for BackupOptions<'_> {
    fn default() -> Self {
        BackupOptions {
            hostname: None,
            tags: Vec::new(),
            time: None,
            parent: None,
            force_rescan: false,
            walk: WalkOptions::default(),
            pack_target_size: PACK_TARGET_SIZE,
            index_flush_packs: INDEX_FLUSH_PACKS,
            cancel: Arc::default(),
            prescan: false,
            on_issue: None,
            tree_sink: None,
            agent_id: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackupProgress {
    /// True when `current` is about to be read; `files_done` and the byte
    /// counts are then those of the previous file.
    pub starting: bool,
    pub files_done: u64,
    pub bytes_done: u64,
    pub bytes_added: u64,
    pub current: Vec<u8>,
    /// Totals of the prescan so far; 0 without one.
    pub files_total: u64,
    pub bytes_total: u64,
    /// True once the prescan has seen everything, so the totals are final.
    pub scan_done: bool,
    /// Bytes of the tree handled so far: finished files plus the part of the
    /// current one that has been read.
    pub bytes_processed: u64,
}

#[derive(Debug, Clone)]
pub struct BackupOutcome {
    pub snapshot_id: BlobId,
    pub root_tree: BlobId,
    pub time: pb::Timestamp,
    pub stats: pb::SnapshotStats,
    pub incomplete: bool,
    pub errors_total: u64,
    pub warnings_total: u64,
}

pub struct Writer<S: BlobStore> {
    store: S,
    keys: WriteKeys,
    index: Index,
    entropy: Entropy,
    /// Open packs of the conformance hooks, see [`Writer::put_blob_raw`].
    #[cfg(feature = "test-hooks")]
    raw: [Option<PackBuilder>; 2],
}

impl<S: BlobStore> Writer<S> {
    /// Checks that the keys belong to the repository `config` describes and
    /// loads the index by LIST, which is discovery only: a shard that is not
    /// listed costs deduplication, never correctness.
    pub async fn new(store: S, keys: WriteKeys, config: &pb::Config) -> Result<Self> {
        config::check_write_version(config)?;
        check_params(config.chunker.as_ref())?;
        if config::repo_id_of(config)? != keys.repo_id {
            return Err(Error::InvalidInput(
                "the keys belong to a different repository".into(),
            ));
        }
        let pinned = config.hpke.as_ref().map(|h| h.public_key.as_slice());
        if pinned != Some(keys.hpke_public.to_bytes().as_slice()) {
            return Err(Error::InvalidInput(
                "the repository public key differs from the one in the keys".into(),
            ));
        }
        let mut index = Index::default();
        index.load_all(&store, &keys.k_idx, &keys.repo_id).await?;
        Ok(Writer {
            store,
            keys,
            index,
            entropy: Entropy::os(),
            #[cfg(feature = "test-hooks")]
            raw: [None, None],
        })
    }

    /// Opens the repository `store` holds with writer keys: reads `config`,
    /// checks that the keys were exported for exactly it, then [`Writer::new`].
    pub async fn connect(store: S, keys: WriteKeys) -> Result<Self> {
        let config_bytes = match store.get("config").await {
            Ok(bytes) => bytes,
            Err(BlobError::NotFound) => return Err(Error::NotInitialized),
            Err(e) => return Err(e.into()),
        };
        keys.verify_config(&config_bytes)?;
        let config = config::decode(&config_bytes)?;
        Self::new(store, keys, &config).await
    }

    /// `(object key, reason)` of every listed index shard that could not be
    /// used; they only cost deduplication.
    pub fn skipped_index_shards(&self) -> &[(String, String)] {
        self.index.skipped()
    }

    /// Drops index records whose pack is missing from the store or has another
    /// size than the record says, so those blobs are uploaded again. Returns
    /// the number of packs dropped. Needs LIST on `packs/`: not for a
    /// write-only credential, which has to trust the index (see ADR 0002).
    pub async fn drop_unbacked_packs(&mut self) -> Result<usize> {
        self.index.retain_listed_packs(&self.store).await
    }

    /// Replaces the randomness source, for deterministic tests.
    pub fn with_entropy(mut self, entropy: Entropy) -> Self {
        self.entropy = entropy;
        self
    }

    pub async fn backup(
        &mut self,
        paths: &[PathBuf],
        mut options: BackupOptions<'_>,
        progress: &dyn Fn(&BackupProgress),
    ) -> Result<BackupOutcome> {
        if paths.is_empty() {
            return Err(Error::InvalidInput("nothing to back up".into()));
        }
        // A missing root fails here, before anything is written.
        let roots = canonical_roots(paths)?;
        let started = options.time.unwrap_or_else(now);
        let parent_id = options.parent.as_ref().map(|p| p.id().to_vec());
        let mut trie = Trie::default();
        for root in &roots {
            let comps: Vec<&[u8]> = root
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(n) => Some(n.as_bytes()),
                    _ => None,
                })
                .collect();
            trie.insert(&comps);
        }

        let rootfd = Walker::open_root()?;
        let mut scan = options.prescan.then(|| {
            Prescan::start(
                roots.iter().cloned().collect(),
                options.walk.clone(),
                Arc::clone(&options.cancel),
            )
        });
        let mut run = Session::new(self, progress);
        run.apply(&mut options);
        run.scan = scan.as_ref().map(|s| Arc::clone(&s.totals));
        let walked = if trie.is_root {
            run.root_dev = fstat(&rootfd).map(|st| conv(st.st_dev))?;
            run.backup_children(&rootfd, b"").await
        } else {
            run.backup_trie(&rootfd, &trie, b"").await
        };
        let top = match walked {
            Ok(top) => top,
            Err(Error::Interrupted) => {
                run.wind_down().await?;
                return Err(Error::Interrupted);
            }
            Err(e) => return Err(e),
        };
        if let Some(scan) = scan.as_mut() {
            scan.finish();
            if run.cancelled() {
                run.wind_down().await?;
                return Err(Error::Interrupted);
            }
            run.report(false, b"");
        }
        let root_tree = run.write_root(top).await?;
        run.wind_down().await?;

        let incomplete = run.errors_total > 0;
        let (errors_total, warnings_total) = (run.errors_total, run.warnings_total);
        let stats = run.stats;
        let snapshot = pb::Snapshot {
            time: Some(started),
            hostname: options.hostname.unwrap_or_else(hostname),
            paths: roots
                .iter()
                .map(|r| r.as_os_str().as_bytes().to_vec())
                .collect(),
            tags: options.tags,
            parent: parent_id.unwrap_or_default(),
            root_tree: root_tree.to_vec(),
            stats: Some(stats),
            incomplete,
            errors: run.errors,
            warnings: run.warnings,
            errors_total: run.errors_total,
            warnings_total: run.warnings_total,
            index_refs: run.refs.iter().map(|r| r.to_vec()).collect(),
            agent_id: options.agent_id.unwrap_or_default(),
            program_version: format!("lodestore-format {}", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        let snapshot_id = self.put_snapshot(&snapshot).await?;
        Ok(BackupOutcome {
            snapshot_id,
            root_tree,
            time: started,
            stats,
            incomplete,
            errors_total,
            warnings_total,
        })
    }

    /// Backs up the bytes `source` yields as one file `/<name>` in a snapshot
    /// tagged `stdin=<name>`. It is cut by the same chunker as a file, so the
    /// same bytes deduplicate against a backup of them as a file. A failing
    /// `source` (or `options.cancel`) uploads what was read so far, writes no
    /// snapshot and returns the error: a truncated stream must never look like
    /// a good backup.
    pub async fn backup_stream(
        &mut self,
        name: &[u8],
        source: &mut dyn Read,
        mut options: BackupOptions<'_>,
        progress: &dyn Fn(&BackupProgress),
    ) -> Result<BackupOutcome> {
        validate_name(name).map_err(|_| {
            Error::InvalidInput(format!(
                "{:?} is not a valid file name: it must be 1 to 1024 bytes, without '/' or NUL, and not . or ..",
                lossy(name)
            ))
        })?;
        let started = options.time.unwrap_or_else(now);
        let parent_id = options.parent.as_ref().map(|p| p.id().to_vec());
        let path = join(b"", name);
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let (user, group) = meta::owner_names(uid, gid);
        let mut node = pb::Node {
            name: name.to_vec(),
            r#type: pb::NodeType::File as i32,
            mode: 0o600,
            uid,
            gid,
            user,
            group,
            mtime: Some(started),
            ..Default::default()
        };

        let mut run = Session::new(self, progress);
        run.apply(&mut options);
        let chunker = Chunker::new(source, &run.w.keys.k_id, run.w.keys.cdc_seed);
        let mut total = 0u64;
        let mut content = Vec::new();
        let mut failure = None;
        for chunk in chunker {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            if run.cancelled() {
                failure = Some(Error::Interrupted);
                break;
            }
            total += chunk.data.len() as u64;
            run.add_blob(BLOB_TYPE_DATA, chunk.id, &chunk.data).await?;
            content.push(chunk.id.to_vec());
            if content.len() > MAX_CONTENT_IDS {
                failure = Some(Error::InvalidInput(
                    "the stream has too many chunks for one tree entry".into(),
                ));
                break;
            }
            run.stats.bytes_read = total;
            run.cur_read = total;
            run.report(false, &path);
        }
        if let Some(e) = failure {
            run.wind_down().await?;
            return Err(e);
        }

        node.size = total;
        node.content = content;
        run.cur_read = 0;
        run.stats.files_new = 1;
        run.stats.bytes_total = total;
        run.files_done = 1;
        run.report(false, &path);
        let root_tree = run.write_root(vec![node]).await?;
        run.wind_down().await?;

        let stats = run.stats;
        let mut tags = options.tags;
        let tag = format!("stdin={}", String::from_utf8_lossy(name));
        if !tags.contains(&tag) {
            tags.push(tag);
        }
        let snapshot = pb::Snapshot {
            time: Some(started),
            hostname: options.hostname.unwrap_or_else(hostname),
            paths: vec![path],
            tags,
            parent: parent_id.unwrap_or_default(),
            root_tree: root_tree.to_vec(),
            stats: Some(stats),
            index_refs: run.refs.iter().map(|r| r.to_vec()).collect(),
            agent_id: options.agent_id.unwrap_or_default(),
            program_version: format!("lodestore-format {}", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        let snapshot_id = self.put_snapshot(&snapshot).await?;
        Ok(BackupOutcome {
            snapshot_id,
            root_tree,
            time: started,
            stats,
            incomplete: false,
            errors_total: 0,
            warnings_total: 0,
        })
    }

    async fn put_snapshot(&mut self, snapshot: &pb::Snapshot) -> Result<BlobId> {
        let (id, bytes) = seal_snapshot(
            snapshot,
            &self.keys.hpke_public,
            &self.keys.repo_id,
            &mut self.entropy,
        )?;
        put_named(&self.store, &snapshot_object_key(&id), bytes).await?;
        Ok(id)
    }

    /// Stores one blob frame exactly as given: any codec, any payload bytes
    /// and any claimed plaintext size. The id is the keyed hash of `payload`,
    /// which for `CODEC_RAW` is the plaintext. Held until
    /// [`Writer::commit_snapshot_raw`].
    ///
    /// For format conformance tests; any writer-keys holder can produce such
    /// objects, so a reader must not trust them.
    #[cfg(feature = "test-hooks")]
    pub fn put_blob_raw(
        &mut self,
        blob_type: u8,
        codec: u8,
        payload: &[u8],
        claimed_ulen: u32,
    ) -> Result<BlobId> {
        let id = blob_id(&self.keys.k_id, payload);
        let slot = &mut self.raw[usize::from(blob_type == BLOB_TYPE_TREE)];
        if slot.is_none() {
            *slot = Some(PackBuilder::new(
                blob_type,
                &self.keys.repo_id,
                &self.keys.hpke_public,
                &mut self.entropy,
            )?);
        }
        slot.as_mut().expect("builder was just created").add_raw(
            &id,
            codec,
            payload,
            claimed_ulen,
            &mut self.entropy,
        )?;
        Ok(id)
    }

    /// Stores serialized tree bytes without decoding or validating them.
    ///
    /// For format conformance tests; any writer-keys holder can produce such
    /// objects, so a reader must not trust them.
    #[cfg(feature = "test-hooks")]
    pub fn put_tree_raw(&mut self, tree_bytes: &[u8]) -> Result<BlobId> {
        let (codec, payload) = crate::pack::compress(tree_bytes)?;
        let claimed = u32::try_from(tree_bytes.len())
            .map_err(|_| Error::InvalidInput("tree does not fit a pack".into()))?;
        let id = blob_id(&self.keys.k_id, tree_bytes);
        let slot = &mut self.raw[1];
        if slot.is_none() {
            *slot = Some(PackBuilder::new(
                BLOB_TYPE_TREE,
                &self.keys.repo_id,
                &self.keys.hpke_public,
                &mut self.entropy,
            )?);
        }
        slot.as_mut().expect("builder was just created").add_raw(
            &id,
            codec,
            &payload,
            claimed,
            &mut self.entropy,
        )?;
        Ok(id)
    }

    /// Uploads what the `put_*_raw` hooks hold, indexes it, and writes a
    /// snapshot over `root_tree` with nothing else checked.
    ///
    /// For format conformance tests; any writer-keys holder can produce such
    /// objects, so a reader must not trust them.
    #[cfg(feature = "test-hooks")]
    pub async fn commit_snapshot_raw(
        &mut self,
        root_tree: BlobId,
        paths: Vec<Vec<u8>>,
    ) -> Result<BlobId> {
        let mut records = Vec::new();
        for slot in 0..2 {
            let Some(builder) = self.raw[slot].take() else {
                continue;
            };
            if builder.is_empty() {
                continue;
            }
            let done = builder.finish(&mut self.entropy)?;
            put_named(&self.store, &pack_object_key(&done.pack_id), done.bytes).await?;
            records.push(done.record);
        }
        let mut index_refs = Vec::new();
        if !records.is_empty() {
            let (shard_id, bytes) = seal_shard(
                &self.keys.k_idx,
                &self.keys.repo_id,
                records,
                &mut self.entropy,
            )?;
            put_named(&self.store, &index_object_key(&shard_id), bytes).await?;
            index_refs.push(shard_id.to_vec());
        }
        let snapshot = pb::Snapshot {
            time: Some(now()),
            hostname: hostname(),
            paths,
            root_tree: root_tree.to_vec(),
            index_refs,
            program_version: format!("lodestore-format {}", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        let (id, bytes) = seal_snapshot(
            &snapshot,
            &self.keys.hpke_public,
            &self.keys.repo_id,
            &mut self.entropy,
        )?;
        put_named(&self.store, &snapshot_object_key(&id), bytes).await?;
        Ok(id)
    }
}

/// The canonical form of the backup arguments, which is what a snapshot
/// records as its `paths`. Fails for a path that does not exist.
pub fn canonical_roots(paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    paths
        .iter()
        .map(|p| {
            std::fs::canonicalize(p)
                .map_err(|e| Error::InvalidInput(format!("{}: {e}", p.display())))
        })
        .collect()
}

fn join(path: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = path.to_vec();
    p.push(b'/');
    p.extend_from_slice(name);
    p
}

/// The prescan thread of one run. It is asked to stop whenever the run ends,
/// however it ends.
struct Prescan {
    totals: Arc<ScanTotals>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Prescan {
    fn start(roots: Vec<PathBuf>, walk: WalkOptions, cancel: Arc<AtomicBool>) -> Self {
        let totals = Arc::new(ScanTotals::default());
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn({
            let (totals, stop) = (Arc::clone(&totals), Arc::clone(&stop));
            move || scan::scan(&roots, &walk, &stop, &cancel, &totals)
        });
        Prescan {
            totals,
            stop,
            handle: Some(handle),
        }
    }

    /// Waits for the scan to see everything.
    fn finish(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Prescan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The path components leading to the backed-up roots. A node marked as a
/// root is walked in full and swallows anything below it.
#[derive(Default)]
struct Trie {
    children: BTreeMap<Vec<u8>, Trie>,
    is_root: bool,
}

impl Trie {
    fn insert(&mut self, comps: &[&[u8]]) {
        if self.is_root {
            return;
        }
        match comps.split_first() {
            None => {
                self.is_root = true;
                self.children.clear();
            }
            Some((head, rest)) => self.children.entry(head.to_vec()).or_default().insert(rest),
        }
    }
}

/// State of one backup run.
// ponytail: the pipeline is sequential (walk, chunk, hash, compress, encrypt
// and upload on one task), so throughput is one core; the agent adds bounded
// channels between stages later, behind this same API.
struct Session<'a, 'p, S: BlobStore> {
    w: &'a mut Writer<S>,
    progress: &'a dyn Fn(&BackupProgress),
    data: Option<PackBuilder>,
    tree: Option<PackBuilder>,
    pending: HashSet<(BlobId, u8)>,
    uploaded: Vec<pb::PackRecord>,
    refs: BTreeSet<BlobId>,
    stats: pb::SnapshotStats,
    errors: Vec<pb::SnapshotIssue>,
    warnings: Vec<pb::SnapshotIssue>,
    errors_total: u64,
    warnings_total: u64,
    files_done: u64,
    hardlinks: HashMap<(u64, u64), u64>,
    link_members: HashMap<u64, (u64, [u8; 32])>,
    parent: Option<ParentTree<'p>>,
    force_rescan: bool,
    walk: WalkOptions,
    root_dev: u64,
    pack_target: usize,
    flush_packs: usize,
    cancel: Arc<AtomicBool>,
    on_issue: Option<IssueFn<'p>>,
    tree_sink: Option<TreeSinkFn<'p>>,
    scan: Option<Arc<ScanTotals>>,
    /// Bytes read so far of the file being backed up.
    cur_read: u64,
}

impl<'a, 'p, S: BlobStore> Session<'a, 'p, S> {
    fn new(w: &'a mut Writer<S>, progress: &'a dyn Fn(&BackupProgress)) -> Self {
        Session {
            w,
            progress,
            data: None,
            tree: None,
            pending: HashSet::new(),
            uploaded: Vec::new(),
            refs: BTreeSet::new(),
            stats: pb::SnapshotStats::default(),
            errors: Vec::new(),
            warnings: Vec::new(),
            errors_total: 0,
            warnings_total: 0,
            files_done: 0,
            hardlinks: HashMap::new(),
            link_members: HashMap::new(),
            parent: None,
            force_rescan: false,
            walk: WalkOptions::default(),
            root_dev: 0,
            pack_target: PACK_TARGET_SIZE,
            flush_packs: INDEX_FLUSH_PACKS,
            cancel: Arc::default(),
            on_issue: None,
            tree_sink: None,
            scan: None,
            cur_read: 0,
        }
    }

    /// Takes the settings of the run out of `options`.
    fn apply(&mut self, options: &mut BackupOptions<'p>) {
        self.parent = options.parent.take();
        self.force_rescan = options.force_rescan;
        self.walk = std::mem::take(&mut options.walk);
        self.pack_target = options.pack_target_size;
        self.flush_packs = options.index_flush_packs.max(1);
        self.cancel = Arc::clone(&options.cancel);
        self.on_issue = options.on_issue.take();
        self.tree_sink = options.tree_sink.take();
    }

    /// Uploads the open packs and the index shard naming them.
    async fn wind_down(&mut self) -> Result<()> {
        self.flush(BLOB_TYPE_DATA).await?;
        self.flush(BLOB_TYPE_TREE).await?;
        self.commit_shard().await
    }

    fn issue(&mut self, warning: bool, kind: pb::IssueKind, path: &[u8], message: String) {
        if let Some(on_issue) = &self.on_issue {
            on_issue(&IssueEvent {
                warning,
                kind,
                path,
                message: &message,
            });
        }
        let (list, total) = if warning {
            (&mut self.warnings, &mut self.warnings_total)
        } else {
            (&mut self.errors, &mut self.errors_total)
        };
        *total += 1;
        if list.len() < MAX_ISSUES {
            list.push(pb::SnapshotIssue {
                path: path.to_vec(),
                kind: kind as i32,
                message,
            });
        }
    }

    fn xattr_issue(&mut self, path: &[u8], e: Errno) {
        self.issue(
            true,
            pb::IssueKind::Unreadable,
            path,
            format!("extended attributes not captured: {e}"),
        );
    }

    fn io_issue(&mut self, path: &[u8], e: Errno) {
        let kind = if e == Errno::NOENT {
            pb::IssueKind::Vanished
        } else {
            pb::IssueKind::Unreadable
        };
        self.issue(false, kind, path, e.to_string());
    }

    /// Adds a blob unless the index (or this run) already has it.
    async fn add_blob(&mut self, blob_type: u8, id: BlobId, data: &[u8]) -> Result<()> {
        if let Some(loc) = self.w.index.get(&id, blob_type) {
            self.refs.insert(loc.shard);
            return Ok(());
        }
        if self.pending.contains(&(id, blob_type)) {
            return Ok(());
        }
        let slot = if blob_type == BLOB_TYPE_DATA {
            &mut self.data
        } else {
            &mut self.tree
        };
        if slot.is_none() {
            *slot = Some(PackBuilder::new(
                blob_type,
                &self.w.keys.repo_id,
                &self.w.keys.hpke_public,
                &mut self.w.entropy,
            )?);
        }
        let builder = slot.as_mut().expect("builder was just created");
        let frame_len = builder.add(&id, data, &mut self.w.entropy)?;
        let full = builder.size() >= self.pack_target;
        self.pending.insert((id, blob_type));
        if blob_type == BLOB_TYPE_DATA {
            self.stats.bytes_added += frame_len as u64;
        }
        if full {
            self.flush(blob_type).await?;
            if self.uploaded.len() >= self.flush_packs {
                self.commit_shard().await?;
            }
        }
        Ok(())
    }

    /// Uploads the open pack of this type, if any.
    async fn flush(&mut self, blob_type: u8) -> Result<()> {
        let slot = if blob_type == BLOB_TYPE_DATA {
            &mut self.data
        } else {
            &mut self.tree
        };
        let Some(builder) = slot.take() else {
            return Ok(());
        };
        if builder.is_empty() {
            return Ok(());
        }
        let done = builder.finish(&mut self.w.entropy)?;
        put_named(&self.w.store, &pack_object_key(&done.pack_id), done.bytes).await?;
        self.uploaded.push(done.record);
        Ok(())
    }

    /// Writes one shard for the packs uploaded since the last one. Only called
    /// once those packs are in the store.
    async fn commit_shard(&mut self) -> Result<()> {
        if self.uploaded.is_empty() {
            return Ok(());
        }
        let records = std::mem::take(&mut self.uploaded);
        let (shard_id, bytes) = seal_shard(
            &self.w.keys.k_idx,
            &self.w.keys.repo_id,
            records.clone(),
            &mut self.w.entropy,
        )?;
        put_named(&self.w.store, &index_object_key(&shard_id), bytes).await?;
        for record in &records {
            for entry in parse_entries(&record.entries)? {
                self.pending.remove(&(entry.blob_id, entry.blob_type));
            }
        }
        self.w.index.add_shard(
            shard_id,
            &pb::IndexShard {
                packs: records,
                supersedes: Vec::new(),
            },
        )?;
        self.refs.insert(shard_id);
        Ok(())
    }

    fn sink_tree(&mut self, id: &BlobId, bytes: &[u8]) {
        if let Some(sink) = self.tree_sink.as_mut() {
            sink(id, bytes);
        }
    }

    /// Writes the nodes of one directory as one or more tree blobs and
    /// returns their ids in order.
    async fn write_tree(&mut self, nodes: Vec<pb::Node>) -> Result<Vec<Vec<u8>>> {
        let mut ids = Vec::new();
        for piece in split_nodes(nodes) {
            let bytes = encode_tree(piece);
            let id = blob_id(&self.w.keys.k_id, &bytes);
            self.sink_tree(&id, &bytes);
            self.add_blob(BLOB_TYPE_TREE, id, &bytes).await?;
            ids.push(id.to_vec());
        }
        Ok(ids)
    }

    /// The snapshot root is a single tree blob.
    async fn write_root(&mut self, nodes: Vec<pb::Node>) -> Result<BlobId> {
        let ids = self.write_tree(nodes).await?;
        match ids.as_slice() {
            [] => {
                let bytes = encode_tree(Vec::new());
                let id = blob_id(&self.w.keys.k_id, &bytes);
                self.sink_tree(&id, &bytes);
                self.add_blob(BLOB_TYPE_TREE, id, &bytes).await?;
                Ok(id)
            }
            [one] => Ok(one.as_slice().try_into().expect("tree ids are 32 bytes")),
            _ => Err(Error::InvalidInput(
                "too many top-level entries for one root tree".into(),
            )),
        }
    }

    /// Walks the path components down to the roots. Directories on the way
    /// are recorded bare; roots are walked in full.
    fn backup_trie<'b>(
        &'b mut self,
        dirfd: &'b OwnedFd,
        trie: &'b Trie,
        path: &'b [u8],
    ) -> Fut<'b, Vec<pb::Node>> {
        Box::pin(async move {
            let mut nodes = Vec::new();
            for (name, child) in &trie.children {
                let full = join(path, name);
                let st = Walker::stat(dirfd, name)
                    .map_err(|e| Error::InvalidInput(format!("{}: {e}", lossy(&full))))?;
                if child.is_root {
                    if self.excluded(&full, &st) {
                        continue;
                    }
                    self.root_dev = conv(st.st_dev);
                    if let Some(node) = self.backup_entry(dirfd, name, &st, &full).await? {
                        nodes.push(node);
                    }
                    continue;
                }
                if Walker::node_type(&st) != Some(pb::NodeType::Dir) {
                    return Err(Error::InvalidInput(format!(
                        "{} is not a directory",
                        lossy(&full)
                    )));
                }
                let sub = Walker::open_dir(dirfd, name)
                    .map_err(|e| Error::InvalidInput(format!("{}: {e}", lossy(&full))))?;
                let children = self.backup_trie(&sub, child, &full).await?;
                let mut node = Walker::node_of(name, &st, pb::NodeType::Dir, true);
                node.subtrees = self.write_tree(children).await?;
                nodes.push(node);
            }
            Ok(nodes)
        })
    }

    fn backup_children<'b>(
        &'b mut self,
        dirfd: &'b OwnedFd,
        path: &'b [u8],
    ) -> Fut<'b, Vec<pb::Node>> {
        Box::pin(async move {
            let mut entries = match Walker::read_dir(dirfd) {
                Ok(entries) => entries,
                Err(e) => {
                    self.io_issue(path, e);
                    return Ok(Vec::new());
                }
            };
            if self.walk.exclude_caches && Walker::has_cache_tag(dirfd) {
                entries.retain(|e| e.name == CACHEDIR_TAG);
            }
            let mut nodes = Vec::new();
            for entry in entries {
                let full = join(path, &entry.name);
                let st = match entry.stat {
                    Ok(st) => st,
                    Err(e) => {
                        self.io_issue(&full, e);
                        continue;
                    }
                };
                if self.excluded(&full, &st) {
                    continue;
                }
                if let Some(node) = self.backup_entry(dirfd, &entry.name, &st, &full).await? {
                    nodes.push(node);
                }
            }
            Ok(nodes)
        })
    }

    fn excluded(&self, path: &[u8], st: &Stat) -> bool {
        self.walk
            .exclude
            .as_ref()
            .is_some_and(|m| m.is_match(path, Walker::node_type(st) == Some(pb::NodeType::Dir)))
    }

    /// `None` means the entry was skipped and recorded as an issue.
    async fn backup_entry(
        &mut self,
        dirfd: &OwnedFd,
        name: &[u8],
        st: &Stat,
        path: &[u8],
    ) -> Result<Option<pb::Node>> {
        match Walker::node_type(st) {
            Some(pb::NodeType::Dir) => {
                if self.walk.one_file_system && conv::<_, u64>(st.st_dev) != self.root_dev {
                    self.stats.dirs += 1;
                    return Ok(Some(Walker::node_of(name, st, pb::NodeType::Dir, false)));
                }
                let sub = match Walker::open_dir(dirfd, name) {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) => {
                        self.io_issue(path, Errno::NOENT);
                        return Ok(None);
                    }
                    Err(e) => {
                        // The directory stays in the tree, empty, so a
                        // restore shows where something is missing.
                        self.io_issue(path, e);
                        self.stats.dirs += 1;
                        return Ok(Some(Walker::node_of(name, st, pb::NodeType::Dir, false)));
                    }
                };
                let children = self.backup_children(&sub, path).await?;
                let mut node = Walker::node_of(name, st, pb::NodeType::Dir, false);
                match meta::capture_xattrs(&sub) {
                    Ok(x) => node.xattrs = x,
                    Err(e) => self.xattr_issue(path, e),
                }
                node.subtrees = self.write_tree(children).await?;
                self.stats.dirs += 1;
                Ok(Some(node))
            }
            Some(pb::NodeType::File) => self.backup_file(dirfd, name, st, path).await,
            Some(ty) => Ok(Some(self.backup_special(dirfd, name, st, ty, path))),
            None => {
                self.issue(
                    true,
                    pb::IssueKind::UnsupportedType,
                    path,
                    "unsupported file type, skipped".into(),
                );
                Ok(None)
            }
        }
    }

    /// Symlinks, fifos and device nodes: metadata only, nothing is opened.
    fn backup_special(
        &mut self,
        dirfd: &OwnedFd,
        name: &[u8],
        st: &Stat,
        ty: pb::NodeType,
        path: &[u8],
    ) -> pb::Node {
        let mut node = Walker::node_of(name, st, ty, false);
        if ty == pb::NodeType::Symlink {
            match readlinkat(dirfd, name, Vec::new()) {
                Ok(target) => node.link_target = target.into_bytes(),
                Err(e) => self.io_issue(path, e),
            }
        } else {
            self.issue(
                true,
                pb::IssueKind::UnsupportedType,
                path,
                "special file: only its metadata is stored".into(),
            );
        }
        match meta::capture_xattrs_at(dirfd, name) {
            Ok(x) => node.xattrs = x,
            Err(e) => self.xattr_issue(path, e),
        }
        node
    }

    /// `walked` is the walk's `stat` of the entry, which the file is compared
    /// with once it has been read to notice a change underneath the read.
    async fn backup_file(
        &mut self,
        dirfd: &OwnedFd,
        name: &[u8],
        walked: &Stat,
        path: &[u8],
    ) -> Result<Option<pb::Node>> {
        if self.cancelled() {
            return Err(Error::Interrupted);
        }
        self.cur_read = 0;
        self.report(true, path);
        let fd = match Walker::open_file(dirfd, name) {
            Ok(fd) => fd,
            Err(e) => {
                self.io_issue(path, e);
                return Ok(None);
            }
        };
        let before = match fstat(&fd) {
            Ok(st) if Walker::node_type(&st) == Some(pb::NodeType::File) => st,
            Ok(_) => {
                self.issue(
                    true,
                    pb::IssueKind::ChangedDuringRead,
                    path,
                    "no longer a regular file, skipped".into(),
                );
                return Ok(None);
            }
            Err(e) => {
                self.io_issue(path, e);
                return Ok(None);
            }
        };
        let mut node = Walker::node_of(name, &before, pb::NodeType::File, false);
        match meta::capture_xattrs(&fd) {
            Ok(x) => node.xattrs = x,
            Err(e) => self.xattr_issue(path, e),
        }
        node.hardlink_group = Walker::hardlink_group(&mut self.hardlinks, &before);

        let previous = match self.parent.as_mut() {
            Some(parent) => match parent.lookup(path).await {
                Ok(found) => found.filter(|p| p.r#type == pb::NodeType::File as i32),
                Err(e) => {
                    // An unreadable parent only costs the shortcut: read every
                    // remaining file in full instead of failing the backup.
                    self.parent = None;
                    self.issue(
                        true,
                        pb::IssueKind::Unspecified,
                        path,
                        format!("parent snapshot became unreadable, continuing without it: {e}"),
                    );
                    None
                }
            },
            None => None,
        };
        if let Some(previous) = &previous
            && self.reuse_content(&mut node, previous)
        {
            self.stats.files_unchanged += 1;
            self.stats.bytes_total += node.size;
            self.check_link_member(path, &mut node);
            self.file_done(path);
            return Ok(Some(node));
        }

        // ponytail: every member of a hardlink group is read and chunked again
        // (dedup makes the upload free); a (dev, inode) content cache saves the
        // read when trees with many large links matter.
        let logical: u64 = conv(before.st_size);
        let extents = if logical > 0 {
            data_extents(&fd, logical)
        } else {
            None
        };
        let observer = fd.try_clone()?;
        let file = std::fs::File::from(fd);
        let source: Box<dyn std::io::Read> = match &extents {
            Some(list) => Box::new(ExtentReader::new(file, list.clone())),
            None => Box::new(file),
        };
        if let Some(list) = extents {
            // A file that is all hole carries one empty extent, so that
            // "extents present" always means sparse.
            node.sparse_extents = if list.is_empty() {
                vec![pb::Extent {
                    offset: 0,
                    length: 0,
                }]
            } else {
                list.iter()
                    .map(|&(offset, length)| pb::Extent { offset, length })
                    .collect()
            };
        }

        let mut total = 0u64;
        let mut content = Vec::new();
        let chunker = Chunker::new(source, &self.w.keys.k_id, self.w.keys.cdc_seed);
        for chunk in chunker {
            let chunk = match chunk {
                Ok(c) => c,
                Err(Error::Io(e)) => {
                    self.cur_read = 0;
                    self.issue(false, pb::IssueKind::Unreadable, path, e.to_string());
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            if self.cancelled() {
                return Err(Error::Interrupted);
            }
            total += chunk.data.len() as u64;
            self.add_blob(BLOB_TYPE_DATA, chunk.id, &chunk.data).await?;
            content.push(chunk.id.to_vec());
            if content.len() > MAX_CONTENT_IDS {
                self.cur_read = 0;
                self.issue(
                    false,
                    pb::IssueKind::TooLarge,
                    path,
                    "file has too many chunks for one tree entry, skipped".into(),
                );
                return Ok(None);
            }
            self.cur_read = total;
            self.report(false, path);
        }
        self.cur_read = 0;

        let sparse = !node.sparse_extents.is_empty();
        let size = if sparse { logical } else { total };
        let after = fstat(&observer).unwrap_or(before);
        let stamp = |st: &Stat| {
            (
                st.st_size,
                st.st_mtime,
                st.st_mtime_nsec,
                st.st_ctime,
                st.st_ctime_nsec,
            )
        };
        if stamp(&after) != stamp(walked) {
            self.issue(
                true,
                pb::IssueKind::ChangedDuringRead,
                path,
                "file changed while it was being read".into(),
            );
            let mut fresh = Walker::node_of(name, &after, pb::NodeType::File, false);
            fresh.xattrs = std::mem::take(&mut node.xattrs);
            fresh.hardlink_group = node.hardlink_group;
            fresh.sparse_extents = std::mem::take(&mut node.sparse_extents);
            node = fresh;
        }

        node.size = size;
        node.content = content;
        if previous.is_some() {
            self.stats.files_changed += 1;
        } else {
            self.stats.files_new += 1;
        }
        self.stats.bytes_total += size;
        self.stats.bytes_read += total;
        self.check_link_member(path, &mut node);
        self.file_done(path);
        Ok(Some(node))
    }

    /// A file that changes between the reads of two of its names would give one
    /// hardlink group two contents, which restore rejects; that member is kept
    /// as a separate file instead.
    fn check_link_member(&mut self, path: &[u8], node: &mut pb::Node) {
        if node.hardlink_group == 0 {
            return;
        }
        let member = (node.size, *blake3::hash(&node.content.concat()).as_bytes());
        if *self
            .link_members
            .entry(node.hardlink_group)
            .or_insert(member)
            != member
        {
            node.hardlink_group = 0;
            self.issue(
                true,
                pb::IssueKind::ChangedDuringRead,
                path,
                "file changed between reads of its hard links, stored as a separate file".into(),
            );
        }
    }

    fn file_done(&mut self, path: &[u8]) {
        self.files_done += 1;
        self.report(false, path);
    }

    fn report(&self, starting: bool, path: &[u8]) {
        let done = self.scan.as_ref().is_some_and(|s| s.done());
        let (files_total, bytes_total) = self
            .scan
            .as_ref()
            .map_or((0, 0), |s| (s.files(), s.bytes()));
        (self.progress)(&BackupProgress {
            starting,
            files_done: self.files_done,
            bytes_done: self.stats.bytes_read,
            bytes_added: self.stats.bytes_added,
            current: path.to_vec(),
            files_total,
            bytes_total,
            scan_done: done,
            bytes_processed: self.stats.bytes_total + self.cur_read,
        });
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Takes the content of `previous` for `node` when the file looks
    /// untouched: same size, mtime, ctime and inode, an mtime clear of the
    /// 2 s window before the parent started (a write in the same clock tick
    /// as the parent's read can leave the mtime unchanged), and every chunk
    /// still in the index. The index shards of those chunks join the
    /// snapshot's references.
    fn reuse_content(&mut self, node: &mut pb::Node, previous: &pb::Node) -> bool {
        let Some(parent) = &self.parent else {
            return false;
        };
        let started = parent.time();
        let settled = node
            .mtime
            .as_ref()
            .is_some_and(|m| (m.seconds, m.nanos) < (started.seconds - 2, started.nanos));
        if self.force_rescan
            || !settled
            || node.size != previous.size
            || node.mtime != previous.mtime
            || node.ctime != previous.ctime
            || node.inode != previous.inode
        {
            return false;
        }
        let mut shards = Vec::with_capacity(previous.content.len());
        for raw in &previous.content {
            let Some(loc) = BlobId::try_from(raw.as_slice())
                .ok()
                .and_then(|id| self.w.index.get(&id, BLOB_TYPE_DATA))
            else {
                return false;
            };
            shards.push(loc.shard);
        }
        self.refs.extend(shards);
        node.content.clone_from(&previous.content);
        node.sparse_extents.clone_from(&previous.sparse_extents);
        true
    }
}

fn lossy(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}
