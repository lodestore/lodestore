//! The binding between a dirty bitmap and the parent snapshot: what a
//! snapshot records, the names of bitmaps, the state cache and the one
//! function that decides between an incremental and a full read.

use lodestore_format::crypto::{BlobId, RepoId};
use lodestore_format::image::{
    BitmapBinding, BitmapNames, DecideInput, Decision, FullReason, ImageMeta, ImageState,
    InstanceSnapshot, MAX_IMAGE_SIZE, MAX_STATE_BYTES, ReadMode, ReadPlan, StateRead,
    disk_meta_key, instance_snapshots, source_identity,
};
use lodestore_format::pb;
use lodestore_format::snapshot::SnapshotEntry;
use lodestore_format::source::{self, Meta, SourceKey};
use proptest::prelude::*;
use serde_json::{Value, json};

pub(crate) const REPO: RepoId = [7; 16];
pub(crate) const HOST: &[u8] = b"h1";
const MIB: u64 = 1 << 20;
const SIZE: u64 = 12 * MIB;

fn key() -> SourceKey {
    SourceKey::image("disk").unwrap()
}

fn names() -> BitmapNames {
    BitmapNames::new(&REPO, HOST, &key())
}

fn id(n: u8) -> BlobId {
    [n; 32]
}

fn snapshot(
    n: u8,
    parent: Option<u8>,
    size: Option<u64>,
    generation: Option<u64>,
) -> InstanceSnapshot {
    InstanceSnapshot {
        id: id(n),
        parent: parent.map(id),
        disk_size: size,
        binding: generation.map(|g| names().binding(g)),
        binding_unreadable: false,
    }
}

/// An owned `DecideInput` that a case edits one field at a time.
#[derive(Clone)]
struct Case {
    names: BitmapNames,
    bitmap: Option<u64>,
    export_has_bitmap: bool,
    export_has_next: bool,
    export_size: u64,
    source: [u8; 32],
    state: StateRead,
    instance: Vec<InstanceSnapshot>,
}

fn state(parent: u8, generation: u64, size: u64, source: [u8; 32]) -> ImageState {
    ImageState {
        parent: id(parent),
        bitmap_gen: generation,
        disk_size: size,
        source,
    }
}

fn source_a() -> [u8; 32] {
    source_identity("nbd+unix:///disk?socket=/run/a.sock")
}

fn base() -> Case {
    Case {
        names: names(),
        bitmap: Some(3),
        export_has_bitmap: true,
        export_has_next: true,
        export_size: SIZE,
        source: source_a(),
        state: StateRead::Found(state(2, 3, SIZE, source_a())),
        instance: vec![
            snapshot(1, None, Some(SIZE), Some(2)),
            snapshot(2, Some(1), Some(SIZE), Some(3)),
        ],
    }
}

impl Case {
    fn decide(&self) -> Decision {
        lodestore_format::image::decide(&DecideInput {
            names: &self.names,
            bitmap: self.bitmap,
            export_has_bitmap: self.export_has_bitmap,
            export_has_next: self.export_has_next,
            export_size: self.export_size,
            source: self.source,
            state: &self.state,
            instance: &self.instance,
        })
    }
}

#[test]
fn decide_incremental_when_everything_matches() {
    let decision = base().decide();
    assert_eq!(decision.plan, ReadPlan::Incremental { parent: id(2) });
    assert_eq!(decision.record, Some(names().binding(4)));
    assert_eq!(decision.restart, 4);
}

fn meta_of(
    read_mode: ReadMode,
    size: u64,
    binding: Option<&BitmapBinding>,
    reason: Option<FullReason>,
) -> Meta {
    ImageMeta {
        read_mode,
        disk_size: size,
    }
    .to_meta_bound("disk", binding, reason)
}

#[test]
fn image_binding_meta_round_trips() {
    let full = names().binding(2);
    let incremental = names().binding(3);
    let cases = [
        (ReadMode::Full, Some(&full), Some(FullReason::NoState)),
        (ReadMode::Incremental, Some(&incremental), None),
        (ReadMode::Full, None, Some(FullReason::NoBitmap)),
    ];
    for (mode, binding, reason) in cases {
        let meta = meta_of(mode, 20 * MIB, binding, reason);
        let encoded = source::encode(&key(), &meta).unwrap().unwrap();
        let (decoded_key, decoded) = source::decode(Some(&encoded)).unwrap();
        assert_eq!(decoded_key, key());
        let image = ImageMeta::from_meta(&decoded, "disk").unwrap();
        assert_eq!(image.read_mode, mode);
        assert_eq!(image.disk_size, 20 * MIB);
        assert_eq!(
            BitmapBinding::from_meta(&decoded, "disk").unwrap().as_ref(),
            binding
        );
        assert_eq!(FullReason::from_meta(&decoded, "disk").unwrap(), reason);
    }
    let plain = ImageMeta {
        read_mode: ReadMode::Full,
        disk_size: 5,
    };
    assert_eq!(
        plain.to_meta_bound("disk", None, None),
        plain.to_meta("disk")
    );
}

