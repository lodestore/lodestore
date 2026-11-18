//! Canonical source keys: one spelling per identity, no two identities on one
//! key, and nothing outside the canonical form parses.

use crate::gc_support;

use gc_support::Fx;

use lodestore_format::Error;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::source::{
    self, MAX_KEY_LEN, MAX_META_ENTRIES, MAX_META_KEY_LEN, MAX_META_VALUE_LEN, Meta, SourceKey,
    SourceType,
};
use lodestore_format::writer::BackupOptions;
use proptest::prelude::*;

const UUID_A: &str = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";
const UUID_B: &str = "7f3e9a10-2b4c-4d5e-8f60-a1b2c3d4e5f6";

const HARD: [&str; 16] = [
    "a/b", "%", "%2F", "%2f", " ", "\n", "\0", "ü", "a", "A", "..", "~", "a%b", "x y", "@", ":",
];

fn uuid(bytes: [u8; 16]) -> String {
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[derive(Debug, Clone, PartialEq)]
enum Id {
    Files,
    Vm(String, [u8; 16]),
    Ct(String, [u8; 16]),
    Pg(u64, u32),
    Wal(u64),
    My([u8; 16], String),
}

fn build(id: &Id) -> SourceKey {
    match id {
        Id::Files => SourceKey::files(),
        Id::Vm(c, u) => SourceKey::proxmox_vm(c, &uuid(*u)).unwrap(),
        Id::Ct(c, u) => SourceKey::proxmox_ct(c, &uuid(*u)).unwrap(),
        Id::Pg(s, o) => SourceKey::postgres(*s, *o).unwrap(),
        Id::Wal(s) => SourceKey::postgres_wal(*s).unwrap(),
        Id::My(u, s) => SourceKey::mysql(&uuid(*u), s).unwrap(),
    }
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => prop::collection::vec(any::<char>(), 1..=24).prop_map(|c| c.into_iter().collect()),
        1 => prop::sample::select(HARD.to_vec()).prop_map(String::from),
    ]
}

fn small_uuid() -> impl Strategy<Value = [u8; 16]> {
    prop::array::uniform16(0u8..2)
}

fn small_num() -> impl Strategy<Value = u64> {
    prop_oneof![1u64..4, 1u64..]
}

fn ids() -> impl Strategy<Value = Id> {
    prop_oneof![
        Just(Id::Files),
        (text(), small_uuid()).prop_map(|(c, u)| Id::Vm(c, u)),
        (text(), small_uuid()).prop_map(|(c, u)| Id::Ct(c, u)),
        (small_num(), 1u32..4).prop_map(|(s, o)| Id::Pg(s, o)),
        small_num().prop_map(Id::Wal),
        (small_uuid(), text()).prop_map(|(u, s)| Id::My(u, s)),
    ]
}

fn metas() -> impl Strategy<Value = Meta> {
    prop::collection::btree_map("[a-z0-9_]{1,64}", "\\PC{0,64}", 0..=MAX_META_ENTRIES)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn source_keys_round_trip(id in ids()) {
        let key = build(&id);
        prop_assert_eq!(SourceKey::parse(key.as_str()).unwrap(), key.clone());
        prop_assert!(!key.as_str().contains(['@', ':']));
    }

    #[test]
    fn source_keys_are_injective(a in ids(), b in ids(), same in any::<bool>()) {
        let b = if same { a.clone() } else { b };
        prop_assert_eq!(build(&a) == build(&b), a == b);
    }

    #[test]
    fn source_decode_round_trips(id in ids(), meta in metas()) {
        let key = build(&id);
        let encoded = source::encode(&key, &meta).unwrap();
        prop_assert_eq!(encoded.is_none(), key.is_files());
        let expected = if key.is_files() { Meta::new() } else { meta };
        prop_assert_eq!(source::decode(encoded.as_ref()).unwrap(), (key, expected));
    }

    #[test]
    fn source_parse_never_panics(text in "\\PC{0,600}") {
        if let Ok(key) = SourceKey::parse(&text) {
            prop_assert_eq!(SourceKey::parse(key.as_str()).unwrap(), key);
        }
    }
}

