//! Trees: the nodes of one directory, ordered by name. A directory too big
//! for one blob is split into several tree blobs listed in `Node.subtrees`,
//! which read back as one ordered run.

use std::collections::{HashMap, VecDeque};

use prost::Message;

use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::fs::filter::PathMatcher;
use crate::pb;
use crate::reader::{Reader, SelectionTotals};
use crate::selection::{Selection, Verdict};
use crate::store::BlobStore;

/// Directories nested deeper than this make a snapshot unreadable.
pub const MAX_DEPTH: usize = 4096;

/// The most directory entries one walk of a snapshot may visit; a tree that
/// shares one directory under many parents is counted at every visit.
pub const MAX_WALK_NODES: u64 = 1 << 30;

/// The most decoded directory bytes one walk of a snapshot may visit, counted
/// like [`MAX_WALK_NODES`].
pub const MAX_WALK_TREE_BYTES: u64 = 512 << 30;

/// The most entries one directory may have.
pub const MAX_DIR_ENTRIES: u64 = 1 << 23;

/// The most entries one operation holds decoded at once, over all the
/// directories it has open.
pub const MAX_HELD_ENTRIES: u64 = 1 << 24;

/// The most tree bytes one directory may have on the wire: the sum of the
/// decoded lengths of the tree blobs it is made of. Writer, reader and check
/// use the same number.
pub const MAX_DIR_TREE_BYTES: u64 = 1 << 30;

/// The most tree bytes one operation holds at once, summed over the
/// directories it has open, counted like [`MAX_DIR_TREE_BYTES`].
pub const MAX_HELD_TREE_BYTES: u64 = 2 << 30;

/// What is left of the node and byte allowance of one operation on a snapshot.
#[derive(Debug, Clone, Copy)]
pub struct TreeBudget {
    dir: u64,
    held: u64,
    dir_bytes: u64,
    held_bytes: u64,
    nodes: u64,
    bytes: u64,
    nodes_left: u64,
    bytes_left: u64,
}

impl Default for TreeBudget {
    fn default() -> Self {
        Self::new(MAX_WALK_NODES, MAX_WALK_TREE_BYTES)
    }
}

impl TreeBudget {
    pub fn new(nodes: u64, bytes: u64) -> Self {
        TreeBudget {
            dir: MAX_DIR_ENTRIES,
            held: MAX_HELD_ENTRIES,
            dir_bytes: MAX_DIR_TREE_BYTES,
            held_bytes: MAX_HELD_TREE_BYTES,
            nodes,
            bytes,
            nodes_left: nodes,
            bytes_left: bytes,
        }
    }

    /// Replaces the per-directory and held-at-once limits.
    pub fn with_dir_limits(mut self, dir: u64, held: u64) -> Self {
        self.dir = dir;
        self.held = held;
        self
    }

    /// Replaces the per-directory and held-at-once limits on tree bytes.
    pub fn with_dir_byte_limits(mut self, dir_bytes: u64, held_bytes: u64) -> Self {
        self.dir_bytes = dir_bytes;
        self.held_bytes = held_bytes;
        self
    }

    /// Refuses a directory that would be `n` tree bytes.
    pub fn check_dir_bytes(&self, n: u64) -> Result<()> {
        if n > self.dir_bytes {
            return Err(Error::WalkLimit {
                what: "bytes in one directory",
                limit: self.dir_bytes,
            });
        }
        Ok(())
    }

    /// Refuses an operation that would hold `n` tree bytes at once.
    pub fn check_held_bytes(&self, n: u64) -> Result<()> {
        if n > self.held_bytes {
            return Err(Error::WalkLimit {
                what: "bytes held at once",
                limit: self.held_bytes,
            });
        }
        Ok(())
    }

    /// Refuses a walk that has visited `nodes` entries and `bytes` of
    /// directory data, with the error [`TreeBudget::charge_nodes`] and
    /// [`TreeBudget::charge_bytes`] give.
    pub fn check_walk(&self, nodes: u64, bytes: u64) -> Result<()> {
        if nodes > self.nodes {
            return Err(Error::WalkLimit {
                what: "entries",
                limit: self.nodes,
            });
        }
        if bytes > self.bytes {
            return Err(Error::WalkLimit {
                what: "bytes of directory data",
                limit: self.bytes,
            });
        }
        Ok(())
    }