#[test]
fn bitmap_names_round_trip() {
    let names = names();
    for n in [1, 2, 10, u64::MAX - 1] {
        assert_eq!(names.parse(&names.name(n)), Some(n));
    }
    assert_eq!(names.first(), names.name(1));
    let name = names.name(5);
    let rest = name.strip_prefix("lode-").unwrap();
    let (repo, rest) = rest.split_once('-').unwrap();
    let (instance, generation) = rest.split_once("-g").unwrap();
    assert_eq!(repo, "07".repeat(8));
    assert_eq!(generation, "5");
    assert_eq!(instance.len(), 16);
    assert!(
        instance
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    let other_host = BitmapNames::new(&REPO, b"h2", &key());
    let other_key = BitmapNames::new(&REPO, HOST, &SourceKey::image("other").unwrap());
    assert_ne!(names.name(1), other_host.name(1));
    assert_ne!(names.name(1), other_key.name(1));
}

#[test]
fn image_state_round_trips() {
    let s = state(9, 3, SIZE, source_a());
    let bytes = s.encode(&REPO, HOST, &key());
    assert_eq!(bytes, s.encode(&REPO, HOST, &key()));
    assert!(bytes.len() <= MAX_STATE_BYTES);
    assert_eq!(ImageState::parse(&bytes, &REPO, HOST, &key()).unwrap(), s);
}

fn edited(good: &[u8], edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(good).unwrap();
    edit(value.as_object_mut().unwrap());
    serde_json::to_vec(&value).unwrap()
}

fn good_state() -> Vec<u8> {
    state(9, 3, SIZE, source_a()).encode(&REPO, HOST, &key())
}

#[test]
fn image_state_rejects_hostile_files() {
    let good = good_state();
    let set = |field: &'static str, value: Value| {
        edited(&good, move |o| {
            o.insert(field.to_owned(), value);
        })
    };
    let without = |field: &'static str| {
        edited(&good, move |o| {
            o.remove(field);
        })
    };
    let mut duplicate = good.clone();
    duplicate.pop();
    duplicate.extend_from_slice(b",\"version\":1}");
    let too_big: Value = serde_json::from_str("18446744073709551616").unwrap();
    let rows: Vec<(&str, Vec<u8>)> = vec![
        ("one byte over the limit", vec![b' '; MAX_STATE_BYTES + 1]),
        ("not JSON", b"nope".to_vec()),
        ("an array", b"[]".to_vec()),
        ("empty", Vec::new()),
        ("an extra field", set("path", json!("/dev/vda"))),
        ("a missing field", without("chunk_size")),
        ("a missing source", without("source")),
        ("version 2", set("version", json!(2))),
        ("another repository", set("repo_id", json!("08".repeat(16)))),
        ("another host", set("host", json!("6832"))),
        ("another key", set("key", json!("image/other"))),
        ("parent of 63 digits", set("parent", json!("a".repeat(63)))),
        ("parent in capitals", set("parent", json!("A".repeat(64)))),
        ("source not hex", set("source", json!("g".repeat(64)))),
        ("generation 0", set("bitmap_gen", json!(0))),
        ("generation as text", set("bitmap_gen", json!("3"))),
        ("negative generation", set("bitmap_gen", json!(-1))),
        ("fractional generation", set("bitmap_gen", json!(3.5))),
        ("generation past u64", set("bitmap_gen", too_big)),
        (
            "disk too large",
            set("disk_size", json!(MAX_IMAGE_SIZE + 1)),
        ),
        ("another chunk size", set("chunk_size", json!(1_048_576))),
        ("a repeated key", duplicate),
        ("trailing space", [good.as_slice(), b" "].concat()),
    ];
    for (label, bytes) in rows {
        assert_ne!(bytes, good, "{label}");
        assert!(
            ImageState::parse(&bytes, &REPO, HOST, &key()).is_err(),
            "{label}"
        );
    }
    assert!(ImageState::parse(&good, &[8; 16], HOST, &key()).is_err());
    assert!(ImageState::parse(&good, &REPO, b"h2", &key()).is_err());
    assert!(ImageState::parse(&good, &REPO, HOST, &SourceKey::image("other").unwrap()).is_err());
    assert!(ImageState::parse(&good, &REPO, HOST, &key()).is_ok());
}

