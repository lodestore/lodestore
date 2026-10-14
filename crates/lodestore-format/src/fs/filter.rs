//! Path exclusion with `.gitignore` semantics.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::error::{Error, Result};

/// Decides whether an absolute path is excluded. Patterns follow
/// `.gitignore` and are relative to the filesystem root `/`:
///
/// - `*.log` matches at any depth, `/srv/data/tmp` only that exact path;
/// - `build/` matches directories only;
/// - `**/node_modules` matches at any depth;
/// - `!keep.log` re-includes what an earlier pattern excluded; the last
///   matching pattern wins;
/// - blank lines and lines starting with `#` are ignored.
///
/// A path is excluded when it or any directory above it matches. This is
/// the one place the semantics live: `lode backup --exclude`, a future
/// `BackupPolicy` and `--include` all go through it.
#[derive(Clone, Debug)]
pub struct PathMatcher {
    builder: GitignoreBuilder,
    patterns: Gitignore,
}

impl PathMatcher {
    pub fn new<I, P>(patterns: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        let mut builder = GitignoreBuilder::new("/");
        for pattern in patterns {
            add_line(&mut builder, pattern.as_ref())?;
        }
        let patterns = build(&builder)?;
        Ok(PathMatcher { builder, patterns })
    }

    /// Adds the patterns of a file, one per line, after the ones so far.
    pub fn add_file(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display())))?;
        for line in text.lines() {
            add_line(&mut self.builder, line)?;
        }
        self.patterns = build(&self.builder)?;
        Ok(())
    }

    /// `path` is absolute; `is_dir` says whether it is a directory.
    pub fn is_match(&self, path: &[u8], is_dir: bool) -> bool {
        self.patterns
            .matched_path_or_any_parents(Path::new(OsStr::from_bytes(path)), is_dir)
            .is_ignore()
    }
}

fn add_line(builder: &mut GitignoreBuilder, line: &str) -> Result<()> {
    builder
        .add_line(None, line)
        .map(|_| ())
        .map_err(|e| Error::InvalidInput(format!("exclude pattern {line:?}: {e}")))
}

fn build(builder: &GitignoreBuilder) -> Result<Gitignore> {
    builder
        .build()
        .map_err(|e| Error::InvalidInput(format!("exclude patterns: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_like_gitignore_from_the_root() {
        let m = PathMatcher::new([
            "*.log",
            "!keep.log",
            "build/",
            "/srv/anchored",
            "**/node_modules",
            "# comment",
            "",
        ])
        .unwrap();
        for (path, dir, excluded) in [
            ("/a/b/x.log", false, true),
            ("/a/keep.log", false, false),
            ("/a/build", true, true),
            ("/a/build", false, false),
            ("/a/build/inner.txt", false, true),
            ("/srv/anchored", true, true),
            ("/other/srv/anchored", true, false),
            ("/p/q/node_modules/m.js", false, true),
            ("/p/q/readme", false, false),
        ] {
            assert_eq!(m.is_match(path.as_bytes(), dir), excluded, "{path}");
        }
    }
}
