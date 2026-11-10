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
use crate::tree::{MAX_DEPTH, TreeBudget, count_tree_nodes, decode_tree};

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
    /// What the snapshot being checked already has, so that the same finding
    /// is not added twice to one snapshot.
    snapshot_seen: HashSet<(FindingKind, String)>,
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

    /// Adds a finding to the snapshot at `key` unless it has this one.
    fn add_once(&mut self, key: &str, kind: FindingKind, detail: String) {
        if self.snapshot_seen.insert((kind, detail.clone())) {
            self.findings.add(kind, key, detail);
        }
    }

    /// Adds a member to the hardlink groups of the snapshot being checked. A
    /// member that differs from the first of its group is a finding of this
    /// snapshot alone: another snapshot that shares the directory may not
    /// have the other member.
    fn join(&mut self, key: &str, groups: &mut Groups, group: u64, member: Member, name: &str) {
        if *groups.entry(group).or_insert(member) != member {
            let detail =
                format!("tree {name}: members of one hardlink group differ in size or content");
            self.add_once(key, FindingKind::UnreadableTree, detail);
        }
    }

    /// Adds the members of the directory `from` and of everything below it
    /// that the snapshot has not taken yet.
    fn merge_links(
        &mut self,
        key: &str,
        links: &HashMap<BlobId, Links>,
        merged: &mut HashSet<BlobId>,
        groups: &mut Groups,
        from: BlobId,
    ) {
        let mut stack = vec![from];
        while let Some(dir) = stack.pop() {
            if !merged.insert(dir) {
                continue;
            }
            let Some(known) = links.get(&dir) else {
                continue;
            };
            let name = hex32(&dir);
            for &(group, member) in &known.members {
                self.join(key, groups, group, member, &name);
            }
            stack.extend(known.below.iter().copied());
        }
    }

    /// The finding a path to a directory gives the snapshot, at most one.
    fn path_hit(&mut self, key: &str, hit: &mut bool, detail: String) {
        if !std::mem::replace(hit, true) {
            self.add_once(key, FindingKind::UnreadableTree, detail);
        }
    }

    /// A finding that holds for every snapshot that reaches the directory
    /// being walked: it goes to this snapshot and into `found`, which the
    /// directory's memo keeps for the snapshots that come later.
    fn report(&mut self, key: &str, found: &mut Found, kind: FindingKind, detail: String) {
        if found.is_none() {
            *found = Some((kind, detail.clone()));
        }
        self.add_once(key, kind, detail);
    }

    fn bad_blob(
        &mut self,
        key: &str,
        found: &mut Found,
        id: &BlobId,
        kind: FindingKind,
        detail: String,
    ) {
        self.bad.insert(*id, (kind, detail.clone()));
        self.report(key, found, kind, detail);
    }

    /// Reports `id` again for a snapshot that reaches it after the first.
    fn again(&mut self, key: &str, found: &mut Found, id: &BlobId) {
        if let Some((kind, detail)) = self.bad.get(id).cloned() {
            self.report(key, found, kind, detail);
        }
    }

    fn unknown_blob(&mut self, key: &str, found: &mut Found, id: &BlobId) {
        let detail = format!(
            "references blob {} that no index shard describes",
            hex32(id)
        );
        self.bad_blob(key, found, id, FindingKind::UnknownBlob, detail);
    }
}

/// The first finding at or below a directory that is true for any path to it.
type Found = Option<(FindingKind, String)>;

/// Entries and tree bytes a walk holds.
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    entries: u64,
    bytes: u64,
}

