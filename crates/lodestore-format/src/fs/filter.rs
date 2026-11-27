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
    rules: Rules,
}

#[derive(Clone, Debug)]
enum Rules {
    Gitignore {
        builder: GitignoreBuilder,
        patterns: Gitignore,
    },
    Tar(Vec<TarPattern>),
}

#[derive(Clone, Debug)]
struct TarPattern {
    /// Matched against `.` followed by the path, as tar names a member.
    glob: Vec<u8>,
    anchored: bool,
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
        Ok(PathMatcher {
            rules: Rules::Gitignore { builder, patterns },
        })
    }

    /// The excludes of a Proxmox VE container, matched as the GNU tar of
    /// vzdump matches them: a pattern that starts with `/` is anchored at the
    /// root (`--anchored --exclude=.{pattern}`), any other one matches the
    /// whole name or the part after any `/` (`--no-anchored`). `*`, `?` and
    /// `[...]` are those of `fnmatch(3)` without `FNM_PATHNAME`, so they also
    /// match `/`, and a backslash quotes the next byte. A pattern that ends
    /// with `/` matches nothing, as in tar. Only the path itself is matched:
    /// like tar, the walk does not enter an excluded directory.
    pub fn tar_excludes<I, P>(patterns: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        let rules = patterns
            .into_iter()
            .map(|p| {
                let p = p.as_ref().as_bytes();
                let anchored = p.first() == Some(&b'/');
                let glob = if anchored {
                    [b".", p].concat()
                } else {
                    p.to_vec()
                };
                TarPattern { glob, anchored }
            })
            .collect();
        PathMatcher {
            rules: Rules::Tar(rules),
        }
    }

    /// Adds the patterns of a file, one per line, after the ones so far.
    pub fn add_file(&mut self, path: &Path) -> Result<()> {
        let Rules::Gitignore { builder, patterns } = &mut self.rules else {
            return Err(Error::InvalidInput(
                "container excludes take no pattern file".into(),
            ));
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display())))?;
        for line in text.lines() {
            add_line(builder, line)?;
        }
        *patterns = build(builder)?;
        Ok(())
    }

    /// `path` is absolute; `is_dir` says whether it is a directory.
    pub fn is_match(&self, path: &[u8], is_dir: bool) -> bool {
        match &self.rules {
            Rules::Gitignore { patterns, .. } => patterns
                .matched_path_or_any_parents(Path::new(OsStr::from_bytes(path)), is_dir)
                .is_ignore(),
            Rules::Tar(rules) => {
                let name = [b".", path].concat();
                rules.iter().any(|r| r.is_match(&name))
            }
        }
    }
}

impl TarPattern {
    /// As gnulib's `exclude_fnmatch`: the whole name, then, unless anchored,
    /// the rest after each `/` that is not followed by another `/`.
    fn is_match(&self, name: &[u8]) -> bool {
        if fnmatch(&self.glob, name) {
            return true;
        }
        !self.anchored
            && name.iter().enumerate().any(|(i, &b)| {
                b == b'/'
                    && name.get(i + 1..).is_some_and(|rest| {
                        rest.first() != Some(&b'/') && fnmatch(&self.glob, rest)
                    })
            })
    }
}

/// `fnmatch(3)` without flags, on bytes: `*` and `?` match any byte, `/`
/// included, `[...]` is a bracket expression and a backslash quotes the next
/// byte. Only the last `*` is a backtrack point, which is enough when `*`
/// matches everything, and nothing recurses.
fn fnmatch(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    loop {
        let step = match pattern.get(p) {
            None if n == name.len() => return true,
            None => None,
            Some(b'*') => {
                p += 1;
                star = Some((p, n));
                continue;
            }
            Some(&c) => name.get(n).and_then(|&b| match c {
                b'?' => Some(p + 1),
                b'[' => match bracket(pattern, p + 1, b) {
                    Bracket::Matched(next) => Some(next),
                    Bracket::Missed => None,
                    Bracket::Literal => (b == b'[').then_some(p + 1),
                },
                b'\\' => pattern.get(p + 1).and_then(|&q| (q == b).then_some(p + 2)),
                _ => (c == b).then_some(p + 1),
            }),
        };
        match (step, star) {
            (Some(next), _) => {
                p = next;
                n += 1;
            }
            (None, Some((sp, sn))) if sn < name.len() => {
                star = Some((sp, sn + 1));
                p = sp;
                n = sn + 1;
            }
            (None, _) => return false,
        }
    }
}