fn state_error(bytes: &[u8], repo: &RepoId, host: &[u8], key: &SourceKey) -> String {
    ImageState::parse(bytes, repo, host, key)
        .unwrap_err()
        .to_string()
}

#[test]
fn image_state_names_the_check_that_failed() {
    let good = good_state();
    let set = |field: &'static str, value: Value| {
        edited(&good, move |o| {
            o.insert(field.to_owned(), value);
        })
    };
    let renamed = edited(&good, |o| {
        o.remove("chunk_size");
        o.insert("chunk_sizes".to_owned(), json!(1));
    });
    let rows: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "an extra field",
            set("path", json!("/dev/vda")),
            "exactly the known fields",
        ),
        ("a renamed field", renamed, "exactly the known fields"),
        (
            "another repository",
            set("repo_id", json!("08".repeat(16))),
            "another repository or instance",
        ),
        (
            "another host",
            set("host", json!("6832")),
            "another repository or instance",
        ),
        (
            "another key",
            set("key", json!("image/other")),
            "another repository or instance",
        ),
        (
            "a source in capitals",
            set("source", json!("A".repeat(64))),
            "source is not 64 lower-case hex digits",
        ),
        (
            "a parent in capitals",
            set("parent", json!("A".repeat(64))),
            "parent is not 64 lower-case hex digits",
        ),
    ];
    for (label, bytes, expected) in rows {
        let message = state_error(&bytes, &REPO, HOST, &key());
        assert!(message.contains(expected), "{label}: {message}");
    }
}

fn state_of_len(len: usize) -> (Vec<u8>, Vec<u8>, SourceKey) {
    let s = state(9, 3, SIZE, source_a());
    for name in ["d", "dd"] {
        let key = SourceKey::image(name).unwrap();
        let bare = s.encode(&REPO, b"", &key).len();
        if let Some(extra) = len.checked_sub(bare).filter(|n| n % 2 == 0) {
            let host = vec![b'h'; extra / 2];
            let bytes = s.encode(&REPO, &host, &key);
            assert_eq!(bytes.len(), len);
            return (bytes, host, key);
        }
    }
    unreachable!("one of the two key lengths has the right parity");
}

#[test]
fn image_state_size_limit_is_exact() {
    let (bytes, host, key) = state_of_len(MAX_STATE_BYTES);
    assert!(ImageState::parse(&bytes, &REPO, &host, &key).is_ok());
    let (bytes, host, key) = state_of_len(MAX_STATE_BYTES + 1);
    let message = state_error(&bytes, &REPO, &host, &key);
    assert!(message.contains("too large"), "{message}");
}

#[test]
fn bitmap_names_reject_other_names() {
    let names = names();
    let name = names.name(3);
    let prefix = name.trim_end_matches('3').to_owned();
    let elsewhere = [
        BitmapNames::new(&[8; 16], HOST, &key()).name(3),
        BitmapNames::new(&REPO, b"h2", &key()).name(3),
        BitmapNames::new(&REPO, HOST, &SourceKey::image("other").unwrap()).name(3),
    ];
    for other in elsewhere {
        assert_eq!(names.parse(&other), None, "{other}");
    }
    let rows = [
        format!("{prefix}0"),
        format!("{prefix}03"),
        format!("{prefix}{}", u64::MAX),
        format!("{prefix}{}", u128::from(u64::MAX) + 1),
        format!("{prefix}+3"),
        format!("{prefix}-3"),
        prefix.clone(),
        format!("{name} "),
        format!(" {name}"),
        name.to_ascii_uppercase(),
        name.replacen("lode-", "", 1),
        String::new(),
    ];
    for row in rows {
        assert_eq!(names.parse(&row), None, "{row:?}");
    }
    assert_eq!(names.parse(&name), Some(3));
}