#[test]
fn source_parse_rejects_non_canonical() {
    let long_key = format!("mysql/{UUID_A}/{}", "%C3%BC".repeat(127));
    let table = [
        "files",
        "files/x",
        "oracle/x",
        "proxmox-vm",
        "proxmox-vm/pve1",
        "proxmox-vm/pve1/x/y",
        "proxmox-vm//0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d",
        "proxmox-vm/pve1/",
        "proxmox-vm/pve1/0B1C2D3E-4F50-4A6B-8C7D-9E0F1A2B3C4D",
        "proxmox-vm/pve1/0b1c2d3e4f504a6b8c7d9e0f1a2b3c4d",
        "proxmox-vm/pve1/101",
        "proxmox-ct/pve1/105",
        "proxmox-ct/pve1/web-ct",
        "proxmox-ct/pve1/0B1C2D3E-4F50-4A6B-8C7D-9E0F1A2B3C4D",
        "proxmox-ct/pve1/0b1c2d3e4f504a6b8c7d9e0f1a2b3c4d",
        "proxmox-ct/pve1",
        "postgres/0/1",
        "postgres/01/1",
        "postgres/1/0",
        "postgres/1/01",
        "postgres/1",
        "postgres/18446744073709551616/1",
        "postgres/1/4294967296",
        "postgres/a/1",
        "postgres/%31/1",
        "postgres/%2B1/1",
        "postgres/1/%2B1",
        "postgres-wal/%2B1",
        "postgres-wal/0",
        "postgres-wal/1/2",
        "mysql/abc/shop",
        "mysql/0B1C2D3E-4F50-4A6B-8C7D-9E0F1A2B3C4D/shop",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/a%2fb",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/%41",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/a%4",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/a%G1",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/a b",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/a@b",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/%FF",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/ü",
    ];
    for bad in table {
        assert!(
            matches!(SourceKey::parse(bad), Err(Error::InvalidInput(_))),
            "{bad:?} parsed"
        );
    }
    let segment_256 = format!("mysql/{UUID_A}/{}", "a".repeat(256));
    assert!(matches!(
        SourceKey::parse(&segment_256),
        Err(Error::InvalidInput(m)) if m.contains("1 to 255")
    ));
    assert!(matches!(
        SourceKey::parse("mysql/abc/shop"),
        Err(Error::InvalidInput(m)) if m.contains("server is identified by its uuid")
    ));
    assert!(matches!(
        SourceKey::parse(&long_key),
        Err(Error::InvalidInput(m)) if m.contains("too long")
    ));
    let prefix = format!("mysql/{UUID_A}/");
    let fill = MAX_KEY_LEN - prefix.len();
    let at_limit = format!("{prefix}{}{}", "%2F".repeat(fill / 3), "a".repeat(fill % 3));
    assert_eq!(at_limit.len(), MAX_KEY_LEN);
    assert!(SourceKey::parse(&at_limit).is_ok());
    assert!(matches!(
        SourceKey::parse(&format!("{at_limit}a")),
        Err(Error::InvalidInput(m)) if m.contains("too long")
    ));
    let segment_255 = format!("mysql/{UUID_A}/{}", "a".repeat(255));
    assert_eq!(
        SourceKey::parse(&segment_255).unwrap().kind(),
        SourceType::Mysql
    );
    assert!(SourceKey::parse("").unwrap().is_files());
}

#[test]
fn source_keys_have_one_spelling_per_identity() {
    let vm = SourceKey::proxmox_vm("pve1", UUID_A).unwrap();
    assert_eq!(vm.as_str(), format!("proxmox-vm/pve1/{UUID_A}"));
    assert_ne!(vm, SourceKey::proxmox_vm("pve1", UUID_B).unwrap());
    assert_ne!(vm, SourceKey::proxmox_ct("pve1", UUID_A).unwrap());
    assert_eq!(
        SourceKey::mysql(UUID_A, "a/b").unwrap().as_str(),
        format!("mysql/{UUID_A}/a%2Fb")
    );
    assert_ne!(
        SourceKey::postgres(7, 16384).unwrap(),
        SourceKey::postgres(7, 16385).unwrap()
    );
    assert_ne!(
        SourceKey::postgres(7, 1).unwrap(),
        SourceKey::postgres_wal(7).unwrap()
    );
    assert!(SourceKey::postgres_wal(7).unwrap().kind().is_wal_batch());
    assert!(SourceKey::proxmox_ct("pve1", "105").is_err());
    assert!(SourceKey::proxmox_vm("pve1", &UUID_A.to_uppercase()).is_err());
    assert!(SourceKey::postgres(0, 1).is_err());
    assert!(SourceKey::postgres(1, 0).is_err());
    assert_eq!(SourceKey::files().to_string(), "files");
}

fn raw(key: &str, meta: &[(&str, &str)]) -> pb::Source {
    pb::Source {
        key: key.into(),
        meta: meta
            .iter()
            .map(|(k, v)| pb::SourceMeta {
                key: (*k).into(),
                value: (*v).into(),
            })
            .collect(),
    }
}

