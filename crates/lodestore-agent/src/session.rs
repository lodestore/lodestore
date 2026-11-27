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
use crate::discovery::{self, Discovery};
use crate::identity::{AgentConfig, Identity, Paths, load_pair, replace_pair, write_json_file};
use crate::jobs::JobHub;
use crate::pve::ROOT_UID;
use crate::pve::socket::PveSessions;
use crate::shared_channel::SharedChannel;
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
pub const CAPABILITIES: [&str; 9] = [
    "restore_v1",
    "limits_v1",
    "gc_v1",
    "restore_roots_v1",
    "shard_limit_v1",
    "source:files",
    "host_key_v1",
    "location_v1",
    "sas_refresh_v1",
];
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
    run_with(args, UpdateRuntime::detect(), discovery::Config::system()).await
}

pub async fn run_with(
    args: RunArgs,
    runtime: UpdateRuntime,
    discovery: discovery::Config,
) -> Result<(), AgentError> {
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
    let pve = Arc::new(PveSessions::new(ROOT_UID));
    tokio::spawn(socket::serve_with(
        Arc::clone(&hub),
        pve,
        AgentSettings::load(&args.paths).socket_path(),
        socket_stop.clone(),
    ));
    let discovery = Discovery::spawn(discovery, args.paths.clone(), args.shutdown.clone());
    hub.set_discovery(discovery.clone());
    let result = run_sessions(&args, &hub, &runtime, &discovery).await;
    socket_stop.cancel();
    hub.shutdown();
    hub.wait_idle(JOBS_DRAIN).await;
    result
}

async fn run_sessions(
    args: &RunArgs,
    hub: &Arc<JobHub>,
    runtime: &UpdateRuntime,
    discovery: &Discovery,
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
            discovery,
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
            Err(AgentError::CertificateStale) => {
                tracing::error!(
                    "the server does not accept this agent's certificate; if this host is legitimate, re-enroll it: run install.sh with --force and a new join token"
                );
                backoff = BACKOFF_MAX;
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

fn apply_host_key(paths: &Paths, host_key: &str) {
    if let Err(error) = store_host_key(paths, host_key) {
        tracing::warn!(%error, "could not store the SFTP host key");
    }
}

fn store_host_key(paths: &Paths, host_key: &str) -> Result<(), AgentError> {
    let path = paths.repository_json();
    let Some(mut file) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        tracing::warn!("repository.json is unreadable; SFTP host key not stored");
        return Ok(());
    };
    if file.get("backend").and_then(|b| b.as_str()) != Some("sftp") {
        tracing::warn!("SFTP host key received for a repository that is not SFTP; ignored");
        return Ok(());
    }
    let Some(options) = file.get_mut("options").and_then(|o| o.as_object_mut()) else {
        tracing::warn!("repository.json has no options; SFTP host key not stored");
        return Ok(());
    };
    if options.get("host_key").and_then(|k| k.as_str()) == Some(host_key) {
        return Ok(());
    }
    if lodestore_format::store::SftpStore::host_key_fingerprint(host_key).is_err() {
        tracing::warn!("SFTP host key from the server is not a usable ed25519/ECDSA key; ignored");
        return Ok(());
    }
    options.insert("host_key".into(), host_key.into());
    write_json_file(&path, &file)?;
    tracing::info!("SFTP host key updated");
    Ok(())
}

fn apply_location(paths: &Paths, location: &lodestore_proto::agent::v1::RepoLocation) {
    if let Err(error) = store_location(paths, location) {
        tracing::warn!(%error, "could not store the agent storage credentials");
    }
}

/// The options a pushed location may change: the agent's own credentials and
/// the server credentials they take the place of. Every other option, a
/// secret one such as an Azure encryption key included, must stay as it is.
fn credential_names(backend: &str) -> &'static [&'static str] {
    match backend {
        "azblob" => &["sas_token", "account_key"],
        "gcs" => &["credential", "token"],
        "sftp" => &["user", "key"],
        _ => &[],
    }
}

