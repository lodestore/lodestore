use bytes::Bytes;
use subtle::ConstantTimeEq;

use crate::config;
use crate::crypto::Entropy;
use crate::error::{Error, Result};
use crate::fs::now;
use crate::keys::{
    KdfCost, MAX_ARGON2_TOTAL_WORK, MasterKey, ReadKeys, RecoveryKey, open_password_slot,
    open_recovery_slot, seal_password_slot, seal_recovery_slot, slot_kdf_work, slot_object_name,
};
use crate::pb;
use crate::store::{BlobDelete, BlobError, BlobStore, put_named};

const CONFIG_KEY: &str = "config";
const KEYS_PREFIX: &str = "keys/";
const LAYOUT_PREFIXES: [&str; 4] = ["packs", "index", "snapshots", "locks"];

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub kdf: KdfCost,
    pub program_version: String,
    pub created: Option<pb::Timestamp>,
}

impl Default for InitOptions {
    fn default() -> Self {
        InitOptions {
            kdf: KdfCost::default(),
            program_version: format!("lodestore-format {}", env!("CARGO_PKG_VERSION")),
            created: None,
        }
    }
}

/// A repository that exists only in memory. Nothing is written until
/// [`PendingInit::commit`], so a caller can show the recovery key and wait for
/// confirmation first.
pub struct PendingInit {
    keys: ReadKeys,
    recovery: RecoveryKey,
    config: pb::Config,
    config_bytes: Vec<u8>,
    kdf: KdfCost,
    created: pb::Timestamp,
}

impl PendingInit {
    pub fn generate(entropy: &mut Entropy, options: InitOptions) -> Result<Self> {
        let master = MasterKey::generate(entropy);
        let repo_id = entropy.bytes::<16>();
        let keys = ReadKeys::derive(master, repo_id);
        let recovery = RecoveryKey::generate(entropy);
        let created = options.created.unwrap_or_else(now);
        let config = config::new_config(
            repo_id,
            keys.public_key_bytes(),
            created,
            &options.program_version,
        );
        let config_bytes = config::encode(&config);
        Ok(PendingInit {
            keys,
            recovery,
            config,
            config_bytes,
            kdf: options.kdf,
            created,
        })
    }

    pub fn read_keys(&self) -> &ReadKeys {
        &self.keys
    }

    pub fn recovery_key(&self) -> &RecoveryKey {
        &self.recovery
    }

    pub fn config(&self) -> &pb::Config {
        &self.config
    }

    pub fn config_bytes(&self) -> &[u8] {
        &self.config_bytes
    }

    /// Fails unless `store` holds neither a `config` nor key slots. Slots
    /// without a `config` mean an earlier init was interrupted; they are never
    /// cleaned up automatically.
    pub async fn ensure_uninitialized<S: BlobStore>(store: &S) -> Result<()> {
        Self::ensure_no_config(store).await?;
        if !store.list(KEYS_PREFIX).await?.is_empty() {
            return Err(Error::InvalidInput(
                "keys/ is not empty but there is no config: an earlier init was interrupted; remove keys/ and run init again"
                    .into(),
            ));
        }
        Ok(())
    }

