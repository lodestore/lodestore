#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| lodestore_fuzzkit::tree_decode(data));