    /// Refuses a directory that would have `n` entries.
    pub fn check_dir(&self, n: u64) -> Result<()> {
        if n > self.dir {
            return Err(Error::WalkLimit {
                what: "entries in one directory",
                limit: self.dir,
            });
        }
        Ok(())
    }

    /// Refuses an operation that would hold `n` entries at once.
    pub fn check_held(&self, n: u64) -> Result<()> {
        if n > self.held {
            return Err(Error::WalkLimit {
                what: "entries held at once",
                limit: self.held,
            });
        }
        Ok(())
    }

    /// Starts a new operation with the full allowance.
    pub fn reset(&mut self) {
        self.nodes_left = self.nodes;
        self.bytes_left = self.bytes;
    }

    /// Charges `n` bytes; a charge that does not fit spends the rest, so the
    /// bytes stay spent for an operation that goes on after the error.
    pub fn charge_bytes(&mut self, n: u64) -> Result<()> {
        let Some(left) = self.bytes_left.checked_sub(n) else {
            self.bytes_left = 0;
            return Err(self.bytes_spent());
        };
        self.bytes_left = left;
        Ok(())
    }

    /// Charges `n` entries; a charge that does not fit spends the rest.
    pub fn charge_nodes(&mut self, n: u64) -> Result<()> {
        let Some(left) = self.nodes_left.checked_sub(n) else {
            self.nodes_left = 0;
            return Err(Error::WalkLimit {
                what: "entries",
                limit: self.nodes,
            });
        };
        self.nodes_left = left;
        Ok(())
    }

    /// Refuses work once the bytes are spent, before anything is read for it.
    pub fn check_bytes_left(&self) -> Result<()> {
        if self.bytes_left == 0 {
            return Err(self.bytes_spent());
        }
        Ok(())
    }

    fn bytes_spent(&self) -> Error {
        Error::WalkLimit {
            what: "bytes of directory data",
            limit: self.bytes,
        }
    }
}

/// Longest node name in bytes; above the `NAME_MAX` of common Linux
/// filesystems, so snapshots from other systems stay readable.
pub const MAX_NAME_LEN: usize = 1024;

/// A tree blob is cut once its serialized nodes pass this size.
pub const TREE_SPLIT_SIZE: usize = 1 << 20;

pub fn encode_tree(nodes: Vec<pb::Node>) -> Vec<u8> {
    pb::Tree { nodes }.encode_to_vec()
}