    async fn ensure_no_config<S: BlobStore>(store: &S) -> Result<()> {
        match store.get(CONFIG_KEY).await {
            Ok(_) => Err(Error::AlreadyInitialized),
            Err(BlobError::NotFound) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes the key slots, creates the layout and writes `config` last. A
    /// repository without `config` is not a repository, so an interrupted
    /// init leaves nothing that opens.
    ///
    /// Only `config` decides who wins when two inits race: a loser always gets
    /// `AlreadyInitialized`, and its slots stay behind unusable, because they
    /// are bound to its own repository id. Callers that want to refuse leftover
    /// slots check [`PendingInit::ensure_uninitialized`] first.
    pub async fn commit<S: BlobStore>(
        self,
        store: &S,
        password: &[u8],
        entropy: &mut Entropy,
    ) -> Result<()> {
        Self::ensure_no_config(store).await?;

        let password_slot = seal_password_slot(
            self.keys.master(),
            self.keys.repo_id(),
            password,
            self.kdf,
            Some(self.created),
            entropy,
        )?;
        let recovery_slot = seal_recovery_slot(
            self.keys.master(),
            self.keys.repo_id(),
            &self.recovery,
            Some(self.created),
            entropy,
        )?;
        for slot in [password_slot, recovery_slot] {
            put_named(store, &slot_object_name(&slot), slot).await?;
        }
        store.ensure_prefixes(&LAYOUT_PREFIXES).await?;

        match store
            .put_if_absent(CONFIG_KEY, Bytes::from(self.config_bytes.clone()))
            .await
        {
            Ok(()) => Ok(()),
            // A retried or timed-out PUT may have landed; the config is ours
            // only if these exact bytes are there.
            Err(_) if self.config_is_stored(store).await => Ok(()),
            Err(BlobError::AlreadyExists) => Err(Error::AlreadyInitialized),
            Err(e) => Err(e.into()),
        }
    }

    async fn config_is_stored<S: BlobStore>(&self, store: &S) -> bool {
        matches!(store.get(CONFIG_KEY).await, Ok(b) if b[..] == self.config_bytes[..])
    }
}

/// Result of [`Repository::change_password`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdOutcome {
    pub new_slot: String,
    pub old_slot_removed: bool,
    /// Why the old slot is still there, when it could not be removed.
    pub warning: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenedWith {
    Password,
    Recovery,
}

/// An opened repository: validated `config` plus the keys its slot unlocked.
pub struct Repository {
    config: pb::Config,
    config_bytes: Vec<u8>,
    keys: ReadKeys,
    opened_slot: String,
    opened_with: OpenedWith,
    skipped_slots: Vec<(String, String)>,
}

impl Repository {
    /// Opens with the repository password; only password slots are tried.
    pub async fn open<S: BlobStore>(store: &S, password: &[u8]) -> Result<Self> {
        Self::open_with(OpenedWith::Password, store, |slot, repo_id| {
            open_password_slot(slot, repo_id, password)
        })
        .await
    }

    /// Opens with a recovery key; only recovery slots are tried.
    pub async fn open_recovery<S: BlobStore>(store: &S, key: &RecoveryKey) -> Result<Self> {
        Self::open_with(OpenedWith::Recovery, store, |slot, repo_id| {
            open_recovery_slot(slot, repo_id, key)
        })
        .await
    }

    async fn open_with<S: BlobStore>(
        opened_with: OpenedWith,
        store: &S,
        unlock: impl Fn(&[u8], &crate::crypto::RepoId) -> Result<Option<MasterKey>>,
    ) -> Result<Self> {
        let config_bytes = match store.get(CONFIG_KEY).await {
            Ok(b) => b.to_vec(),
            Err(BlobError::NotFound) => return Err(Error::NotInitialized),
            Err(e) => return Err(e.into()),
        };
        let config = config::decode(&config_bytes)?;
        config::check_read_version(&config)?;
        config::check_suite(&config)?;
        let repo_id = config::repo_id_of(&config)?;

        let mut skipped = Vec::new();
        let mut candidates = Vec::new();
        for entry in store.list(KEYS_PREFIX).await? {
            let bytes = store.get(&entry.key).await?;
            if slot_object_name(&bytes) != entry.key {
                let e = Error::corrupt(entry.key.clone(), "object name does not match its content");
                skipped.push((entry.key, e.to_string()));
                continue;
            }
            candidates.push((slot_kdf_work(&bytes), entry.key, bytes));
        }
        candidates.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut tried = false;
        let mut spent = 0u64;
        let mut over_budget = false;
        for (work, key, bytes) in candidates {
            let Some(total) = spent
                .checked_add(work)
                .filter(|t| *t <= MAX_ARGON2_TOTAL_WORK)
            else {
                over_budget = true;
                break;
            };
            spent = total;
            match unlock(&bytes, &repo_id) {
                Ok(Some(master)) => {
                    let keys = ReadKeys::derive(master, repo_id);
                    let pinned = config.hpke.as_ref().map(|h| h.public_key.as_slice());
                    if pinned != Some(keys.public_key_bytes().as_slice()) {
                        return Err(Error::corrupt(
                            "config",
                            "public key does not match the key unlocked by the slot",
                        ));
                    }
                    return Ok(Repository {
                        config,
                        config_bytes,
                        keys,
                        opened_slot: key,
                        opened_with,
                        skipped_slots: skipped,
                    });
                }
                Ok(None) => tried = true,
                Err(e @ Error::Corrupt { .. }) => skipped.push((key, e.to_string())),
                Err(e) => return Err(e),
            }
        }
        if over_budget {
            return Err(Error::corrupt(
                "keys/",
                format!(
                    "the key slots ask for more key-derivation work than one opening may spend ({MAX_ARGON2_TOTAL_WORK}); remove the key slots that are not needed"
                ),
            ));
        }
        match skipped.into_iter().next() {
            Some((object, reason)) if !tried => Err(Error::Corrupt { object, reason }),
            _ => Err(Error::WrongSecret),
        }
    }

    pub fn config(&self) -> &pb::Config {
        &self.config
    }

    pub fn config_bytes(&self) -> &[u8] {
        &self.config_bytes
    }

    pub fn config_hash(&self) -> [u8; 32] {
        config::config_hash(&self.config_bytes)
    }

    pub fn read_keys(&self) -> &ReadKeys {
        &self.keys
    }

    /// `(object key, reason)` of every key slot that was left out because it
    /// is damaged or planted.
    pub fn skipped_slots(&self) -> &[(String, String)] {
        &self.skipped_slots
    }

    /// Object key of the slot that opened this repository.
    pub fn opened_slot(&self) -> &str {
        &self.opened_slot
    }

    /// Writes one more password slot; the existing slots stay as they are.
    /// Returns the new slot's object key.
    pub async fn add_password_slot<S: BlobStore>(
        &self,
        store: &S,
        password: &[u8],
        entropy: &mut Entropy,
    ) -> Result<String> {
        let slot = seal_password_slot(
            self.keys.master(),
            self.keys.repo_id(),
            password,
            KdfCost::default(),
            Some(now()),
            entropy,
        )?;
        put_slot(store, slot).await
    }

    /// Writes one more recovery slot for `key`. Returns its object key.
    pub async fn add_recovery_slot<S: BlobStore>(
        &self,
        store: &S,
        key: &RecoveryKey,
        entropy: &mut Entropy,
    ) -> Result<String> {
        let slot = seal_recovery_slot(
            self.keys.master(),
            self.keys.repo_id(),
            key,
            Some(now()),
            entropy,
        )?;
        put_slot(store, slot).await
    }

    /// Replaces the password slot this repository was opened with: the new
    /// slot is written and read back with the new password first, and only
    /// then is the old one deleted. A refused delete is reported in the
    /// outcome, not as an error, because the new password already works.
    /// Recovery slots are never touched.
    pub async fn change_password<S: BlobStore + BlobDelete>(
        &self,
        store: &S,
        new_password: &[u8],
        entropy: &mut Entropy,
    ) -> Result<PasswdOutcome> {
        if self.opened_with == OpenedWith::Recovery {
            return Err(Error::InvalidInput(
                "the repository was opened with a recovery key, which has no password slot to replace; open it with the password, or add a password with `lode key add`"
                    .into(),
            ));
        }
        let new_slot = self.add_password_slot(store, new_password, entropy).await?;

        let stored = store.get(&new_slot).await?;
        let reopened = open_password_slot(&stored, self.keys.repo_id(), new_password)?;
        let same_key = reopened
            .as_ref()
            .is_some_and(|m| bool::from(m.as_bytes().ct_eq(self.keys.master().as_bytes())));
        if !same_key {
            return Err(Error::corrupt(
                new_slot,
                "the new slot does not open the repository; the old password slot was kept",
            ));
        }

        let (old_slot_removed, warning) = match store.delete(&self.opened_slot).await {
            Ok(()) | Err(BlobError::NotFound) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        Ok(PasswdOutcome {
            new_slot,
            old_slot_removed,
            warning,
        })
    }
}

async fn put_slot<S: BlobStore>(store: &S, slot: Vec<u8>) -> Result<String> {
    let name = slot_object_name(&slot);
    put_named(store, &name, slot).await?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ops::Range;
    use std::sync::Mutex;

    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::SeedableRng;

    use super::*;
    use crate::store::{ListEntry, LocalFsStore};

    struct Recording<S> {
        inner: S,
        log: Mutex<Vec<String>>,
    }

    impl<S> Recording<S> {
        fn log(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }
    }

    impl<S: BlobStore> BlobStore for Recording<S> {
        async fn put_if_absent(
            &self,
            key: &str,
            data: Bytes,
        ) -> std::result::Result<(), BlobError> {
            self.log(format!("put {key}"));
            self.inner.put_if_absent(key, data).await
        }

        async fn get(&self, key: &str) -> std::result::Result<Bytes, BlobError> {
            self.inner.get(key).await
        }

        async fn get_range(
            &self,
            key: &str,
            range: Range<u64>,
        ) -> std::result::Result<Bytes, BlobError> {
            self.inner.get_range(key, range).await
        }

        async fn list(&self, prefix: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
            self.inner.list(prefix).await
        }

        async fn ensure_prefixes(&self, prefixes: &[&str]) -> std::result::Result<(), BlobError> {
            self.log(format!("prefixes {}", prefixes.join(",")));
            self.inner.ensure_prefixes(prefixes).await
        }
    }

    fn seeded(seed: u8) -> Entropy {
        Entropy::from_rng(ChaCha20Rng::from_seed([seed; 32]))
    }

    fn low_options() -> InitOptions {
        InitOptions {
            kdf: KdfCost {
                memory_kib: 64,
                iterations: 1,
                parallelism: 1,
            },
            ..InitOptions::default()
        }
    }

    #[tokio::test]
    async fn repo_init_open_local() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let store = Recording {
            inner: LocalFsStore::new(root.clone()),
            log: Mutex::new(Vec::new()),
        };
        let mut ent = seeded(1);

        assert!(matches!(
            Repository::open(&store, b"pw").await,
            Err(Error::NotInitialized)
        ));

        let pending = PendingInit::generate(&mut ent, low_options()).unwrap();
        let repo_id = *pending.read_keys().repo_id();
        let public_key = pending.read_keys().public_key_bytes();
        pending.commit(&store, b"pw", &mut ent).await.unwrap();

        let names: BTreeSet<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let expected: BTreeSet<String> = ["config", "keys", "packs", "index", "snapshots", "locks"]
            .map(String::from)
            .into();
        assert_eq!(names, expected);
        assert_eq!(std::fs::read_dir(root.join("keys")).unwrap().count(), 2);

        let log = store.log.lock().unwrap().clone();
        assert_eq!(log.last().map(String::as_str), Some("put config"));
        assert_eq!(log.iter().filter(|l| l.starts_with("put keys/")).count(), 2);
        let slot_at = log.iter().position(|l| l.starts_with("put keys/")).unwrap();
        let prefixes_at = log.iter().position(|l| l.starts_with("prefixes")).unwrap();
        assert!(slot_at < prefixes_at && prefixes_at < log.len() - 1);

        let repo = Repository::open(&store, b"pw").await.unwrap();
        assert_eq!(repo.read_keys().repo_id(), &repo_id);
        assert_eq!(repo.read_keys().public_key_bytes(), public_key);
        assert_eq!(repo.config().hpke.as_ref().unwrap().public_key, public_key);

        assert!(matches!(
            Repository::open(&store, b"other").await,
            Err(Error::WrongSecret)
        ));

        let config_before = std::fs::read(root.join("config")).unwrap();
        let again = PendingInit::generate(&mut ent, low_options()).unwrap();
        assert!(matches!(
            again.commit(&store, b"pw2", &mut ent).await,
            Err(Error::AlreadyInitialized)
        ));
        assert_eq!(std::fs::read(root.join("config")).unwrap(), config_before);
        assert_eq!(std::fs::read_dir(root.join("keys")).unwrap().count(), 2);
        assert!(Repository::open(&store, b"pw").await.is_ok());
    }

    #[tokio::test]
    async fn recovery_open_with_key_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(dir.path().join("repo"));
        let mut ent = seeded(6);
        let pending = PendingInit::generate(&mut ent, low_options()).unwrap();
        let key = pending.recovery_key().clone();
        let repo_id = *pending.read_keys().repo_id();
        pending.commit(&store, b"pw", &mut ent).await.unwrap();

        let repo = Repository::open_recovery(&store, &key).await.unwrap();
        assert_eq!(repo.read_keys().repo_id(), &repo_id);

        let other = RecoveryKey::generate(&mut ent);
        assert!(matches!(
            Repository::open_recovery(&store, &other).await,
            Err(Error::WrongSecret)
        ));
        // A recovery key text is not a password, and the password is not a recovery key.
        assert!(matches!(
            Repository::open(&store, key.encode().as_bytes()).await,
            Err(Error::WrongSecret)
        ));
    }

    #[tokio::test]
    async fn recovery_init_refuses_orphan_slots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("keys")).unwrap();
        std::fs::write(root.join("keys").join("0".repeat(64)), b"x").unwrap();
        let store = LocalFsStore::new(root.clone());
        assert!(matches!(
            PendingInit::ensure_uninitialized(&store).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(!root.join("config").exists());
    }

    #[tokio::test]
    async fn open_rejects_swapped_config_and_renamed_slot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let store = LocalFsStore::new(root.clone());
        let mut ent = seeded(5);
        let pending = PendingInit::generate(&mut ent, low_options()).unwrap();
        pending.commit(&store, b"pw", &mut ent).await.unwrap();

        let slot_paths: Vec<_> = std::fs::read_dir(root.join("keys"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let renamed: Vec<_> = slot_paths
            .iter()
            .enumerate()
            .map(|(i, p)| p.with_file_name(format!("{i:064}")))
            .collect();
        for (from, to) in slot_paths.iter().zip(&renamed) {
            std::fs::rename(from, to).unwrap();
        }
        assert!(matches!(
            Repository::open(&store, b"pw").await,
            Err(Error::Corrupt { .. })
        ));
        for (from, to) in slot_paths.iter().zip(&renamed) {
            std::fs::rename(to, from).unwrap();
        }

        let mut config = config::decode(&std::fs::read(root.join("config")).unwrap()).unwrap();
        config.hpke.as_mut().unwrap().public_key[0] ^= 1;
        std::fs::write(root.join("config"), config::encode(&config)).unwrap();
        assert!(matches!(
            Repository::open(&store, b"pw").await,
            Err(Error::Corrupt { .. })
        ));
    }

    /// A password slot with the given KDF cost whose object name sorts below
    /// `real`, so a name-ordered open would reach it first.
    fn planted_slot_below(real: &str, memory_kib: u32, iterations: u32) -> (String, Vec<u8>) {
        use prost::Message;
        for n in 0u64.. {
            let slot = pb::KeySlot {
                version: crate::keys::SLOT_VERSION,
                kind: pb::SlotKind::Password as i32,
                kdf: Some(pb::KdfParams {
                    algorithm: crate::keys::KDF_ARGON2ID,
                    memory_kib,
                    iterations,
                    parallelism: 1,
                    salt: n.to_le_bytes().repeat(2),
                }),
                nonce: vec![0; 24],
                ciphertext: vec![0; 48],
                created: None,
            };
            let bytes = slot.encode_to_vec();
            let name = slot_object_name(&bytes);
            if name.as_str() < real {
                return (name, bytes);
            }
        }
        unreachable!()
    }

    async fn repo_with_one_slot(dir: &std::path::Path) -> (LocalFsStore, String) {
        let store = LocalFsStore::new(dir.join("repo"));
        let mut ent = seeded(8);
        let pending = PendingInit::generate(&mut ent, low_options()).unwrap();
        pending.commit(&store, b"pw", &mut ent).await.unwrap();
        let repo = Repository::open(&store, b"pw").await.unwrap();
        let real = repo.opened_slot().to_owned();
        (store, real)
    }

    #[tokio::test]
    async fn open_skips_planted_slots() {
        let dir = tempfile::tempdir().unwrap();
        let (store, real) = repo_with_one_slot(dir.path()).await;
        let keys_dir = dir.path().join("repo").join("keys");
        let junk = b"not a slot";
        let junk_name = slot_object_name(junk);
        std::fs::write(keys_dir.join(&junk_name["keys/".len()..]), junk).unwrap();
        std::fs::write(keys_dir.join("0".repeat(64)), junk).unwrap();

        let repo = Repository::open(&store, b"pw").await.unwrap();
        assert_eq!(repo.opened_slot(), real);
        assert_eq!(repo.skipped_slots().len(), 2);
        assert!(matches!(
            Repository::open(&store, b"other").await,
            Err(Error::WrongSecret)
        ));
    }

    #[tokio::test]
    async fn incompat_features_refused_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = repo_with_one_slot(dir.path()).await;
        let opened = Repository::open(&store, b"pw").await.unwrap();
        let mut config = opened.config().clone();
        config.incompat_features = vec!["future".into()];
        std::fs::write(
            dir.path().join("repo").join("config"),
            config::encode(&config),
        )
        .unwrap();

        let err = Repository::open(&store, b"pw").await.err().unwrap();
        assert!(err.to_string().contains("cannot read or write"), "{err}");
        let err = crate::reader::Reader::open(
            LocalFsStore::new(dir.path().join("repo")),
            opened.read_keys().duplicate(),
            &config,
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("cannot read or write"), "{err}");
    }

    async fn plant_heavy_slots(dir: &std::path::Path, real: &str, count: usize) {
        let keys_dir = dir.join("repo").join("keys");
        for n in 0..count {
            let slot = pb::KeySlot {
                version: crate::keys::SLOT_VERSION,
                kind: pb::SlotKind::Password as i32,
                kdf: Some(pb::KdfParams {
                    algorithm: crate::keys::KDF_ARGON2ID,
                    memory_kib: crate::keys::MAX_ARGON2_MEMORY_KIB,
                    iterations: crate::keys::MAX_ARGON2_ITERATIONS,
                    parallelism: 1,
                    salt: (n as u64).to_le_bytes().repeat(2),
                }),
                nonce: vec![0; 24],
                ciphertext: vec![0; 48],
                created: None,
            };
            let bytes = prost::Message::encode_to_vec(&slot);
            let name = slot_object_name(&bytes);
            assert_ne!(name, real);
            std::fs::write(keys_dir.join(&name["keys/".len()..]), bytes).unwrap();
        }
    }

    #[tokio::test]
    async fn kdf_budget_opens_an_honest_repository() {
        let dir = tempfile::tempdir().unwrap();
        let (store, real) = repo_with_one_slot(dir.path()).await;
        plant_heavy_slots(dir.path(), &real, 3).await;
        let repo = Repository::open(&store, b"pw").await.unwrap();
        assert_eq!(repo.opened_slot(), real);
    }

    #[tokio::test]
    async fn kdf_budget_stops_before_the_heavy_slots() {
        let dir = tempfile::tempdir().unwrap();
        let (store, real) = repo_with_one_slot(dir.path()).await;
        plant_heavy_slots(dir.path(), &real, 3).await;
        let started = std::time::Instant::now();
        let err = Repository::open(&store, b"wrong").await.err().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        match err {
            Error::Corrupt { object, reason } => {
                assert_eq!(object, "keys/");
                assert!(reason.contains("16777216"), "{reason}");
            }
            other => panic!("{other}"),
        }
    }

    #[tokio::test]
    async fn kdf_budget_does_not_block_the_recovery_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(dir.path().join("repo"));
        let mut ent = seeded(9);
        let pending = PendingInit::generate(&mut ent, low_options()).unwrap();
        let key = pending.recovery_key().clone();
        pending.commit(&store, b"pw", &mut ent).await.unwrap();
        let real = Repository::open(&store, b"pw")
            .await
            .unwrap()
            .opened_slot()
            .to_owned();
        plant_heavy_slots(dir.path(), &real, 3).await;
        let repo = Repository::open_recovery(&store, &key).await.unwrap();
        assert_ne!(repo.opened_slot(), real);
    }

