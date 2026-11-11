#![cfg(feature = "test-hooks")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use lodestore_fuzzkit::TARGETS;
use lodestore_fuzzkit::seeds::seeds;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(by: usize) {
    let now = CURRENT.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            CURRENT.fetch_sub(layout.size(), Relaxed);
            grow(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../lodestore-format/tests/fixtures/v1")
}

fn fuzz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz")
}

fn run_bounded(name: &str, f: fn(&[u8]), input: &[u8]) {
    let start = CURRENT.load(Relaxed);
    PEAK.store(start, Relaxed);
    f(input);
    let grown = PEAK.load(Relaxed).saturating_sub(start);
    let limit = 64 * input.len() + (64 << 20);
    assert!(
        grown <= limit,
        "{name}: {grown} bytes allocated for a {}-byte input",
        input.len()
    );
}

#[test]
fn every_target_has_a_fuzz_target_file() {
    let dir = fuzz_dir().join("fuzz_targets");
    let files: BTreeSet<String> = fs::read_dir(&dir)
        .expect("fuzz/fuzz_targets exists")
        .map(|e| {
            let path = e.expect("directory entry").path();
            path.file_stem()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let targets: BTreeSet<String> = TARGETS.iter().map(|(n, _)| (*n).to_owned()).collect();
    assert_eq!(targets, files);
}

#[test]
fn every_target_has_a_seed() {
    let seeds = seeds(&fixtures());
    for (name, _) in TARGETS {
        assert!(seeds.iter().any(|(t, _)| t == name), "{name} has no seed");
    }
}

#[test]
fn seeds_and_regressions_never_panic() {
    let seeds = seeds(&fixtures());
    for (name, f) in TARGETS {
        run_bounded(name, *f, &[]);
        for (_, input) in seeds.iter().filter(|(t, _)| t == name) {
            run_bounded(name, *f, input);
        }
        let Ok(dir) = fs::read_dir(fuzz_dir().join("regressions").join(name)) else {
            continue;
        };
        for entry in dir {
            let input = fs::read(entry.expect("directory entry").path()).expect("regression file");
            run_bounded(name, *f, &input);
        }
    }
}
