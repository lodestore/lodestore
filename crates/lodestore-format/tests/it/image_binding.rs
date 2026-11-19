//! The binding between a dirty bitmap and the parent snapshot: what a
//! snapshot records, the names of bitmaps, the state cache and the one
//! function that decides between an incremental and a full read.

use lodestore_format::crypto::{BlobId, RepoId};
use lodestore_format::image::{
    BitmapBinding, BitmapNames, DecideInput, Decision, FullReason, ImageMeta, ImageState,
    InstanceSnapshot, MAX_STATE_BYTES, ReadMode, ReadPlan, StateRead, source_identity,
};
use lodestore_format::source::{self, Meta, SourceKey};

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