enum Bracket {
    /// The byte is in the set; the pattern goes on at this index.
    Matched(usize),
    Missed,
    /// No closing `]`: the `[` is an ordinary byte.
    Literal,
}

/// The bracket expression whose first byte after `[` is `pattern[start]`.
fn bracket(pattern: &[u8], start: usize, byte: u8) -> Bracket {
    let mut i = start;
    let negated = matches!(pattern.get(i), Some(b'!' | b'^'));
    if negated {
        i += 1;
    }
    let mut found = false;
    let mut first = true;
    loop {
        let Some(&c) = pattern.get(i) else {
            return Bracket::Literal;
        };
        if c == b']' && !first {
            i += 1;
            break;
        }
        first = false;
        if c == b'['
            && pattern.get(i + 1) == Some(&b':')
            && let Some(names) = pattern.get(i + 2..)
            && let Some(end) = names.windows(2).position(|w| w == b":]")
        {
            found |= names.get(..end).is_some_and(|name| class(name, byte));
            i += end + 4;
            continue;
        }
        let Some((low, after)) = quoted(pattern, i) else {
            return Bracket::Literal;
        };
        match (pattern.get(after), pattern.get(after + 1)) {
            (Some(b'-'), Some(&high)) if high != b']' => {
                let Some((high, end)) = quoted(pattern, after + 1) else {
                    return Bracket::Literal;
                };
                found |= (low..=high).contains(&byte);
                i = end;
            }
            _ => {
                found |= low == byte;
                i = after;
            }
        }
    }
    if found == negated {
        Bracket::Missed
    } else {
        Bracket::Matched(i)
    }
}

/// The byte at `i` of a bracket expression and the index after it; a
/// backslash quotes the byte that follows.
fn quoted(pattern: &[u8], i: usize) -> Option<(u8, usize)> {
    match *pattern.get(i)? {
        b'\\' => pattern.get(i + 1).map(|&q| (q, i + 2)),
        c => Some((c, i + 1)),
    }
}

/// A character class of the C locale; an unknown name matches nothing.
fn class(name: &[u8], b: u8) -> bool {
    match name {
        b"alnum" => b.is_ascii_alphanumeric(),
        b"alpha" => b.is_ascii_alphabetic(),
        b"blank" => b == b' ' || b == b'\t',
        b"cntrl" => b.is_ascii_control(),
        b"digit" => b.is_ascii_digit(),
        b"graph" => b.is_ascii_graphic(),
        b"lower" => b.is_ascii_lowercase(),
        b"print" => b.is_ascii_graphic() || b == b' ',
        b"punct" => b.is_ascii_punctuation(),
        b"space" => b.is_ascii_whitespace() || b == 0x0b,
        b"upper" => b.is_ascii_uppercase(),
        b"xdigit" => b.is_ascii_hexdigit(),
        _ => false,
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

    #[test]
    fn fnmatch_matches_slash_and_brackets() {
        for (pattern, name, matched) in [
            ("*", "a/b", true),
            ("a?c", "a/c", true),
            ("*.log", "x/y.log", true),
            ("[a-c]x", "bx", true),
            ("[!a-c]x", "bx", false),
            ("[]]", "]", true),
            ("[[:digit:]]9", "19", true),
            ("[ab", "[ab", true),
            (r"\*", "*", true),
            (r"\*", "x", false),
            (r"a\", r"a\", false),
            ("*a*b", "xaxxb", true),
            ("*a*b", "xaxxc", false),
            ("", "", true),
        ] {
            assert_eq!(
                fnmatch(pattern.as_bytes(), name.as_bytes()),
                matched,
                "{pattern} {name}"
            );
        }
    }

    #[test]
    fn tar_excludes_anchor_like_vzdump() {
        let m = PathMatcher::tar_excludes(["/tmp/?*", "*.cache", "tmp2", "/srv/"]);
        for (path, excluded) in [
            ("/tmp", false),
            ("/tmp/a", true),
            ("/tmp/a/b", true),
            ("/x/tmp/a", false),
            ("/home/u/.cache", true),
            ("/home/u/b.cache", true),
            ("/tmp2", true),
            ("/var/tmp2", true),
            ("/var/tmp22", false),
            ("/srv", false),
            ("/srv/x", false),
        ] {
            assert_eq!(m.is_match(path.as_bytes(), false), excluded, "{path}");
        }
        assert!(
            PathMatcher::tar_excludes(["x"])
                .add_file(Path::new("/nonexistent"))
                .is_err()
        );
    }
}
