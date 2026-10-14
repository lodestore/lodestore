use std::sync::Arc;

use anyhow::Context;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::AgentError;
use crate::identity::{Trust, TrustMode};

pub fn sha256_hex(der: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(der))
}

pub fn ca_der(ca_pem: &str) -> Result<CertificateDer<'static>, AgentError> {
    Ok(CertificateDer::from_pem_slice(ca_pem.as_bytes()).context("parsing the CA certificate")?)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(aws_lc_rs::default_provider())
}

fn builder() -> Result<rustls::ConfigBuilder<ClientConfig, rustls::WantsVerifier>, AgentError> {
    Ok(ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .context("selecting TLS versions")?)
}

fn finish(mut config: ClientConfig) -> Arc<ClientConfig> {
    config.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(config)
}

#[derive(Debug)]
struct AcceptAnyServer(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Accepts any server certificate. Only for the single `GetTrustAnchor` call
/// whose answer is checked against the fingerprint from the join token;
/// nothing secret is sent over this channel.
pub fn bootstrap_config() -> Result<Arc<ClientConfig>, AgentError> {
    let config = builder()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer(provider())))
        .with_no_client_auth();
    Ok(finish(config))
}

fn with_roots(
    roots: RootCertStore,
    client: Option<(&str, &str)>,
) -> Result<Arc<ClientConfig>, AgentError> {
    let builder = builder()?.with_root_certificates(roots);
    let config = match client {
        None => builder.with_no_client_auth(),
        Some((cert_pem, key_pem)) => {
            let chain = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .context("parsing the agent certificate")?;
            let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
                .context("parsing the agent key")?;
            builder
                .with_client_auth_cert(chain, key)
                .context("loading the agent identity")?
        }
    };
    Ok(finish(config))
}

pub fn pinned_config(
    ca_pem: &str,
    client: Option<(&str, &str)>,
) -> Result<Arc<ClientConfig>, AgentError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(ca_der(ca_pem)?)
        .context("adding the CA certificate")?;
    with_roots(roots, client)
}

pub fn system_config(client: Option<(&str, &str)>) -> Result<Arc<ClientConfig>, AgentError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    with_roots(roots, client)
}

pub fn bundle_config(
    path: &str,
    client: Option<(&str, &str)>,
) -> Result<Arc<ClientConfig>, AgentError> {
    let pem = std::fs::read(path).with_context(|| format!("reading the CA bundle {path}"))?;
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&pem) {
        roots
            .add(cert.with_context(|| format!("parsing the CA bundle {path}"))?)
            .with_context(|| format!("adding a certificate from {path}"))?;
    }
    if roots.is_empty() {
        return Err(AgentError::Other(anyhow::anyhow!(
            "the CA bundle {path} has no certificates"
        )));
    }
    with_roots(roots, client)
}

pub fn pair_matches(cert_pem: &str, key_pem: &str) -> bool {
    with_roots(RootCertStore::empty(), Some((cert_pem, key_pem))).is_ok()
}

/// Server verification as recorded at registration time. `ca_pem` is the
/// built-in CA saved at registration and only matters for `Pinned`, where it
/// must still hash to the fingerprint pinned from the join token.
pub fn client_config(
    trust: &Trust,
    ca_pem: &str,
    client: Option<(&str, &str)>,
) -> Result<Arc<ClientConfig>, AgentError> {
    match (trust.mode, trust.ca_bundle.as_deref()) {
        (TrustMode::Pinned, _) => {
            if trust.ca_sha256.as_deref() != Some(sha256_hex(&ca_der(ca_pem)?).as_str()) {
                return Err(AgentError::TrustAnchorMismatch);
            }
            pinned_config(ca_pem, client)
        }
        (TrustMode::System, _) => system_config(client),
        (TrustMode::Bundle, Some(path)) => bundle_config(path, client),
        (TrustMode::Bundle, None) => Err(AgentError::Other(anyhow::anyhow!(
            "agent.json selects a CA bundle but names no file"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256};

    use super::*;

    fn ca_pem() -> String {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.self_signed(&key).unwrap().pem()
    }

    #[test]
    fn pinned_trust_rejects_a_replaced_ca_file() {
        let (pinned, other) = (ca_pem(), ca_pem());
        let trust = Trust {
            mode: TrustMode::Pinned,
            ca_sha256: Some(sha256_hex(&ca_der(&pinned).unwrap())),
            ca_bundle: None,
        };
        client_config(&trust, &pinned, None).unwrap();
        assert!(matches!(
            client_config(&trust, &other, None),
            Err(AgentError::TrustAnchorMismatch)
        ));
        let unpinned = Trust {
            ca_sha256: None,
            ..trust
        };
        assert!(client_config(&unpinned, &pinned, None).is_err());
    }
}