#[test]
fn image_binding_meta_rejects() {
    let bound = |binding: &BitmapBinding| {
        ImageMeta {
            read_mode: ReadMode::Full,
            disk_size: SIZE,
        }
        .to_meta_bound("disk", Some(binding), Some(FullReason::NoState))
    };
    let name_key = disk_meta_key("disk", "bitmap_name");
    let gen_key = disk_meta_key("disk", "bitmap_gen");
    let reason_key = disk_meta_key("disk", "full_reason");
    let good = bound(&names().binding(2));
    assert!(BitmapBinding::from_meta(&good, "disk").unwrap().is_some());

    let mut no_generation = good.clone();
    no_generation.remove(&gen_key);
    let mut no_name = good.clone();
    no_name.remove(&name_key);
    let mut rows = vec![no_generation, no_name];
    for generation in ["0", "01", "-1", "18446744073709551616", "", "2 "] {
        let mut meta = good.clone();
        meta.insert(gen_key.clone(), generation.to_owned());
        rows.push(meta);
    }
    for name in ["", "a\nb", "a\u{1f}", "a\u{7f}"] {
        let mut meta = good.clone();
        meta.insert(name_key.clone(), name.to_owned());
        rows.push(meta);
    }
    for meta in rows {
        assert!(BitmapBinding::from_meta(&meta, "disk").is_err(), "{meta:?}");
    }
    let mut spaced = good.clone();
    spaced.insert(name_key.clone(), "a b".to_owned());
    let binding = BitmapBinding::from_meta(&spaced, "disk").unwrap().unwrap();
    assert_eq!(binding.name, "a b");

    let mut unknown = good.clone();
    unknown.insert(reason_key, "because".to_owned());
    assert!(FullReason::from_meta(&unknown, "disk").is_err());
    assert_eq!(FullReason::parse("because"), None);
    assert_eq!(FullReason::parse("no_state"), Some(FullReason::NoState));

    let elsewhere = {
        let mut meta = good.clone();
        meta.retain(|k, _| !k.contains("bitmap") && !k.contains("full_reason"));
        meta.insert(disk_meta_key("other", "bitmap_name"), "x".to_owned());
        meta
    };
    assert_eq!(BitmapBinding::from_meta(&elsewhere, "disk").unwrap(), None);
    assert_eq!(FullReason::from_meta(&elsewhere, "disk").unwrap(), None);
}

fn entry(n: u8, host: &[u8], claim: &SourceKey, meta: &Meta, parent: &[u8]) -> SnapshotEntry {
    SnapshotEntry {
        id: id(n),
        snapshot: pb::Snapshot {
            hostname: host.to_vec(),
            parent: parent.to_vec(),
            source: source::encode(claim, meta).unwrap(),
            ..Default::default()
        },
        modified_ms: None,
    }
}

#[test]
fn instance_snapshots_pick_the_instance() {
    let image = ImageMeta {
        read_mode: ReadMode::Full,
        disk_size: SIZE,
    };
    let bound = |generation| image.to_meta_bound("disk", Some(&names().binding(generation)), None);
    let other_key = SourceKey::image("other").unwrap();
    let mut broken_binding = bound(3);
    broken_binding.remove(&disk_meta_key("disk", "bitmap_gen"));
    let mut broken_size = bound(4);
    broken_size.insert(disk_meta_key("disk", "size"), "big".to_owned());
    let unparseable = pb::Snapshot {
        hostname: HOST.to_vec(),
        source: Some(pb::Source {
            key: "image/".to_owned(),
            meta: Vec::new(),
        }),
        ..Default::default()
    };
    let undecodable = |n: u8, host: &[u8], claim: &str, parent: &[u8]| SnapshotEntry {
        id: id(n),
        snapshot: pb::Snapshot {
            hostname: host.to_vec(),
            parent: parent.to_vec(),
            source: Some(pb::Source {
                key: claim.to_owned(),
                meta: vec![pb::SourceMeta {
                    key: "Bad".to_owned(),
                    value: "x".to_owned(),
                }],
            }),
            ..Default::default()
        },
        modified_ms: None,
    };
    let entries = vec![
        entry(1, HOST, &key(), &bound(2), &[]),
        entry(2, HOST, &other_key, &image.to_meta("other"), &[]),
        entry(3, b"h2", &key(), &bound(2), &[]),
        entry(4, HOST, &SourceKey::files(), &Meta::new(), &[]),
        SnapshotEntry {
            id: id(5),
            snapshot: unparseable,
            modified_ms: None,
        },
        entry(6, HOST, &key(), &broken_binding, &[1; 31]),
        entry(7, HOST, &key(), &broken_size, &[1; 32]),
        entry(8, HOST, &key(), &bound(5), &[2; 32]),
        undecodable(9, HOST, key().as_str(), &[1; 32]),
        undecodable(10, HOST, other_key.as_str(), &[1; 32]),
        undecodable(11, b"h2", key().as_str(), &[1; 32]),
    ];
    let got = instance_snapshots(&entries, &key(), HOST);
    assert_eq!(
        got,
        vec![
            snapshot(1, None, Some(SIZE), Some(2)),
            InstanceSnapshot {
                binding_unreadable: true,
                ..snapshot(6, None, Some(SIZE), None)
            },
            snapshot(7, Some(1), None, Some(4)),
            snapshot(8, Some(2), Some(SIZE), Some(5)),
            InstanceSnapshot {
                binding_unreadable: true,
                ..snapshot(9, Some(1), None, None)
            },
        ]
    );
    let mut case = base();
    case.instance.push(got[got.len() - 1].clone());
    assert_eq!(reason(&case), FullReason::ParentNotNewest);
    case.state = StateRead::Found(state(9, 3, SIZE, source_a()));
    assert_eq!(reason(&case), FullReason::ParentUnreadable);
}