/// Cuts a sorted run of nodes into pieces of about [`TREE_SPLIT_SIZE`].
pub fn split_nodes(nodes: Vec<pb::Node>) -> Vec<Vec<pb::Node>> {
    let mut pieces = Vec::new();
    let mut current = Vec::new();
    let mut size = 0usize;
    for node in nodes {
        size += node.encoded_len() + 8;
        current.push(node);
        if size >= TREE_SPLIT_SIZE {
            pieces.push(std::mem::take(&mut current));
            size = 0;
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

pub fn validate_name(name: &[u8]) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return Err(Error::corrupt("tree", "invalid node name"));
    }
    Ok(())
}

fn check_ids(ids: &[Vec<u8>], what: &str) -> Result<()> {
    if ids.iter().any(|id| id.len() != 32) {
        return Err(Error::corrupt(
            "tree",
            format!("{what} id must be 32 bytes"),
        ));
    }
    Ok(())
}

#[derive(Clone, PartialEq, prost::Message)]
struct NodeCount {}

/// `Tree` with its nodes left undecoded, for counting them.
#[derive(Clone, PartialEq, prost::Message)]
struct TreeCount {
    #[prost(message, repeated, tag = "1")]
    nodes: Vec<NodeCount>,
}

/// The number of nodes in a tree blob, counted before the blob becomes a
/// `Vec<pb::Node>`, so a blob that would overrun a limit is never decoded.
/// Fails like [`decode_tree`] on bytes that are not a tree.
pub(crate) fn count_tree_nodes(bytes: &[u8]) -> Result<u64> {
    let tree = TreeCount::decode(bytes)
        .map_err(|e| Error::corrupt("tree", format!("undecodable: {e}")))?;
    Ok(tree.nodes.len() as u64)
}

/// Decodes one tree blob and validates it. `previous` is the last name of the
/// piece before this one in the same directory, so ordering is checked across
/// the cut as well as inside it.
pub fn decode_tree(bytes: &[u8], previous: Option<&[u8]>) -> Result<Vec<pb::Node>> {
    let tree =
        pb::Tree::decode(bytes).map_err(|e| Error::corrupt("tree", format!("undecodable: {e}")))?;
    let mut last = previous;
    for node in &tree.nodes {
        validate_name(&node.name)?;
        if last.is_some_and(|l| l >= node.name.as_slice()) {
            return Err(Error::corrupt("tree", "names are not strictly ascending"));
        }
        last = Some(&node.name);

        let ty = pb::NodeType::try_from(node.r#type)
            .map_err(|_| Error::corrupt("tree", "unknown node type"))?;
        if ty == pb::NodeType::Unspecified {
            return Err(Error::corrupt("tree", "node type is missing"));
        }
        check_ids(&node.content, "content")?;
        check_ids(&node.subtrees, "subtree")?;
        if (ty != pb::NodeType::File
            && (!node.content.is_empty()
                || !node.sparse_extents.is_empty()
                || node.hardlink_group != 0))
            || (ty != pb::NodeType::Dir && !node.subtrees.is_empty())
            || (ty == pb::NodeType::Symlink && node.link_target.is_empty())
        {
            return Err(Error::corrupt(
                "tree",
                "node carries data its type cannot have",
            ));
        }
    }
    Ok(tree.nodes)
}

/// What a [`TreeCursor`] hands out.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Step {
    /// The next node of the directory being read; a directory is entered with
    /// [`TreeCursor::descend`], or skipped by not calling it.
    Node(pb::Node),
    /// The directory entered last has no more nodes.
    Leave,
}

struct Level {
    nodes: std::vec::IntoIter<pb::Node>,
    /// Entries the directory was read with; its buffer holds them all until
    /// the level is left, whatever has been handed out.
    len: u64,
    /// Tree bytes the directory was read with, held until the level is left.
    /// A directory node kept on the walk's frames, with its `subtrees` list,
    /// is an entry of the level above and counted through that level.
    bytes: u64,
    /// Length of the cursor path before this level was entered.
    path_len: usize,
}

/// Depth-first walk over a snapshot tree with an explicit stack, so a deep
/// tree cannot overflow the call stack and the depth limit is one comparison.
/// Directories are read only when the caller asks to descend.
pub(crate) struct TreeCursor {
    levels: Vec<Level>,
    /// Absolute path of the directory being read; empty at the top.
    path: Vec<u8>,
}

impl TreeCursor {
    /// `nodes` are the entries of the directory at `base` (an absolute path,
    /// empty for the top of the snapshot).
    pub(crate) fn new(nodes: Vec<pb::Node>, bytes: u64, base: Vec<u8>) -> Self {
        TreeCursor {
            levels: vec![Level {
                len: nodes.len() as u64,
                bytes,
                nodes: nodes.into_iter(),
                path_len: 0,
            }],
            path: base,
        }
    }

    pub(crate) fn next(&mut self) -> Option<Step> {
        let top = self.levels.last_mut()?;
        if let Some(node) = top.nodes.next() {
            return Some(Step::Node(node));
        }
        if self.levels.len() == 1 {
            return None;
        }
        let done = self.levels.pop()?;
        self.path.truncate(done.path_len);
        Some(Step::Leave)
    }

    /// Absolute path of an entry of the directory being read.
    pub(crate) fn path_of(&self, name: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(self.path.len() + 1 + name.len());
        p.extend_from_slice(&self.path);
        p.push(b'/');
        p.extend_from_slice(name);
        p
    }

    /// Reads the entries of `dir` and makes them the ones [`next`] returns.
    pub(crate) async fn descend<S: BlobStore>(
        &mut self,
        reader: &mut Reader<S>,
        dir: &pb::Node,
    ) -> Result<()> {
        if self.levels.len() > MAX_DEPTH {
            return Err(Error::corrupt("tree", "directories are nested too deeply"));
        }
        let (above_entries, above_bytes) = self.levels.iter().fold((0u64, 0u64), |(e, b), l| {
            (e.saturating_add(l.len), b.saturating_add(l.bytes))
        });
        let (nodes, bytes) = reader
            .load_tree(&dir.subtrees, above_entries, above_bytes)
            .await?;
        let len = nodes.len() as u64;
        let path_len = self.path.len();
        self.path = self.path_of(&dir.name);
        self.levels.push(Level {
            len,
            bytes,
            nodes: nodes.into_iter(),
            path_len,
        });
        Ok(())
    }
}

/// What a [`SelectedWalk`] hands out.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Action {
    /// A directory to create or enter; everything until the matching
    /// [`Action::Leave`] belongs to it. `ancestor` marks a directory that is
    /// only on the way to something selected.
    Enter {
        node: pb::Node,
        ancestor: bool,
    },
    Leave,
    /// A node that is not a directory.
    Entry(pb::Node),
}

