//! The structure check: what the bucket lists against what the index and the
//! snapshots say.
//!
//! The bucket is read in the opposite order of a backup's writes (packs, then
//! index shards, then the snapshot): snapshots first, then the index shards,
//! then the pack listing. Everything a snapshot seen first refers to is then
//! already in the shards and in the listing, so a backup that finishes while
//! the check runs cannot produce a finding.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::check::{Finding, FindingKind, Findings, Note, NoteKind, Notes, is_data_error};
use crate::config::MAX_CONFIG_SIZE;
use crate::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId};
use crate::error::{Error, Result, within_size};
use crate::gc::mark::load_shards;
use crate::index::{BlobLocation, INDEX_PREFIX, Index, PACKS_PREFIX, hex32, parse_hex32};
use crate::keys::ReadKeys;
use crate::locks::{LOCKS_PREFIX, MIN_PAUSE, read_gc_locks};
use crate::pack::pack_object_key;
use crate::pb;
use crate::reader::{Reader, ReaderCache};
use crate::snapshot::{MAX_SNAPSHOT_SIZE, SNAPSHOT_PREFIX, open_snapshot, snapshot_object_key};
use crate::store::{BlobError, BlobStore, ListEntry, read_bounded};
use crate::tree::{MAX_DEPTH, MAX_DIR_ENTRIES, MAX_HELD_ENTRIES, check_node, decode_tree};

const KEYS_PREFIX: &str = "keys/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructureStep {
    Listing,
    Indexes,
    Snapshots,
}

#[derive(Debug, Default)]
pub struct StructureReport {
    pub snapshots_checked: u64,
    pub trees_checked: u64,
    pub packs_listed: u64,
    pub missing_packs: u64,
    /// Bytes of every object under `packs/`, `index/`, `snapshots/`,
    /// `locks/` and `keys/`, plus `config`, as the store lists them.
    pub physical_bytes: u64,
    pub objects: u64,
    pub listed_packs: HashMap<BlobId, u64>,
    /// The snapshots that opened, for a later restore sample.
    pub snapshots: Vec<BlobId>,
    pub findings: Vec<Finding>,
    pub findings_total: u64,
    pub notes: Vec<Note>,
    /// Every note, also the ones past [`MAX_FINDINGS`](crate::check::MAX_FINDINGS).
    pub notes_total: u64,
}

