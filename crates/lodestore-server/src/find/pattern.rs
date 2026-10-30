use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};

pub const PATTERN_CHARS: usize = 255;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PatternError(pub String);

pub enum Pattern {
    Substring(String),
    Name(GlobMatcher),
    Path(GlobMatcher),
}

fn basename(path: &[u8]) -> &[u8] {
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

fn glob(text: &str) -> Result<GlobMatcher, PatternError> {
    GlobBuilder::new(text)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|e| PatternError(format!("The pattern is not valid: {}.", e.kind())))
}

impl Pattern {
    /// Text with a `/` is a glob for the whole path from the top of the snapshot, text with
    /// `* ? [ {` a glob for names, anything else a fragment of a name.
    pub fn compile(text: &str) -> Result<Pattern, PatternError> {
        if text.trim().is_empty() || text.contains('\0') {
            return Err(PatternError("The pattern is empty or has a NUL.".into()));
        }
        if text.chars().count() > PATTERN_CHARS {
            return Err(PatternError(format!(
                "The pattern must be 1 to {PATTERN_CHARS} characters."
            )));
        }
        if text.contains('/') {
            let rooted = if text.starts_with('/') {
                text.to_string()
            } else {
                format!("/{text}")
            };
            return glob(&rooted).map(Pattern::Path);
        }
        if text.contains(['*', '?', '[', '{']) {
            return glob(text).map(Pattern::Name);
        }
        Ok(Pattern::Substring(text.to_lowercase()))
    }

    pub fn matches(&self, path: &[u8]) -> bool {
        match self {
            Pattern::Substring(needle) => String::from_utf8_lossy(basename(path))
                .to_lowercase()
                .contains(needle.as_str()),
            Pattern::Name(glob) => glob.is_match(Path::new(OsStr::from_bytes(basename(path)))),
            Pattern::Path(glob) => glob.is_match(Path::new(OsStr::from_bytes(path))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(text: &str) -> Pattern {
        Pattern::compile(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }

    #[test]
    fn substring_matches_the_name_anywhere_without_regard_to_case() {
        let p = pattern("nginx.conf");
        assert!(p.matches(b"/etc/nginx/nginx.conf"));
        assert!(p.matches(b"/x/NGINX.CONF.bak"));
        assert!(!p.matches(b"/etc/nginx.conf.d/x"));
    }

    #[test]
    fn substring_folds_case_beyond_ascii() {
        let p = pattern("\u{41e}\u{422}\u{427}\u{401}\u{422}");
        assert!(p.matches("/d/\u{43e}\u{442}\u{447}\u{451}\u{442}.txt".as_bytes()));
    }

    #[test]
    fn name_glob_matches_the_whole_name_only() {
        let p = pattern("*.conf");
        assert!(p.matches(b"/etc/a.conf"));
        assert!(p.matches(b"/A.CONF"));
        assert!(!p.matches(b"/etc/a.confx"));
        assert!(!p.matches(b"/etc/a.conf/b"));
    }

    #[test]
    fn path_glob_keeps_single_star_inside_a_directory() {
        let p = pattern("/etc/nginx/**");
        assert!(p.matches(b"/etc/nginx/nginx.conf"));
        assert!(p.matches(b"/etc/nginx/sites/x"));
        assert!(!p.matches(b"/etc/nginx2/x"));
        let p = pattern("etc/nginx/*.conf");
        assert!(p.matches(b"/etc/nginx/a.conf"));
        assert!(!p.matches(b"/etc/nginx/sub/a.conf"));
    }

    #[test]
    fn non_utf8_names_are_matched_by_bytes() {
        assert!(pattern("*.conf").matches(b"/etc/caf\xff.conf"));
    }

    #[test]
    fn rejects_text_that_cannot_be_a_pattern() {
        for text in ["", "   ", "a\0b", "[a"] {
            assert!(Pattern::compile(text).is_err(), "{text:?}");
        }
        assert!(Pattern::compile(&"a".repeat(PATTERN_CHARS + 1)).is_err());
        assert!(Pattern::compile(&"a".repeat(PATTERN_CHARS)).is_ok());
    }
}
