use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, DisconnectReason, Hello, RenewRequest, S3Credentials, UpdateAssign, UpdateState,
    UpdateStatus, Versions,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

use crate::config::AgentSettings;
use crate::connect::channel;
use crate::identity::{AgentConfig, Identity, Paths, load_pair, replace_pair, write_json_file};
use crate::jobs::JobHub;
use crate::update::control::{
    Action, SWITCHING_WAIT, UpdateControl, deliver, send_switching, status,
};
use crate::update::guard::{self, StartAction};
use crate::update::{UpdateOutcome, UpdateRuntime, apply, fetch};
use crate::{AgentError, NetOptions, facts, socket, tls};

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const STABLE_STREAM: Duration = Duration::from_secs(60);
const RENEW_RETRY: Duration = Duration::from_secs(60);
const MAX_SKEW_SECS: i64 = 300;
const JOBS_DRAIN: Duration = Duration::from_secs(30);
pub const CAPABILITIES: [&str; 3] = ["restore_v1", "limits_v1", "gc_v1"];
const CAP_UPDATE: &str = "update_v1";

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
    Restart,
    Closed { lived: Duration },
}

type UpdateDone = (String, UpdateOutcome);

/// The task that downloads and installs an update. A session that ends while
/// the file is still being downloaded takes the download with it; once the
/// install has begun the task is left to finish and reports to the session
/// that is up by then.
struct UpdateTask {
    handle: JoinHandle<()>,
    runtime: UpdateRuntime,
}

