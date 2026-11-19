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

fn run_rotation(granule: u64) {
    let mut rig = Rig::new(granule, DISK);
    round_writes(&mut rig, 0);

    let export = rig.export(&[]);
    let first = rig.backup(&[]);
    export.stop();
    assert_eq!(read_mode(&first), ("full", Some("no_bitmap")));
    rig.assert_restores_to_the_disk(&first);
    let mut current = text(&first, "next_bitmap").to_owned();
    let mut following = text(&first, "bitmap_to_create").to_owned();
    let mut previous = first["snapshot_id"].as_str().unwrap().to_owned();

    for round in 1..=5u64 {
        if round == 1 {
            rig.add_bitmap(&current);
            rig.add_bitmap(&following);
            round_writes(&mut rig, round);
        } else {
            round_writes(&mut rig, round);
            rig.add_bitmap(&following);
        }

        let export = rig.export(&[&current, &following]);
        rig.log.clear();
        let summary = rig.backup(&["--bitmap", &current]);
        export.stop();

        assert_eq!(
            text(&summary, "bitmap"),
            current,
            "granule {granule}, round {round}"
        );
        assert_eq!(text(&summary, "next_bitmap"), following, "round {round}");
        if round == 1 {
            assert_eq!(read_mode(&summary), ("full", Some("no_state")));
        } else {
            assert_eq!(
                read_mode(&summary),
                ("incremental", None),
                "granule {granule}, round {round}"
            );
            assert_eq!(
                summary["parent"].as_str(),
                Some(previous.as_str()),
                "round {round}"
            );
            rig.assert_reads_inside(&current);
        }
        rig.assert_restores_to_the_disk(&summary);

        window_write(&mut rig, round);
        rig.remove_bitmap(&current);
        previous = summary["snapshot_id"].as_str().unwrap().to_owned();
        current = following;
        following = text(&summary, "bitmap_to_create").to_owned();
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