    #[tokio::test]
    async fn kdf_budget_allows_a_sum_exactly_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = repo_with_one_slot(dir.path()).await;
        let honest = 64u64;
        let keys_dir = dir.path().join("repo").join("keys");
        for (n, extra) in [0u64, 1].into_iter().enumerate() {
            let memory_kib = u32::try_from((MAX_ARGON2_TOTAL_WORK - honest) / 16).unwrap();
            let slot = pb::KeySlot {
                version: crate::keys::SLOT_VERSION,
                kind: pb::SlotKind::Password as i32,
                kdf: Some(pb::KdfParams {
                    algorithm: crate::keys::KDF_ARGON2ID,
                    memory_kib: memory_kib + u32::try_from(extra).unwrap(),
                    iterations: 16,
                    parallelism: 1,
                    salt: (n as u64).to_le_bytes().repeat(2),
                }),
                nonce: vec![0; 24],
                ciphertext: vec![0; 48],
                created: None,
            };
            let bytes = prost::Message::encode_to_vec(&slot);
            let name = slot_object_name(&bytes);
            std::fs::write(keys_dir.join(&name["keys/".len()..]), bytes).unwrap();
        }
        let calls = Mutex::new(Vec::new());
        let result = Repository::open_with(OpenedWith::Password, &store, |slot, _| {
            calls.lock().unwrap().push(slot_kdf_work(slot));
            Ok(None)
        })
        .await;
        assert!(matches!(result, Err(Error::Corrupt { .. })));
        let calls = calls.into_inner().unwrap();
        assert_eq!(calls, [0, honest, MAX_ARGON2_TOTAL_WORK - honest]);
    }

    #[tokio::test]
    async fn open_tries_cheap_slots_first() {
        let dir = tempfile::tempdir().unwrap();
        let (store, real) = repo_with_one_slot(dir.path()).await;
        let (name, bytes) = planted_slot_below(
            &real,
            crate::keys::MAX_ARGON2_MEMORY_KIB,
            crate::keys::MAX_ARGON2_ITERATIONS,
        );
        let keys_dir = dir.path().join("repo").join("keys");
        std::fs::write(keys_dir.join(&name["keys/".len()..]), bytes).unwrap();

        let expensive = Mutex::new(0u32);
        let repo = Repository::open_with(OpenedWith::Password, &store, |slot, repo_id| {
            if slot_kdf_work(slot) > 1 << 16 {
                *expensive.lock().unwrap() += 1;
                return Ok(None);
            }
            open_password_slot(slot, repo_id, b"pw")
        })
        .await
        .unwrap();
        assert_eq!(repo.opened_slot(), real);
        assert_eq!(*expensive.lock().unwrap(), 0);
    }

    struct LostResponse<S> {
        inner: S,
        error: fn() -> BlobError,
    }

    impl<S: BlobStore> BlobStore for LostResponse<S> {
        async fn put_if_absent(
            &self,
            key: &str,
            data: Bytes,
        ) -> std::result::Result<(), BlobError> {
            self.inner.put_if_absent(key, data).await?;
            if key == CONFIG_KEY {
                return Err((self.error)());
            }
            Ok(())
        }

        async fn get(&self, key: &str) -> std::result::Result<Bytes, BlobError> {
            self.inner.get(key).await
        }

        async fn get_range(
            &self,
            key: &str,
            range: Range<u64>,
        ) -> std::result::Result<Bytes, BlobError> {
            self.inner.get_range(key, range).await
        }

        async fn list(&self, prefix: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
            self.inner.list(prefix).await
        }

        async fn ensure_prefixes(&self, prefixes: &[&str]) -> std::result::Result<(), BlobError> {
            self.inner.ensure_prefixes(prefixes).await
        }
    }

    #[tokio::test]
    async fn init_survives_a_lost_config_response() {
        for error in [(|| BlobError::AlreadyExists) as fn() -> BlobError, || {
            BlobError::Backend {
                message: "timeout".into(),
                temporary: true,
            }
        }] {
            let dir = tempfile::tempdir().unwrap();
            let store = LostResponse {
                inner: LocalFsStore::new(dir.path().join("repo")),
                error,
            };
            let mut ent = seeded(9);
            let pending = PendingInit::generate(&mut ent, low_options()).unwrap();
            let repo_id = *pending.read_keys().repo_id();
            pending.commit(&store, b"pw", &mut ent).await.unwrap();
            let repo = Repository::open(&store, b"pw").await.unwrap();
            assert_eq!(repo.read_keys().repo_id(), &repo_id);
        }
    }
}
