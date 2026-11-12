#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| lodestore_fuzzkit::recovery_key(data));
