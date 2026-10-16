//! Enrollment refusals carry the `lodestore-error` metadata key with one of
//! `token_invalid` (unknown, expired or used token) or `version_unsupported`.

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lodestore_proto::agent::v1::enrollment_server::Enrollment;
use lodestore_proto::agent::v1::{
    EnrollRequest, EnrollResponse, GetTrustAnchorRequest, RepoLocation, S3Credentials, TrustAnchor,
};
use lodestore_proto::{ERROR_TOKEN_INVALID, ERROR_VERSION_UNSUPPORTED};
use tonic::{Code, Request, Response, Status};

use crate::db::now_ms;
use crate::grpc::{internal, select_version, status_with};
use crate::hosts::tokens;
use crate::repos::db::writer_material;
use crate::state::AppState;

const MAX_FIELD: usize = 255;
const MAX_AGENT_ID: usize = 64;

pub struct EnrollmentService {
    pub state: AppState,
}

fn field(name: &str, value: &str) -> Result<(), Status> {
    if value.len() > MAX_FIELD {
        return Err(Status::invalid_argument(format!("{name} is too long")));
    }
    Ok(())
}

fn valid_agent_id(id: &str) -> bool {
    id.len() <= MAX_AGENT_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[tonic::async_trait]
impl Enrollment for EnrollmentService {
    async fn get_trust_anchor(
        &self,
        _request: Request<GetTrustAnchorRequest>,
    ) -> Result<Response<TrustAnchor>, Status> {
        Ok(Response::new(TrustAnchor {
            ca_pem: self.state.pki.ca_pem.clone(),
        }))
    }

    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let state = &self.state;
        let peer_ip = request
            .remote_addr()
            .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |a| a.ip());
        state.auth.limits.check_enroll(peer_ip)?;
        let req = request.into_inner();
        let selected_version = select_version(req.versions.as_ref()).ok_or_else(|| {
            status_with(
                Code::FailedPrecondition,
                "no common protocol version",
                ERROR_VERSION_UNSUPPORTED,
            )
        })?;
        let invalid = || {
            status_with(
                Code::PermissionDenied,
                "the join token is invalid, expired or already used",
                ERROR_TOKEN_INVALID,
            )
        };
        field("hostname", &req.hostname)?;
        field("os", &req.os)?;
        field("arch", &req.arch)?;
        field("agent_version", &req.agent_version)?;
        if !valid_agent_id(&req.previous_agent_id) {
            return Err(Status::invalid_argument("previous_agent_id is invalid"));
        }

        let secret = tokens::decode_secret(&req.join_secret).ok_or_else(invalid)?;
        let token_hash = tokens::hash_secret(&secret);
        let repository_id = tokens::peek(&state.db, &token_hash)
            .await
            .map_err(internal)?
            .ok_or_else(invalid)?;
        let material = writer_material(state, &repository_id)
            .await
            .map_err(internal)?;

        let reused = !req.previous_agent_id.is_empty()
            && sqlx::query("SELECT 1 FROM agents WHERE id = $1")
                .bind(&req.previous_agent_id)
                .fetch_optional(&state.db.pool)
                .await
                .map_err(internal)?
                .is_none();
        let agent_id = if reused {
            req.previous_agent_id.clone()
        } else {
            uuid::Uuid::now_v7().to_string()
        };
        let lifetime = Duration::from_secs(state.config.agent_cert_lifetime_secs.max(1) as u64);
        let mut tx = state.db.pool.begin().await.map_err(internal)?;
        let consumed = tokens::consume(&mut tx, &token_hash, &agent_id)
            .await
            .map_err(internal)?
            .ok_or_else(invalid)?;
        let cert = state
            .pki
            .sign_agent_csr(&req.csr_der, &agent_id, lifetime)
            .map_err(|e| Status::invalid_argument(format!("{e:#}")))?;
        let hostname = if req.hostname.is_empty() {
            "unknown"
        } else {
            req.hostname.as_str()
        };
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
             enrolled_at, cert_serial, cert_issued_at, cert_not_after) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', $8, $9, $10, $11)",
        )
        .bind(&agent_id)
        .bind(hostname)
        .bind(&consumed.repository_id)
        .bind(&consumed.labels_json)
        .bind(&req.os)
        .bind(&req.arch)
        .bind(&req.agent_version)
        .bind(now_ms())
        .bind(&cert.serial_hex)
        .bind(cert.issued_at * 1000)
        .bind(cert.expires_at * 1000)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        let mut rebound = 0;
        if reused {
            rebound = sqlx::query(
                "UPDATE snapshots SET host_id = $1 \
                 WHERE repository_id = $2 AND agent_id_claim = $1 AND host_id IS NULL",
            )
            .bind(&agent_id)
            .bind(&consumed.repository_id)
            .execute(&mut *tx)
            .await
            .map_err(internal)?
            .rows_affected();
        }
        tx.commit().await.map_err(internal)?;
        tracing::info!(agent_id = %agent_id, hostname, reused, rebound, "agent enrolled");

        let server_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(Response::new(EnrollResponse {
            agent_id,
            leaf_pem: cert.pem,
            ca_pem: state.pki.ca_pem.clone(),
            issued_at: cert.issued_at,
            expires_at: cert.expires_at,
            writer_keys_text: material.writer_keys_text.to_string(),
            location: Some(RepoLocation {
                backend: material.location.backend,
                options: material.location.options.into_iter().collect(),
                repository_id: consumed.repository_id,
            }),
            agent_s3: material.agent_creds.map(|k| S3Credentials {
                access_key_id: k.access_key_id,
                secret_access_key: k.secret_access_key,
            }),
            selected_version,
            server_time,
        }))
    }
}
