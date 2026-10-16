//! A set of absolute snapshot paths, compared byte for byte.
//!
//! Unlike [`crate::fs::filter::PathMatcher`] there is no pattern language: a
//! name holding `*`, `[`, `!`, `#` or a trailing space selects exactly itself.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::tree::MAX_DEPTH;

/// What a walk should do with a path, given a [`Selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing at or below this path is selected.
    Skip,
    /// The path is not selected itself, but something below it is.
    Enter,
    /// The path or one of its ancestors is selected: everything below comes along.
    TakeAll,
}

#[derive(Default)]
struct Node {
    selected: bool,
    children: BTreeMap<Vec<u8>, Node>,
}

pub struct Selection {
    root: Node,
    selected: usize,
}

impl std::fmt::Debug for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selection")
            .field("paths", &self.selected)
            .finish_non_exhaustive()
    }
}

impl Drop for Selection {
    fn drop(&mut self) {
        let mut pending = vec![std::mem::take(&mut self.root)];
        while let Some(mut node) = pending.pop() {
            pending.extend(std::mem::take(&mut node.children).into_values());
        }
    }
}

fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

impl Selection {
    /// The whole snapshot.
    pub fn all() -> Self {
        Selection {
            root: Node {
                selected: true,
                ..Node::default()
            },
            selected: 1,
        }
    }

    /// Absolute paths as bytes. `/` or an empty path selects the whole
    /// snapshot; relative paths, `.`, `..` and NUL are refused.
    pub fn from_paths<I: IntoIterator<Item = Vec<u8>>>(paths: I) -> Result<Self> {
        let mut sel = Selection {
            root: Node::default(),
            selected: 0,
        };
        for path in paths {
            let bad = || {
                Error::InvalidInput(format!(
                    "{:?} is not an absolute snapshot path",
                    String::from_utf8_lossy(&path)
                ))
            };
            if !path.is_empty() && path[0] != b'/' {
                return Err(bad());
            }
            let mut node = &mut sel.root;
            let mut depth = 0usize;
            for part in components(&path) {
                depth += 1;
                if part == b"." || part == b".." || part.contains(&0) || depth > MAX_DEPTH {
                    return Err(bad());
                }
                node = node.children.entry(part.to_vec()).or_default();
            }
            if !node.selected {
                node.selected = true;
                sel.selected += 1;
            }
        }
        Ok(sel)
    }

    pub fn is_all(&self) -> bool {
        self.root.selected
    }

    /// How many distinct paths were selected.
    pub fn len(&self) -> usize {
        self.selected
    }

    pub fn is_empty(&self) -> bool {
        self.selected == 0
    }

    /// `path` is absolute.
    pub fn verdict(&self, path: &[u8]) -> Verdict {
        let mut node = &self.root;
        if node.selected {
            return Verdict::TakeAll;
        }
        for part in components(path) {
            match node.children.get(part) {
                Some(child) if child.selected => return Verdict::TakeAll,
                Some(child) => node = child,
                None => return Verdict::Skip,
            }
        }
        if node.children.is_empty() {
            Verdict::Skip
        } else {
            Verdict::Enter
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_verdicts() {
        let sel = Selection::from_paths([b"/a/x".to_vec(), b"/c".to_vec()]).unwrap();
        assert_eq!(sel.len(), 2);
        assert_eq!(sel.verdict(b"/a"), Verdict::Enter);
        assert_eq!(sel.verdict(b"/a/x"), Verdict::TakeAll);
        assert_eq!(sel.verdict(b"/a/y"), Verdict::Skip);
        assert_eq!(sel.verdict(b"/b"), Verdict::Skip);
        assert_eq!(sel.verdict(b"/c/deep/w"), Verdict::TakeAll);
        assert!(Selection::from_paths([b"/".to_vec()]).unwrap().is_all());
        assert!(Selection::from_paths([Vec::new()]).unwrap().is_all());
        assert_eq!(Selection::all().verdict(b"/q"), Verdict::TakeAll);
        assert_eq!(
            Selection::from_paths(Vec::<Vec<u8>>::new())
                .unwrap()
                .verdict(b"/q"),
            Verdict::Skip
        );
    }

    #[test]
    fn selection_names_are_literal() {
        let sel =
            Selection::from_paths([b"/odd/!keep [1] #x ".to_vec(), b"/n/\xff*".to_vec()]).unwrap();
        assert_eq!(sel.verdict(b"/odd/!keep [1] #x "), Verdict::TakeAll);
        assert_eq!(sel.verdict(b"/odd/!keep [1] #x"), Verdict::Skip);
        assert_eq!(sel.verdict(b"/odd/other"), Verdict::Skip);
        assert_eq!(sel.verdict(b"/n/\xff*"), Verdict::TakeAll);
        assert_eq!(sel.verdict(b"/n/\xffa"), Verdict::Skip);
    }
}
