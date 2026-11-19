#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| lodestore_fuzzkit::nbd_client(data));