impl Drop for UpdateTask {
    fn drop(&mut self) {
        if self.runtime.flight.cancel_download() {
            self.handle.abort();
            for temp in apply::temp_files(&self.runtime.exe) {
                let _ = std::fs::remove_file(temp);
            }
        }
    }
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub async fn run(args: RunArgs) -> Result<(), AgentError> {
    run_with(args, UpdateRuntime::detect()).await
}

pub async fn run_with(args: RunArgs, runtime: UpdateRuntime) -> Result<(), AgentError> {
    let first = load_identity(&args.paths)?;
    let (paths, started) = (args.paths.clone(), runtime.clone());
    let start = tokio::task::spawn_blocking(move || guard::on_start(&paths, &started, now_ms()))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
    match start {
        Ok(StartAction::Restart) => return Ok(()),
        Ok(StartAction::Continue) => {}
        Err(error) => tracing::warn!(%error, "the update state could not be checked at start"),
    }
    let hub = JobHub::open(&args.paths, &first.agent_id)?;
    hub.set_net(args.net.clone());
    let socket_stop = args.shutdown.child_token();
    tokio::spawn(socket::serve(
        Arc::clone(&hub),
        AgentSettings::load(&args.paths).socket_path(),
        socket_stop.clone(),
    ));
    let result = run_sessions(&args, &hub, &runtime).await;
    socket_stop.cancel();
    hub.shutdown();
    hub.wait_idle(JOBS_DRAIN).await;
    result
}

async fn run_sessions(
    args: &RunArgs,
    hub: &Arc<JobHub>,
    runtime: &UpdateRuntime,
) -> Result<(), AgentError> {
    let config = AgentConfig::load(&args.paths)?.ok_or_else(|| {
        AgentError::NotEnrolled(format!("{} is missing", args.paths.agent_json().display()))
    })?;
    let first = load_identity(&args.paths)?;
    tracing::info!(agent_id = %first.agent_id, "agent starting");

    let (done_tx, mut done_rx) = mpsc::channel(1);
    let mut backoff = BACKOFF_MIN;
    loop {
        if args.shutdown.is_cancelled() {
            return Ok(());
        }
        let identity = load_identity(&args.paths)?;
        warn_if_expired(&identity);
        let session = session_once(
            args,
            &config,
            &identity,
            hub,
            runtime,
            &done_tx,
            &mut done_rx,
        );
        // connecting does not watch the token, and a switch must not wait for it
        let result = tokio::select! {
            result = session => result,
            _ = args.shutdown.cancelled() => Ok(Outcome::Shutdown),
        };
        match result {
            Ok(Outcome::Shutdown | Outcome::Restart) => return Ok(()),
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

/// A new mTLS channel to the server with the agent's current certificate.
pub async fn agent_channel(paths: &Paths, net: &NetOptions) -> Result<Channel, AgentError> {
    let config = AgentConfig::load(paths)?.ok_or_else(|| {
        AgentError::NotEnrolled(format!("{} is missing", paths.agent_json().display()))
    })?;
    channel_for(paths, &config, net).await
}

async fn channel_for(
    paths: &Paths,
    config: &AgentConfig,
    net: &NetOptions,
) -> Result<Channel, AgentError> {
    let ca_pem = std::fs::read_to_string(paths.ca())?;
    let (cert_pem, key_pem) = load_pair(paths, tls::pair_matches)?;
    let tls_config = tls::client_config(&config.trust, &ca_pem, Some((&cert_pem, &key_pem)))?;
    channel(&config.grpc_address, &config.server_name, tls_config, net).await
}

fn update_message(status: UpdateStatus) -> AgentMessage {
    AgentMessage {
        body: Some(AgentBody::UpdateStatus(status)),
    }
}

fn spawn_update(
    args: &RunArgs,
    channel: Channel,
    runtime: &UpdateRuntime,
    hub: &Arc<JobHub>,
    assign: UpdateAssign,
    done: mpsc::Sender<UpdateDone>,
) -> UpdateTask {
    let (paths, update, hub) = (args.paths.clone(), runtime.clone(), Arc::clone(hub));
    let shutdown = args.shutdown.clone();
    let handle = tokio::spawn(async move {
        let downloaded = fetch::download(channel, &assign, &update.exe).await;
        if !update.flight.finish_download() {
            if let Ok((binary, _)) = downloaded {
                let _ = tokio::fs::remove_file(&binary).await;
            }
            return;
        }
        let outcome = match downloaded {
            Ok((binary, signature)) => {
                let busy = || hub.busy();
                apply::install_files(&paths, &update, &assign, &binary, &signature, &busy).await
            }
            Err(reason) => UpdateOutcome::Rejected(reason),
        };
        deliver(&done, assign.update_id, outcome, &shutdown).await;
    });
    UpdateTask {
        handle,
        runtime: runtime.clone(),
    }
}

async fn session_once(
    args: &RunArgs,
    config: &AgentConfig,
    identity: &Identity,
    hub: &Arc<JobHub>,
    runtime: &UpdateRuntime,
    done_tx: &mpsc::Sender<UpdateDone>,
    done_rx: &mut mpsc::Receiver<UpdateDone>,
) -> Result<Outcome, AgentError> {
    let paths = &args.paths;
    let channel = channel_for(paths, config, &args.net).await?;

    let (tx, rx) = mpsc::channel::<AgentMessage>(8);
    let jobs = hub.session_reset();
    let mut capabilities = CAPABILITIES.map(String::from).to_vec();
    if runtime.enabled {
        capabilities.push(CAP_UPDATE.to_string());
    }
    tx.send(AgentMessage {
        body: Some(AgentBody::Hello(Hello {
            versions: Some(Versions {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            }),
            agent_version: runtime.version.clone(),
            jobs,
            capabilities,
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
            crate::jobs::distrusted::record_or_warn(paths, &ack.distrusted_shards);
            ack.server_time
        }
        _ => {
            return Err(AgentError::Connect(
                "the server did not acknowledge the session".into(),
            ));
        }
    };
    tracing::info!(agent_id = %identity.agent_id, "connected to the server");

    let mut control = UpdateControl::load(paths, Arc::clone(&runtime.flight));
    let mut _update_task: Option<UpdateTask> = None;
    let (report_paths, report_runtime) = (paths.clone(), runtime.clone());
    let report =
        tokio::task::spawn_blocking(move || guard::after_connect(&report_paths, &report_runtime))
            .await
            .map_err(anyhow::Error::from)?;
    if let Some(report) = report
        && tx.send(update_message(report)).await.is_err()
    {
        return Ok(Outcome::Closed {
            lived: Duration::ZERO,
        });
    }

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
                let mut heartbeat = facts.next().await;
                heartbeat.hook_facts = Some(facts::hook_facts(paths));
                let message = AgentMessage { body: Some(AgentBody::Heartbeat(heartbeat)) };
                if tx.send(message).await.is_err() {
                    return Ok(Outcome::Closed { lived: started.elapsed() });
                }
            }
            Some((update_id, outcome)) = done_rx.recv() => {
                control.finish();
                _update_task = None;
                let reply = match outcome {
                    UpdateOutcome::Switched => {
                        send_switching(
                            &tx,
                            status(&update_id, UpdateState::Switching, ""),
                            SWITCHING_WAIT,
                        )
                        .await;
                        return Ok(Outcome::Restart);
                    }
                    UpdateOutcome::Rejected(reason) => {
                        status(&update_id, UpdateState::Rejected, &reason)
                    }
                    UpdateOutcome::Busy => status(&update_id, UpdateState::Busy, ""),
                };
                if tx.send(update_message(reply)).await.is_err() {
                    return Ok(Outcome::Closed { lived: started.elapsed() });
                }
            }
            _ = hub.changed() => {
                for message in hub.outbound() {
                    if tx.send(message).await.is_err() {
                        return Ok(Outcome::Closed { lived: started.elapsed() });
                    }
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
                    Some(ServerBody::UpdateStatusAck(ack)) => control.on_ack(paths, &ack.update_id),
                    Some(ServerBody::UpdateAssign(assign)) => {
                        let action = if runtime.enabled {
                            control.on_assign(&assign, hub.busy())
                        } else {
                            Action::Reply(status(
                                &assign.update_id,
                                UpdateState::Rejected,
                                "Self-update is not available on this host.",
                            ))
                        };
                        match action {
                            Action::Ignore => {}
                            Action::Reply(reply) => {
                                if tx.send(update_message(reply)).await.is_err() {
                                    return Ok(Outcome::Closed { lived: started.elapsed() });
                                }
                            }
                            Action::Start => {
                                _update_task = Some(spawn_update(
                                    args, channel.clone(), runtime, hub, assign, done_tx.clone(),
                                ));
                            }
                        }
                    }
                    Some(ServerBody::UpdateS3Credentials(update)) => {
                        if let Some(key) = update.credentials {
                            store_s3_credentials(paths, &key)?;
                        }
                    }
                    Some(body) => hub.handle(body),
                    None => {}
                },
                Ok(None) => return Ok(Outcome::Closed { lived: started.elapsed() }),
                Err(s) => return Err(AgentError::from_status(&s)),
            }
        }
    }
}
