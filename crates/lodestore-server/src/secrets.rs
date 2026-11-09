//! Secrets at rest: XChaCha20-Poly1305 under the unlock key, with the secret's
//! kind and id bound in as associated data.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use data_encoding::{BASE64, BASE64_NOPAD, HEXLOWER_PERMISSIVE};
use lodestore_format::crypto::{self, Entropy, NONCE_LEN};
use zeroize::Zeroizing;

use crate::config::Config;
use crate::db::{Db, now_ms};

const AAD_PREFIX: &[u8] = b"lodestore secret v2";
const FORMAT: i64 = 2;
const LEGACY_AAD_PREFIX: &[u8] = b"lodestore secret v1";
const LEGACY_FORMAT: i64 = 1;

#[derive(Clone)]
pub struct Sealer {
    db: Db,
    key: Arc<Zeroizing<[u8; 32]>>,
}

pub fn parse_unlock_key(text: &str) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let text = text.trim();
    let bytes = Zeroizing::new(
        if text.len() == 64 {
            HEXLOWER_PERMISSIVE.decode(text.as_bytes()).ok()
        } else {
            None
        }
        .or_else(|| BASE64.decode(text.as_bytes()).ok())
        .or_else(|| BASE64_NOPAD.decode(text.as_bytes()).ok())
        .context("the unlock key must be 32 bytes in hex or base64")?,
    );
    let key: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("the unlock key must be 32 bytes"))?;
    Ok(Zeroizing::new(key))
}

fn unlock_key_file(path: &Path) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    for _ in 0..2 {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => {
                std::fs::remove_file(path).context("removing an empty unlock.key")?;
            }
            Ok(text) => {
                return parse_unlock_key(&Zeroizing::new(text)).context("reading unlock.key");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("reading unlock.key"),
        }
        if let Some(key) = create_unlock_key_file(path)? {
            return Ok(key);
        }
    }
    anyhow::bail!("could not create or read unlock.key")
}

