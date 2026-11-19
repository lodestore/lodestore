use crate::common::{init_repo, lode};

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use lodestore_testkit::qemu::{self, NbdExport, Qcow2, ReadLog};
use serde_json::Value;

const MIB: u64 = 1 << 20;
const HOST: &str = "qemu-host";
const DISK: u64 = 40 * MIB;

/// A qcow2 disk, a repository and the dirty granules each live bitmap must hold.
struct Rig {
    _tmp: tempfile::TempDir,
    socks: tempfile::TempDir,
    repo: PathBuf,
    qcow: Qcow2,
    log: ReadLog,
    granule: u64,
    size: u64,
    bitmaps: Vec<(String, BTreeSet<u64>)>,
}

impl Rig {
    fn new(granule: u64, size: u64) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let socks = qemu::socket_dir();
        let qcow = Qcow2::create(&tmp.path().join("disk.qcow2"), size);
        let log = ReadLog::start(&NbdExport::socket_in(socks.path()), socks.path());
        Self {
            _tmp: tmp,
            socks,
            repo,
            qcow,
            log,
            granule,
            size,
            bitmaps: Vec::new(),
        }
    }

    fn touch(&mut self, offset: u64, len: u64) {
        let granules = offset / self.granule..=(offset + len - 1) / self.granule;
        for (_, dirty) in &mut self.bitmaps {
            dirty.extend(granules.clone());
        }
    }

    fn write(&mut self, offset: u64, len: u64, pattern: u8) {
        self.qcow.write(offset, len, pattern);
        self.touch(offset, len);
    }

    fn zero(&mut self, offset: u64, len: u64) {
        self.qcow.write_zeroes(offset, len);
        self.touch(offset, len);
    }

    fn add_bitmap(&mut self, name: &str) {
        self.qcow.add_bitmap(name, self.granule);
        self.bitmaps.push((name.to_owned(), BTreeSet::new()));
    }

    fn remove_bitmap(&mut self, name: &str) {
        self.qcow.remove_bitmap(name);
        self.bitmaps.retain(|(n, _)| n != name);
    }

    fn resize(&mut self, size: u64) {
        self.qcow.resize(size);
        self.bitmaps.clear();
        self.size = size;
    }

    fn has_bitmap(&self, name: &str) -> bool {
        self.bitmaps.iter().any(|(n, _)| n == name)
    }

    fn ensure_bitmap(&mut self, name: &str) {
        if !self.has_bitmap(name) {
            self.add_bitmap(name);
        }
    }

    fn export(&self, bitmaps: &[&str]) -> NbdExport {
        NbdExport::start(&self.qcow, self.socks.path(), bitmaps)
    }

    fn backup(&self, extra: &[&str]) -> Value {
        let out = lode(&self.repo)
            .env("LODE_TEST_IMAGE_SEGMENT_CHUNKS", "4")
            .args([
                "backup", "--json", "--host", HOST, "--name", "disk", "--image",
            ])
            .arg(self.log.uri())
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["type"] == "backup_summary")
            .expect("a backup_summary line")
    }

    fn restored(&self, summary: &Value) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        lode(&self.repo)
            .args([
                "restore",
                summary["snapshot_id"].as_str().unwrap(),
                "--target",
            ])
            .arg(tmp.path())
            .assert()
            .success();
        fs::read(tmp.path().join("disk.img")).unwrap()
    }

    fn reference(&self) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let raw = tmp.path().join("disk.raw");
        self.qcow.to_raw(&raw);
        fs::read(raw).unwrap()
    }

    fn assert_restores_to_the_disk(&self, summary: &Value) {
        let expected = self.reference();
        let restored = self.restored(summary);
        assert_eq!(restored.len(), expected.len(), "restored size");
        let first_diff = restored.iter().zip(&expected).position(|(a, b)| a != b);
        assert!(
            first_diff.is_none(),
            "restore differs from the disk at byte {first_diff:?}"
        );
    }

    fn assert_reads_inside(&self, bitmap: &str) {
        let (_, dirty) = self
            .bitmaps
            .iter()
            .find(|(n, _)| n == bitmap)
            .expect("a live bitmap");
        let reads = self.log.reads();
        assert!(
            !reads.is_empty(),
            "an incremental run reads the dirty ranges"
        );
        let mut total = 0u64;
        for &(offset, len) in &reads {
            assert!(
                len > 0 && offset + u64::from(len) <= self.size,
                "read {offset}+{len}"
            );
            let end = offset + u64::from(len) - 1;
            for granule in offset / self.granule..=end / self.granule {
                assert!(
                    dirty.contains(&granule),
                    "read {offset}+{len} leaves the dirty granules of {bitmap} (granule {granule})"
                );
            }
            total += u64::from(len);
        }
        let dirty_bytes: u64 = dirty
            .iter()
            .map(|g| ((g + 1) * self.granule).min(self.size) - g * self.granule)
            .sum();
        assert!(
            total <= dirty_bytes,
            "read {total} bytes, dirty {dirty_bytes}"
        );
    }
}