fn with(edit: impl FnOnce(&mut Case)) -> Case {
    let mut case = base();
    edit(&mut case);
    case
}

fn found(case: &mut Case) -> &mut ImageState {
    match &mut case.state {
        StateRead::Found(state) => state,
        _ => unreachable!("the base case has a state"),
    }
}

fn newest(case: &mut Case) -> &mut InstanceSnapshot {
    case.instance.last_mut().unwrap()
}

fn reason(case: &Case) -> FullReason {
    match case.decide().plan {
        ReadPlan::Full(reason) => reason,
        ReadPlan::Incremental { .. } => panic!("expected a full read"),
    }
}

#[test]
fn decide_checks_every_field_in_order() {
    use FullReason::*;
    let rows: Vec<(&str, Case, FullReason)> = vec![
        ("no --bitmap", with(|c| c.bitmap = None), NoBitmap),
        (
            "the export lacks the context",
            with(|c| c.export_has_bitmap = false),
            BitmapMissing,
        ),
        ("no state", with(|c| c.state = StateRead::Missing), NoState),
        (
            "an unreadable state",
            with(|c| c.state = StateRead::Unreadable),
            StateUnreadable,
        ),
        (
            "another source",
            with(|c| c.source = source_identity("nbd+unix:///disk?socket=/run/b.sock")),
            SourceChanged,
        ),
        ("no snapshots", with(|c| c.instance.clear()), NoParent),
        (
            "the state names an older snapshot",
            with(|c| found(c).parent = id(1)),
            ParentNotNewest,
        ),
        (
            "the newest has no binding",
            with(|c| newest(c).binding = None),
            ParentUnreadable,
        ),
        (
            "the newest has no size",
            with(|c| newest(c).disk_size = None),
            ParentUnreadable,
        ),
        (
            "a name of another prefix",
            with(|c| {
                newest(c).binding = Some(BitmapBinding {
                    name: "lode-0000000000000000-0000000000000000-g3".to_owned(),
                    generation: 3,
                });
            }),
            NameChanged,
        ),
        (
            "--bitmap above the parent generation",
            with(|c| c.bitmap = Some(4)),
            GenMismatch,
        ),
        (
            "--bitmap below the parent generation",
            with(|c| c.bitmap = Some(2)),
            GenMismatch,
        ),
        (
            "the state generation differs",
            with(|c| found(c).bitmap_gen = 2),
            GenMismatch,
        ),
        (
            "the export grew",
            with(|c| c.export_size = SIZE + MIB),
            SizeChanged,
        ),
        (
            "the export shrank",
            with(|c| c.export_size = SIZE - 4096),
            SizeChanged,
        ),
        (
            "the state size differs",
            with(|c| found(c).disk_size = SIZE + 4096),
            SizeChanged,
        ),
        (
            "the parent size differs",
            with(|c| newest(c).disk_size = Some(SIZE + 4096)),
            SizeChanged,
        ),
        (
            "no state outranks a size change",
            with(|c| {
                c.state = StateRead::Missing;
                c.export_size = SIZE + MIB;
            }),
            NoState,
        ),
        (
            "another source outranks a stale parent",
            with(|c| {
                c.source = source_identity("other");
                found(c).parent = id(1);
            }),
            SourceChanged,
        ),
        (
            "a stale parent outranks a generation",
            with(|c| {
                found(c).parent = id(1);
                c.bitmap = Some(9);
            }),
            ParentNotNewest,
        ),
        (
            "no --bitmap outranks a missing context",
            with(|c| {
                c.bitmap = None;
                c.export_has_bitmap = false;
            }),
            NoBitmap,
        ),
        (
            "a missing context outranks a missing state",
            with(|c| {
                c.export_has_bitmap = false;
                c.state = StateRead::Missing;
            }),
            BitmapMissing,
        ),
        (
            "a changed name outranks a generation",
            with(|c| {
                newest(c).binding = Some(BitmapBinding {
                    name: "x".to_owned(),
                    generation: 3,
                });
                c.bitmap = Some(4);
            }),
            NameChanged,
        ),
    ];
    for (label, case, expected) in rows {
        assert_eq!(reason(&case), expected, "{label}");
    }
}

