//! Built-in certificate authority: the server leaf for the gRPC listener and
//! agent certificates signed from CSRs. The server never trusts what a CSR
//! asks for; names, usages and lifetime all come from here.

use std::collections::BTreeSet;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, SanType, SerialNumber,
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use time::OffsetDateTime;
use zeroize::Zeroizing;

use crate::config::Config;
use crate::db::Db;
use crate::secrets::Sealer;

pub const AGENT_URI_PREFIX: &str = "spiffe://lodestore/agent/";

const PKI_ID: &str = "default";
const CA_YEARS_DAYS: i64 = 3650;
const LEAF_DAYS: i64 = 365;
const REISSUE_BEFORE_DAYS: i64 = 30;
const BACKDATE_MINUTES: i64 = 5;

pub struct Pki {
    pub ca_pem: String,
    pub ca_sha256_hex: String,
    leaf: RwLock<LeafState>,
    external: bool,
    issuer: Issuer<'static, KeyPair>,
}

struct LeafState {
    cert_pem: String,
    key_pem: Zeroizing<String>,
    key_secret: String,
    sans: Vec<String>,
    not_after: i64,
}

#[derive(Debug, Clone)]
pub struct IssuedCert {
    pub pem: String,
    pub serial_hex: String,
    pub issued_at: i64,
    pub expires_at: i64,
}

struct Leaf {
    cert_pem: String,
    key_pem: Zeroizing<String>,
    not_after: i64,
}

fn sha256_hex(der: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(der))
}

fn host_of(address: &str) -> &str {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match address.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => address,
    }
}

fn leaf_sans(config: &Config) -> Vec<String> {
    let mut sans = vec![
        host_of(&config.grpc.public_address).to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ];
    sans.extend(
        config
            .grpc
            .extra_sans
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    );
    sans.sort();
    sans.dedup();
    sans
}

pub fn needs_reissue(
    stored_sans: &[String],
    wanted_sans: &[String],
    not_after: i64,
    now: i64,
) -> bool {
    fn set(sans: &[String]) -> BTreeSet<&String> {
        sans.iter().collect()
    }
    set(stored_sans) != set(wanted_sans) || not_after - now < REISSUE_BEFORE_DAYS * 86_400
}

fn now_secs() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp())
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
}

fn new_key() -> anyhow::Result<KeyPair> {
    KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating a key")
}

fn issue_leaf(issuer: &Issuer<'static, KeyPair>, sans: &[String]) -> anyhow::Result<Leaf> {
    let now = now_secs();
    let key = new_key()?;
    let mut params = CertificateParams::new(sans.to_vec())?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "lodestore-server");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.not_before = now - time::Duration::minutes(BACKDATE_MINUTES);
    params.not_after = now + time::Duration::days(LEAF_DAYS);
    let not_after = params.not_after.unix_timestamp();
    let cert = params.signed_by(&key, issuer)?;
    Ok(Leaf {
        cert_pem: cert.pem(),
        key_pem: Zeroizing::new(key.serialize_pem()),
        not_after,
    })
}

impl Pki {
    pub async fn load_or_create(db: &Db, sealer: &Sealer, config: &Config) -> anyhow::Result<Pki> {
        let external = crate::grpc::external_identity(&config.grpc)?;
        let row = sqlx::query(
            "SELECT ca_cert_pem, ca_key_secret_id, grpc_cert_pem, grpc_key_secret_id, \
             grpc_sans, grpc_not_after FROM pki WHERE id = $1",
        )
        .bind(PKI_ID)
        .fetch_optional(&db.pool)
        .await?;
        let pki = match row {
            Some(row) => {
                let ca_pem: String = row.try_get("ca_cert_pem")?;
                let ca_key =
                    key_from_secret(sealer, &row.try_get::<String, _>("ca_key_secret_id")?).await?;
                let leaf_secret: String = row.try_get("grpc_key_secret_id")?;
                let leaf_bytes = sealer.get_secret(&leaf_secret).await?;
                let stored: String = row.try_get("grpc_sans")?;
                Pki {
                    ca_sha256_hex: ca_fingerprint(&ca_pem)?,
                    issuer: Issuer::from_ca_cert_pem(&ca_pem, ca_key)
                        .context("loading the CA certificate")?,
                    ca_pem,
                    leaf: RwLock::new(LeafState {
                        cert_pem: row.try_get("grpc_cert_pem")?,
                        key_pem: Zeroizing::new(std::str::from_utf8(&leaf_bytes)?.to_owned()),
                        key_secret: leaf_secret,
                        sans: stored.split(',').map(str::to_string).collect(),
                        not_after: row.try_get("grpc_not_after")?,
                    }),
                    external: external.is_some(),
                }
            }
            None => Self::create(db, sealer, config, external.is_some()).await?,
        };
        pki.renew_leaf_if_due(db, sealer, config).await?;
        Ok(pki)
    }

    pub fn grpc_not_after(&self) -> i64 {
        self.leaf
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .not_after
    }

    #[doc(hidden)]
    pub fn set_grpc_not_after(&self, not_after: i64) {
        self.leaf
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .not_after = not_after;
    }

