#![cfg(feature = "test-hooks")]
#![forbid(unsafe_code)]
//! Every function here is the body of one fuzz target. None of them panics on
//! any input; equality assertions exist only where they are written down.

use lodestore_format::pack::parse_prefix;

pub fn pack_prefix(data: &[u8]) {
    let _ = parse_prefix(data);
}

pub type Target = fn(&[u8]);

pub const TARGETS: &[(&str, Target)] = &[("pack_prefix", pack_prefix)];