fn read_mode(summary: &Value) -> (&str, Option<&str>) {
    (
        summary["image"]["read_mode"].as_str().unwrap(),
        summary["image"]["full_reason"].as_str(),
    )
}

fn text<'a>(summary: &'a Value, field: &str) -> &'a str {
    summary["image"][field].as_str().unwrap()
}

/// Writes inside one chunk, across a chunk boundary, across both segment
/// boundaries (the test uses four-chunk segments), a zeroed range and the
/// disk tail; the offsets move with the round so rounds overlap and differ.
fn round_writes(rig: &mut Rig, round: u64) {
    let pattern = |k: u64| ((round * 37 + k * 11) as u8) | 1;
    if round == 0 {
        rig.write(0, MIB, pattern(0));
        rig.write(24 * MIB, 2 * MIB, pattern(1));
        rig.write(DISK - MIB, MIB, pattern(2));
        return;
    }
    rig.write(2 * MIB + 300 * round + 17, 300, pattern(3));
    rig.write(20 * MIB - 4096 - 512 * round, 8192, pattern(4));
    rig.write(16 * MIB - 32768 - 4096 * round, 65536, pattern(5));
    rig.write(32 * MIB - 50000 + 700 * round, 100_000, pattern(6));
    rig.zero(8 * MIB + 100_000 * round + 3, MIB);
    rig.write(DISK - 1000, 1000, pattern(7));
}

fn window_write(rig: &mut Rig, round: u64) {
    rig.write(5 * MIB + 123 + 7 * round, 5000, 0xee);
}

fn generation_name(name: &str, generation: u64) -> String {
    let (prefix, _) = name.rsplit_once("-g").expect("a generation suffix");
    format!("{prefix}-g{generation}")
}

/// The rotation a backup script performs: the bitmap to read, the one that
/// follows it and the last snapshot.
struct Chain {
    current: String,
    following: String,
    previous: String,
}

impl Chain {
    fn start(rig: &mut Rig) -> (Self, Value) {
        round_writes(rig, 0);
        let export = rig.export(&[]);
        let first = rig.backup(&[]);
        export.stop();
        assert_eq!(read_mode(&first), ("full", Some("no_bitmap")));
        rig.assert_restores_to_the_disk(&first);
        let chain = Self {
            current: text(&first, "next_bitmap").to_owned(),
            following: text(&first, "bitmap_to_create").to_owned(),
            previous: first["snapshot_id"].as_str().unwrap().to_owned(),
        };
        (chain, first)
    }

    fn generation(&self) -> u64 {
        self.current.rsplit_once("-g").unwrap().1.parse().unwrap()
    }

    fn skip_generation(&mut self) {
        let next = self.generation() + 1;
        self.current = generation_name(&self.current, next);
        self.following = generation_name(&self.current, next + 1);
    }