    pub fn grpc_identity(&self) -> (String, Zeroizing<String>) {
        let leaf = self.leaf.read().unwrap_or_else(|p| p.into_inner());
        (leaf.cert_pem.clone(), leaf.key_pem.clone())
    }

    /// Issues a new built-in server certificate when the wanted names changed
    /// or the current one is close to its end. Returns whether it did.
    pub async fn renew_leaf_if_due(
        &self,
        db: &Db,
        sealer: &Sealer,
        config: &Config,
    ) -> anyhow::Result<bool> {
        if self.external {
            return Ok(false);
        }
        let wanted = leaf_sans(config);
        let (stored, not_after, old_secret) = {
            let leaf = self.leaf.read().unwrap_or_else(|p| p.into_inner());
            (leaf.sans.clone(), leaf.not_after, leaf.key_secret.clone())
        };
        let now = OffsetDateTime::now_utc().unix_timestamp();
        if !needs_reissue(&stored, &wanted, not_after, now) {
            return Ok(false);
        }
        self.reissue_leaf(db, sealer, &wanted, &old_secret).await?;
        Ok(true)
    }

    async fn reissue_leaf(
        &self,
        db: &Db,
        sealer: &Sealer,
        sans: &[String],
        old_secret: &str,
    ) -> anyhow::Result<()> {
        let leaf = issue_leaf(&self.issuer, sans)?;
        let mut tx = db.pool.begin().await?;
        let secret = sealer
            .put_secret_on(&mut tx, "pki-grpc-key", leaf.key_pem.as_bytes())
            .await?;
        sqlx::query(
            "UPDATE pki SET grpc_cert_pem = $1, grpc_key_secret_id = $2, grpc_sans = $3, \
             grpc_not_after = $4 WHERE id = $5",
        )
        .bind(&leaf.cert_pem)
        .bind(&secret)
        .bind(sans.join(","))
        .bind(leaf.not_after)
        .bind(PKI_ID)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM secrets WHERE id = $1")
            .bind(old_secret)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(sans = %sans.join(","), "reissued the gRPC server certificate");
        *self.leaf.write().unwrap_or_else(|p| p.into_inner()) = LeafState {
            cert_pem: leaf.cert_pem,
            key_pem: leaf.key_pem,
            key_secret: secret,
            sans: sans.to_vec(),
            not_after: leaf.not_after,
        };
        Ok(())
    }

    async fn create(
        db: &Db,
        sealer: &Sealer,
        config: &Config,
        external: bool,
    ) -> anyhow::Result<Pki> {
        let now = now_secs();
        let ca_key = new_key()?;
        let ca_key_pem = Zeroizing::new(ca_key.serialize_pem());
        let mut ca_params = CertificateParams::default();
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "Lodestore CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params.not_before = now - time::Duration::minutes(BACKDATE_MINUTES);
        ca_params.not_after = now + time::Duration::days(CA_YEARS_DAYS);
        let ca_cert = ca_params.self_signed(&ca_key)?;
        let ca_pem = ca_cert.pem();
        let ca_sha256 = sha256_hex(ca_cert.der());
        let issuer = Issuer::new(ca_params, ca_key);

        let sans = leaf_sans(config);
        let leaf = issue_leaf(&issuer, &sans)?;

        let mut tx = db.pool.begin().await?;
        let ca_secret = sealer
            .put_secret_on(&mut tx, "pki-ca-key", ca_key_pem.as_bytes())
            .await?;
        let leaf_secret = sealer
            .put_secret_on(&mut tx, "pki-grpc-key", leaf.key_pem.as_bytes())
            .await?;
        sqlx::query(
            "INSERT INTO pki (id, ca_cert_pem, ca_key_secret_id, ca_sha256, grpc_cert_pem, \
             grpc_key_secret_id, grpc_sans, grpc_not_after) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(PKI_ID)
        .bind(&ca_pem)
        .bind(&ca_secret)
        .bind(&ca_sha256)
        .bind(&leaf.cert_pem)
        .bind(&leaf_secret)
        .bind(sans.join(","))
        .bind(leaf.not_after)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(Pki {
            ca_pem,
            ca_sha256_hex: ca_sha256,
            leaf: RwLock::new(LeafState {
                cert_pem: leaf.cert_pem,
                key_pem: leaf.key_pem,
                key_secret: leaf_secret,
                sans,
                not_after: leaf.not_after,
            }),
            external,
            issuer,
        })
    }

    pub fn trust_pin(&self) -> Option<String> {
        (!self.external).then(|| self.ca_sha256_hex.clone())
    }

    pub fn sign_agent_csr(
        &self,
        csr_der: &[u8],
        agent_id: &str,
        lifetime: Duration,
    ) -> anyhow::Result<IssuedCert> {
        let mut csr = CertificateSigningRequestParams::from_der(&csr_der.to_vec().into())
            .context("invalid certificate signing request")?;
        let now = now_secs();
        let lifetime = time::Duration::try_from(lifetime).context("certificate lifetime")?;

        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, agent_id);
        csr.params.distinguished_name = name;
        csr.params.subject_alt_names = vec![SanType::URI(
            format!("{AGENT_URI_PREFIX}{agent_id}")
                .try_into()
                .context("agent id as a URI")?,
        )];
        csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        csr.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        csr.params.is_ca = IsCa::NoCa;
        csr.params.name_constraints = None;
        csr.params.crl_distribution_points = Vec::new();
        csr.params.custom_extensions = Vec::new();
        csr.params.not_before = now - time::Duration::minutes(BACKDATE_MINUTES);
        csr.params.not_after = now + lifetime;
        let mut serial = lodestore_format::crypto::Entropy::os().bytes::<16>();
        serial[0] &= 0x7f;
        serial[0] |= 0x01;
        csr.params.serial_number = Some(SerialNumber::from_slice(&serial));

        let issued_at = csr.params.not_before.unix_timestamp();
        let expires_at = csr.params.not_after.unix_timestamp();
        let cert = csr
            .signed_by(&self.issuer)
            .context("signing the agent certificate")?;
        let (_, parsed) = x509_parser::parse_x509_certificate(cert.der())
            .map_err(|e| anyhow::anyhow!("parsing the issued certificate: {e}"))?;
        Ok(IssuedCert {
            pem: cert.pem(),
            serial_hex: parsed.tbs_certificate.serial.to_str_radix(16),
            issued_at,
            expires_at,
        })
    }
}