impl Held {
    fn plus(self, other: Held) -> Held {
        Held {
            entries: self.entries.saturating_add(other.entries),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    fn max(self, other: Held) -> Held {
        Held {
            entries: self.entries.max(other.entries),
            bytes: self.bytes.max(other.bytes),
        }
    }

    fn check(self, budget: &TreeBudget) -> Result<()> {
        budget.check_held(self.entries)?;
        budget.check_held_bytes(self.bytes)
    }
}

/// A member of a hardlink group: the size and a hash of the chunk ids, which
/// is what a restore compares.
type Member = (u64, [u8; 32]);

/// The first member of each hardlink group a snapshot has met.
type Groups = HashMap<u64, Member>;

/// The hardlink members of a directory and of everything below it, kept apart
/// from the memo because whether they conflict depends on the snapshot.
struct Links {
    /// The members of this directory alone, with their groups.
    members: Vec<(u64, Member)>,
    /// The subdirectories that have members in their own subtrees.
    below: Vec<BlobId>,
}

/// What a finished walk of a directory leaves for the snapshots that reach it
/// again. Nothing in it depends on the path the directory was reached by, so a
/// directory is walked once.
struct DirMemo {
    /// The most a walk holds from this directory down, the directory counted
    /// whole.
    below: Held,
    /// Levels of directory nodes from this directory down: 0 without any.
    height: usize,
    /// Entries and tree bytes a restore charges for this directory and
    /// everything below it, every occurrence of a shared directory counted.
    walk: Held,
    finding: Found,
}

/// What a directory gives the directory above it.
struct Done {
    below: Held,
    height: usize,
    walk: Held,
    finding: Found,
}

impl Done {
    fn empty(finding: Found) -> Done {
        Done {
            below: Held::default(),
            height: 0,
            walk: Held::default(),
            finding,
        }
    }
}

/// A directory whose entries are checked and whose subdirectories are being
/// walked.
struct Frame {
    dir: BlobId,
    depth: usize,
    /// Every directory above this one.
    above: Held,
    /// This directory and every directory above it.
    held: Held,
    /// This directory alone. The `subtrees` lists of its subdirectories are
    /// part of its tree bytes.
    own: Held,
    subdirs: Vec<Vec<BlobId>>,
    /// The most held from this directory down, over the subdirectories walked
    /// so far.
    below: Held,
    height: usize,
    walk: Held,
    finding: Found,
    members: Vec<(u64, Member)>,
    linked: Vec<BlobId>,
}

impl Frame {
    /// The level above the root of a snapshot: it holds nothing and ends up
    /// with the totals of the whole snapshot.
    fn above_root(root: BlobId) -> Frame {
        Frame {
            dir: [0; 32],
            depth: 0,
            above: Held::default(),
            held: Held::default(),
            own: Held::default(),
            subdirs: vec![vec![root]],
            below: Held::default(),
            height: 0,
            walk: Held::default(),
            finding: None,
            members: Vec::new(),
            linked: Vec::new(),
        }
    }

    fn done(self) -> Done {
        Done {
            below: self.below,
            height: self.height,
            walk: self.walk,
            finding: self.finding,
        }
    }
}

/// Hands what a finished subdirectory holds to the directory above it.
fn climb(frames: &mut [Frame], done: &Done) {
    if let Some(parent) = frames.last_mut() {
        parent.below = parent.below.max(parent.own.plus(done.below));
        parent.height = parent.height.max(done.height.saturating_add(1));
        parent.walk = parent.walk.plus(done.walk);
        if parent.finding.is_none() {
            parent.finding.clone_from(&done.finding);
        }
    }
}

/// Remembers a walked directory and hands what it holds to the directory
/// above it.
fn close(memo: &mut HashMap<BlobId, DirMemo>, frames: &mut [Frame], dir: BlobId, done: Done) {
    climb(frames, &done);
    memo.insert(
        dir,
        DirMemo {
            below: done.below,
            height: done.height,
            walk: done.walk,
            finding: done.finding,
        },
    );
}

fn too_deep(name: &str) -> String {
    format!("tree {name}: directories are nested too deeply")
}

/// The finding a visit to a memoized directory gets from the path to it, or
/// none.
fn path_finding(
    known: &DirMemo,
    above: Held,
    depth: usize,
    budget: &TreeBudget,
    name: &str,
) -> Option<String> {
    if let Err(e) = above.plus(known.below).check(budget) {
        return Some(over_path(name, &e));
    }
    // The deepest directory node sits `height - 1` levels below this directory.
    if known.height >= 1 && depth.saturating_add(known.height.saturating_sub(1)) > MAX_DEPTH {
        return Some(too_deep(name));
    }
    None
}

/// Why a directory was not read to the end.
enum Stop {
    /// A finding true for any path to the directory.
    Finding(FindingKind, String),
    /// A pack that a cleanup still running may have removed: no finding.
    Pending,
}

/// A limit of one directory: the directory was not decoded.
fn too_large(name: &str, e: &Error) -> String {
    format!(
        "tree {name}: {e}; nothing below it was checked; lode restore --allow-large-directories lifts this limit"
    )
}

/// A limit of a path: the directory was read and what is below it is checked.
fn over_path(name: &str, e: &Error) -> String {
    format!("tree {name}: {e}; lode restore --allow-large-directories lifts this limit")
}

fn tally(entries: &[ListEntry], bytes: &mut u64, objects: &mut u64) {
    for e in entries {
        *bytes = bytes.saturating_add(e.size);
        *objects = objects.saturating_add(1);
    }
}

/// Checks the structure of the repository in `store`. Reads the listings, the
/// index shards, the snapshots and the tree blobs, and, only for a file whose
/// chunk sizes in the index do not add up, its chunks: each distinct chunk
/// once for each directory the file is written in, within one walk budget for
/// the whole run: once the budget is spent no other chunk is read, so the run
/// reads at most one chunk past it. No other data is read, and no pack is read
/// whole.
pub async fn check_structure<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    progress: &(dyn Fn(StructureStep, u64, u64) + Sync),
) -> Result<StructureReport> {
    check_with_budget(store, keys, config, progress, TreeBudget::default()).await
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
    let budget = TreeBudget::default().with_dir_limits(dir, held);
    check_with_budget(store, keys, config, progress, budget).await
}

/// [`check_structure`] with a budget of the caller's, for tests.
#[cfg(any(test, feature = "test-hooks"))]
pub async fn check_structure_with_budget<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    progress: &(dyn Fn(StructureStep, u64, u64) + Sync),
    budget: TreeBudget,
) -> Result<StructureReport> {
    check_with_budget(store, keys, config, progress, budget).await
}

