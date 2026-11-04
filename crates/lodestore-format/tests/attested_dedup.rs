//! A host with writer credentials plants an index shard that claims blobs a
//! later backup will need; once the server has upgraded the repository the
//! writer must not trust it.

mod adv_support;
mod gc_support;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use adv_support::{attacker_keys, open_all_shards, plant_blobs, plant_shard_where};
use gc_support::{Fx, assert_same_files};
use lodestore_format::attest::ensure_upgraded;
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::index::index_object_key;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::store::BlobDelete;
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const FILE_SIZE: usize = 100 << 10;

fn content(seed: u8) -> Vec<u8> {
    let mut data = vec![0u8; FILE_SIZE];
    ChaCha20Rng::from_seed([seed; 32]).fill_bytes(&mut data);
    data
}

fn src_with(fx: &Fx, label: &str, seed: u8) -> PathBuf {
    let src = fx.tmp.path().join(format!("src-{label}"));
    fs::create_dir(&src).unwrap();
    fs::write(src.join("file"), content(seed)).unwrap();
    src
}

async fn upgrade(fx: &Fx) {
    ensure_upgraded(&fx.store, fx.repo.read_keys(), &mut Entropy::os())
        .await
        .unwrap();
}

async fn attested_writer<'a>(
    fx: &'a Fx,
    own: &HashSet<BlobId>,
) -> Writer<&'a lodestore_format::store::LocalFsStore> {
    let keys = fx.repo.read_keys().write_keys(fx.repo.config_hash());
    Writer::connect_attested(&fx.store, keys, own)
        .await
        .unwrap()
}

async fn backup(
    w: &mut Writer<&lodestore_format::store::LocalFsStore>,
    src: &PathBuf,
) -> BackupOutcome {
    w.backup(std::slice::from_ref(src), BackupOptions::default(), &|_| {})
        .await
        .unwrap()
}

async fn assert_restores(fx: &Fx, out: &BackupOutcome, src: &PathBuf) {
    let mut reader =
        Reader::open(&fx.store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    let target = tempfile::tempdir_in(fx.tmp.path()).unwrap();
    let report = reader
        .restore(
            &out.snapshot_id,
            target.path(),
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let canonical = src.canonicalize().unwrap();
    assert_same_files(
        src,
        &target.path().join(canonical.strip_prefix("/").unwrap()),
    );
}

#[tokio::test]
async fn attested_dedup_skips_planted_shard() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 200 << 10)]).await;
    upgrade(&fx).await;

    let keys = attacker_keys(&fx.repo);
    let future = blob_id(&keys.k_id, &content(7));
    plant_blobs(&fx.store, &keys, &[(future, BLOB_TYPE_DATA)]).await;

    let src = src_with(&fx, "host-b", 7);
    let mut w = attested_writer(&fx, &HashSet::new()).await;
    let out = backup(&mut w, &src).await;
    assert!(out.stats.bytes_added as usize >= FILE_SIZE);
    assert_restores(&fx, &out, &src).await;
}

#[tokio::test]
async fn attested_dedup_reuses_own_unattested_shards() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 200 << 10)]).await;
    upgrade(&fx).await;

    let src = fx.make_src("host-b", &[("b", 2, 2 << 20)]);
    let mut first = attested_writer(&fx, &HashSet::new()).await;
    let out = backup(&mut first, &src).await;
    let own: HashSet<BlobId> = first.written_shards().iter().copied().collect();
    assert!(!own.is_empty());
    assert!(own.is_disjoint(first.attested_shards()));

    let mut second = attested_writer(&fx, &own).await;
    let again = backup(&mut second, &src).await;
    assert!(again.stats.bytes_added * 100 <= out.stats.bytes_added);

    let mut forgetful = attested_writer(&fx, &HashSet::new()).await;
    let redone = backup(&mut forgetful, &src).await;
    assert!(redone.stats.bytes_added * 2 > out.stats.bytes_added);
    assert_restores(&fx, &again, &src).await;
}

#[tokio::test]
async fn attested_dedup_without_marker_loads_everything() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 200 << 10)]).await;

    let keys = attacker_keys(&fx.repo);
    let future = blob_id(&keys.k_id, &content(7));
    plant_blobs(&fx.store, &keys, &[(future, BLOB_TYPE_DATA)]).await;

    let src = src_with(&fx, "host-b", 7);
    let mut w = attested_writer(&fx, &HashSet::new()).await;
    let out = backup(&mut w, &src).await;
    assert!((out.stats.bytes_added as usize) < FILE_SIZE / 2);
}

#[tokio::test]
async fn attested_writer_ignores_every_supersedes() {
    let fx = Fx::new().await;
    let src = src_with(&fx, "host-a", 5);
    let first = {
        let mut w = fx.open_writer().await;
        backup(&mut w, &src).await
    };
    let keys = attacker_keys(&fx.repo);
    let real = open_all_shards(&fx.store, &keys).await;
    assert_eq!(real.len(), 1);
    let real_id = real[0].0;

    let data = blob_id(&keys.k_id, &content(5));
    let planted = plant_blobs(&fx.store, &keys, &[(data, BLOB_TYPE_DATA)]).await;
    let donor = open_all_shards(&fx.store, &keys)
        .await
        .into_iter()
        .find(|(id, _)| *id == planted)
        .unwrap();
    fx.store.delete(&index_object_key(&planted)).await.unwrap();
    plant_shard_where(
        &fx.store,
        &keys,
        donor.1.packs.clone(),
        vec![real_id],
        |id| *id > real_id,
    )
    .await;
    upgrade(&fx).await;

    let mut plain = fx.open_writer().await;
    let via_new = backup(&mut plain, &src).await;
    assert_restores(&fx, &via_new, &src).await;

    let mut attested = attested_writer(&fx, &HashSet::new()).await;
    let via_connect = backup(&mut attested, &src).await;
    assert_restores(&fx, &via_connect, &src).await;
    assert_restores(&fx, &first, &src).await;
}
