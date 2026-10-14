//! Filesystem side of the data path: walking a tree for backup and writing a
//! tree back for restore. Both work on directory file descriptors and never
//! resolve a multi-component path.

pub mod filter;
pub mod meta;
pub mod restore;
pub mod walk;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::pb;

/// The current time, for manifests and key slots.
pub(crate) fn now() -> pb::Timestamp {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    timestamp(
        i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        d.subsec_nanos(),
    )
}

/// Host name as bytes, for the snapshot manifest.
pub fn hostname() -> Vec<u8> {
    rustix::system::uname().nodename().to_bytes().to_vec()
}

/// Converts between the integer widths `stat` uses on different platforms.
pub(crate) fn conv<A: TryInto<B>, B: Default>(a: A) -> B {
    a.try_into().unwrap_or_default()
}

pub(crate) fn timestamp<S: TryInto<i64>, N: TryInto<u32>>(seconds: S, nanos: N) -> pb::Timestamp {
    pb::Timestamp {
        seconds: conv(seconds),
        nanos: conv(nanos),
    }
}
