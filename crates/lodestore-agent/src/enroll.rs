use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use lodestore_format::keys::WriteKeys;
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::{EnrollRequest, GetTrustAnchorRequest, Versions};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use serde_json::json;

use crate::connect::{channel, host_of};
use crate::identity::{
    AgentConfig, Identity, Paths, Trust, TrustMode, write_json_file, write_private_file,
};
use crate::token::JoinToken;
use crate::{AgentError, NetOptions, tls};

pub struct EnrollArgs {
    pub token: String,
    pub paths: Paths,
    pub hostname: Option<String>,
    pub net: NetOptions,
}

#[derive(Debug)]
pub struct Enrolled {
    pub agent_id: String,
}

pub(crate) fn new_key_and_csr() -> Result<(KeyPair, Vec<u8>), AgentError> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating the agent key")?;
    let csr = CertificateParams::default()
        .serialize_request(&key)
        .context("building the certificate request")?;
    Ok((key, csr.der().to_vec()))
}

fn default_hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub async fn enroll(args: EnrollArgs) -> Result<Enrolled, AgentError> {
    enroll_inner(args, "").await
}

pub async fn enroll_with_previous(
    args: EnrollArgs,
    previous_agent_id: &str,
) -> Result<Enrolled, AgentError> {
    if !valid_agent_id(previous_agent_id) {
        return Err(AgentError::Other(anyhow::anyhow!(
            "the previous agent id must be 1 to 64 characters of letters, digits, '-' or '_'"
        )));
    }
    enroll_inner(args, previous_agent_id).await
}

async fn enroll_inner(args: EnrollArgs, previous_agent_id: &str) -> Result<Enrolled, AgentError> {
    if let Some(existing) = Identity::load(&args.paths)? {
        return Err(AgentError::AlreadyEnrolled(existing.agent_id));
    }
    let token = JoinToken::parse(&args.token)?;
    let server_name = host_of(&token.grpc_address).to_string();

    let (trust, server_tls, pinned_ca) = match token.ca_sha256.clone() {
        Some(pin) => {
            let bootstrap = channel(
                &token.grpc_address,
                &server_name,
                tls::bootstrap_config()?,
                &args.net,
            )
            .await?;
            let anchor = EnrollmentClient::new(bootstrap)
                .get_trust_anchor(GetTrustAnchorRequest {})
                .await
                .map_err(|s| AgentError::from_status(&s))?
                .into_inner();
            if tls::sha256_hex(&tls::ca_der(&anchor.ca_pem)?) != pin {
                return Err(AgentError::TrustAnchorMismatch);
            }
            let tls = tls::pinned_config(&anchor.ca_pem, None)?;
            let trust = Trust {
                mode: TrustMode::Pinned,
                ca_sha256: Some(pin),
                ca_bundle: None,
            };
            (trust, tls, Some(anchor.ca_pem))
        }
        None => {
            let bundle = args
                .net
                .ca_bundle
                .as_ref()
                .map(std::fs::canonicalize)
                .transpose()?
                .map(|p| p.to_string_lossy().into_owned());
            let (mode, tls) = match &bundle {
                Some(path) => (TrustMode::Bundle, tls::bundle_config(path, None)?),
                None => (TrustMode::System, tls::system_config(None)?),
            };
            let trust = Trust {
                mode,
                ca_sha256: None,
                ca_bundle: bundle,
            };
            (trust, tls, None)
        }
    };

    let (key, csr_der) = new_key_and_csr()?;
    let secure = channel(&token.grpc_address, &server_name, server_tls, &args.net).await?;
    let response = EnrollmentClient::new(secure)
        .enroll(EnrollRequest {
            join_secret: token.secret.to_string(),
            csr_der,
            hostname: args.hostname.unwrap_or_else(default_hostname),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            versions: Some(Versions {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            }),
            previous_agent_id: previous_agent_id.to_string(),
        })
        .await
        .map_err(|s| AgentError::from_status(&s))?
        .into_inner();

    if !response.writer_keys_text.is_empty() {
        WriteKeys::from_file_text(&response.writer_keys_text)
            .map_err(|e| AgentError::Other(anyhow::anyhow!("the writer keys are invalid: {e}")))?;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if (now - response.server_time).abs() > 300 {
        tracing::warn!(
            skew_s = now - response.server_time,
            "the local clock differs from the server's by more than 5 minutes"
        );
    }

    let location = response.location.clone().unwrap_or_default();
    let paths = &args.paths;
    let ca_pem = pinned_ca.as_deref().unwrap_or(&response.ca_pem);
    write_private_file(&paths.ca(), ca_pem.as_bytes())?;
    write_private_file(&paths.key(), key.serialize_pem().as_bytes())?;
    write_private_file(&paths.cert(), response.leaf_pem.as_bytes())?;
    write_private_file(&paths.writer_keys(), response.writer_keys_text.as_bytes())?;
    if let Some(s3) = &response.agent_s3 {
        write_json_file(
            &paths.s3_credentials(),
            &json!({
                "access_key_id": s3.access_key_id,
                "secret_access_key": s3.secret_access_key,
            }),
        )?;
    }
    write_json_file(
        &paths.repository_json(),
        &json!({
            "repository_id": location.repository_id,
            "backend": location.backend,
            "options": location.options,
        }),
    )?;
    AgentConfig {
        grpc_address: token.grpc_address.clone(),
        server_name,
        trust,
    }
    .save(paths)?;
    Identity {
        agent_id: response.agent_id.clone(),
        repository_id: location.repository_id,
        issued_at: response.issued_at,
        expires_at: response.expires_at,
    }
    .save(paths)?;

    Ok(Enrolled {
        agent_id: response.agent_id,
    })
}