#[test]
fn source_meta_limits() {
    let key = format!("proxmox-vm/pve1/{UUID_A}");
    let many = |n: usize| -> Vec<(String, String)> {
        (0..n).map(|i| (format!("k{i:02}"), "v".into())).collect()
    };
    let with = |entries: &[(String, String)]| {
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        raw(&key, &pairs)
    };
    assert!(source::decode(Some(&with(&many(MAX_META_ENTRIES)))).is_ok());
    assert!(source::decode(Some(&with(&many(MAX_META_ENTRIES + 1)))).is_err());

    let ok = |k: &str, v: &str| source::decode(Some(&raw(&key, &[(k, v)]))).is_ok();
    assert!(ok(&"a".repeat(MAX_META_KEY_LEN), "v"));
    assert!(!ok(&"a".repeat(MAX_META_KEY_LEN + 1), "v"));
    assert!(!ok("", "v"));
    assert!(!ok("A", "v"));
    assert!(!ok("a-b", "v"));
    assert!(ok("a_9", &"v".repeat(MAX_META_VALUE_LEN)));
    assert!(!ok("a", &"v".repeat(MAX_META_VALUE_LEN + 1)));

    assert!(source::decode(Some(&raw(&key, &[("b", "1"), ("a", "2")]))).is_err());
    assert!(source::decode(Some(&raw(&key, &[("a", "1"), ("a", "2")]))).is_err());
    assert!(source::decode(Some(&raw(&key, &[("a", "1"), ("b", "2")]))).is_ok());

    let parsed = SourceKey::parse(&key).unwrap();
    let too_many: Meta = many(MAX_META_ENTRIES + 1).into_iter().collect();
    assert!(source::encode(&parsed, &too_many).is_err());
    let bad_key = Meta::from([("A".into(), "v".into())]);
    assert!(source::encode(&parsed, &bad_key).is_err());
    let long_value = Meta::from([("a".into(), "v".repeat(MAX_META_VALUE_LEN + 1))]);
    assert!(source::encode(&parsed, &long_value).is_err());

    let (files, meta) = source::decode(Some(&raw("", &[("b", "1"), ("a", "2")]))).unwrap();
    assert!(files.is_files() && meta.is_empty());
    assert_eq!(source::decode(None).unwrap().0, SourceKey::files());
    assert_eq!(
        source::encode(&SourceKey::files(), &too_many).unwrap(),
        None
    );
}

#[test]
fn source_labels() {
    let meta = |pairs: &[(&str, &str)]| -> Meta {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let vm = SourceKey::proxmox_vm("pve1", UUID_A).unwrap();
    let ct = SourceKey::proxmox_ct("pve1", UUID_B).unwrap();
    let pg = SourceKey::postgres(7, 16384).unwrap();
    let my = SourceKey::mysql(UUID_A, "shop").unwrap();
    let label = |k: &SourceKey, m: &Meta| source::label(k, m);

    assert_eq!(
        label(&vm, &meta(&[("vmid", "101"), ("name", "web-01")])).as_deref(),
        Some("VM 101 · web-01")
    );
    assert_eq!(
        label(&vm, &meta(&[("vmid", "101")])).as_deref(),
        Some("VM 101")
    );
    assert_eq!(label(&vm, &meta(&[("name", "web-01")])), None);
    assert_eq!(
        label(&ct, &meta(&[("vmid", "105"), ("name", "dns")])).as_deref(),
        Some("CT 105 · dns")
    );
    assert_eq!(
        label(
            &pg,
            &meta(&[("database", "app"), ("server_version", "16.4")])
        )
        .as_deref(),
        Some("app · PostgreSQL 16")
    );
    assert_eq!(
        label(&my, &meta(&[("server_version", "8.4.2")])).as_deref(),
        Some("shop · MySQL 8")
    );
    assert_eq!(label(&my, &Meta::new()).as_deref(), Some("shop"));
    assert_eq!(label(&SourceKey::files(), &meta(&[("vmid", "1")])), None);
    assert_eq!(
        label(
            &SourceKey::postgres_wal(7).unwrap(),
            &meta(&[("database", "a")])
        ),
        None
    );
}

#[tokio::test]
async fn writer_records_the_claimed_source() {
    let fx = Fx::new().await;
    let key = SourceKey::proxmox_vm("pve1", UUID_A).unwrap();
    let meta = Meta::from([("vmid".into(), "101".into())]);
    let claimed = source::encode(&key, &meta).unwrap();
    assert!(claimed.is_some());

    let mut w = fx.open_writer().await;
    let src = fx.make_src("claimed", &[("f", 1, 1000)]);
    let options = BackupOptions {
        source: claimed.clone(),
        ..Default::default()
    };
    let with = w.backup(&[src], options, &|_| {}).await.unwrap();
    let src = fx.make_src("plain", &[("f", 2, 1000)]);
    let without = w
        .backup(&[src], BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    let options = BackupOptions {
        source: claimed.clone(),
        ..Default::default()
    };
    let stream = w
        .backup_stream(b"dump", &mut &b"rows"[..], options, &|_| {})
        .await
        .unwrap();

    let reader =
        Reader::open(&fx.store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    let recorded = reader.snapshot(&with.snapshot_id).await.unwrap();
    assert_eq!(recorded.source, claimed);
    assert_eq!(
        source::decode(recorded.source.as_ref()).unwrap(),
        (key, meta)
    );
    assert_eq!(
        reader.snapshot(&without.snapshot_id).await.unwrap().source,
        None
    );
    assert_eq!(
        reader.snapshot(&stream.snapshot_id).await.unwrap().source,
        claimed
    );
}
