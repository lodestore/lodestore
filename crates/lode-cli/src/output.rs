use std::cell::Cell;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use data_encoding::BASE64;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lodestore_format::crypto::BlobId;
use lodestore_format::index::hex32;
use lodestore_format::pb;
use lodestore_format::provenance::LatestWarning;
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

/// Version of the JSON schema; changing a field's meaning needs a new value.
pub const JSON_VERSION: u8 = 1;

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    v: u8,
    #[serde(rename = "type")]
    ty: &'a str,
    #[serde(flatten)]
    body: T,
}

fn line<T: Serialize>(ty: &str, body: T) -> String {
    let env = Envelope {
        v: JSON_VERSION,
        ty,
        body,
    };
    serde_json::to_string(&env).expect("JSON values built from plain data always serialize")
}

/// Warns on stderr about objects the library left out as unusable.
pub fn warn_skipped(skipped: &[(String, String)]) {
    for (key, reason) in skipped {
        eprintln!("warning: skipped {key}: {reason}");
    }
}

/// Explains on stderr why `latest` is `chosen`.
pub fn warn_latest(chosen: &BlobId, warnings: &[LatestWarning]) {
    let short = |id: &BlobId| hex32(id)[..8].to_owned();
    for warning in warnings {
        match warning {
            LatestWarning::Legacy => eprintln!(
                "warning: this repository has no server signatures yet (no Lodestore 0.2 server \
has opened it), so `latest` is the snapshot the storage wrote last. A host with writer \
credentials could have written it; check it with `lode snapshots` or name a snapshot by id."
            ),
            LatestWarning::FutureTime { count, newest } => eprintln!(
                "warning: {count} snapshot(s) claim a time in the future (newest {}); `latest` \
ignores the times snapshots claim.",
                short(newest)
            ),
            LatestWarning::FutureStored { count, newest } => eprintln!(
                "warning: {count} snapshot(s) were stored with a time in the future and were \
skipped (newest {}); name one by id to restore it.",
                short(newest)
            ),
            LatestWarning::NewerUnverified { count, newest } => eprintln!(
                "warning: {count} unsigned snapshot(s) were written after {} and were skipped \
(newest {}); name one by id to restore it.",
                short(chosen),
                short(newest)
            ),
            LatestWarning::UnreadableProvenance { count } => eprintln!(
                "warning: {count} provenance record(s) could not be opened and were ignored."
            ),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Output {
    pub json: bool,
}

impl Output {
    /// One JSON line on stdout.
    pub fn emit<T: Serialize>(self, ty: &str, body: T) {
        let mut out = std::io::stdout().lock();
        // A closed pipe must not turn a finished backup into a panic.
        let _ = writeln!(out, "{}", line(ty, body));
    }

    /// One JSON line on `out`, for commands that stream many of them.
    pub fn emit_to<T: Serialize>(
        self,
        out: &mut impl Write,
        ty: &str,
        body: T,
    ) -> std::io::Result<()> {
        writeln!(out, "{}", line(ty, body))
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    exit_code: u8,
    message: &'a str,
}

/// The `--json` form of a failure: one line on stderr.
pub fn error_line(exit_code: u8, message: &str) -> String {
    line("error", ErrorBody { exit_code, message })
}

/// A path as a JSON string when its bytes are UTF-8, else `{"b64": "<bytes>"}`.
pub struct JsonPath<'a>(pub &'a [u8]);

impl Serialize for JsonPath<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(self.0) {
            Ok(text) => s.serialize_str(text),
            Err(_) => {
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry("b64", &BASE64.encode(self.0))?;
                map.end()
            }
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC (days-to-civil conversion after H. Hinnant).
pub fn utc(t: Option<&pb::Timestamp>) -> String {
    let secs = t.map_or(0, |t| t.seconds);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// RFC 3339 in UTC with nanoseconds. Instants outside years 1 to 9999 are
/// clamped to the nearest bound, as a forged or odd timestamp must not crash
/// the machine-readable output.
pub fn rfc3339(t: Option<&pb::Timestamp>) -> String {
    let t = t.cloned().unwrap_or_default();
    let clamped = pb::Timestamp {
        seconds: t.seconds.clamp(-62_135_596_800, 253_402_300_799),
        nanos: 0,
    };
    format!(
        "{}.{:09}Z",
        utc(Some(&clamped)).replacen(' ', "T", 1),
        t.nanos.min(999_999_999)
    )
}

#[derive(Serialize)]
pub struct StatsJson {
    pub files_new: u64,
    pub files_changed: u64,
    pub files_unchanged: u64,
    pub dirs: u64,
    pub bytes_total: u64,
    pub bytes_read: u64,
    pub bytes_added: u64,
}

impl From<Option<&pb::SnapshotStats>> for StatsJson {
    fn from(s: Option<&pb::SnapshotStats>) -> Self {
        let s = s.cloned().unwrap_or_default();
        StatsJson {
            files_new: s.files_new,
            files_changed: s.files_changed,
            files_unchanged: s.files_unchanged,
            dirs: s.dirs,
            bytes_total: s.bytes_total,
            bytes_read: s.bytes_read,
            bytes_added: s.bytes_added,
        }
    }
}

pub fn hex(id: &[u8]) -> String {
    <[u8; 32]>::try_from(id).map_or_else(|_| String::new(), |id| hex32(&id))
}

/// A spinner on stderr, only when stderr is a terminal.
pub fn spinner() -> Option<ProgressBar> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
    bar.set_style(ProgressStyle::with_template("{msg}").expect("the template is valid"));
    Some(bar)
}

/// Lets a progress callback run at most four times a second.
pub struct Throttle {
    last: Cell<Option<Instant>>,
}

impl Throttle {
    const INTERVAL: Duration = Duration::from_millis(250);

    pub fn new() -> Self {
        Throttle {
            last: Cell::new(None),
        }
    }

    pub fn ready(&self) -> bool {
        let now = Instant::now();
        match self.last.get() {
            Some(prev) if now.duration_since(prev) < Self::INTERVAL => false,
            _ => {
                self.last.set(Some(now));
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_carries_version_and_type() {
        let text = line("x", serde_json::json!({"a": 1}));
        assert_eq!(text, r#"{"v":1,"type":"x","a":1}"#);
    }

    #[test]
    fn path_is_string_or_b64() {
        let json = |b: &[u8]| serde_json::to_string(&JsonPath(b)).unwrap();
        assert_eq!(json(b"/a/b"), r#""/a/b""#);
        assert_eq!(json(b"/a/\xff"), r#"{"b64":"L2Ev/w=="}"#);
    }

    #[test]
    fn time_has_nanoseconds_in_utc() {
        let t = pb::Timestamp {
            seconds: 1_600_000_000,
            nanos: 5,
        };
        assert_eq!(rfc3339(Some(&t)), "2020-09-13T12:26:40.000000005Z");
    }

    #[test]
    fn time_outside_the_calendar_does_not_panic() {
        let at = |seconds| pb::Timestamp { seconds, nanos: 7 };
        assert_eq!(rfc3339(Some(&at(-1))), "1969-12-31T23:59:59.000000007Z");
        assert_eq!(
            rfc3339(Some(&at(i64::MIN))),
            "0001-01-01T00:00:00.000000007Z"
        );
        assert_eq!(
            rfc3339(Some(&at(i64::MAX))),
            "9999-12-31T23:59:59.000000007Z"
        );
        assert_eq!(rfc3339(None), "1970-01-01T00:00:00.000000000Z");
    }

    #[test]
    fn utc_formats_known_instants() {
        let at = |seconds| pb::Timestamp { seconds, nanos: 0 };
        assert_eq!(utc(Some(&at(0))), "1970-01-01 00:00:00");
        assert_eq!(utc(Some(&at(1_600_000_000))), "2020-09-13 12:26:40");
        assert_eq!(utc(Some(&at(951_782_400))), "2000-02-29 00:00:00");
    }

    #[test]
    fn error_line_is_one_json_object() {
        let text = error_line(12, "wrong password or recovery key");
        assert_eq!(
            text,
            r#"{"v":1,"type":"error","exit_code":12,"message":"wrong password or recovery key"}"#
        );
    }
}