async fn key_from_secret(sealer: &Sealer, id: &str) -> anyhow::Result<KeyPair> {
    let bytes = sealer.get_secret(id).await?;
    KeyPair::from_pem(std::str::from_utf8(&bytes)?).context("parsing a stored key")
}

fn ca_fingerprint(ca_pem: &str) -> anyhow::Result<String> {
    use rustls_pki_types::{CertificateDer, pem::PemObject};
    let der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).context("parsing the CA PEM")?;
    Ok(sha256_hex(&der))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sans(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn csr_der() -> Vec<u8> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .der()
            .to_vec()
    }

    fn test_pki() -> Pki {
        let key = new_key().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let cert = params.self_signed(&key).unwrap();
        Pki {
            ca_pem: cert.pem(),
            ca_sha256_hex: sha256_hex(cert.der()),
            leaf: RwLock::new(LeafState {
                cert_pem: String::new(),
                key_pem: Zeroizing::new(String::new()),
                key_secret: String::new(),
                sans: Vec::new(),
                not_after: 0,
            }),
            external: false,
            issuer: Issuer::new(params, key),
        }
    }

    #[test]
    fn csr_with_a_broken_signature_is_rejected() {
        let pki = test_pki();
        let lifetime = Duration::from_secs(3600);
        let der = csr_der();
        pki.sign_agent_csr(&der, "agent-1", lifetime).unwrap();

        let mut tampered = der.clone();
        *tampered.last_mut().unwrap() ^= 0xff;
        let error = pki
            .sign_agent_csr(&tampered, "agent-1", lifetime)
            .unwrap_err();
        assert!(
            format!("{error:#}").to_lowercase().contains("signature"),
            "{error:#}"
        );

        let mut other_key = der;
        let at = other_key.len() / 2;
        other_key[at] ^= 0x01;
        assert!(pki.sign_agent_csr(&other_key, "agent-1", lifetime).is_err());
    }

    #[tokio::test]
    async fn leaf_is_renewed_before_it_expires() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().to_path_buf(),
            database_url: format!("sqlite://{}/pki.db?mode=rwc", dir.path().display()),
            unlock_key: Some(crate::config::Secret::new("07".repeat(32))),
            ..Config::default()
        };
        let db = Db::connect(&config.database_url).await.unwrap();
        let sealer = Sealer::load(&db, &config).await.unwrap();
        let pki = Pki::load_or_create(&db, &sealer, &config).await.unwrap();

        let first = pki.grpc_identity().0;
        assert!(!pki.renew_leaf_if_due(&db, &sealer, &config).await.unwrap());
        assert_eq!(pki.grpc_identity().0, first);

        let soon = OffsetDateTime::now_utc().unix_timestamp() + 10 * 86_400;
        pki.leaf.write().unwrap().not_after = soon;
        assert!(pki.renew_leaf_if_due(&db, &sealer, &config).await.unwrap());
        let second = pki.grpc_identity().0;
        assert_ne!(second, first);
        assert!(!pki.renew_leaf_if_due(&db, &sealer, &config).await.unwrap());

        let reloaded = Pki::load_or_create(&db, &sealer, &config).await.unwrap();
        assert_eq!(reloaded.grpc_identity().0, second);
    }

    #[test]
    fn leaf_reissue_rules() {
        let now = 1_000_000_000;
        let day = 86_400;
        let same = sans(&["a.example", "localhost"]);
        assert!(!needs_reissue(&same, &same, now + 100 * day, now));
        let added = sans(&["a.example", "b.example", "localhost"]);
        assert!(needs_reissue(&same, &added, now + 100 * day, now));
        assert!(needs_reissue(&added, &same, now + 100 * day, now));
        assert!(needs_reissue(&same, &same, now + 29 * day, now));
        assert!(!needs_reissue(&same, &same, now + 31 * day, now));
    }
}