fn create_unlock_key_file(path: &Path) -> anyhow::Result<Option<Zeroizing<[u8; 32]>>> {
    let key = Entropy::os().key32();
    let text = Zeroizing::new(format!("{}\n", data_encoding::HEXLOWER.encode(&*key)));
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .context("creating unlock.key")?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::hard_link(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    match written {
        Ok(()) => {
            tracing::warn!(
                "unlock key is stored next to the database; move it out with LODESTORE_UNLOCK_KEY_FILE or a secret store"
            );
            Ok(Some(key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e).context("creating unlock.key"),
    }
}

const CHECK_ID: &str = "unlock-check";
const CHECK_PLAINTEXT: &[u8] = b"lodestore unlock check";

impl Sealer {
    pub async fn load(db: &Db, config: &Config) -> anyhow::Result<Sealer> {
        let (key, source) = if let Some(k) = &config.unlock_key {
            (parse_unlock_key(k.expose())?, "env")
        } else if let Some(path) = &config.unlock_key_file {
            let text = Zeroizing::new(std::fs::read_to_string(path).with_context(|| {
                format!("reading LODESTORE_UNLOCK_KEY_FILE {}", path.display())
            })?);
            (
                parse_unlock_key(&text).context("reading the unlock key file")?,
                "file",
            )
        } else {
            std::fs::create_dir_all(&config.data_dir)?;
            (
                unlock_key_file(&config.data_dir.join("unlock.key"))?,
                "data_dir",
            )
        };
        let sealer = Sealer {
            db: db.clone(),
            key: Arc::new(key),
        };
        sealer.verify_key().await?;
        sealer.migrate_v1().await?;
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ('unlock_key_source', $1) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(source)
        .execute(&db.pool)
        .await?;
        Ok(sealer)
    }

    async fn verify_key(&self) -> anyhow::Result<()> {
        use sqlx::Row;
        let row = sqlx::query("SELECT format, nonce, ciphertext FROM secrets WHERE id = $1")
            .bind(CHECK_ID)
            .fetch_optional(&self.db.pool)
            .await?;
        if let Some(row) = row {
            let format: i64 = row.try_get("format")?;
            let nonce: Vec<u8> = row.try_get("nonce")?;
            let ciphertext: Vec<u8> = row.try_get("ciphertext")?;
            let opened = match format {
                LEGACY_FORMAT => self.open_v1(CHECK_ID, CHECK_ID, &nonce, &ciphertext),
                FORMAT => self.open(CHECK_ID, CHECK_ID, &nonce, &ciphertext),
                n => anyhow::bail!("secret {CHECK_ID} has an unknown format {n}"),
            };
            return opened
                .map(drop)
                .map_err(|_| anyhow::anyhow!("the unlock key does not match this database"));
        }
        let (nonce, ciphertext) = self.seal(CHECK_ID, CHECK_ID, CHECK_PLAINTEXT)?;
        sqlx::query(
            "INSERT INTO secrets (id, kind, format, nonce, ciphertext, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
        )
        .bind(CHECK_ID)
        .bind(CHECK_ID)
        .bind(FORMAT)
        .bind(nonce)
        .bind(ciphertext)
        .bind(now_ms())
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    async fn migrate_v1(&self) -> anyhow::Result<usize> {
        use sqlx::Row;
        let mut tx = self.db.pool.begin().await?;
        let rows = sqlx::query("SELECT id, kind, nonce, ciphertext FROM secrets WHERE format = $1")
            .bind(LEGACY_FORMAT)
            .fetch_all(&mut *tx)
            .await?;
        for row in &rows {
            let id: String = row.try_get("id")?;
            let kind: String = row.try_get("kind")?;
            let nonce: Vec<u8> = row.try_get("nonce")?;
            let ciphertext: Vec<u8> = row.try_get("ciphertext")?;
            let plaintext = self
                .open_v1(&kind, &id, &nonce, &ciphertext)
                .with_context(|| format!("migrating secret {id} to format {FORMAT}"))?;
            let (nonce, ciphertext) = self.seal(&kind, &id, &plaintext)?;
            sqlx::query(
                "UPDATE secrets SET nonce = $1, ciphertext = $2, format = $3 \
                 WHERE id = $4 AND format = $5",
            )
            .bind(nonce)
            .bind(ciphertext)
            .bind(FORMAT)
            .bind(&id)
            .bind(LEGACY_FORMAT)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        if !rows.is_empty() {
            tracing::info!(count = rows.len(), "re-sealed secrets written by 0.1");
        }
        Ok(rows.len())
    }

    fn aad(kind: &str, id: &str) -> anyhow::Result<Vec<u8>> {
        let mut aad = AAD_PREFIX.to_vec();
        for field in [kind, id] {
            aad.extend_from_slice(&u32::try_from(field.len())?.to_le_bytes());
            aad.extend_from_slice(field.as_bytes());
        }
        Ok(aad)
    }

    /// Only for rows written by 0.1, whose AAD had no field separators.
    fn aad_v1(kind: &str, id: &str) -> Vec<u8> {
        [LEGACY_AAD_PREFIX, kind.as_bytes(), id.as_bytes()].concat()
    }

    fn open_v1(
        &self,
        kind: &str,
        id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        self.open_with_aad(&Self::aad_v1(kind, id), id, nonce, ciphertext)
    }

    fn open_with_aad(
        &self,
        aad: &[u8],
        id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        let sealed = [nonce, ciphertext].concat();
        crypto::aead_open(&self.key, aad, &sealed)
            .map(Zeroizing::new)
            .map_err(|_| {
                anyhow::anyhow!("secret {id} cannot be opened: wrong unlock key or corrupted data")
            })
    }

    pub fn seal(
        &self,
        kind: &str,
        id: &str,
        plaintext: &[u8],
    ) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let mut sealed = crypto::aead_seal(
            &self.key,
            &Self::aad(kind, id)?,
            plaintext,
            &mut Entropy::os(),
        )
        .map_err(|e| anyhow::anyhow!("sealing a secret: {e}"))?;
        let ciphertext = sealed.split_off(NONCE_LEN);
        Ok((sealed, ciphertext))
    }

    pub fn open(
        &self,
        kind: &str,
        id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        self.open_with_aad(&Self::aad(kind, id)?, id, nonce, ciphertext)
    }

    pub async fn put_secret(&self, kind: &str, plaintext: &[u8]) -> anyhow::Result<String> {
        let mut conn = self.db.pool.acquire().await?;
        self.put_secret_on(&mut conn, kind, plaintext).await
    }

    pub async fn put_secret_on(
        &self,
        conn: &mut sqlx::AnyConnection,
        kind: &str,
        plaintext: &[u8],
    ) -> anyhow::Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let (nonce, ciphertext) = self.seal(kind, &id, plaintext)?;
        sqlx::query(
            "INSERT INTO secrets (id, kind, format, nonce, ciphertext, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(kind)
        .bind(FORMAT)
        .bind(nonce)
        .bind(ciphertext)
        .bind(now_ms())
        .execute(conn)
        .await?;
        Ok(id)
    }

    pub async fn get_secret(&self, id: &str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        use sqlx::Row;
        let row = sqlx::query("SELECT kind, format, nonce, ciphertext FROM secrets WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.db.pool)
            .await?
            .with_context(|| format!("secret {id} is missing"))?;
        let format: i64 = row.try_get("format")?;
        anyhow::ensure!(
            format == FORMAT,
            "secret {id} has an unknown format {format}"
        );
        let kind: String = row.try_get("kind")?;
        let nonce: Vec<u8> = row.try_get("nonce")?;
        let ciphertext: Vec<u8> = row.try_get("ciphertext")?;
        self.open(&kind, id, &nonce, &ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{Sealer, unlock_key_file};

    #[test]
    fn aad_separates_kind_and_id() {
        assert_ne!(
            Sealer::aad("a", "bc").unwrap(),
            Sealer::aad("ab", "c").unwrap()
        );
        assert_ne!(Sealer::aad("", "x").unwrap(), Sealer::aad("x", "").unwrap());
    }

    #[test]
    fn unlock_key_file_is_created_whole_and_a_crashed_empty_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unlock.key");
        std::fs::write(&path, "").unwrap();

        let key = unlock_key_file(&path).unwrap();
        let again = unlock_key_file(&path).unwrap();
        assert_eq!(*key, *again);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["unlock.key"]);
    }
}