    fn run(&mut self, rig: &mut Rig, round: u64, export_bitmaps: bool) -> Value {
        rig.ensure_bitmap(&self.current);
        round_writes(rig, round);
        rig.ensure_bitmap(&self.following);

        let names = [self.current.as_str(), self.following.as_str()];
        let export = rig.export(if export_bitmaps { &names } else { &[] });
        rig.log.clear();
        let summary = rig.backup(&["--bitmap", &self.current]);
        export.stop();

        if read_mode(&summary).0 == "incremental" {
            assert_eq!(
                summary["parent"].as_str(),
                Some(self.previous.as_str()),
                "round {round}"
            );
            assert_eq!(
                text(&summary, "next_bitmap"),
                self.following,
                "round {round}"
            );
            rig.assert_reads_inside(&self.current);
        }
        rig.assert_restores_to_the_disk(&summary);

        window_write(rig, round);
        let next = text(&summary, "next_bitmap").to_owned();
        if next != self.current && rig.has_bitmap(&self.current) {
            rig.remove_bitmap(&self.current);
        }
        self.previous = summary["snapshot_id"].as_str().unwrap().to_owned();
        self.current = next;
        self.following = text(&summary, "bitmap_to_create").to_owned();
        summary
    }
}

fn run_rotation(granule: u64) {
    let mut rig = Rig::new(granule, DISK);
    let (mut chain, _) = Chain::start(&mut rig);
    for round in 1..=5u64 {
        let summary = chain.run(&mut rig, round, true);
        let expected = if round == 1 {
            ("full", Some("no_state"))
        } else {
            ("incremental", None)
        };
        assert_eq!(
            read_mode(&summary),
            expected,
            "granule {granule}, round {round}"
        );
    }
}

#[test]
#[ignore = "needs qemu-img, qemu-nbd and qemu-io"]
fn qemu_incremental_matches_the_disk_at_every_granularity() {
    if !qemu::available() {
        return;
    }
    for granule in [512, 64 << 10, 4 << 20, 16 << 20] {
        run_rotation(granule);
    }
}

#[test]
#[ignore = "needs qemu-img, qemu-nbd and qemu-io"]
fn qemu_export_without_the_bitmap_reads_in_full() {
    if !qemu::available() {
        return;
    }
    let mut rig = Rig::new(64 << 10, DISK);
    let (mut chain, _) = Chain::start(&mut rig);
    for round in 1..=2 {
        chain.run(&mut rig, round, true);
    }
    let lost = chain.run(&mut rig, 3, false);
    assert_eq!(read_mode(&lost), ("full", Some("bitmap_missing")));
    let restart = chain.run(&mut rig, 4, true);
    assert_eq!(read_mode(&restart), ("full", Some("no_state")));
    let again = chain.run(&mut rig, 5, true);
    assert_eq!(read_mode(&again), ("incremental", None));
}

#[test]
#[ignore = "needs qemu-img, qemu-nbd and qemu-io"]
fn qemu_resize_forces_a_full_read() {
    if !qemu::available() {
        return;
    }
    let mut rig = Rig::new(64 << 10, DISK);
    let (mut chain, _) = Chain::start(&mut rig);
    for round in 1..=2 {
        chain.run(&mut rig, round, true);
    }

    rig.resize(DISK + 8 * MIB);
    rig.write(DISK + MIB, 70_000, 0x5a);
    chain.skip_generation();
    let grown = chain.run(&mut rig, 3, true);
    assert_eq!(read_mode(&grown), ("full", Some("gen_mismatch")));
    assert_eq!(
        chain.run(&mut rig, 4, true)["image"]["read_mode"],
        "incremental"
    );

    rig.resize(DISK + 4 * MIB);
    chain.skip_generation();
    let shrunk = chain.run(&mut rig, 5, true);
    assert_eq!(read_mode(&shrunk), ("full", Some("gen_mismatch")));

    rig.resize(DISK);
    rig.resize(DISK + 4 * MIB);
    chain.skip_generation();
    let regrown = chain.run(&mut rig, 6, true);
    assert_eq!(read_mode(&regrown), ("full", Some("gen_mismatch")));
    assert_eq!(
        chain.run(&mut rig, 7, true)["image"]["read_mode"],
        "incremental"
    );
}