async fn check_with_budget<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    progress: &(dyn Fn(StructureStep, u64, u64) + Sync),
    budget: TreeBudget,
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
            report.physical_bytes = report.physical_bytes.saturating_add(bytes.len() as u64);
            report.objects = report.objects.saturating_add(1);
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
        snapshot_seen: HashSet::new(),
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
    // Only `check_file` charges this reader, for the chunks of files whose
    // sizes do not add up, and nothing on the check's path starts it again, so
    // it is one budget for the whole run; the verdict on the budget of a
    // snapshot is `check_walk`'s, from the counts of the memo.
    reader.set_walk_budget(budget);
    let mut memo: HashMap<BlobId, DirMemo> = HashMap::new();
    let mut links: HashMap<BlobId, Links> = HashMap::new();
    let mut trees_read: HashSet<BlobId> = HashSet::new();
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
        c.snapshot_seen.clear();
        let mut groups = Groups::new();
        let mut merged: HashSet<BlobId> = HashSet::new();
        // A finding that depends on the path to a directory is reported once
        // per snapshot.
        let mut path_found = false;
        // A directory is its tree pieces, read as one run like a restore does,
        // with its depth and what the directories above it hold counted the
        // way the restore's cursor counts them. Subdirectories are walked
        // before their parent is done, so the most held below each directory
        // is known when it is. The bottom frame stands for the level above
        // the root and ends up with what a restore of the snapshot charges.
        let mut frames: Vec<Frame> = vec![Frame::above_root(root)];
        // The frames from the bottom whose subtree alone passes a limit of a
        // path: the snapshot has that finding however the rest is walked, so
        // their remaining subdirectories are not walked and they let go of
        // what they held, keeping a fixed amount each. The frames above them,
        // still walked, hold no more tree data than the limits of a path plus
        // one directory. A frame is pushed after its directory is read, and a
        // directory is read once per check, so there are no more frames than
        // directories read, as there are entries in the memo; neither the
        // limits of a path nor the depth bound their number.
        let mut saturated = 0usize;
        while let Some(top) = frames.last_mut() {
            let Some(pieces) = top.subdirs.pop() else {
                let Some(mut frame) = frames.pop() else {
                    break;
                };
                saturated = saturated.min(frames.len());
                if frames.is_empty() {
                    if let Err(e) = budget.check_walk(frame.walk.entries, frame.walk.bytes) {
                        c.add_once(&key, FindingKind::UnreadableTree, e.to_string());
                    }
                    break;
                }
                let dir = frame.dir;
                let members = std::mem::take(&mut frame.members);
                let linked = std::mem::take(&mut frame.linked);
                close(&mut memo, &mut frames, dir, frame.done());
                if !members.is_empty() || !linked.is_empty() {
                    links.insert(
                        dir,
                        Links {
                            members,
                            below: linked,
                        },
                    );
                    merged.insert(dir);
                    if let Some(parent) = frames.last_mut() {
                        parent.linked.push(dir);
                    }
                }
                continue;
            };
            let depth = top.depth.saturating_add(1);
            let above = top.held;
            let dir = match pieces.as_slice() {
                [one] => *one,
                _ => *blake3::hash(&pieces.concat()).as_bytes(),
            };
            let name = hex32(pieces.first().unwrap_or(&dir));
            let mut found: Found = None;
            if let Some(known) = memo.get(&dir) {
                if let Some(detail) = path_finding(known, above, depth, &budget, &name) {
                    c.path_hit(&key, &mut path_found, detail);
                }
                if let Some((kind, detail)) = known.finding.clone() {
                    c.add_once(&key, kind, detail);
                }
                let done = Done {
                    below: known.below,
                    height: known.height,
                    walk: known.walk,
                    finding: known.finding.clone(),
                };
                climb(&mut frames, &done);
                if links.contains_key(&dir) {
                    c.merge_links(&key, &links, &mut merged, &mut groups, dir);
                    if let Some(parent) = frames.last_mut() {
                        parent.linked.push(dir);
                    }
                }
                continue;
            }
            let mut nodes = Vec::new();
            let mut previous: Option<Vec<u8>> = None;
            let mut read = 0u64;
            let unreadable = |tree: &BlobId, e: &Error| {
                Stop::Finding(
                    FindingKind::UnreadableTree,
                    format!("tree {}: {}", hex32(tree), reason(e)),
                )
            };
            // Every limit is checked before the data it guards is decoded, and
            // before it is read where the index already says enough, in the
            // order a restore checks them.
            let stop: Option<Stop> = 'read: {
                let listed = pieces.iter().fold(0u64, |sum, piece| {
                    let ulen = c.index.smallest_ulen(piece, BLOB_TYPE_TREE);
                    sum.saturating_add(u64::from(ulen.unwrap_or(0)))
                });
                if let Err(e) = budget.check_dir_bytes(listed) {
                    break 'read Some(Stop::Finding(
                        FindingKind::UnreadableTree,
                        too_large(&name, &e),
                    ));
                }
                for tree in &pieces {
                    let Some(loc) = c.find(tree, BLOB_TYPE_TREE).await? else {
                        let detail = format!(
                            "references blob {} that no index shard describes",
                            hex32(tree)
                        );
                        break 'read Some(Stop::Finding(FindingKind::UnknownBlob, detail));
                    };
                    if !c.pack_present(&loc.pack_id, &key).await? {
                        break 'read Some(if c.pending.contains(&loc.pack_id) {
                            Stop::Pending
                        } else {
                            let detail = format!(
                                "tree {}: its pack {} is missing from the bucket",
                                hex32(tree),
                                pack_object_key(&loc.pack_id)
                            );
                            Stop::Finding(FindingKind::UnreadableTree, detail)
                        });
                    }
                    let got = reader.read_blob(tree, BLOB_TYPE_TREE).await;
                    trees_read.insert(*tree);
                    let bytes = match got {
                        Ok(bytes) => bytes,
                        Err(e) if is_data_error(&e) => break 'read Some(unreadable(tree, &e)),
                        Err(e) => return Err(e),
                    };
                    read = read.saturating_add(bytes.len() as u64);
                    if let Err(e) = budget.check_dir_bytes(read) {
                        break 'read Some(Stop::Finding(
                            FindingKind::UnreadableTree,
                            too_large(&name, &e),
                        ));
                    }
                    let count = match count_tree_nodes(&bytes) {
                        Ok(count) => count,
                        Err(e) if is_data_error(&e) => break 'read Some(unreadable(tree, &e)),
                        Err(e) => return Err(e),
                    };
                    let n = (nodes.len() as u64).saturating_add(count);
                    if let Err(e) = budget.check_dir(n) {
                        break 'read Some(Stop::Finding(
                            FindingKind::UnreadableTree,
                            too_large(&name, &e),
                        ));
                    }
                    let piece = match decode_tree(&bytes, previous.as_deref()) {
                        Ok(piece) => piece,
                        Err(e) if is_data_error(&e) => break 'read Some(unreadable(tree, &e)),
                        Err(e) => return Err(e),
                    };
                    if let Some(last) = piece.last() {
                        previous = Some(last.name.clone());
                    }
                    nodes.extend(piece);
                }
                None
            };
            match stop {
                None => {}
                Some(Stop::Finding(kind, detail)) => {
                    c.report(&key, &mut found, kind, detail);
                    close(&mut memo, &mut frames, dir, Done::empty(found));
                    continue;
                }
                Some(Stop::Pending) => {
                    close(&mut memo, &mut frames, dir, Done::empty(found));
                    continue;
                }
            }
            let own = Held {
                entries: nodes.len() as u64,
                bytes: read,
            };
            let reached = above.plus(own);
            if let Err(e) = reached.check(&budget) {
                c.path_hit(&key, &mut path_found, over_path(&name, &e));
            }
            let mut height = 0;
            let mut subdirs = Vec::new();
            let mut members = Vec::new();
            let mut lies = Held::default();
            for node in &nodes {
                let mut all_known = true;
                for raw in &node.content {
                    let Some(chunk) = blob_ref(raw) else {
                        all_known = false;
                        continue;
                    };
                    if !seen_chunks.insert(chunk) {
                        all_known &= !c.bad.contains_key(&chunk);
                        c.again(&key, &mut found, &chunk);
                        continue;
                    }
                    match c.find(&chunk, BLOB_TYPE_DATA).await? {
                        None => {
                            all_known = false;
                            c.unknown_blob(&key, &mut found, &chunk);
                        }
                        Some(loc) => {
                            c.pack_present(&loc.pack_id, &key).await?;
                        }
                    }
                }
                if node.r#type == pb::NodeType::File as i32 {
                    if all_known {
                        match reader.check_file(node).await {
                            Ok((_, None)) => {}
                            Ok((_, Some(len))) => {
                                lies = lies.plus(Held {
                                    entries: node.content.len() as u64,
                                    bytes: len,
                                });
                            }
                            // The directory is read once per check, so the file
                            // is unchecked for every snapshot that reaches it.
                            Err(Error::WalkLimit { what, limit }) => {
                                let file = String::from_utf8_lossy(&node.name);
                                let detail = format!(
                                    "tree {name}: {file} was not checked: in one run the check reads at most {limit} {what} of the chunks of files whose sizes in the index do not add up"
                                );
                                c.report(&key, &mut found, FindingKind::UnreadableTree, detail);
                            }
                            Err(e) if is_data_error(&e) => {
                                let detail = format!("tree {name}: {e}");
                                c.report(&key, &mut found, FindingKind::UnreadableTree, detail);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    if node.hardlink_group != 0 {
                        let member = (node.size, *blake3::hash(&node.content.concat()).as_bytes());
                        c.join(&key, &mut groups, node.hardlink_group, member, &name);
                        members.push((node.hardlink_group, member));
                    }
                }
                if node.r#type == pb::NodeType::Dir as i32 {
                    height = 1;
                    if depth > MAX_DEPTH {
                        c.path_hit(&key, &mut path_found, too_deep(&name));
                    }
                    // `decode_tree` has refused an id that is not 32 bytes.
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
                above,
                held: reached,
                own,
                subdirs,
                below: own,
                height,
                walk: own.plus(lies),
                finding: found,
                members,
                linked: Vec::new(),
            });
            while let Some(frame) = frames.get_mut(saturated) {
                let subtree = Held {
                    entries: reached.entries.saturating_sub(frame.above.entries),
                    bytes: reached.bytes.saturating_sub(frame.above.bytes),
                };
                if subtree.check(&budget).is_ok() && depth.saturating_sub(frame.depth) <= MAX_DEPTH
                {
                    break;
                }
                // A snapshot that reaches this directory has the finding of
                // its path, so what is below need not be walked or merged.
                frame.subdirs = Vec::new();
                frame.members = Vec::new();
                frame.linked = Vec::new();
                saturated += 1;
            }
        }
    }
    report.trees_checked = trees_read.len() as u64;
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
