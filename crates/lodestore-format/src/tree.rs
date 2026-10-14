//! Trees: the nodes of one directory, ordered by name. A directory too big
//! for one blob is split into several tree blobs listed in `Node.subtrees`,
//! which read back as one ordered run.

use std::collections::{HashMap, VecDeque};

use prost::Message;

use crate::error::{Error, Result};
use crate::fs::filter::PathMatcher;
use crate::pb;
use crate::reader::Reader;
use crate::store::BlobStore;

/// Directories nested deeper than this make a snapshot unreadable.
pub const MAX_DEPTH: usize = 4096;

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
    pub(crate) fn new(nodes: Vec<pb::Node>, base: Vec<u8>) -> Self {
        TreeCursor {
            levels: vec![Level {
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
        let nodes = reader.load_tree(&dir.subtrees).await?;
        let path_len = self.path.len();
        self.path = self.path_of(&dir.name);
        self.levels.push(Level {
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
    /// [`Action::Leave`] belongs to it.
    Enter(pb::Node),
    Leave,
    /// A node that is not a directory.
    Entry(pb::Node),
}

struct Frame {
    node: pb::Node,
    /// Whether `Enter` has been handed out for it.
    entered: bool,
    /// Everything below is selected.
    all: bool,
}

/// A [`TreeCursor`] that keeps only what a restore asked for. Without a
/// matcher that is everything. With one, a node is selected when its path
/// matches (a directory then brings its whole subtree) and the directories
/// above a selected node are entered on the way; directories with no match
/// below are never handed out.
pub(crate) struct SelectedWalk<'m> {
    cursor: TreeCursor,
    matcher: Option<&'m PathMatcher>,
    frames: Vec<Frame>,
    queue: VecDeque<Action>,
}

impl<'m> SelectedWalk<'m> {
    pub(crate) fn new(cursor: TreeCursor, matcher: Option<&'m PathMatcher>) -> Self {
        SelectedWalk {
            cursor,
            matcher,
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
                self.queue.push_back(Action::Enter(frame.node.clone()));
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
            let inherited = self.frames.last().map_or(self.matcher.is_none(), |f| f.all);
            let selected = inherited
                || self
                    .matcher
                    .is_some_and(|m| m.is_match(&self.cursor.path_of(&node.name), is_dir));
            if !is_dir {
                if selected {
                    self.enter_ancestors();
                    self.queue.push_back(Action::Entry(node));
                }
                continue;
            }
            if selected {
                self.enter_ancestors();
                self.queue.push_back(Action::Enter(node.clone()));
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

/// Reads every tree of the selected part of a snapshot, before anything is
/// written, and checks what a single tree cannot show: the depth, that the
/// chunks of each file add up to its size, and that the members of a
/// hardlink group agree. Name rules and ordering (also across the pieces of a
/// split directory) are checked as each tree is decoded.
// ponytail: the trees are read once here and again by the restore itself;
// an LRU cache of decoded trees would halve that if it ever shows in a profile.
pub(crate) async fn validate_subtree<S: BlobStore>(
    reader: &mut Reader<S>,
    nodes: Vec<pb::Node>,
    base: Vec<u8>,
    matcher: Option<&PathMatcher>,
) -> Result<()> {
    let mut walk = SelectedWalk::new(TreeCursor::new(nodes, base), matcher);
    let mut groups: HashMap<u64, (u64, [u8; 32])> = HashMap::new();
    while let Some(action) = walk.next(reader).await? {
        let (Action::Enter(node) | Action::Entry(node)) = action else {
            continue;
        };
        if node.r#type != pb::NodeType::File as i32 {
            continue;
        }
        reader.check_file(&node).await?;
        if node.hardlink_group == 0 {
            continue;
        }
        let member = (node.size, *blake3::hash(&node.content.concat()).as_bytes());
        if *groups.entry(node.hardlink_group).or_insert(member) != member {
            return Err(Error::corrupt(
                String::from_utf8_lossy(&node.name),
                "members of one hardlink group differ in size or content",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