/// Stores the credentials of a location the server pushed in `repository.json`;
/// a location that would change anything else is ignored.
pub fn store_location(
    paths: &Paths,
    new: &lodestore_proto::agent::v1::RepoLocation,
) -> Result<(), AgentError> {
    let path = paths.repository_json();
    let Some(mut file) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        tracing::warn!("repository.json is unreadable; agent storage credentials not stored");
        return Ok(());
    };
    if file.get("repository_id").and_then(|v| v.as_str()) != Some(new.repository_id.as_str())
        || file.get("backend").and_then(|v| v.as_str()) != Some(new.backend.as_str())
    {
        tracing::warn!("storage location is for another repository or backend; ignored");
        return Ok(());
    }
    let proposed = lodestore_format::store::RepoLocation {
        backend: new.backend.clone(),
        options: new.options.clone().into_iter().collect(),
    };
    if let Err(error) = proposed.validate() {
        tracing::warn!(%error, "storage location from the server is not valid; ignored");
        return Ok(());
    }
    let Some(current) = file.get("options").and_then(|o| o.as_object()).cloned() else {
        tracing::warn!("repository.json has no options; agent storage credentials not stored");
        return Ok(());
    };
    let credentials = credential_names(&new.backend);
    let names = current
        .keys()
        .map(String::as_str)
        .chain(new.options.keys().map(String::as_str));
    for name in names {
        if credentials.contains(&name) {
            continue;
        }
        if current.get(name).and_then(|v| v.as_str()) != new.options.get(name).map(String::as_str) {
            tracing::warn!(
                option = name,
                "server tried to change a non-credential option; ignored"
            );
            return Ok(());
        }
    }
    let options: serde_json::Map<String, serde_json::Value> = new
        .options
        .iter()
        .map(|(k, v)| (k.clone(), v.clone().into()))
        .collect();
    if options == current {
        return Ok(());
    }
    file["options"] = options.into();
    write_json_file(&path, &file)?;
    tracing::info!("agent storage credentials updated");
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
    channel: Arc<SharedChannel>,
    runtime: &UpdateRuntime,
    hub: &Arc<JobHub>,
    assign: UpdateAssign,
    done: mpsc::Sender<UpdateDone>,
) -> UpdateTask {
    let (paths, update, hub) = (args.paths.clone(), runtime.clone(), Arc::clone(hub));
    let shutdown = args.shutdown.clone();
    let handle = tokio::spawn(async move {
        let downloaded = fetch::download(&channel, &assign, &update.exe).await;
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

#[allow(clippy::too_many_arguments)]
async fn session_once(
    args: &RunArgs,
    config: &AgentConfig,
    identity: &Identity,
    hub: &Arc<JobHub>,
    runtime: &UpdateRuntime,
    discovery: &Discovery,
    done_tx: &mpsc::Sender<UpdateDone>,
    done_rx: &mut mpsc::Receiver<UpdateDone>,
) -> Result<Outcome, AgentError> {
    let paths = &args.paths;
    let channel = channel_for(paths, config, &args.net).await?;
    hub.channel().replace(channel.clone());

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
                heartbeat.restore_facts = Some(facts::restore_facts(paths));
                heartbeat.source_facts = discovery.current();
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
                                    args, hub.channel(), runtime, hub, assign, done_tx.clone(),
                                ));
                            }
                        }
                    }
                    Some(ServerBody::UpdateS3Credentials(update)) => {
                        if let Some(key) = update.credentials {
                            store_s3_credentials(paths, &key)?;
                        }
                    }
                    Some(ServerBody::UpdateHostKey(update)) => {
                        apply_host_key(paths, &update.host_key);
                    }
                    Some(ServerBody::UpdateLocation(update)) => {
                        if let Some(location) = update.location {
                            apply_location(paths, &location);
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

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPXNmebOc3wEWwOMVZdkvtvsvLmchHYR1uQeOBlJtQWZ";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBPSPxzrVHcF2zQSv3p5NxFPivXZiF0zvk7gjPiCjLBd";
    const KEY_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC7";
    const KEY_CERT: &str =
        "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29t";

    #[test]
    fn capabilities_include_sas_refresh() {
        assert!(CAPABILITIES.contains(&"sas_refresh_v1"));
    }

    fn paths(dir: &tempfile::TempDir) -> Paths {
        Paths {
            config_dir: dir.path().join("etc"),
            state_dir: dir.path().join("var"),
        }
    }

    fn write_repository(paths: &Paths, backend: &str, host_key: &str) -> Vec<u8> {
        let value = serde_json::json!({
            "repository_id": "r1",
            "backend": backend,
            "options": {
                "endpoint": "127.0.0.1:22",
                "user": "backup",
                "key": "placeholder",
                "host_key": host_key,
                "root": "/srv/repo",
            },
        });
        write_json_file(&paths.repository_json(), &value).unwrap();
        std::fs::read(paths.repository_json()).unwrap()
    }

    fn stored(paths: &Paths) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(paths.repository_json()).unwrap()).unwrap()
    }

    #[test]
    fn store_host_key_rewrites_only_the_host_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        write_repository(&paths, "sftp", KEY_A);
        let mut expected = stored(&paths);
        store_host_key(&paths, KEY_B).unwrap();
        expected["options"]["host_key"] = KEY_B.into();
        assert_eq!(stored(&paths), expected);

        let bytes = std::fs::read(paths.repository_json()).unwrap();
        let mtime = std::fs::metadata(paths.repository_json())
            .unwrap()
            .modified()
            .unwrap();
        store_host_key(&paths, KEY_B).unwrap();
        assert_eq!(std::fs::read(paths.repository_json()).unwrap(), bytes);
        let after = std::fs::metadata(paths.repository_json())
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(after, mtime);
    }

    #[test]
    fn store_host_key_ignores_other_backends() {
        for backend in ["s3", "local"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = paths(&dir);
            let before = write_repository(&paths, backend, KEY_A);
            store_host_key(&paths, KEY_B).unwrap();
            assert_eq!(std::fs::read(paths.repository_json()).unwrap(), before);
        }
    }

    #[test]
    fn store_host_key_keeps_the_file_on_a_bad_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let before = write_repository(&paths, "sftp", KEY_A);
        for bad in ["", "garbage", KEY_RSA, KEY_CERT] {
            store_host_key(&paths, bad).unwrap();
            assert_eq!(std::fs::read(paths.repository_json()).unwrap(), before);
        }
    }

    fn proposed(
        repository_id: &str,
        backend: &str,
        options: &[(&str, &str)],
    ) -> lodestore_proto::agent::v1::RepoLocation {
        lodestore_proto::agent::v1::RepoLocation {
            backend: backend.into(),
            repository_id: repository_id.into(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn write_azure(paths: &Paths) -> Vec<u8> {
        let value = serde_json::json!({
            "repository_id": "r1",
            "backend": "azblob",
            "options": {
                "container": "c",
                "endpoint": "http://127.0.0.1:1/acct",
                "account_name": "acct",
                "account_key": "old-account-key",
            },
        });
        write_json_file(&paths.repository_json(), &value).unwrap();
        std::fs::read(paths.repository_json()).unwrap()
    }

    const AZURE: [(&str, &str); 3] = [
        ("container", "c"),
        ("endpoint", "http://127.0.0.1:1/acct"),
        ("account_name", "acct"),
    ];

    #[test]
    fn store_location_changes_only_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        write_azure(&paths);
        let mut options = AZURE.to_vec();
        options.push(("sas_token", "sv=1&sig=new"));
        store_location(&paths, &proposed("r1", "azblob", &options)).unwrap();
        let file = stored(&paths);
        assert_eq!(file["options"]["sas_token"], "sv=1&sig=new");
        assert!(file["options"].get("account_key").is_none());
        assert_eq!(file["options"]["container"], "c");
        assert_eq!(file["repository_id"], "r1");

        let bytes = std::fs::read(paths.repository_json()).unwrap();
        let mtime = std::fs::metadata(paths.repository_json())
            .unwrap()
            .modified()
            .unwrap();
        store_location(&paths, &proposed("r1", "azblob", &options)).unwrap();
        assert_eq!(std::fs::read(paths.repository_json()).unwrap(), bytes);
        let after = std::fs::metadata(paths.repository_json())
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(after, mtime);

        write_repository(&paths, "sftp", KEY_A);
        let mut expected = stored(&paths);
        let sftp = [
            ("endpoint", "127.0.0.1:22"),
            ("host_key", KEY_A),
            ("root", "/srv/repo"),
            ("user", "agent"),
            ("key", "new-private-key"),
        ];
        store_location(&paths, &proposed("r1", "sftp", &sftp)).unwrap();
        expected["options"]["user"] = "agent".into();
        expected["options"]["key"] = "new-private-key".into();
        assert_eq!(stored(&paths), expected);
    }

    #[test]
    fn store_location_refuses_another_repository_or_backend() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let before = write_azure(&paths);
        let mut with_sas = AZURE.to_vec();
        with_sas.push(("sas_token", "sv=1&sig=new"));
        let moved = |name: &str, value: &'static str| {
            let mut o = with_sas.clone();
            o.iter_mut().for_each(|(k, v)| {
                if *k == name {
                    *v = value;
                }
            });
            o
        };
        let mut without_container = with_sas.clone();
        without_container.retain(|(k, _)| *k != "container");
        let mut extra = with_sas.clone();
        extra.push(("publish", "x"));
        for refused in [
            proposed("other", "azblob", &with_sas),
            proposed("r1", "gcs", &[("bucket", "b"), ("credential", "c")]),
            proposed("r1", "azblob", &moved("endpoint", "http://evil/acct")),
            proposed("r1", "azblob", &moved("container", "other")),
            proposed("r1", "azblob", &moved("account_name", "other")),
            proposed("r1", "azblob", &without_container),
            proposed("r1", "azblob", &extra),
            proposed("r1", "azblob", &[]),
        ] {
            store_location(&paths, &refused).unwrap();
            assert_eq!(std::fs::read(paths.repository_json()).unwrap(), before);
        }

        write_repository(&paths, "sftp", KEY_A);
        let before = std::fs::read(paths.repository_json()).unwrap();
        for (name, value) in [
            ("endpoint", "elsewhere:22"),
            ("root", "/other"),
            ("host_key", KEY_B),
        ] {
            let mut o = vec![
                ("endpoint", "127.0.0.1:22"),
                ("host_key", KEY_A),
                ("root", "/srv/repo"),
                ("user", "agent"),
                ("key", "k"),
            ];
            o.iter_mut().for_each(|(k, v)| {
                if *k == name {
                    *v = value;
                }
            });
            store_location(&paths, &proposed("r1", "sftp", &o)).unwrap();
            assert_eq!(
                std::fs::read(paths.repository_json()).unwrap(),
                before,
                "{name}"
            );
        }
    }

    #[test]
    fn store_location_keeps_the_encryption_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let mut file = serde_json::from_slice::<serde_json::Value>(&write_azure(&paths)).unwrap();
        file["options"]["encryption_key"] = "old-cpk".into();
        write_json_file(&paths.repository_json(), &file).unwrap();
        let before = std::fs::read(paths.repository_json()).unwrap();
        let mut options = AZURE.to_vec();
        options.push(("sas_token", "sv=1&sig=new"));
        let mut changed = options.clone();
        changed.push(("encryption_key", "attacker-cpk"));
        for refused in [changed, options] {
            store_location(&paths, &proposed("r1", "azblob", &refused)).unwrap();
            assert_eq!(std::fs::read(paths.repository_json()).unwrap(), before);
        }
    }

    #[test]
    fn store_location_keeps_going_when_the_file_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let before = write_azure(&paths);
        let mut options = AZURE.to_vec();
        options.push(("sas_token", "sv=1&sig=new"));
        std::fs::create_dir(paths.state_dir.join(".repository.json.tmp")).unwrap();
        let location = proposed("r1", "azblob", &options);
        assert!(store_location(&paths, &location).is_err());
        apply_location(&paths, &location);
        assert_eq!(std::fs::read(paths.repository_json()).unwrap(), before);
    }

    #[test]
    fn apply_host_key_keeps_going_when_the_file_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let before = write_repository(&paths, "sftp", KEY_A);
        std::fs::create_dir(paths.state_dir.join(".repository.json.tmp")).unwrap();
        assert!(store_host_key(&paths, KEY_B).is_err());
        apply_host_key(&paths, KEY_B);
        assert_eq!(std::fs::read(paths.repository_json()).unwrap(), before);
    }
}