/// What a [`SelectedWalk`] asks of whoever decides what is selected.
pub(crate) trait Selector {
    /// Everything is selected, whatever the path.
    fn all(&self) -> bool;
    fn verdict(&self, path: &[u8], is_dir: bool) -> Verdict;
}

/// `--include` patterns: a match brings its subtree, and every directory is
/// looked into for further matches.
impl Selector for Option<&PathMatcher> {
    fn all(&self) -> bool {
        self.is_none()
    }

    fn verdict(&self, path: &[u8], is_dir: bool) -> Verdict {
        if self.is_some_and(|m| m.is_match(path, is_dir)) {
            Verdict::TakeAll
        } else {
            Verdict::Enter
        }
    }
}

impl Selector for &Selection {
    fn all(&self) -> bool {
        self.is_all()
    }

    fn verdict(&self, path: &[u8], _: bool) -> Verdict {
        Selection::verdict(self, path)
    }
}

struct Frame {
    node: pb::Node,
    /// Whether `Enter` has been handed out for it.
    entered: bool,
    /// Everything below is selected.
    all: bool,
}

/// A [`TreeCursor`] that keeps only what a restore asked for. A node is
/// selected when its path is (a directory then brings its whole subtree) and
/// the directories above a selected node are entered on the way; directories
/// with nothing selected below are never handed out. Directories the selector
/// rules out are not read from the store.
pub(crate) struct SelectedWalk<X: Selector> {
    cursor: TreeCursor,
    selector: X,
    frames: Vec<Frame>,
    queue: VecDeque<Action>,
}

