pub mod agent;
pub mod dispatch;
pub mod enrollment;
pub mod registry;
pub mod restore;

use std::time::Duration;

use anyhow::Context;
use lodestore_proto::agent::v1::agent_server::AgentServer;
use lodestore_proto::agent::v1::enrollment_server::EnrollmentServer;
use lodestore_proto::agent::v1::{DisconnectReason, Versions};
use lodestore_proto::{ERROR_METADATA_KEY, PROTOCOL_VERSION};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Code, Request, Status};
use x509_parser::extensions::GeneralName;

use crate::catalog::RepositoryUnavailable;
use crate::config::GrpcConfig;
use crate::pki::AGENT_URI_PREFIX;
use crate::state::AppState;

pub fn status_with(code: Code, message: &str, marker: &'static str) -> Status {
    let mut status = Status::new(code, message);
    status
        .metadata_mut()
        .insert(ERROR_METADATA_KEY, MetadataValue::from_static(marker));
    status
}

pub fn internal(error: impl std::fmt::Display) -> Status {
    tracing::error!(%error, "gRPC handler failed");
    Status::internal("internal error")
}

pub fn internal_or_unavailable(error: anyhow::Error) -> Status {
    match error.downcast_ref::<RepositoryUnavailable>() {
        Some(_) => Status::failed_precondition("the repository is unavailable"),
        None => internal(error),
    }
}

pub fn select_version(versions: Option<&Versions>) -> Option<u32> {
    let v = versions?;
    (v.min <= PROTOCOL_VERSION && PROTOCOL_VERSION <= v.max).then_some(PROTOCOL_VERSION)
}

pub fn peer_agent_id<T>(req: &Request<T>) -> Result<String, Status> {
    let unauthenticated = || Status::unauthenticated("a client certificate is required");
    let certs = req.peer_certs().ok_or_else(unauthenticated)?;
    let leaf = certs.first().ok_or_else(unauthenticated)?;
    let (_, cert) =
        x509_parser::parse_x509_certificate(leaf.as_ref()).map_err(|_| unauthenticated())?;
    let san = cert
        .subject_alternative_name()
        .map_err(|_| unauthenticated())?
        .ok_or_else(unauthenticated)?;
    san.value
        .general_names
        .iter()
        .find_map(|name| match name {
            GeneralName::URI(uri) => uri.strip_prefix(AGENT_URI_PREFIX).map(str::to_string),
            _ => None,
        })
        .filter(|id| !id.is_empty())
        .ok_or_else(unauthenticated)
}

pub fn external_identity(
    grpc: &GrpcConfig,
) -> anyhow::Result<Option<(String, zeroize::Zeroizing<String>)>> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};

    let (cert_file, key_file) = match (&grpc.tls_cert_file, &grpc.tls_key_file) {
        (None, None) => return Ok(None),
        (Some(cert), Some(key)) => (cert, key),
        _ => anyhow::bail!("grpc.tls_cert_file and grpc.tls_key_file must be set together"),
    };
    let cert = std::fs::read_to_string(cert_file)
        .with_context(|| format!("reading {}", cert_file.display()))?;
    let key = zeroize::Zeroizing::new(
        std::fs::read_to_string(key_file)
            .with_context(|| format!("reading {}", key_file.display()))?,
    );
    CertificateDer::from_pem_slice(cert.as_bytes())
        .with_context(|| format!("{} holds no PEM certificate", cert_file.display()))?;
    PrivateKeyDer::from_pem_slice(key.as_bytes())
        .with_context(|| format!("{} holds no PEM private key", key_file.display()))?;
    Ok(Some((cert, key)))
}

/// Expiry of the certificate the gRPC listener serves, in milliseconds, and
/// whether it is the operator's own file rather than the built-in one.
pub fn server_cert_not_after(state: &AppState) -> anyhow::Result<(i64, bool)> {
    use rustls_pki_types::CertificateDer;
    use rustls_pki_types::pem::PemObject;

    let Some((pem, _)) = external_identity(&state.config.grpc)? else {
        return Ok((state.pki.grpc_not_after() * 1000, false));
    };
    let der = CertificateDer::from_pem_slice(pem.as_bytes())
        .context("the server certificate holds no PEM certificate")?;
    let (_, cert) = x509_parser::parse_x509_certificate(der.as_ref())
        .map_err(|error| anyhow::anyhow!("parsing the server certificate: {error}"))?;
    Ok((cert.validity().not_after.timestamp() * 1000, true))
}

type Material = (String, zeroize::Zeroizing<String>);

const STOP_GRACE: Duration = Duration::from_secs(10);

fn current_material(state: &AppState) -> anyhow::Result<Material> {
    Ok(external_identity(&state.config.grpc)?.unwrap_or_else(|| state.pki.grpc_identity()))
}

fn build(
    state: &AppState,
    material: &Material,
) -> anyhow::Result<tonic::transport::server::Router> {
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(&material.0, material.1.as_bytes()))
        .client_ca_root(Certificate::from_pem(&state.pki.ca_pem))
        .client_auth_optional(true);
    Ok(Server::builder()
        .tls_config(tls)?
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .add_service(EnrollmentServer::new(enrollment::EnrollmentService {
            state: state.clone(),
        }))
        .add_service(AgentServer::new(agent::AgentService {
            state: state.clone(),
        })))
}

async fn reload(
    state: &AppState,
    current: &Material,
) -> anyhow::Result<Option<(Material, tonic::transport::server::Router)>> {
    state
        .pki
        .renew_leaf_if_due(&state.db, &state.sealer, &state.config)
        .await?;
    let next = current_material(state)?;
    if next.0 == current.0 && next.1.as_str() == current.1.as_str() {
        return Ok(None);
    }
    let router = build(state, &next)?;
    Ok(Some((next, router)))
}

/// Serves gRPC and swaps the TLS certificate without a process restart. The
/// certificate files (or the built-in leaf, which is also renewed before it
/// expires) are checked every `cert_reload_secs`. On a change the listener is
/// restarted on the same socket; agents are told to reconnect and do so within
/// seconds. A certificate that does not load keeps the running one.
pub async fn serve(
    state: AppState,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    tokio::spawn(dispatch::run(state.clone(), shutdown.clone()));
    let socket = listener.into_std()?;
    let interval = Duration::from_secs(state.config.grpc.cert_reload_secs.max(1));
    let mut material = current_material(&state)?;
    let mut router = build(&state, &material)?;
    loop {
        let round = shutdown.child_token();
        let incoming = TcpListenerStream::new(TcpListener::from_std(socket.try_clone()?)?);
        let mut task = tokio::spawn(
            router.serve_with_incoming_shutdown(incoming, round.clone().cancelled_owned()),
        );
        let (next_material, next_router) = loop {
            tokio::select! {
                result = &mut task => return Ok(result??),
                _ = tokio::time::sleep(interval) => {}
            }
            match reload(&state, &material).await {
                Ok(Some(next)) => break next,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "keeping the running gRPC server certificate")
                }
            }
        };
        tracing::info!("the gRPC server certificate changed; restarting the listener");
        state.registry.disconnect_all(DisconnectReason::Shutdown);
        round.cancel();
        if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
            task.abort();
        }
        (material, router) = (next_material, next_router);
    }
}
