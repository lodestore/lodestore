#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| lodestore_fuzzkit::attest_peek(data));
