use super::RepoLocation;
use crate::error::{Error, Result};

const SECRETS_HINT: &str = "put secrets in the environment, not in the repository URL";

fn invalid(url: &str, why: &str) -> Error {
    // The URL is not echoed: a mistake in it may sit next to a secret.
    let _ = url;
    Error::InvalidInput(format!("invalid repository URL: {why}"))
}

/// The part of `url` after `scheme://`, refused when it carries a query, a
/// fragment or a control character.
fn after_scheme<'a>(url: &'a str, scheme: &str) -> Result<&'a str> {
    let rest = url
        .strip_prefix(scheme)
        .and_then(|r| r.strip_prefix("://"))
        .ok_or_else(|| invalid(url, "unsupported scheme"))?;
    if rest.contains(['?', '#']) {
        return Err(invalid(url, SECRETS_HINT));
    }
    if rest.chars().any(char::is_control) {
        return Err(invalid(url, "control character in the URL"));
    }
    Ok(rest)
}

/// A container or bucket name: one non-empty segment that cannot be a
/// `user:secret@` prefix.
fn name<'a>(url: &str, name: &'a str, what: &str) -> Result<&'a str> {
    if name.is_empty() {
        return Err(invalid(url, &format!("the {what} is empty")));
    }
    if name.contains(['@', ':']) {
        return Err(invalid(url, SECRETS_HINT));
    }
    Ok(name)
}

/// A key prefix inside a container: no empty, `.` or `..` segments. One
/// trailing slash is allowed.
fn root<'a>(url: &str, path: &'a str) -> Result<&'a str> {
    let path = path.strip_suffix('/').unwrap_or(path);
    if !path.is_empty() && path.split('/').any(|c| matches!(c, "" | "." | "..")) {
        return Err(invalid(url, "the path has an empty, `.` or `..` segment"));
    }
    Ok(path)
}

