//! Properties of `Index::add_shard` under generated shard sets: any load
//! order, `supersedes` cycles, self references and dangling references.

use std::collections::{BTreeSet, HashSet};

use lodestore_format::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy};
use lodestore_format::crypto::{HPKE_ENC_LEN, SEALED_KEY_LEN};
use lodestore_format::index::{Index, SupersedesTrust, open_shard, seal_shard_superseding};
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::pack::PACK_PREFIX_LEN;
use lodestore_format::pb;
use proptest::prelude::*;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

const PACKS: u8 = 4;
const BLOBS: u8 = 5;
const PACK_SIZE: u64 = 8192;
const OFFSETS: [u32; 3] = [PACK_PREFIX_LEN as u32, 1300, 1500];
const ENC_LENS: [u32; 2] = [60, 100];

type Extent = (BlobId, u32, u32, u32);

#[derive(Debug, Clone)]
struct EntrySpec {
    blob: u8,
    tree: bool,
    offset: u8,
    len: u8,
}

#[derive(Debug, Clone)]
struct RecordSpec {
    pack: u8,
    entries: Vec<EntrySpec>,
}

#[derive(Debug, Clone)]
struct ShardSpec {
    records: Vec<RecordSpec>,
    supersedes: Vec<u8>,
}

fn entry() -> impl Strategy<Value = EntrySpec> {
    (0..BLOBS, any::<bool>(), 0..3u8, 0..2u8).prop_map(|(blob, tree, offset, len)| EntrySpec {
        blob,
        tree,
        offset,
        len,
    })
}

fn record() -> impl Strategy<Value = RecordSpec> {
    (0..PACKS, prop::collection::vec(entry(), 0..4))
        .prop_map(|(pack, entries)| RecordSpec { pack, entries })
}

fn shard() -> impl Strategy<Value = ShardSpec> {
    (
        prop::collection::vec(record(), 0..3),
        prop::collection::vec(any::<u8>(), 0..4),
    )
        .prop_map(|(records, supersedes)| ShardSpec {
            records,
            supersedes,
        })
}

fn scenario() -> impl Strategy<Value = Vec<ShardSpec>> {
    prop::collection::vec(shard(), 1..7)
}

fn scenario_with_orders() -> impl Strategy<Value = (Vec<ShardSpec>, Vec<Vec<usize>>)> {
    scenario().prop_flat_map(|specs| {
        let n = specs.len();
        let order = Just((0..n).collect::<Vec<_>>()).prop_shuffle();
        (Just(specs), prop::collection::vec(order, 3))
    })
}

fn shard_id(i: usize) -> BlobId {
    [i as u8 + 1; 32]
}

fn pack_id(p: u8) -> BlobId {
    [p + 50; 32]
}

fn blob_id(b: u8) -> BlobId {
    [b + 100; 32]
}

fn blob_type(tree: bool) -> u8 {
    if tree { BLOB_TYPE_TREE } else { BLOB_TYPE_DATA }
}

/// A shard id is a hash of its bytes, so a cycle of real ids cannot be
/// written; the specs name shards by synthetic ids and only the decoded shard
/// goes through the seal and open path.
fn build(specs: &[ShardSpec]) -> Vec<(BlobId, pb::IndexShard)> {
    let keys = ReadKeys::derive(MasterKey::from_bytes([1; 32]), [2; 16]);
    let mut entropy = Entropy::from_rng(ChaCha20Rng::from_seed([3; 32]));
    let n = specs.len();
    specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let packs = spec
                .records
                .iter()
                .map(|r| {
                    let mut entries = Vec::new();
                    for e in &r.entries {
                        let enc_len = ENC_LENS[e.len as usize];
                        entries.extend_from_slice(&blob_id(e.blob));
                        entries.push(blob_type(e.tree));
                        entries.extend_from_slice(&OFFSETS[e.offset as usize].to_le_bytes());
                        entries.extend_from_slice(&enc_len.to_le_bytes());
                        entries.extend_from_slice(&(enc_len - 41).to_le_bytes());
                    }
                    pb::PackRecord {
                        pack_id: pack_id(r.pack).to_vec(),
                        hpke_enc: vec![0; HPKE_ENC_LEN],
                        sealed_pack_key: vec![0; SEALED_KEY_LEN],
                        pack_size: PACK_SIZE,
                        entries,
                    }
                })
                .collect();
            let supersedes = spec
                .supersedes
                .iter()
                .map(|v| match *v as usize % (n + 2) {
                    k if k < n => shard_id(k),
                    k if k == n => shard_id(i),
                    _ => [200; 32],
                })
                .collect();
            let (real, bytes) = seal_shard_superseding(
                keys.k_idx(),
                keys.repo_id(),
                packs,
                supersedes,
                &mut entropy,
            )
            .unwrap();
            let opened = open_shard(keys.k_idx(), keys.repo_id(), &real, &bytes).unwrap();
            (shard_id(i), opened)
        })
        .collect()
}

fn load(
    shards: &[(BlobId, pb::IndexShard)],
    order: &[usize],
    trust: Option<HashSet<BlobId>>,
    excluded: &HashSet<BlobId>,
) -> Index {
    let mut index = Index::default();
    if let Some(trusted) = trust {
        index.set_supersedes_trust(SupersedesTrust::Only(trusted));
    }
    index.exclude_packs(excluded.clone());
    for &i in order {
        let (id, shard) = &shards[i];
        index.add_shard(*id, shard).unwrap();
    }
    index
}

fn keys_in_play() -> impl Iterator<Item = (BlobId, u8)> {
    (0..BLOBS).flat_map(|b| [BLOB_TYPE_DATA, BLOB_TYPE_TREE].map(|t| (blob_id(b), t)))
}