#[test]
fn decide_refuses_a_forked_generation() {
    let incremental = |case: &Case| matches!(case.decide().plan, ReadPlan::Incremental { .. });
    let sibling = with(|c| {
        c.instance.insert(1, snapshot(3, Some(1), Some(SIZE), None));
    });
    assert_eq!(reason(&sibling), FullReason::GenMismatch);
    let sibling_with_generation = with(|c| {
        c.instance
            .insert(1, snapshot(3, Some(1), Some(SIZE), Some(1)));
    });
    assert_eq!(reason(&sibling_with_generation), FullReason::GenMismatch);
    for generation in [3, 4] {
        let skewed = with(|c| {
            c.instance
                .insert(0, snapshot(4, None, Some(SIZE), Some(generation)));
        });
        assert_eq!(reason(&skewed), FullReason::GenMismatch, "gen {generation}");
    }
    let earlier = with(|c| {
        c.instance.insert(0, snapshot(5, None, Some(SIZE), Some(2)));
    });
    assert!(incremental(&earlier));
    let unbound = with(|c| {
        c.instance.insert(0, snapshot(6, None, Some(SIZE), None));
        c.instance.insert(1, snapshot(7, Some(9), None, None));
    });
    assert!(incremental(&unbound));
    let unreadable = with(|c| {
        c.instance.insert(
            0,
            InstanceSnapshot {
                binding_unreadable: true,
                ..snapshot(8, None, Some(SIZE), None)
            },
        );
    });
    assert_eq!(reason(&unreadable), FullReason::GenMismatch);
    let unreadable_in_the_chain = with(|c| {
        c.instance.insert(
            1,
            InstanceSnapshot {
                binding_unreadable: true,
                ..snapshot(8, Some(1), Some(SIZE), None)
            },
        );
    });
    assert_eq!(
        reason(&unreadable_in_the_chain),
        FullReason::GenMismatch,
        "an unreadable binding between the root and the newest"
    );
}

#[test]
fn an_unreadable_older_binding_forces_full_reads_for_good() {
    let bound = |mode, generation| {
        ImageMeta {
            read_mode: mode,
            disk_size: SIZE,
        }
        .to_meta_bound("disk", Some(&names().binding(generation)), None)
    };
    let mut broken = bound(ReadMode::Incremental, 3);
    broken.remove(&disk_meta_key("disk", "bitmap_gen"));
    let mut entries = vec![
        entry(1, HOST, &key(), &bound(ReadMode::Full, 2), &[]),
        entry(2, HOST, &key(), &broken, &[1; 32]),
        entry(3, HOST, &key(), &bound(ReadMode::Incremental, 4), &[2; 32]),
    ];
    let chain = [
        (ReadMode::Full, Vec::new()),
        (ReadMode::Incremental, vec![4; 32]),
        (ReadMode::Incremental, vec![5; 32]),
    ];
    for (step, (mode, parent)) in chain.into_iter().enumerate() {
        let newest = 3 + step as u8;
        let generation = 4 + step as u64;
        let case = Case {
            bitmap: Some(generation),
            state: StateRead::Found(state(newest, generation, SIZE, source_a())),
            instance: instance_snapshots(&entries, &key(), HOST),
            ..base()
        };
        assert_eq!(reason(&case), FullReason::GenMismatch, "run {step}");
        let decision = case.decide();
        assert_eq!(decision.record, Some(names().binding(generation + 1)));
        entries.push(entry(
            newest + 1,
            HOST,
            &key(),
            &bound(mode, generation + 1),
            &parent,
        ));
    }
}

#[test]
fn decide_records_the_next_bitmap() {
    let next = Some(names().binding(4));
    let plans = [
        with(|_| {}),
        with(|c| c.state = StateRead::Missing),
        with(|c| c.export_has_bitmap = false),
        with(|c| c.export_size = SIZE + MIB),
        with(|c| c.instance.clear()),
    ];
    for case in plans {
        assert_eq!(case.decide().record, next);
    }
    assert_eq!(with(|c| c.bitmap = None).decide().record, None);
    assert_eq!(with(|c| c.export_has_next = false).decide().record, None);
    let late = with(|c| {
        c.export_has_next = false;
        c.export_has_bitmap = true;
    });
    assert!(matches!(late.decide().plan, ReadPlan::Incremental { .. }));
    assert_eq!(late.decide().record, None);
    assert_eq!(with(|c| c.bitmap = Some(u64::MAX)).decide().record, None);
}