impl<X: Selector> SelectedWalk<X> {
    pub(crate) fn new(cursor: TreeCursor, selector: X) -> Self {
        SelectedWalk {
            cursor,
            selector,
            frames: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    /// Absolute path of an entry of the directory last handed out, valid for
    /// an `Entry` until the next call to [`SelectedWalk::next`].
    pub(crate) fn path_of(&self, name: &[u8]) -> Vec<u8> {
        self.cursor.path_of(name)
    }

    /// Hands out the pending `Enter` of every directory above a selected node.
    fn enter_ancestors(&mut self) {
        for frame in &mut self.frames {
            if !frame.entered {
                frame.entered = true;
                self.queue.push_back(Action::Enter {
                    node: frame.node.clone(),
                    ancestor: true,
                });
            }
        }
    }

    pub(crate) async fn next<S: BlobStore>(
        &mut self,
        reader: &mut Reader<S>,
    ) -> Result<Option<Action>> {
        loop {
            if let Some(action) = self.queue.pop_front() {
                return Ok(Some(action));
            }
            let Some(step) = self.cursor.next() else {
                return Ok(None);
            };
            let node = match step {
                Step::Leave => {
                    if self.frames.pop().is_some_and(|f| f.entered) {
                        return Ok(Some(Action::Leave));
                    }
                    continue;
                }
                Step::Node(node) => node,
            };
            let is_dir = node.r#type == pb::NodeType::Dir as i32;
            let inherited = self
                .frames
                .last()
                .map_or_else(|| self.selector.all(), |f| f.all);
            let verdict = if inherited {
                Verdict::TakeAll
            } else {
                self.selector
                    .verdict(&self.cursor.path_of(&node.name), is_dir)
            };
            let selected = verdict == Verdict::TakeAll;
            if !is_dir {
                if selected {
                    self.enter_ancestors();
                    self.queue.push_back(Action::Entry(node));
                }
                continue;
            }
            if verdict == Verdict::Skip {
                continue;
            }
            if selected {
                self.enter_ancestors();
                self.queue.push_back(Action::Enter {
                    node: node.clone(),
                    ancestor: false,
                });
            }
            self.cursor.descend(reader, &node).await?;
            self.frames.push(Frame {
                node,
                entered: selected,
                all: selected,
            });
        }
    }
}

/// One step of a restore plan: the entries of a [`Selection`] in tree order.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum PlanStep {
    /// A directory to enter; everything until the matching [`PlanStep::Leave`]
    /// belongs to it. An `ancestor` is only on the way to something selected,
    /// and a restore in place must leave its metadata and contents alone.
    Enter {
        node: pb::Node,
        ancestor: bool,
    },
    /// A node that is not a directory.
    Entry(pb::Node),
    Leave,
}

/// Walks the selected part of a snapshot. Only selected directories and the
/// ones leading to them are read; the trees of their unselected siblings are
/// never fetched.
pub struct SelectionWalk<'s> {
    walk: SelectedWalk<&'s Selection>,
}

impl<'s> SelectionWalk<'s> {
    pub async fn start<S: BlobStore>(
        reader: &mut Reader<S>,
        snapshot_id: &BlobId,
        selection: &'s Selection,
    ) -> Result<Self> {
        let (_, top, bytes) = reader.open_root(snapshot_id).await?;
        Ok(SelectionWalk {
            walk: SelectedWalk::new(TreeCursor::new(top, bytes, Vec::new()), selection),
        })
    }

    pub async fn next<S: BlobStore>(&mut self, reader: &mut Reader<S>) -> Result<Option<PlanStep>> {
        Ok(self.walk.next(reader).await?.map(|action| match action {
            Action::Enter { node, ancestor } => PlanStep::Enter { node, ancestor },
            Action::Entry(node) => PlanStep::Entry(node),
            Action::Leave => PlanStep::Leave,
        }))
    }

    /// Absolute path of an entry of the directory last handed out.
    pub fn path_of(&self, name: &[u8]) -> Vec<u8> {
        self.walk.path_of(name)
    }
}

/// Checks one node of a restore the way [`validate_subtree`] does: the chunks
/// of a file add up to its size, and the members of a hardlink group agree.
pub(crate) async fn check_node<S: BlobStore>(
    reader: &mut Reader<S>,
    groups: &mut HashMap<u64, (u64, [u8; 32])>,
    node: &pb::Node,
) -> Result<()> {
    if node.r#type != pb::NodeType::File as i32 {
        return Ok(());
    }
    reader.check_file(node).await?;
    if node.hardlink_group == 0 {
        return Ok(());
    }
    let member = (node.size, *blake3::hash(&node.content.concat()).as_bytes());
    if *groups.entry(node.hardlink_group).or_insert(member) != member {
        return Err(Error::corrupt(
            String::from_utf8_lossy(&node.name),
            "members of one hardlink group differ in size or content",
        ));
    }
    Ok(())
}

/// Reads every tree of the selected part of a snapshot, before anything is
/// written, and checks what a single tree cannot show: the depth, that the
/// chunks of each file add up to its size, and that the members of a
/// hardlink group agree. Name rules and ordering (also across the pieces of a
/// split directory) are checked as each tree is decoded.
pub(crate) async fn validate_subtree<S: BlobStore>(
    reader: &mut Reader<S>,
    nodes: Vec<pb::Node>,
    bytes: u64,
    base: Vec<u8>,
    matcher: Option<&PathMatcher>,
) -> Result<()> {
    let mut walk = SelectedWalk::new(TreeCursor::new(nodes, bytes, base), matcher);
    let mut groups = HashMap::new();
    while let Some(action) = walk.next(reader).await? {
        let (Action::Enter { node, .. } | Action::Entry(node)) = action else {
            continue;
        };
        check_node(reader, &mut groups, &node).await?;
    }
    Ok(())
}