fn extents_of(index: &Index, id: &BlobId, t: u8) -> BTreeSet<Extent> {
    index
        .locations(id, t)
        .iter()
        .map(|l| (l.pack_id, l.offset, l.enc_len, l.ulen))
        .collect()
}

/// Every `(extent, shard)` a blob has in shards, in packs not excluded.
fn placed(
    shards: &[(BlobId, pb::IndexShard)],
    excluded: &HashSet<BlobId>,
    id: &BlobId,
    t: u8,
) -> Vec<(Extent, BlobId)> {
    let mut out = Vec::new();
    for (sid, shard) in shards {
        for rec in &shard.packs {
            let pack: BlobId = rec.pack_id.as_slice().try_into().unwrap();
            if excluded.contains(&pack) {
                continue;
            }
            for e in lodestore_format::pack::parse_entries(&rec.entries).unwrap() {
                if &e.blob_id == id && e.blob_type == t {
                    out.push(((pack, e.offset, e.enc_len, e.ulen), *sid));
                }
            }
        }
    }
    out
}

fn superseded(shards: &[(BlobId, pb::IndexShard)]) -> HashSet<BlobId> {
    shards
        .iter()
        .flat_map(|(_, s)| s.supersedes.iter())
        .map(|s| BlobId::try_from(s.as_slice()).unwrap())
        .collect()
}

fn describe(specs: &[ShardSpec], orders: &[Vec<usize>]) -> String {
    format!("shards: {specs:#?}\norders: {orders:?}")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn index_location_does_not_depend_on_shard_order(
        (specs, orders) in scenario_with_orders()
    ) {
        let shards = build(&specs);
        let gone = superseded(&shards);
        let none = HashSet::new();
        let indexes: Vec<Index> = orders.iter().map(|o| load(&shards, o, None, &none)).collect();
        for (id, t) in keys_in_play() {
            let all = placed(&shards, &none, &id, t);
            let want: BTreeSet<Extent> = all.iter().map(|(e, _)| *e).collect();
            let current: BTreeSet<Extent> =
                all.iter().filter(|(_, s)| !gone.contains(s)).map(|(e, _)| *e).collect();
            for index in &indexes {
                prop_assert_eq!(
                    extents_of(index, &id, t),
                    want.clone(),
                    "{}",
                    describe(&specs, &orders)
                );
                let chosen = index.get(&id, t).map(|l| (l.pack_id, l.offset, l.enc_len, l.ulen));
                if want.is_empty() {
                    prop_assert!(chosen.is_none());
                    continue;
                }
                let chosen = chosen.expect("a placed blob is resolvable");
                if !current.is_empty() {
                    prop_assert!(
                        current.contains(&chosen),
                        "a superseded copy was chosen over a current one\n{}",
                        describe(&specs, &orders)
                    );
                }
                let pool = if current.is_empty() { &want } else { &current };
                if pool.len() == 1 {
                    prop_assert_eq!(&chosen, pool.iter().next().unwrap());
                }
            }
        }
    }

    #[test]
    fn index_never_loses_a_blob(
        (specs, orders) in scenario_with_orders(),
        excluded in prop::collection::vec(0..PACKS, 0..3)
    ) {
        let shards = build(&specs);
        let excluded: HashSet<BlobId> = excluded.into_iter().map(pack_id).collect();
        for order in &orders {
            let index = load(&shards, order, None, &excluded);
            for (id, t) in keys_in_play() {
                let placed_somewhere = !placed(&shards, &excluded, &id, t).is_empty();
                prop_assert_eq!(
                    index.get(&id, t).is_some(),
                    placed_somewhere,
                    "{}",
                    describe(&specs, &orders)
                );
            }
        }
    }

    #[test]
    fn index_ignores_untrusted_supersedes(
        (specs, orders) in scenario_with_orders(),
        trusted in prop::collection::vec(any::<bool>(), 6)
    ) {
        let shards = build(&specs);
        let trusted: HashSet<BlobId> = shards
            .iter()
            .enumerate()
            .filter(|(i, _)| trusted[*i])
            .map(|(_, (id, _))| *id)
            .collect();
        let stripped: Vec<(BlobId, pb::IndexShard)> = shards
            .iter()
            .map(|(id, s)| {
                let mut s = s.clone();
                if !trusted.contains(id) {
                    s.supersedes.clear();
                }
                (*id, s)
            })
            .collect();
        let none = HashSet::new();
        for order in &orders {
            let with = load(&shards, order, Some(trusted.clone()), &none);
            let without = load(&stripped, order, Some(trusted.clone()), &none);
            for (id, t) in keys_in_play() {
                prop_assert_eq!(
                    with.get(&id, t),
                    without.get(&id, t),
                    "{}",
                    describe(&specs, &orders)
                );
                prop_assert_eq!(with.locations(&id, t), without.locations(&id, t));
            }
        }
    }

    #[test]
    fn index_excluded_packs_stay_excluded(
        (specs, orders) in scenario_with_orders(),
        excluded in prop::collection::vec(0..PACKS, 1..3)
    ) {
        let shards = build(&specs);
        let excluded: HashSet<BlobId> = excluded.into_iter().map(pack_id).collect();
        for order in &orders {
            let index = load(&shards, order, None, &excluded);
            for pack in &excluded {
                prop_assert!(!index.has_pack(pack));
            }
            for (id, t) in keys_in_play() {
                for loc in index.locations(&id, t) {
                    prop_assert!(
                        !excluded.contains(&loc.pack_id),
                        "{}",
                        describe(&specs, &orders)
                    );
                }
            }
        }
    }
}