#[test]
fn decide_restart_is_above_every_generation() {
    let generations = |c: &mut Case| {
        c.instance = vec![
            snapshot(1, None, Some(SIZE), Some(2)),
            snapshot(2, Some(1), Some(SIZE), Some(7)),
            snapshot(3, Some(2), Some(SIZE), Some(5)),
        ];
    };
    assert_eq!(with(generations).decide().restart, 8);
    assert_eq!(
        with(|c| {
            generations(c);
            c.bitmap = None;
        })
        .decide()
        .restart,
        8
    );
    assert_eq!(
        with(|c| {
            generations(c);
            c.bitmap = Some(9);
        })
        .decide()
        .restart,
        10
    );
    assert_eq!(
        with(|c| {
            c.bitmap = None;
            c.instance.clear();
        })
        .decide()
        .restart,
        1
    );
}

const SIZES: [u64; 3] = [SIZE, SIZE + MIB, 4096];

#[derive(Debug, Clone)]
struct Raw {
    n: u64,
    bitmap: bool,
    has_bitmap: bool,
    has_next: bool,
    source_ok: bool,
    state_kind: u8,
    parent_ok: bool,
    state_gen_ok: bool,
    state_size_ok: bool,
    export_size_ok: bool,
    empty: bool,
    older: Vec<(Option<u8>, Option<u64>, bool, bool)>,
    newest_parent: Option<u8>,
    newest_binding: bool,
    newest_gen_ok: bool,
    newest_gen: u64,
    name_ok: bool,
    newest_size: Option<usize>,
}

fn raw() -> impl Strategy<Value = Raw> {
    let flags = (
        1u64..=4,
        prop::bool::weighted(0.9),
        prop::bool::weighted(0.9),
        any::<bool>(),
        prop::bool::weighted(0.9),
        0u8..10,
        prop::bool::weighted(0.9),
        prop::bool::weighted(0.9),
        prop::bool::weighted(0.9),
        prop::bool::weighted(0.9),
        prop::bool::weighted(0.05),
    );
    let older = prop::collection::vec(
        (
            prop::option::of(0u8..3),
            prop::option::of(1u64..=4),
            any::<bool>(),
            prop::bool::weighted(0.3),
        ),
        0..3,
    );
    let tail = (
        prop::option::of(0u8..3),
        prop::bool::weighted(0.9),
        prop::bool::weighted(0.85),
        1u64..=4,
        prop::bool::weighted(0.9),
        prop::option::weighted(0.9, 0usize..3),
    );
    (flags, older, tail).prop_map(
        |(
            (
                n,
                bitmap,
                has_bitmap,
                has_next,
                source_ok,
                state_kind,
                parent_ok,
                state_gen_ok,
                state_size_ok,
                export_size_ok,
                empty,
            ),
            older,
            (newest_parent, newest_binding, newest_gen_ok, newest_gen, name_ok, newest_size),
        )| Raw {
            n,
            bitmap,
            has_bitmap,
            has_next,
            source_ok,
            state_kind,
            parent_ok,
            state_gen_ok,
            state_size_ok,
            export_size_ok,
            empty,
            older,
            newest_parent,
            newest_binding,
            newest_gen_ok,
            newest_gen,
            name_ok,
            newest_size,
        },
    )
}

fn parent_of(p: Option<u8>) -> Option<u8> {
    p.map(|p| 10 + p)
}

fn build(raw: &Raw) -> Case {
    let mut instance: Vec<InstanceSnapshot> = raw
        .older
        .iter()
        .enumerate()
        .map(
            |(i, (parent, generation, sized, unreadable))| InstanceSnapshot {
                binding_unreadable: *unreadable,
                ..snapshot(
                    20 + i as u8,
                    parent_of(*parent),
                    sized.then_some(SIZE),
                    generation.filter(|_| !unreadable),
                )
            },
        )
        .collect();
    let generation = if raw.newest_gen_ok {
        raw.n
    } else {
        raw.newest_gen
    };
    let binding = raw.newest_binding.then(|| BitmapBinding {
        name: if raw.name_ok {
            names().name(generation)
        } else {
            "lode-other".to_owned()
        },
        generation,
    });
    if !raw.empty {
        instance.push(InstanceSnapshot {
            id: id(1),
            parent: parent_of(raw.newest_parent).map(id),
            disk_size: raw.newest_size.map(|i| SIZES[i]),
            binding,
            binding_unreadable: false,
        });
    }
    let source = if raw.source_ok {
        source_a()
    } else {
        source_identity("other")
    };
    let state = match raw.state_kind {
        8 => StateRead::Missing,
        9 => StateRead::Unreadable,
        _ => StateRead::Found(ImageState {
            parent: if raw.parent_ok { id(1) } else { id(2) },
            bitmap_gen: if raw.state_gen_ok {
                raw.n
            } else {
                raw.n % 4 + 1
            },
            disk_size: if raw.state_size_ok { SIZE } else { SIZES[1] },
            source,
        }),
    };
    Case {
        names: names(),
        bitmap: raw.bitmap.then_some(raw.n),
        export_has_bitmap: raw.has_bitmap,
        export_has_next: raw.has_next,
        export_size: if raw.export_size_ok { SIZE } else { SIZES[2] },
        source: source_a(),
        state,
        instance,
    }
}

