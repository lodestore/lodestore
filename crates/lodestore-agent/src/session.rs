use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, DisconnectReason, Hello, RenewRequest, S3Credentials, Versions,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

use crate::connect::channel;
use crate::identity::{AgentConfig, Identity, Paths, load_pair, replace_pair, write_json_file};
use crate::{AgentError, NetOptions, facts, tls};

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const STABLE_STREAM: Duration = Duration::from_secs(60);
const RENEW_RETRY: Duration = Duration::from_secs(60);
const MAX_SKEW_SECS: i64 = 300;

pub struct RunArgs {
    pub paths: Paths,
    pub net: NetOptions,
    pub heartbeat: Duration,
    pub shutdown: CancellationToken,
}

enum Outcome {
    Shutdown,
    Renewed,
    Replaced,
    Closed { lived: Duration },
}

fn jittered(backoff: Duration) -> Duration {
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    backoff.mul_f64(0.5 + f64::from(byte[0]) / 512.0)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn warn_if_expired(identity: &Identity) {
    if identity.expires_at <= now_secs() {
        tracing::warn!(
            expires_at = identity.expires_at,
            "this host's clock says the agent certificate has expired; trying to connect anyway. Check the host clock; if it is right, the host was offline too long and must be registered again with install.sh --force and a new join token"
        );
    }
}

fn load_identity(paths: &Paths) -> Result<Identity, AgentError> {
    Identity::load(paths)?.ok_or_else(|| {
        AgentError::NotEnrolled(format!("{} is missing", paths.identity_json().display()))
    })
}

pub async fn run(args: RunArgs) -> Result<(), AgentError> {
    let config = AgentConfig::load(&args.paths)?.ok_or_else(|| {
        AgentError::NotEnrolled(format!("{} is missing", args.paths.agent_json().display()))
    })?;
    let first = load_identity(&args.paths)?;
    tracing::info!(agent_id = %first.agent_id, "agent starting");

    let mut backoff = BACKOFF_MIN;
    loop {
        if args.shutdown.is_cancelled() {
            return Ok(());
        }
        let identity = load_identity(&args.paths)?;
        warn_if_expired(&identity);
        match session_once(&args, &config, &identity).await {
            Ok(Outcome::Shutdown) => return Ok(()),
            Ok(Outcome::Renewed) => {
                tracing::info!("certificate renewed; reconnecting");
                continue;
            }
            Ok(Outcome::Replaced) => {
                tracing::warn!("another session for this agent replaced this one; backing off");
                backoff = BACKOFF_MAX;
            }
            Ok(Outcome::Closed { lived }) => {
                if lived > STABLE_STREAM {
                    backoff = BACKOFF_MIN;
                }
                tracing::info!("session closed; reconnecting");
            }
            Err(AgentError::Revoked) => {
                tracing::error!(
                    "this agent was revoked by the server; reinstall with --force and a new join token to use this host again"
                );
                return Err(AgentError::Revoked);
            }
            Err(e) => tracing::warn!(error = %e, "session failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(jittered(backoff)) => {}
            _ = args.shutdown.cancelled() => return Ok(()),
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

fn store_s3_credentials(paths: &Paths, key: &S3Credentials) -> Result<(), AgentError> {
    let value = serde_json::json!({
        "access_key_id": key.access_key_id,
        "secret_access_key": key.secret_access_key,
    });
    let current: Option<serde_json::Value> = std::fs::read(paths.s3_credentials())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if current.as_ref() == Some(&value) {
        return Ok(());
    }
    write_json_file(&paths.s3_credentials(), &value)?;
    tracing::info!("agent S3 key updated");
    Ok(())
}

fn renew_delay(identity: &Identity, server_now: i64) -> Duration {
    let renew_at = identity.issued_at + (identity.expires_at - identity.issued_at) * 2 / 3;
    Duration::from_secs((renew_at - server_now).max(0) as u64)
}

async fn renew(channel: Channel, paths: &Paths, identity: &Identity) -> Result<(), AgentError> {
    let (key, csr_der) = crate::enroll::new_key_and_csr()?;
    let response = AgentClient::new(channel)
        .renew(RenewRequest { csr_der })
        .await
        .map_err(|s| AgentError::from_status(&s))?
        .into_inner();
    replace_pair(
        paths,
        key.serialize_pem().as_bytes(),
        response.leaf_pem.as_bytes(),
    )?;
    Identity {
        issued_at: response.issued_at,
        expires_at: response.expires_at,
        ..identity.clone()
    }
    .save(paths)
}

async fn session_once(
    args: &RunArgs,
    config: &AgentConfig,
    identity: &Identity,
) -> Result<Outcome, AgentError> {
    let paths = &args.paths;
    let ca_pem = std::fs::read_to_string(paths.ca())?;
    let (cert_pem, key_pem) = load_pair(paths, tls::pair_matches)?;
    let tls_config = tls::client_config(&config.trust, &ca_pem, Some((&cert_pem, &key_pem)))?;
    let channel = channel(
        &config.grpc_address,
        &config.server_name,
        tls_config,
        &args.net,
    )
    .await?;

    let (tx, rx) = mpsc::channel::<AgentMessage>(8);
    tx.send(AgentMessage {
        body: Some(AgentBody::Hello(Hello {
            versions: Some(Versions {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            }),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
        })),
    })
    .await
    .map_err(|_| AgentError::Connect("session channel closed".into()))?;

    let mut inbound = AgentClient::new(channel.clone())
        .session(ReceiverStream::new(rx))
        .await
        .map_err(|s| AgentError::from_status(&s))?
        .into_inner();

    let first = tokio::select! {
        m = inbound.message() => m.map_err(|s| AgentError::from_status(&s))?,
        _ = args.shutdown.cancelled() => return Ok(Outcome::Shutdown),
    };
    let server_now = match first.and_then(|m| m.body) {
        Some(ServerBody::HelloAck(ack)) => {
            let skew = now_secs() - ack.server_time;
            if skew.abs() > MAX_SKEW_SECS {
                tracing::warn!(
                    skew_s = skew,
                    "the local clock differs from the server's by more than 5 minutes"
                );
            }
            ack.server_time
        }
        _ => {
            return Err(AgentError::Connect(
                "the server did not acknowledge the session".into(),
            ));
        }
    };
    tracing::info!(agent_id = %identity.agent_id, "connected to the server");

    let mut ticker = tokio::time::interval(args.heartbeat);
    let started = tokio::time::Instant::now();
    let renew_timer = tokio::time::sleep(renew_delay(identity, server_now));
    let mut facts = facts::Collector::default();
    tokio::pin!(renew_timer);
    loop {
        tokio::select! {
            _ = args.shutdown.cancelled() => return Ok(Outcome::Shutdown),
            _ = &mut renew_timer => match renew(channel.clone(), paths, identity).await {
                Ok(()) => return Ok(Outcome::Renewed),
                Err(AgentError::Revoked) => return Err(AgentError::Revoked),
                Err(error) => {
                    tracing::warn!(%error, "certificate renewal failed; retrying in a minute");
                    renew_timer.as_mut().reset(tokio::time::Instant::now() + RENEW_RETRY);
                }
            },
            _ = ticker.tick() => {
                let message = AgentMessage { body: Some(AgentBody::Heartbeat(facts.next().await)) };
                if tx.send(message).await.is_err() {
                    return Ok(Outcome::Closed { lived: started.elapsed() });
                }
            }
            message = inbound.message() => match message {
                Ok(Some(m)) => match m.body {
                    Some(ServerBody::Disconnect(d)) => {
                        return if d.reason == DisconnectReason::Revoked as i32 {
                            Err(AgentError::Revoked)
                        } else if d.reason == DisconnectReason::Replaced as i32 {
                            Ok(Outcome::Replaced)
                        } else {
                            Ok(Outcome::Closed { lived: started.elapsed() })
                        };
                    }
                    Some(ServerBody::UpdateS3Credentials(update)) => {
                        if let Some(key) = update.credentials {
                            store_s3_credentials(paths, &key)?;
                        }
                    }
                    _ => {}
                },
                Ok(None) => return Ok(Outcome::Closed { lived: started.elapsed() }),
                Err(s) => return Err(AgentError::from_status(&s)),
            }
        }
    }
}