/// What a [`Selection`] would restore, with every tree and file it touches
/// checked as [`validate_subtree`] does.
pub(crate) async fn selection_totals<S: BlobStore>(
    reader: &mut Reader<S>,
    snapshot_id: &BlobId,
    selection: &Selection,
) -> Result<SelectionTotals> {
    let mut walk = SelectionWalk::start(reader, snapshot_id, selection).await?;
    let mut groups = HashMap::new();
    let mut totals = SelectionTotals::default();
    while let Some(step) = walk.next(reader).await? {
        match step {
            PlanStep::Enter { ancestor: true, .. } | PlanStep::Leave => {}
            PlanStep::Enter { .. } => totals.dirs += 1,
            PlanStep::Entry(node) => {
                check_node(reader, &mut groups, &node).await?;
                if node.r#type == pb::NodeType::File as i32 {
                    totals.files += 1;
                    totals.bytes = totals.bytes.saturating_add(node.size);
                }
            }
        }
    }
    Ok(totals)
}

/// Entry points for fuzz harnesses; not part of the format API.
#[cfg(feature = "test-hooks")]
pub mod hooks {
    use super::{Result, Step, TreeCursor, count_tree_nodes, pb};

    pub fn count_nodes(bytes: &[u8]) -> Result<u64> {
        count_tree_nodes(bytes)
    }