// Written from the contract, not from the body of `decide`.
fn everything_holds(case: &Case) -> Option<BlobId> {
    let n = case.bitmap?;
    if !case.export_has_bitmap {
        return None;
    }
    let StateRead::Found(state) = &case.state else {
        return None;
    };
    let (newest, older) = case.instance.split_last()?;
    let binding = newest.binding.as_ref()?;
    let sized = newest.disk_size?;
    let holds = state.source == case.source
        && newest.id == state.parent
        && binding.name == case.names.name(n)
        && binding.generation == n
        && state.bitmap_gen == n
        && older.iter().all(|s| {
            s.binding.as_ref().is_none_or(|b| b.generation < n)
                && !s.binding_unreadable
                && (newest.parent.is_none() || s.parent != newest.parent)
        })
        && state.disk_size == sized
        && sized == case.export_size;
    holds.then_some(state.parent)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn decide_incremental_implies_every_condition(raw in raw()) {
        let case = build(&raw);
        let decision = case.decide();
        match decision.plan {
            ReadPlan::Incremental { parent } => {
                prop_assert_eq!(everything_holds(&case), Some(parent));
            }
            ReadPlan::Full(_) => prop_assert_eq!(everything_holds(&case), None),
        }
        let next = case.bitmap.filter(|_| case.export_has_next).map(|n| names().binding(n + 1));
        prop_assert_eq!(decision.record, next);
        prop_assert!(decision.restart > case.bitmap.unwrap_or(0));
        for s in &case.instance {
            if let Some(b) = &s.binding {
                prop_assert!(decision.restart > b.generation);
            }
        }
    }

    #[test]
    fn bitmap_names_round_trip_for_any_generation(
        repo in any::<[u8; 16]>(),
        host in prop::collection::vec(any::<u8>(), 0..20),
        generation in 1u64..u64::MAX,
    ) {
        let names = BitmapNames::new(&repo, &host, &key());
        prop_assert_eq!(names.parse(&names.name(generation)), Some(generation));
        prop_assert_eq!(names.binding(generation).generation, generation);
    }

    #[test]
    fn image_state_round_trips_for_any_state(
        parent in any::<[u8; 32]>(),
        source in any::<[u8; 32]>(),
        generation in 1u64..,
        disk_size in 0..=MAX_IMAGE_SIZE,
        host in prop::collection::vec(any::<u8>(), 0..20),
    ) {
        let s = ImageState { parent, bitmap_gen: generation, disk_size, source };
        let bytes = s.encode(&REPO, &host, &key());
        prop_assert!(bytes.len() <= MAX_STATE_BYTES);
        prop_assert_eq!(ImageState::parse(&bytes, &REPO, &host, &key()).unwrap(), s);
    }

    #[test]
    fn image_binding_meta_round_trips_for_any_binding(
        generation in 1u64..,
        reason in prop::option::of(0usize..11),
        size in 0..=MAX_IMAGE_SIZE,
    ) {
        let binding = names().binding(generation.min(u64::MAX - 1));
        let reason = reason.map(|i| FullReason::ALL[i]);
        let meta = ImageMeta { read_mode: ReadMode::Full, disk_size: size }
            .to_meta_bound("disk", Some(&binding), reason);
        let encoded = source::encode(&key(), &meta).unwrap().unwrap();
        let (_, decoded) = source::decode(Some(&encoded)).unwrap();
        prop_assert_eq!(BitmapBinding::from_meta(&decoded, "disk").unwrap(), Some(binding));
        prop_assert_eq!(FullReason::from_meta(&decoded, "disk").unwrap(), reason);
        prop_assert_eq!(ImageMeta::from_meta(&decoded, "disk").unwrap().disk_size, size);
    }
}
