use std::path::PathBuf;

use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NbdUri {
    pub socket: PathBuf,
    pub export: String,
}

fn bad<T>(why: &str) -> Result<T> {
    Err(Error::Uri(why.to_owned()))
}

fn hex(c: u8) -> Option<u8> {
    char::from(c).to_digit(16).map(|d| d as u8)
}

fn decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&c) = bytes.get(i) {
        if c == b'%' {
            let pair = bytes
                .get(i + 1)
                .zip(bytes.get(i + 2))
                .and_then(|(h, l)| Some(hex(*h)? << 4 | hex(*l)?));
            let Some(byte) = pair else {
                return bad("a broken percent escape");
            };
            out.push(byte);
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8(out).or_else(|_| bad("not valid UTF-8"))
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl NbdUri {
    pub fn parse(uri: &str) -> Result<Self> {
        let Some(rest) = uri.strip_prefix("nbd+unix://") else {
            return bad("only nbd+unix:// URIs are accepted");
        };
        let Some(rest) = rest.strip_prefix('/') else {
            return bad("the authority must be empty");
        };
        let Some((export, query)) = rest.split_once('?') else {
            return bad("the socket parameter is missing");
        };
        if rest.contains('#') {
            return bad("a fragment is not accepted");
        }
        let export = decode(export)?;
        if export.chars().any(char::is_control) {
            return bad("the export name has control characters");
        }
        let mut socket = None;
        for pair in query.split('&') {
            let Some(("socket", value)) = pair.split_once('=') else {
                return bad("only the socket parameter is accepted");
            };
            if socket.replace(decode(value)?).is_some() {
                return bad("the socket parameter is repeated");
            }
        }
        let Some(socket) = socket else {
            return bad("the socket parameter is missing");
        };
        if !socket.starts_with('/') || socket.chars().any(char::is_control) {
            return bad("the socket must be an absolute filesystem path");
        }
        Ok(Self {
            socket: PathBuf::from(socket),
            export,
        })
    }

    pub fn canonical(&self) -> String {
        format!(
            "nbd+unix:///{}?socket={}",
            encode(&self.export),
            encode(&self.socket.to_string_lossy())
        )
    }
}

pub fn is_nbd_uri(s: &str) -> bool {
    s.starts_with("nbd") && s.contains("://")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(s: &str) -> NbdUri {
        NbdUri::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn uri_accepts_unix_exports() {
        let u = parsed("nbd+unix:///?socket=/tmp/s");
        assert_eq!((u.export.as_str(), u.socket.to_str()), ("", Some("/tmp/s")));
        let u = parsed("nbd+unix:///disk0?socket=/run/x.sock");
        assert_eq!(u.export, "disk0");
        let u = parsed("nbd+unix:///a%20b?socket=/tmp/a%20b");
        assert_eq!(u.export, "a b");
        assert_eq!(u.socket.to_str(), Some("/tmp/a b"));
    }

    #[test]
    fn uri_rejects_everything_else() {
        for s in [
            "nbd://host/x",
            "nbds://host/x",
            "nbd+unix://host/?socket=/tmp/s",
            "nbd+unix:///x",
            "nbd+unix:///x?socket=",
            "nbd+unix:///x?socket=rel/s",
            "nbd+unix:///x?socket=%00abs",
            "nbd+unix:///x?socket=@abstract",
            "nbd+unix:///x?socket=/a&socket=/b",
            "nbd+unix:///x?socket=/a&tls=on",
            "nbd+unix:///x?socket=/a%2",
            "nbd+unix:///x?socket=/a%+1",
            "nbd+unix:///x?socket=/a#frag",
            "nbd+unix:///x%00y?socket=/a",
            "nbd+unix:///x?",
            "/dev/sdb",
            "",
        ] {
            assert!(NbdUri::parse(s).is_err(), "{s:?} must be rejected");
        }
        assert!(!is_nbd_uri("/dev/sdb"));
        assert!(!is_nbd_uri("nbd"));
        assert!(is_nbd_uri("nbd+unix:///?socket=/s"));
        assert!(is_nbd_uri("nbd://host/x"));
    }

    #[test]
    fn uri_canonical_round_trips() {
        for s in [
            "nbd+unix:///?socket=/tmp/s",
            "nbd+unix:///disk0?socket=/run/x.sock",
            "nbd+unix:///a%20b?socket=/tmp/a%20b",
            "nbd+unix:///%C3%A9%2F?socket=/tmp/%C3%A9",
        ] {
            let u = parsed(s);
            assert_eq!(parsed(&u.canonical()), u, "{s}");
        }
        let a = parsed("nbd+unix:///a%20b?socket=/tmp/x").canonical();
        let b = parsed("nbd+unix:///a%20b?socket=%2Ftmp%2Fx").canonical();
        assert_eq!(a, b);
        assert_eq!(a, "nbd+unix:///a%20b?socket=/tmp/x");
    }
}
