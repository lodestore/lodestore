#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| lodestore_fuzzkit::writer_keys(data));
