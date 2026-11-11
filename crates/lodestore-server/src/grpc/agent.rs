use std::net::IpAddr;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::agent_server::Agent;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, Disconnect, DisconnectReason, FetchAgentUpdateRequest, Heartbeat, HelloAck,
    PackBytes, ReadPackRangeRequest, RefreshPackAccessRequest, RefreshPackAccessResponse,
    RenewRequest, RenewResponse, RestorePlanBatch, RestorePlanRequest, S3Credentials,
    ServerMessage, UpdateChunk, UpdateS3Credentials,
};
use lodestore_proto::{ERROR_CERT_STALE, ERROR_REVOKED, ERROR_VERSION_UNSUPPORTED};
use sqlx::Row;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Code, Request, Response, Status, Streaming};

use crate::db::now_ms;
use crate::grpc::{internal, peer_agent_id, restore, select_version, status_with};
use crate::maintenance::attest::load_distrusted;
use crate::repos::db::writer_material;
use crate::state::AppState;

const SEEN_WRITE_INTERVAL: Duration = Duration::from_secs(30);
const MAX_CAPABILITIES: usize = 16;
const MAX_CAPABILITY_LEN: usize = 32;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PREV_CERT_GRACE_MS: i64 = 24 * 60 * 60 * 1000;

pub struct AgentService {
    pub state: AppState,
}

fn revoked() -> Status {
    status_with(
        Code::PermissionDenied,
        "this agent was revoked",
        ERROR_REVOKED,
    )
}

fn peer_cert_serial<T>(request: &Request<T>) -> String {
    request
        .peer_certs()
        .and_then(|certs| {
            let leaf = certs.first()?;
            let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref()).ok()?;
            Some(cert.tbs_certificate.serial.to_str_radix(16))
        })
        .unwrap_or_default()
}

fn cert_stale() -> Status {
    status_with(
        Code::PermissionDenied,
        "this agent certificate is not the one the server issued last; re-enroll the host with a new join token",
        ERROR_CERT_STALE,
    )
}

/// Who is calling: the agent behind the client certificate, after the serial check.
struct Peer {
    agent_id: String,
    repository_id: String,
    serial: String,
    /// The certificate is the current one, not the previous one inside its grace window.
    fresh: bool,
}

/// Every RPC of an agent passes here. The certificate must be the one the server issued last, or the one before it
/// until the first session with the newer one (at most 24 h): a renew whose answer was lost must not lock the agent out.
async fn check_peer<T>(state: &AppState, request: &Request<T>) -> Result<Peer, Status> {
    let agent_id = peer_agent_id(request)?;
    let serial = peer_cert_serial(request);
    let ip = request.remote_addr().map(|a| a.ip());
    check_serial(state, agent_id, serial, ip).await
}

