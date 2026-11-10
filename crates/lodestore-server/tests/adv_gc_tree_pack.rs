//! A holder of the repository's writer keys backs up content no other
//! snapshot shares, and the packs holding it, among them the pack of its root
//! tree, vanish from the bucket. The server's collection must quarantine that
//! one snapshot instead of stopping the whole walk, delete nothing, and finish
//! once an admin excludes it, with the other snapshot still restorable. Only
//! the API of the first release is used, so the scenario can run against that
//! release unchanged.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;
mod gc_support;

use std::collections::BTreeSet;

use adv_support::{attacker_keys, open_all_shards};
use common::{Backend, TestServer, backends};
use gc_support::{
    PASSWORD, assert_stops_then_finishes, backup, enable, fresh_repo, kept_and_forgotten,
    pack_listing,
};
use lodestore_format::crypto::{BLOB_TYPE_TREE, BlobId};
use lodestore_format::index::parse_hex32;
use lodestore_format::pack::{pack_object_key, parse_entries};
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::{open_snapshot, snapshot_object_key};
use lodestore_format::store::{BlobStore, LocalFsStore};

async fn root_tree_of(store: &LocalFsStore, repo: &Repository, snapshot_id: &str) -> BlobId {
    let id = parse_hex32(snapshot_id).unwrap();
    let bytes = store.get(&snapshot_object_key(&id)).await.unwrap();
    let snapshot = open_snapshot(
        repo.read_keys().hpke_private_key(),
        repo.read_keys().repo_id(),
        &id,
        &bytes,
    )
    .unwrap();
    snapshot.root_tree.try_into().unwrap()
}

/// The pack objects whose index records name `tree` as a tree blob.
async fn packs_of_tree(store: &LocalFsStore, repo: &Repository, tree: &BlobId) -> BTreeSet<String> {
    let mut packs = BTreeSet::new();
    for (_, shard) in open_all_shards(store, &attacker_keys(repo)).await {
        for record in &shard.packs {
            let holds = parse_entries(&record.entries)
                .unwrap()
                .iter()
                .any(|e| e.blob_type == BLOB_TYPE_TREE && &e.blob_id == tree);
            if holds {
                let pack_id: BlobId = record.pack_id.as_slice().try_into().unwrap();
                packs.insert(pack_object_key(&pack_id));
            }
        }
    }
    packs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_gc_quarantines_snapshot_with_missing_tree_pack() {
    let backend: Backend = backends().remove(0);
    let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
    let api = ts.api().await;
    let (repo_id, store, root) = fresh_repo(&ts).await;
    let kept = kept_and_forgotten(&ts, &store, &root).await;

    let before = pack_listing(&store).await;
    let lost_snapshot = backup(&ts, &store, "host-x", "lost", &[("x", 3, 200 << 10)]).await;
    let lost: BTreeSet<String> = pack_listing(&store)
        .await
        .into_iter()
        .filter(|key| !before.contains(key))
        .collect();
    assert!(!lost.is_empty(), "the backup wrote no new pack");

    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let lost_root = root_tree_of(&store, &repo, &lost_snapshot.id).await;
    let kept_root = root_tree_of(&store, &repo, &kept.id).await;
    let lost_root_packs = packs_of_tree(&store, &repo, &lost_root).await;
    assert!(
        !lost_root_packs.is_empty() && lost_root_packs.is_subset(&lost),
        "the root tree of the new snapshot is not only in the packs about to vanish: \
         {lost_root_packs:?} vs {lost:?}"
    );
    let kept_root_packs = packs_of_tree(&store, &repo, &kept_root).await;
    assert!(
        !kept_root_packs.is_empty() && kept_root_packs.is_disjoint(&lost),
        "the root tree of the kept snapshot shares a pack with the vanishing ones"
    );

    for key in &lost {
        std::fs::remove_file(root.join(key)).unwrap();
    }
    enable(&ts, &api, &repo_id).await;
    let planted = snapshot_object_key(&parse_hex32(&lost_snapshot.id).unwrap());
    assert_stops_then_finishes(&ts, &api, &repo_id, &store, &kept, &planted).await;
    ts.stop().await;
}
