//! Two inits into one folder at the same time: `config` is written last with
//! create-only semantics, so exactly one of them becomes the repository.

use lodestore_format::Error;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::LocalFsStore;

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

fn pending(entropy: &mut Entropy) -> PendingInit {
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(entropy, options).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_race_one_winner() {
    for round in 0..8 {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(tmp.path().join("repo"));
        let (mut e1, mut e2) = (Entropy::os(), Entropy::os());
        let (p1, p2) = (pending(&mut e1), pending(&mut e2));
        let ids = [*p1.read_keys().repo_id(), *p2.read_keys().repo_id()];

        let (r1, r2) = tokio::join!(
            p1.commit(&store, b"first", &mut e1),
            p2.commit(&store, b"second", &mut e2)
        );

        let (winner, loser_err, win_pw, lose_pw) = match (r1, r2) {
            (Ok(()), Err(e)) => (0, e, &b"first"[..], &b"second"[..]),
            (Err(e), Ok(())) => (1, e, &b"second"[..], &b"first"[..]),
            (a, b) => panic!("round {round}: expected one winner, got {a:?} and {b:?}"),
        };
        assert!(
            matches!(loser_err, Error::AlreadyInitialized),
            "round {round}: {loser_err:?}"
        );

        let repo = Repository::open(&store, win_pw).await.unwrap();
        assert_eq!(repo.read_keys().repo_id(), &ids[winner]);
        assert!(
            matches!(
                Repository::open(&store, lose_pw).await,
                Err(Error::WrongSecret)
            ),
            "round {round}: the loser's password must not open the repository"
        );
    }
}