async fn check_serial(
    state: &AppState,
    agent_id: String,
    serial: String,
    ip: Option<IpAddr>,
) -> Result<Peer, Status> {
    let row = sqlx::query(
        "SELECT repository_id, status, cert_serial, prev_cert_serial, prev_cert_until \
         FROM agents WHERE id = $1",
    )
    .bind(&agent_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?;
    let Some(row) = row else {
        if state.registry.note_unknown_agent() {
            tracing::warn!(
                agent_id,
                serial,
                ?ip,
                "refused a certificate of an agent the catalog does not know; repeats are not logged for ten minutes"
            );
        }
        return Err(cert_stale());
    };
    let status: String = row.try_get("status").map_err(internal)?;
    if status == "revoked" || state.registry.is_revoked(&agent_id) {
        state.registry.revoke(&agent_id);
        return Err(revoked());
    }
    let current: String = row.try_get("cert_serial").map_err(internal)?;
    let previous: Option<String> = row.try_get("prev_cert_serial").map_err(internal)?;
    let until: Option<i64> = row.try_get("prev_cert_until").map_err(internal)?;
    let fresh = serial == current;
    let in_window =
        previous.as_deref() == Some(serial.as_str()) && until.is_some_and(|t| now_ms() < t);
    if !fresh && !in_window {
        if state.registry.note_cert_rejected(&agent_id, &serial, ip) {
            tracing::warn!(
                agent_id,
                serial,
                ?ip,
                "refused an agent certificate that is not the current one; repeats are not logged for ten minutes"
            );
        }
        return Err(cert_stale());
    }
    Ok(Peer {
        agent_id,
        repository_id: row.try_get("repository_id").map_err(internal)?,
        serial,
        fresh,
    })
}

const MAX_MOUNTS: usize = 64;
const MAX_HOOK_SCRIPTS: usize = 256;
const MAX_RESTORE_ROOTS: usize = 32;
const MAX_FIELD_BYTES: usize = 255;

fn clip(text: &str) -> String {
    let mut end = text.len().min(MAX_FIELD_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;
type RestorePlanStream = Pin<Box<dyn Stream<Item = Result<RestorePlanBatch, Status>> + Send>>;
type ReadPackRangeStream = Pin<Box<dyn Stream<Item = Result<PackBytes, Status>> + Send>>;
type FetchAgentUpdateStream = Pin<Box<dyn Stream<Item = Result<UpdateChunk, Status>> + Send>>;

async fn write_seen(state: &AppState, agent_id: &str, heartbeat: Option<&Heartbeat>) {
    let now = now_ms();
    let result = match heartbeat {
        None => {
            sqlx::query("UPDATE agents SET last_seen_at = $1 WHERE id = $2")
                .bind(now)
                .bind(agent_id)
                .execute(&state.db.pool)
                .await
        }
        Some(hb) => {
            let hooks = hb.hook_facts.as_ref().map(|h| {
                serde_json::json!({
                    "allow_inline": h.allow_inline,
                    "scripts": h.scripts.iter().take(MAX_HOOK_SCRIPTS).map(|s| serde_json::json!({
                        "name": clip(&s.name),
                        "ok": s.ok,
                    })).collect::<Vec<_>>(),
                })
            });
            let mut facts = serde_json::json!({
                "uptime_s": hb.uptime_s,
                "mounts": hb.mounts.iter().take(MAX_MOUNTS).map(|m| serde_json::json!({
                    "path": clip(&m.path),
                    "fs_type": clip(&m.fs_type),
                    "total_bytes": m.total_bytes,
                    "free_bytes": m.free_bytes,
                })).collect::<Vec<_>>(),
            });
            if let Some(hooks) = hooks {
                facts["hooks"] = hooks;
            }
            if let Some(restore) = &hb.restore_facts {
                facts["restore"] = serde_json::json!({
                    "roots": restore.roots.iter().take(MAX_RESTORE_ROOTS).map(|r| clip(r)).collect::<Vec<_>>(),
                    "allow_in_place": restore.allow_in_place,
                });
            }
            sqlx::query(
                "UPDATE agents SET last_seen_at = $1, kernel = $2, distro = $3, facts_json = $4 WHERE id = $5",
            )
            .bind(now)
            .bind((!hb.kernel.is_empty()).then(|| clip(&hb.kernel)))
            .bind((!hb.distro.is_empty()).then(|| clip(&hb.distro)))
            .bind(facts.to_string())
            .bind(agent_id)
            .execute(&state.db.pool)
            .await
        }
    };
    if let Err(error) = result {
        tracing::warn!(%error, agent_id, "could not record agent activity");
    }
}

async fn store_capabilities(
    state: &AppState,
    agent_id: &str,
    capabilities: &[String],
) -> anyhow::Result<()> {
    sqlx::query("UPDATE agents SET capabilities_json = $1 WHERE id = $2")
        .bind(serde_json::to_string(capabilities)?)
        .bind(agent_id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

#[tonic::async_trait]
impl Agent for AgentService {
    type SessionStream = SessionStream;
    type RestorePlanStream = RestorePlanStream;
    type ReadPackRangeStream = ReadPackRangeStream;
    type FetchAgentUpdateStream = FetchAgentUpdateStream;

    async fn session(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let state = self.state.clone();
        let peer = check_peer(&state, &request).await?;
        let Peer {
            agent_id,
            repository_id,
            serial: cert_serial,
            fresh,
        } = peer;
        let peer_ip = request.remote_addr().map(|a| a.ip());
        let mut inbound = request.into_inner();

        let first = tokio::time::timeout(HELLO_TIMEOUT, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no Hello within 10 seconds"))??;
        let hello = match first.and_then(|m| m.body) {
            Some(AgentBody::Hello(hello)) => hello,
            _ => return Err(Status::invalid_argument("the first message must be Hello")),
        };
        let selected_version = select_version(hello.versions.as_ref()).ok_or_else(|| {
            status_with(
                Code::FailedPrecondition,
                "no common protocol version",
                ERROR_VERSION_UNSUPPORTED,
            )
        })?;

        if fresh {
            // the first session with the new certificate ends the previous one's grace window
            sqlx::query(
                "UPDATE agents SET prev_cert_serial = NULL, prev_cert_until = NULL \
                 WHERE id = $1 AND cert_serial = $2 AND prev_cert_serial IS NOT NULL",
            )
            .bind(&agent_id)
            .bind(&cert_serial)
            .execute(&state.db.pool)
            .await
            .map_err(internal)?;
        }

        let capabilities: Vec<String> = hello
            .capabilities
            .iter()
            .take(MAX_CAPABILITIES)
            .map(|c| c.chars().take(MAX_CAPABILITY_LEN).collect())
            .collect();
        if let Err(error) = store_capabilities(&state, &agent_id, &capabilities).await {
            tracing::warn!(%error, agent_id, "could not store agent capabilities");
            return Err(Status::unavailable(
                "the server could not record what this agent supports; reconnect later",
            ));
        }
        if let Err(error) =
            crate::agent_update::rollout::on_hello(&state, &agent_id, &hello.agent_version).await
        {
            tracing::warn!(%error, agent_id, "could not record the agent version");
            return Err(Status::unavailable(
                "the server could not record the agent version; reconnect later",
            ));
        }
        let mut guard = state.registry.register(&agent_id, &repository_id, peer_ip);
        let rx = guard.take_rx();
        state.registry.set_capabilities(&agent_id, capabilities);
        let cancel = guard.cancel.clone();
        if state.registry.is_revoked(&agent_id) {
            return Err(revoked());
        }
        let server_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let distrusted_shards = load_distrusted(&state, &repository_id)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, %agent_id, "could not load the distrusted shards");
                Vec::new()
            })
            .into_iter()
            .map(|id| id.to_vec())
            .collect();
        state.registry.send(
            &agent_id,
            ServerMessage {
                body: Some(ServerBody::HelloAck(HelloAck {
                    selected_version,
                    server_time,
                    distrusted_shards,
                })),
            },
        );
        match writer_material(&state, &repository_id).await {
            Ok(material) => {
                if let Some(key) = material.agent_creds {
                    state.registry.send(
                        &agent_id,
                        ServerMessage {
                            body: Some(ServerBody::UpdateS3Credentials(UpdateS3Credentials {
                                credentials: Some(S3Credentials {
                                    access_key_id: key.access_key_id,
                                    secret_access_key: key.secret_access_key,
                                }),
                            })),
                        },
                    );
                }
            }
            Err(error) => tracing::warn!(%error, %agent_id, "could not load the agent S3 key"),
        }
        crate::grpc::dispatch::messages::session_started(&state, &agent_id, &hello.jobs).await;
        crate::agent_update::rollout::kick(&state);
        tracing::info!(agent_id = %agent_id, cert_serial = %cert_serial, "agent session started");

        tokio::spawn(async move {
            let mut last_write: Option<Instant> = None;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    message = inbound.message() => match message {
                        Ok(Some(_)) if state.registry.is_revoked(&agent_id) => {
                            state.registry.send(&agent_id, ServerMessage {
                                body: Some(ServerBody::Disconnect(Disconnect {
                                    reason: DisconnectReason::Revoked as i32,
                                })),
                            });
                            break;
                        }
                        Ok(Some(AgentMessage { body: Some(AgentBody::Heartbeat(hb)) })) => {
                            state.registry.touch(&agent_id);
                            if last_write.is_none_or(|t| t.elapsed() >= SEEN_WRITE_INTERVAL) {
                                write_seen(&state, &agent_id, Some(&hb)).await;
                                last_write = Some(Instant::now());
                            }
                        }
                        Ok(Some(AgentMessage { body: Some(body) })) => {
                            crate::grpc::dispatch::messages::on_message(&state, &agent_id, body).await;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
            }
            write_seen(&state, &agent_id, None).await;
            tracing::info!(agent_id = %agent_id, "agent session closed");
            drop(guard);
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx).map(Ok))))
    }

    async fn restore_plan(
        &self,
        request: Request<RestorePlanRequest>,
    ) -> Result<Response<Self::RestorePlanStream>, Status> {
        let agent_id = check_peer(&self.state, &request).await?.agent_id;
        let job_id = request.into_inner().job_id;
        let stream = restore::restore_plan(&self.state, &agent_id, &job_id).await?;
        Ok(Response::new(Box::pin(stream)))
    }

    async fn refresh_pack_access(
        &self,
        request: Request<RefreshPackAccessRequest>,
    ) -> Result<Response<RefreshPackAccessResponse>, Status> {
        let agent_id = check_peer(&self.state, &request).await?.agent_id;
        let response =
            restore::refresh_pack_access(&self.state, &agent_id, request.into_inner()).await?;
        Ok(Response::new(response))
    }

    async fn read_pack_range(
        &self,
        request: Request<ReadPackRangeRequest>,
    ) -> Result<Response<Self::ReadPackRangeStream>, Status> {
        let agent_id = check_peer(&self.state, &request).await?.agent_id;
        let stream = restore::read_pack_range(&self.state, &agent_id, request.into_inner()).await?;
        Ok(Response::new(Box::pin(stream)))
    }

    async fn fetch_agent_update(
        &self,
        request: Request<FetchAgentUpdateRequest>,
    ) -> Result<Response<Self::FetchAgentUpdateStream>, Status> {
        let agent_id = check_peer(&self.state, &request).await?.agent_id;
        let stream =
            crate::agent_update::fetch::fetch(&self.state, &agent_id, request.into_inner()).await?;
        Ok(Response::new(Box::pin(stream)))
    }

    async fn renew(
        &self,
        request: Request<RenewRequest>,
    ) -> Result<Response<RenewResponse>, Status> {
        let state = &self.state;
        let peer = check_peer(state, &request).await?;
        let (agent_id, ip) = (peer.agent_id, request.remote_addr().map(|a| a.ip()));
        let lifetime_secs = state.config.agent_cert_lifetime_secs.max(1);
        let cert = state
            .pki
            .sign_agent_csr(
                &request.into_inner().csr_der,
                &agent_id,
                Duration::from_secs(lifetime_secs as u64),
            )
            .map_err(|e| Status::invalid_argument(format!("{e:#}")))?;
        let issued_at = cert.expires_at - lifetime_secs;
        // The previous serial is the caller's, not the stored one: a renew whose answer was lost comes again with
        // the certificate that was current when it was sent, and the window stretches only while that is the current one.
        let now = now_ms();
        let updated = sqlx::query(
            "UPDATE agents SET cert_serial = $1, cert_issued_at = $2, cert_not_after = $3, \
             prev_cert_serial = $5, \
             prev_cert_until = CASE WHEN cert_serial = $5 THEN $6 ELSE prev_cert_until END \
             WHERE id = $4 AND status <> 'revoked' \
             AND (cert_serial = $5 OR (prev_cert_serial = $5 AND prev_cert_until > $7))",
        )
        .bind(&cert.serial_hex)
        .bind(issued_at * 1000)
        .bind(cert.expires_at * 1000)
        .bind(&agent_id)
        .bind(&peer.serial)
        .bind(now + PREV_CERT_GRACE_MS)
        .bind(now)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
        if updated.rows_affected() == 0 {
            return Err(match check_serial(state, agent_id, peer.serial, ip).await {
                Err(status) => status,
                Ok(_) => revoked(),
            });
        }
        tracing::info!(agent_id = %agent_id, cert_serial = %cert.serial_hex, "agent certificate renewed");
        Ok(Response::new(RenewResponse {
            leaf_pem: cert.pem,
            issued_at,
            expires_at: cert.expires_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::clip;

    #[test]
    fn heartbeat_strings_are_bounded_on_a_char_boundary() {
        assert_eq!(clip("short"), "short");
        assert_eq!(clip(&"a".repeat(1000)).len(), 255);
        assert!(clip(&"é".repeat(300)).len() <= 255);
    }
}