    pub fn cursor_paths(nodes: Vec<pb::Node>, bytes: u64, base: Vec<u8>) -> Vec<Vec<u8>> {
        let mut cursor = TreeCursor::new(nodes, bytes, base);
        let mut paths = Vec::new();
        while let Some(step) = cursor.next() {
            if let Step::Node(node) = step {
                paths.push(cursor.path_of(&node.name));
            }
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::{Rng, SeedableRng};

    use super::*;

    fn node(name: &[u8]) -> pb::Node {
        pb::Node {
            name: name.to_vec(),
            r#type: pb::NodeType::File as i32,
            ..Default::default()
        }
    }

    fn rejects(nodes: Vec<pb::Node>, previous: Option<&[u8]>) {
        let err = decode_tree(&encode_tree(nodes), previous).unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
    }

    #[test]
    fn tree_rejects_bad_names() {
        let ok = decode_tree(&encode_tree(vec![node(b"a"), node(b"b")]), None).unwrap();
        assert_eq!(ok.len(), 2);

        for bad in [&b"."[..], b"..", b"", b"a/b", b"a\0b", b"/"] {
            rejects(vec![node(bad)], None);
        }
        rejects(vec![node(b"a"), node(b"a")], None);
        rejects(vec![node(b"b"), node(b"a")], None);

        // order across the cut between two pieces of one directory
        assert!(decode_tree(&encode_tree(vec![node(b"c")]), Some(b"b")).is_ok());
        rejects(vec![node(b"b")], Some(b"b"));
        rejects(vec![node(b"a"), node(b"z")], Some(b"m"));

        rejects(vec![node(&[b'a'; MAX_NAME_LEN + 1])], None);
        assert!(decode_tree(&encode_tree(vec![node(&[b'a'; MAX_NAME_LEN])]), None).is_ok());
        let mut empty_link = node(b"x");
        empty_link.r#type = pb::NodeType::Symlink as i32;
        rejects(vec![empty_link.clone()], None);
        empty_link.link_target = b"t".to_vec();
        assert!(decode_tree(&encode_tree(vec![empty_link]), None).is_ok());
        let mut dir_with_extents = node(b"x");
        dir_with_extents.r#type = pb::NodeType::Dir as i32;
        dir_with_extents.sparse_extents = vec![pb::Extent {
            offset: 0,
            length: 1,
        }];
        rejects(vec![dir_with_extents], None);
        let mut link_in_dir = node(b"x");
        link_in_dir.r#type = pb::NodeType::Fifo as i32;
        link_in_dir.hardlink_group = 3;
        rejects(vec![link_in_dir], None);

        let mut unspecified = node(b"x");
        unspecified.r#type = 0;
        rejects(vec![unspecified], None);
        let mut short_id = node(b"x");
        short_id.content = vec![vec![1; 5]];
        rejects(vec![short_id], None);
        let mut dir_with_content = node(b"x");
        dir_with_content.r#type = pb::NodeType::Dir as i32;
        dir_with_content.content = vec![vec![1; 32]];
        rejects(vec![dir_with_content], None);
    }

    #[test]
    fn split_keeps_order_and_size_bound() {
        let nodes: Vec<pb::Node> = (0u32..100_000)
            .map(|i| node(format!("file-{i:08}").as_bytes()))
            .collect();
        let total = nodes.len();
        let pieces = split_nodes(nodes);
        assert!(pieces.len() > 1);
        assert_eq!(pieces.iter().map(Vec::len).sum::<usize>(), total);

        let mut previous: Option<Vec<u8>> = None;
        for piece in pieces {
            let bytes = encode_tree(piece);
            assert!(bytes.len() < 2 * TREE_SPLIT_SIZE);
            let decoded = decode_tree(&bytes, previous.as_deref()).unwrap();
            previous = decoded.last().map(|n| n.name.clone());
        }
    }

    fn generated_tree(rng: &mut ChaCha20Rng) -> Vec<pb::Node> {
        let below = |rng: &mut ChaCha20Rng, n: u32| rng.next_u32() % n;
        (0..below(rng, 50))
            .map(|i| {
                let mut node = node(format!("n{i:03}").as_bytes());
                match below(rng, 3) {
                    0 => {
                        node.content = (0..below(rng, 5)).map(|_| vec![7u8; 32]).collect();
                    }
                    1 => {
                        node.r#type = pb::NodeType::Dir as i32;
                        node.subtrees = (0..below(rng, 3)).map(|_| vec![9u8; 32]).collect();
                    }
                    _ => {
                        node.r#type = pb::NodeType::Symlink as i32;
                        node.link_target = b"t".to_vec();
                    }
                }
                node
            })
            .collect()
    }

    #[test]
    fn count_tree_nodes_agrees_with_decode() {
        let mut rng = ChaCha20Rng::from_seed([31; 32]);
        for _ in 0..200 {
            let bytes = encode_tree(generated_tree(&mut rng));
            let decoded = decode_tree(&bytes, None).unwrap();
            assert_eq!(count_tree_nodes(&bytes).unwrap(), decoded.len() as u64);
        }
    }

    #[test]
    fn count_tree_nodes_survives_hostile_bytes() {
        let mut rng = ChaCha20Rng::from_seed([32; 32]);
        for _ in 0..2000 {
            let valid = encode_tree(generated_tree(&mut rng));
            let mut hostile = valid.clone();
            match rng.next_u32() % 4 {
                0 if !hostile.is_empty() => {
                    let at = rng.next_u64() as usize % hostile.len();
                    hostile[at] ^= 1 << (rng.next_u32() % 8);
                }
                1 if !hostile.is_empty() => {
                    hostile.truncate(rng.next_u64() as usize % hostile.len());
                }
                2 => {
                    let mut extra = vec![0u8; 1 + rng.next_u32() as usize % 64];
                    rng.fill_bytes(&mut extra);
                    hostile.extend(extra);
                }
                _ => {
                    hostile = vec![0u8; rng.next_u32() as usize % 300];
                    rng.fill_bytes(&mut hostile);
                }
            }
            match count_tree_nodes(&hostile) {
                Ok(n) => {
                    if let Ok(nodes) = decode_tree(&hostile, None) {
                        assert_eq!(n, nodes.len() as u64);
                    }
                }
                Err(e) => assert!(matches!(e, Error::Corrupt { .. }), "{e:?}"),
            }
        }
    }
}