fn bucket_location(url: &str, backend: &str, key: &str) -> Result<RepoLocation> {
    let rest = after_scheme(url, backend)?;
    let (bucket, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut options = vec![(key, name(url, bucket, key)?)];
    let path = root(url, path)?;
    if !path.is_empty() {
        options.push(("root", path));
    }
    Ok(RepoLocation::new(backend, options))
}

impl RepoLocation {
    /// Parses `azblob://<container>[/<root>]`, `gcs://<bucket>[/<root>]` and
    /// `sftp://<user>@<host>[:<port>][/<path>]`. A URL carries no secrets: a
    /// query, a fragment or `user:secret@` is refused. The SFTP key and host
    /// key, like every other credential, are added to the options by the caller.
    pub fn from_url(url: &str) -> Result<Self> {
        let scheme = url.split_once("://").map_or("", |(s, _)| s);
        match scheme {
            "azblob" => bucket_location(url, "azblob", "container"),
            "gcs" => bucket_location(url, "gcs", "bucket"),
            "sftp" => sftp_location(url),
            _ => Err(invalid(url, "unsupported scheme")),
        }
    }
}

fn sftp_location(url: &str) -> Result<RepoLocation> {
    let rest = after_scheme(url, "sftp")?;
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let (user, endpoint) = authority
        .split_once('@')
        .ok_or_else(|| invalid(url, "an sftp URL needs a user: sftp://<user>@<host>"))?;
    if user.is_empty() || endpoint.is_empty() {
        return Err(invalid(url, "an sftp URL needs a user and a host"));
    }
    if user.contains(':') {
        return Err(invalid(url, SECRETS_HINT));
    }
    if endpoint.contains('@') || endpoint.starts_with(':') {
        return Err(invalid(url, "the sftp host is malformed"));
    }
    if path.split('/').any(|c| c == "..") {
        return Err(invalid(url, "the path has a `..` segment"));
    }
    let mut options = vec![("user", user), ("endpoint", endpoint)];
    if !path.is_empty() {
        options.push(("root", path));
    }
    Ok(RepoLocation::new("sftp", options))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn opts(loc: &RepoLocation) -> Vec<(&str, &str)> {
        loc.options
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn from_url_azblob_and_gcs_keep_the_root() {
        let loc = RepoLocation::from_url("azblob://c/r/x").unwrap();
        assert_eq!(loc.backend, "azblob");
        assert_eq!(opts(&loc), [("container", "c"), ("root", "r/x")]);
        assert_eq!(loc.display(), "azblob://c/r/x");

        let loc = RepoLocation::from_url("gcs://b/r").unwrap();
        assert_eq!(opts(&loc), [("bucket", "b"), ("root", "r")]);
        assert_eq!(loc.display(), "gcs://b/r");

        for bare in ["azblob://c", "azblob://c/", "gcs://b/"] {
            let loc = RepoLocation::from_url(bare).unwrap();
            assert_eq!(loc.options.len(), 1, "{bare}");
        }
    }

    #[test]
    fn from_url_sftp_takes_user_endpoint_and_path_as_is() {
        let loc = RepoLocation::from_url("sftp://lode@backup.example:2222/srv/repo").unwrap();
        assert_eq!(loc.backend, "sftp");
        assert_eq!(
            opts(&loc),
            [
                ("endpoint", "backup.example:2222"),
                ("root", "/srv/repo"),
                ("user", "lode")
            ]
        );
        assert_eq!(loc.display(), "sftp://lode@backup.example:2222/srv/repo");

        let loc = RepoLocation::from_url("sftp://u@h").unwrap();
        assert_eq!(opts(&loc), [("endpoint", "h"), ("user", "u")]);
        let loc = RepoLocation::from_url("sftp://u@[::1]:22/data").unwrap();
        assert_eq!(loc.options["endpoint"], "[::1]:22");
    }

    #[test]
    fn from_url_refuses_what_it_cannot_take() {
        let bad = [
            "azblob://",
            "azblob:///r",
            "gcs://",
            "azblob://c//x",
            "azblob://c/../x",
            "azblob://c/./x",
            "gcs://b/r/../x",
            "azblob://c?sas=sig",
            "gcs://b/r?token=x",
            "azblob://c#frag",
            "azblob://acct:key@c",
            "gcs://user@b",
            "azblob://c/r\0x",
            "sftp://host",
            "sftp://@host/x",
            "sftp://u@",
            "sftp://u@/x",
            "sftp://u:secret@host/x",
            "sftp://u@h/a/../b",
            "sftp://u@h/x\0",
            "sftp://u@h/x?key=1",
            "sftp://u@:22/x",
            "sftp://u@a@b/x",
            "s3://bucket",
            "/local/path",
            "",
        ];
        for url in bad {
            let err = RepoLocation::from_url(url).unwrap_err();
            assert!(matches!(err, Error::InvalidInput(_)), "{url:?}: {err:?}");
        }
    }

    #[test]
    fn from_url_errors_do_not_echo_the_url() {
        let err = RepoLocation::from_url("sftp://u:hunter2@h/x").unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
        let err = RepoLocation::from_url("azblob://c?sas=hunter2").unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    fn segment() -> impl Strategy<Value = String> {
        "[A-Za-z0-9][A-Za-z0-9._-]{0,11}"
            .prop_filter("not a dot segment", |s| s != "." && s != "..")
    }

    fn root_option() -> impl Strategy<Value = Option<String>> {
        proptest::option::of(proptest::collection::vec(segment(), 1..4).prop_map(|v| v.join("/")))
    }

    proptest! {
        #[test]
        fn from_url_round_trips_the_display_of_a_location(
            backend in prop_oneof![Just(("azblob", "container")), Just(("gcs", "bucket"))],
            name in segment(),
            root in root_option(),
        ) {
            let (backend, key) = backend;
            let mut options = vec![(key, name.as_str())];
            if let Some(root) = &root {
                options.push(("root", root.as_str()));
            }
            let loc = RepoLocation::new(backend, options);
            let parsed = RepoLocation::from_url(&loc.display()).unwrap();
            prop_assert_eq!(parsed, loc);
        }

        #[test]
        fn from_url_round_trips_an_sftp_location(
            user in segment(),
            host in "[a-z][a-z0-9.-]{0,15}",
            port in proptest::option::of(1u16..),
            root in root_option(),
        ) {
            let endpoint = port.map_or(host.clone(), |p| format!("{host}:{p}"));
            let mut options = vec![("user", user.as_str()), ("endpoint", endpoint.as_str())];
            let absolute = root.as_ref().map(|r| format!("/{r}"));
            if let Some(root) = &absolute {
                options.push(("root", root.as_str()));
            }
            let loc = RepoLocation::new("sftp", options);
            let parsed = RepoLocation::from_url(&loc.display()).unwrap();
            prop_assert_eq!(parsed, loc);
        }

        #[test]
        fn from_url_never_panics(url in "\\PC{0,40}") {
            let _ = RepoLocation::from_url(&url);
        }
    }
}
