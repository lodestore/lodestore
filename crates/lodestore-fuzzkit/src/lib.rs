#![cfg(feature = "test-hooks")]
#![forbid(unsafe_code)]
//! Every function here is the body of one fuzz target. None of them panics on
//! any input; equality assertions exist only where they are written down.

pub mod seeds;

use lodestore_format::pack::{parse_entries, parse_prefix, parse_trailer};

pub fn pack_prefix(data: &[u8]) {
    let _ = parse_prefix(data);
}

pub fn pack_trailer(data: &[u8]) {
    let Some((size, rest)) = data.split_first_chunk::<8>() else {
        return;
    };
    let _ = parse_trailer(rest, u64::from_le_bytes(*size));
}

pub fn pack_entries(data: &[u8]) {
    let _ = parse_entries(data);
}

pub type Target = fn(&[u8]);

pub const TARGETS: &[(&str, Target)] = &[
    ("pack_prefix", pack_prefix),
    ("pack_trailer", pack_trailer),
    ("pack_entries", pack_entries),
];