fn reason(e: &Error) -> String {
    match e {
        Error::Corrupt { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

fn blob_ref(raw: &[u8]) -> Option<BlobId> {
    raw.try_into().ok()
}

struct Checker<'a, S: BlobStore> {
    store: &'a S,
    keys: &'a ReadKeys,
    index: Index,
    reloaded: bool,
    listed: HashMap<BlobId, u64>,
    pending: HashSet<BlobId>,
    present: HashSet<BlobId>,
    missing: HashSet<BlobId>,
    noted_pending: HashSet<BlobId>,
    /// Blobs found unknown or unreadable, so that every snapshot that
    /// refers to one gets the finding, not only the first.
    bad: HashMap<BlobId, (FindingKind, String)>,
    findings: Findings,
    notes: Notes,
}

impl<S: BlobStore> Checker<'_, S> {
    fn note_pending(&mut self, pack: &BlobId) {
        if self.noted_pending.insert(*pack) {
            self.notes.add(
                NoteKind::PendingCleanup,
                pack_object_key(pack),
                "in the index and marked for deletion by a cleanup that is still running",
            );
        }
    }

    /// Where the index puts a blob. A miss loads the shards again, once per
    /// check: a cleanup may have replaced a shard since they were read.
    async fn find(&mut self, id: &BlobId, blob_type: u8) -> Result<Option<BlobLocation>> {
        if let Some(loc) = self.index.get(id, blob_type) {
            return Ok(Some(*loc));
        }
        if self.reloaded {
            return Ok(None);
        }
        self.reloaded = true;
        let shards = load_shards(self.store, self.keys).await?;
        match shards.index(&HashSet::new()) {
            Ok(index) => self.index = index,
            Err(e) if is_data_error(&e) => {}
            Err(e) => return Err(e),
        }
        Ok(self.index.get(id, blob_type).copied())
    }

    /// Whether the pack can be read; a pack that cannot is reported once.
    async fn pack_present(&mut self, pack: &BlobId, referenced_by: &str) -> Result<bool> {
        if self.listed.contains_key(pack) || self.present.contains(pack) {
            return Ok(true);
        }
        if self.missing.contains(pack) {
            return Ok(false);
        }
        if self.pending.contains(pack) {
            self.note_pending(pack);
            return Ok(false);
        }
        let key = pack_object_key(pack);
        match self.store.get_range(&key, 0..1).await {
            Ok(_) => {
                self.present.insert(*pack);
                Ok(true)
            }
            Err(BlobError::NotFound) => {
                self.missing.insert(*pack);
                self.findings.add(
                    FindingKind::MissingPack,
                    key,
                    format!("referenced by {referenced_by} but not in the bucket"),
                );
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn bad_blob(&mut self, snapshot_key: &str, id: &BlobId, kind: FindingKind, detail: String) {
        self.findings.add(kind, snapshot_key, detail.clone());
        self.bad.insert(*id, (kind, detail));
    }

    /// Reports `id` again for a snapshot that reaches it after the first.
    fn again(&mut self, snapshot_key: &str, id: &BlobId) {
        if let Some((kind, detail)) = self.bad.get(id) {
            self.findings.add(*kind, snapshot_key, detail.clone());
        }
    }

    fn unknown_blob(&mut self, snapshot_key: &str, id: &BlobId) {
        let detail = format!(
            "references blob {} that no index shard describes",
            hex32(id)
        );
        self.bad_blob(snapshot_key, id, FindingKind::UnknownBlob, detail);
    }
}

/// A directory whose entries are checked and whose subdirectories are being
/// walked.
struct Frame {
    dir: BlobId,
    depth: usize,
    /// Entries of this directory and of every directory above it.
    held: u64,
    len: u64,
    subdirs: Vec<Vec<BlobId>>,
    /// The most entries held from this directory down, over the subdirectories
    /// walked so far.
    below: u64,
    exact: bool,
}

/// Counts what a finished subdirectory holds into the directory above it.
fn add_below(frames: &mut [Frame], below: u64, exact: bool) {
    if let Some(parent) = frames.last_mut() {
        parent.below = parent.below.max(parent.len.saturating_add(below));
        parent.exact &= exact;
    }
}

fn record_below(map: &mut HashMap<BlobId, (u64, bool)>, dir: BlobId, below: u64, exact: bool) {
    let known = map.entry(dir).or_insert((0, false));
    if exact {
        *known = (below, true);
    } else {
        known.0 = known.0.max(below);
    }
}

fn too_large(name: &str, e: &Error) -> String {
    format!("tree {name}: {e}; lode restore --allow-large-directories restores it")
}

fn tally(entries: &[ListEntry], bytes: &mut u64, objects: &mut u64) {
    for e in entries {
        *bytes += e.size;
        *objects += 1;
    }
}

/// Checks the structure of the repository in `store`. Reads the listings, the
/// index shards, the snapshots and the tree blobs; no data blob and no pack
/// body is read.
pub async fn check_structure<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    progress: &(dyn Fn(StructureStep, u64, u64) + Sync),
) -> Result<StructureReport> {
    check_with_limits(
        store,
        keys,
        config,
        progress,
        MAX_DIR_ENTRIES,
        MAX_HELD_ENTRIES,
    )
    .await
}

/// [`check_structure`] with smaller directory limits, for tests.
#[cfg(any(test, feature = "test-hooks"))]
pub async fn check_structure_with_dir_limits<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    progress: &(dyn Fn(StructureStep, u64, u64) + Sync),
    dir: u64,
    held: u64,
) -> Result<StructureReport> {
    check_with_limits(store, keys, config, progress, dir, held).await
}

async fn check_with_limits<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    progress: &(dyn Fn(StructureStep, u64, u64) + Sync),
    dir_limit: u64,
    held_limit: u64,
) -> Result<StructureReport> {
    let mut report = StructureReport::default();
    let mut findings = Findings::default();
    let mut notes = Notes::default();

    progress(StructureStep::Listing, 0, 0);
    let snapshot_objects = store.list(SNAPSHOT_PREFIX).await?;
    tally(
        &snapshot_objects,
        &mut report.physical_bytes,
        &mut report.objects,
    );
    let mut snapshots = Vec::new();
    for entry in snapshot_objects {
        let id = entry
            .key
            .strip_prefix(SNAPSHOT_PREFIX)
            .and_then(parse_hex32);
        match id.filter(|id| snapshot_object_key(id) == entry.key) {
            Some(id) => snapshots.push((entry.key, id, entry.size)),
            None => notes.add(
                NoteKind::IgnoredObject,
                entry.key,
                "the name is not the id of a snapshot",
            ),
        }
    }

    progress(StructureStep::Indexes, 0, 0);
    let shards = load_shards(store, keys).await?;
    for (key, why) in &shards.skipped {
        findings.add(FindingKind::UnreadableIndex, key.clone(), why.clone());
    }
    let index = match shards.index(&HashSet::new()) {
        Ok(index) => index,
        Err(e) if is_data_error(&e) => {
            findings.add(FindingKind::UnreadableIndex, INDEX_PREFIX, reason(&e));
            report.findings = findings.list;
            report.findings_total = findings.total;
            report.notes = notes.list;
            report.notes_total = notes.total;
            return Ok(report);
        }
        Err(e) => return Err(e),
    };

    let pack_objects = store.list(PACKS_PREFIX).await?;
    tally(
        &pack_objects,
        &mut report.physical_bytes,
        &mut report.objects,
    );
    let mut listed = HashMap::new();
    for entry in &pack_objects {
        let id = entry.key.rsplit('/').next().and_then(parse_hex32);
        if let Some(id) = id.filter(|id| pack_object_key(id) == entry.key) {
            listed.insert(id, entry.size);
        }
    }
    for prefix in [INDEX_PREFIX, KEYS_PREFIX] {
        tally(
            &store.list(prefix).await?,
            &mut report.physical_bytes,
            &mut report.objects,
        );
    }
    match store.list(LOCKS_PREFIX).await {
        Ok(entries) => tally(&entries, &mut report.physical_bytes, &mut report.objects),
        Err(BlobError::Denied { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    match read_bounded(store, "config", None, MAX_CONFIG_SIZE, "config").await {
        Ok(bytes) => {
            report.physical_bytes += bytes.len() as u64;
            report.objects += 1;
        }
        Err(Error::Store(BlobError::NotFound)) => {}
        Err(e @ Error::Corrupt { .. }) => {
            findings.add(FindingKind::UnreadableConfig, "config", reason(&e));
        }
        Err(e) => return Err(e),
    }

    let pending = match read_gc_locks(store, keys.k_idx(), keys.repo_id()).await {
        Ok(locks) => {
            if locks.denied {
                notes.add(
                    NoteKind::LocksUnreadable,
                    LOCKS_PREFIX,
                    "the store refused to list locks/, so a running cleanup can't be ruled out",
                );
            }
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            let window_ms = MIN_PAUSE.as_millis() as i64;
            let mut fresh = HashSet::new();
            for m in &locks.manifests {
                if m.modified_ms
                    .is_some_and(|t| now_ms.saturating_sub(t) < window_ms)
                {
                    fresh.extend(m.candidate_packs.iter().copied());
                } else {
                    notes.add(
                        NoteKind::StaleCleanup,
                        m.key.clone(),
                        "a cleanup manifest that is old or undated; its packs are not excused",
                    );
                }
            }
            fresh
        }
        Err(e) if is_data_error(&e) => {
            notes.add(NoteKind::LocksUnreadable, LOCKS_PREFIX, reason(&e));
            HashSet::new()
        }
        Err(e) => return Err(e),
    };

    let mut indexed: Vec<BlobId> = shards
        .shards
        .iter()
        .flat_map(|(_, shard)| shard.packs.iter())
        .filter_map(|p| blob_ref(&p.pack_id))
        .collect();
    indexed.sort_unstable();
    indexed.dedup();
    let indexed_set: HashSet<BlobId> = indexed.iter().copied().collect();

    let mut c = Checker {
        store,
        keys,
        index,
        reloaded: false,
        listed,
        pending,
        present: HashSet::new(),
        missing: HashSet::new(),
        noted_pending: HashSet::new(),
        bad: HashMap::new(),
        findings,
        notes,
    };
    let mut size_checked: HashSet<BlobId> = HashSet::new();
    for (_, shard) in &shards.shards {
        for p in &shard.packs {
            let Some(pack) = blob_ref(&p.pack_id) else {
                continue;
            };
            if let Some(&actual) = c.listed.get(&pack)
                && actual != p.pack_size
                && size_checked.insert(pack)
            {
                c.findings.add(
                    FindingKind::PackMismatch,
                    pack_object_key(&pack),
                    format!(
                        "the index records {} bytes, the bucket has {actual}",
                        p.pack_size
                    ),
                );
            }
        }
    }
    for pack in &indexed {
        if c.listed.contains_key(pack) {
            continue;
        }
        if c.pending.contains(pack) {
            c.note_pending(pack);
        } else if c.missing.insert(*pack) {
            c.findings.add(
                FindingKind::MissingPack,
                pack_object_key(pack),
                "in the index but not in the bucket",
            );
        }
    }
    let mut unindexed: Vec<BlobId> = c
        .listed
        .keys()
        .filter(|p| !indexed_set.contains(*p))
        .copied()
        .collect();
    unindexed.sort_unstable();
    for pack in unindexed {
        c.notes.add(
            NoteKind::UnindexedPack,
            pack_object_key(&pack),
            "no index shard describes this pack",
        );
    }

    let mut reader = Reader::open_shared(store, keys.duplicate(), config, ReaderCache::new(0))?;
    reader.set_dir_limits(dir_limit, held_limit);
    let mut seen_dirs: HashSet<BlobId> = HashSet::new();
    // The most entries a walk holds from a directory down, the directory
    // counted; `false` marks a lower bound of a subtree that was only partly
    // walked.
    let mut held_below: HashMap<BlobId, (u64, bool)> = HashMap::new();
    let mut seen_chunks: HashSet<BlobId> = HashSet::new();
    let total = snapshots.len() as u64;
    for (done, (key, id, size)) in snapshots.into_iter().enumerate() {
        progress(StructureStep::Snapshots, done as u64, total);
        if let Err(e) = within_size(&key, size, MAX_SNAPSHOT_SIZE, "snapshot") {
            report.snapshots_checked += 1;
            c.findings
                .add(FindingKind::UnreadableSnapshot, key, reason(&e));
            continue;
        }
        let bytes = match read_bounded(store, &key, Some(size), MAX_SNAPSHOT_SIZE, "snapshot").await
        {
            Ok(bytes) => bytes,
            Err(Error::Store(BlobError::NotFound)) => {
                c.notes.add(
                    NoteKind::IgnoredObject,
                    key,
                    "deleted while the check was running",
                );
                continue;
            }
            Err(e @ Error::Corrupt { .. }) => {
                report.snapshots_checked += 1;
                c.findings
                    .add(FindingKind::UnreadableSnapshot, key, reason(&e));
                continue;
            }
            Err(e) => return Err(e),
        };
        report.snapshots_checked += 1;
        let snapshot = match open_snapshot(keys.hpke_private_key(), keys.repo_id(), &id, &bytes) {
            Ok(snapshot) => snapshot,
            Err(e) if is_data_error(&e) => {
                c.findings
                    .add(FindingKind::UnreadableSnapshot, key, reason(&e));
                continue;
            }
            Err(e) => return Err(e),
        };
        report.snapshots.push(id);
        let Some(root) = blob_ref(&snapshot.root_tree) else {
            c.findings.add(
                FindingKind::UnreadableSnapshot,
                key,
                "root tree id must be 32 bytes",
            );
            continue;
        };
        // shortcut: hardlink members and the depth of a subtree seen in an
        // earlier snapshot are not checked again; a per-snapshot walk would
        // cost a full tree walk per snapshot. A subtree that was only partly
        // walked because it passed the held limit is walked again when a later
        // snapshot reaches it with fewer entries above it.
        let mut groups = HashMap::new();
        let mut held_found = false;
        // A directory is its tree pieces, read as one run like a restore does,
        // with its depth and the entries of the directories above it counted
        // the way the restore's cursor counts them. Subdirectories are walked
        // before their parent is done, so the most entries held below each
        // directory is known when it is.
        let mut frames: Vec<Frame> = Vec::new();
        let mut next = Some((vec![root], 1));
        'dirs: loop {
            let (pieces, depth) = match next.take() {
                Some(dir) => dir,
                None => {
                    let Some(top) = frames.last_mut() else {
                        break;
                    };
                    if let Some(pieces) = top.subdirs.pop() {
                        (pieces, top.depth + 1)
                    } else {
                        let Some(done) = frames.pop() else {
                            break;
                        };
                        if !done.exact {
                            seen_dirs.remove(&done.dir);
                        }
                        record_below(&mut held_below, done.dir, done.below, done.exact);
                        add_below(&mut frames, done.below, done.exact);
                        continue;
                    }
                }
            };
            let above = frames.last().map_or(0, |f| f.held);
            let dir = match pieces.as_slice() {
                [one] => *one,
                _ => *blake3::hash(&pieces.concat()).as_bytes(),
            };
            let name = hex32(pieces.first().unwrap_or(&dir));
            if let Some(&(below, exact)) = held_below.get(&dir) {
                let over = reader.walk_budget().check_held(above.saturating_add(below));
                if exact || over.is_err() || held_found {
                    if let Err(e) = over
                        && !std::mem::replace(&mut held_found, true)
                    {
                        c.findings
                            .add(FindingKind::UnreadableTree, &key, too_large(&name, &e));
                    }
                    c.again(&key, &dir);
                    add_below(&mut frames, below, exact);
                    continue;
                }
            }
            if !seen_dirs.insert(dir) {
                c.again(&key, &dir);
                continue;
            }
            let mut nodes = Vec::new();
            let mut previous: Option<Vec<u8>> = None;
            for tree in &pieces {
                let Some(loc) = c.find(tree, BLOB_TYPE_TREE).await? else {
                    let detail = format!(
                        "references blob {} that no index shard describes",
                        hex32(tree)
                    );
                    c.bad_blob(&key, &dir, FindingKind::UnknownBlob, detail);
                    continue 'dirs;
                };
                if !c.pack_present(&loc.pack_id, &key).await? {
                    continue 'dirs;
                }
                let piece = match reader.read_blob(tree, BLOB_TYPE_TREE).await {
                    Ok(bytes) => decode_tree(&bytes, previous.as_deref()),
                    Err(e) => Err(e),
                };
                report.trees_checked += 1;
                match piece {
                    Ok(piece) => {
                        let n = nodes.len() as u64 + piece.len() as u64;
                        if let Err(e) = reader.walk_budget().check_dir(n) {
                            let detail = too_large(&name, &e);
                            c.bad_blob(&key, &dir, FindingKind::UnreadableTree, detail);
                            continue 'dirs;
                        }
                        if let Some(last) = piece.last() {
                            previous = Some(last.name.clone());
                        }
                        nodes.extend(piece);
                    }
                    Err(e) if is_data_error(&e) => {
                        let detail = format!("tree {}: {}", hex32(tree), reason(&e));
                        c.bad_blob(&key, &dir, FindingKind::UnreadableTree, detail);
                        continue 'dirs;
                    }
                    Err(e) => return Err(e),
                }
            }
            let len = nodes.len() as u64;
            let held = above.saturating_add(len);
            if let Err(e) = reader.walk_budget().check_held(held) {
                if !std::mem::replace(&mut held_found, true) {
                    c.findings
                        .add(FindingKind::UnreadableTree, &key, too_large(&name, &e));
                }
                seen_dirs.remove(&dir);
                record_below(&mut held_below, dir, len, false);
                add_below(&mut frames, len, false);
                continue;
            }
            let mut subdirs = Vec::new();
            for node in &nodes {
                let mut all_known = true;
                for raw in &node.content {
                    let Some(chunk) = blob_ref(raw) else {
                        all_known = false;
                        continue;
                    };
                    if !seen_chunks.insert(chunk) {
                        all_known &= !c.bad.contains_key(&chunk);
                        c.again(&key, &chunk);
                        continue;
                    }
                    match c.find(&chunk, BLOB_TYPE_DATA).await? {
                        None => {
                            all_known = false;
                            c.unknown_blob(&key, &chunk);
                        }
                        Some(loc) => {
                            c.pack_present(&loc.pack_id, &key).await?;
                        }
                    }
                }
                if all_known {
                    match check_node(&mut reader, &mut groups, node).await {
                        Ok(()) => {}
                        Err(e) if is_data_error(&e) => {
                            c.bad_blob(
                                &key,
                                &dir,
                                FindingKind::UnreadableTree,
                                format!("tree {name}: {e}"),
                            );
                        }
                        Err(e) => return Err(e),
                    }
                }
                if node.r#type == pb::NodeType::Dir as i32 {
                    if depth > MAX_DEPTH {
                        let detail = format!("tree {name}: directories are nested too deeply");
                        c.bad_blob(&key, &dir, FindingKind::UnreadableTree, detail);
                        continue;
                    }
                    let subtrees: Vec<BlobId> = node
                        .subtrees
                        .iter()
                        .filter_map(|raw| blob_ref(raw))
                        .collect();
                    if !subtrees.is_empty() {
                        subdirs.push(subtrees);
                    }
                }
            }
            frames.push(Frame {
                dir,
                depth,
                held,
                len,
                subdirs,
                below: len,
                exact: true,
            });
        }
    }
    progress(StructureStep::Snapshots, total, total);

    report.packs_listed = c.listed.len() as u64;
    report.missing_packs = c.missing.len() as u64;
    report.listed_packs = c.listed;
    report.findings = c.findings.list;
    report.findings_total = c.findings.total;
    report.notes = c.notes.list;
    report.notes_total = c.notes.total;
    Ok(report)
}
