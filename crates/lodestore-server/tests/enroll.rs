mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use std::sync::{Arc, Mutex};

use common::{Backend, TestServer, backends, wait_until};
use data_encoding::BASE64URL_NOPAD;
use lodestore_agent::{AgentError, EnrollArgs, NetOptions, RunArgs, connect, tls};
use lodestore_format::keys::WriteKeys;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::{
    AgentMessage, EnrollRequest, EnrollResponse, GetTrustAnchorRequest, Hello, Versions,
};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    PKCS_ECDSA_P256_SHA256,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

fn enroll_args(ts: &TestServer, name: &str, token: String) -> EnrollArgs {
    EnrollArgs {
        token,
        paths: ts.agent_paths(name),
        hostname: Some(format!("host-{name}")),
        net: NetOptions::default(),
    }
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn with_pin(token: &str, pin: &str) -> String {
    let body = token.strip_prefix("lode1.").unwrap();
    let mut payload: Value =
        serde_json::from_slice(&BASE64URL_NOPAD.decode(body.as_bytes()).unwrap()).unwrap();
    payload["f"] = json!(pin);
    format!(
        "lode1.{}",
        BASE64URL_NOPAD.encode(payload.to_string().as_bytes())
    )
}

fn secret_of(token: &str) -> String {
    let body = token.strip_prefix("lode1.").unwrap();
    let payload: Value =
        serde_json::from_slice(&BASE64URL_NOPAD.decode(body.as_bytes()).unwrap()).unwrap();
    payload["s"].as_str().unwrap().to_string()
}

async fn pinned_channel(ts: &TestServer, client_cert: Option<(&str, &str)>) -> (Channel, String) {
    let address = ts.grpc_address();
    let bootstrap = connect::channel(
        &address,
        "127.0.0.1",
        tls::bootstrap_config().unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("bootstrap channel");
    let ca_pem = EnrollmentClient::new(bootstrap)
        .get_trust_anchor(GetTrustAnchorRequest {})
        .await
        .expect("GetTrustAnchor works without a client certificate")
        .into_inner()
        .ca_pem;
    let channel = connect::channel(
        &address,
        "127.0.0.1",
        tls::pinned_config(&ca_pem, client_cert).unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("pinned channel");
    (channel, ca_pem)
}

fn csr_with(params: CertificateParams) -> Vec<u8> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    params.serialize_request(&key).unwrap().der().to_vec()
}

fn enroll_request(token: &str, csr_der: Vec<u8>) -> EnrollRequest {
    EnrollRequest {
        join_secret: secret_of(token),
        csr_der,
        hostname: "raw-host".to_string(),
        os: "linux".to_string(),
        arch: "x86_64".to_string(),
        agent_version: "test".to_string(),
        versions: Some(Versions {
            min: PROTOCOL_VERSION,
            max: PROTOCOL_VERSION,
        }),
        ..Default::default()
    }
}

async fn raw_enroll(
    ts: &TestServer,
    token: &str,
    csr_der: Vec<u8>,
) -> Result<EnrollResponse, tonic::Status> {
    let (channel, _) = pinned_channel(ts, None).await;
    EnrollmentClient::new(channel)
        .enroll(enroll_request(token, csr_der))
        .await
        .map(|r| r.into_inner())
}

async fn count(ts: &TestServer, sql: &'static str) -> i64 {
    sqlx::query(sql)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap()
}

async fn tracer(backend: &Backend) {
    let ts = TestServer::start(backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let token = api
        .join_token(&repo, json!([{ "key": "env", "value": "test" }]))
        .await;

    let args = enroll_args(&ts, "one", token);
    let paths = args.paths.clone();
    let enrolled = lodestore_agent::enroll(args).await.expect("enroll");

    let shutdown = CancellationToken::new();
    let agent = tokio::spawn(lodestore_agent::run(RunArgs {
        paths: paths.clone(),
        net: NetOptions::default(),
        heartbeat: Duration::from_secs(1),
        shutdown: shutdown.clone(),
    }));

    let online = wait_until(Duration::from_secs(10), || async {
        api.hosts().await.iter().any(|h| h["status"] == "online")
    })
    .await;
    assert!(online, "[{}] the host never became online", backend.name());

    let hosts = api.hosts().await;
    assert_eq!(hosts.len(), 1);
    let host = &hosts[0];
    assert_eq!(host["id"], enrolled.agent_id);
    assert_eq!(host["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(host["os"], std::env::consts::OS);
    assert_eq!(host["repository_id"], repo);
    assert_eq!(host["labels"], json!([{ "key": "env", "value": "test" }]));

    assert_eq!(mode(&paths.key()), 0o600);
    assert_eq!(mode(&paths.writer_keys()), 0o600);
    assert_eq!(mode(&paths.state_dir), 0o700);

    let text = std::fs::read_to_string(paths.writer_keys()).unwrap();
    let keys = WriteKeys::from_file_text(&text).expect("a valid writer keys file");
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let config = store.get("config").await.expect("repository config");
    keys.verify_config(&config)
        .expect("keys match the repository config");

    shutdown.cancel();
    agent.await.unwrap().expect("agent stops cleanly");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_tracer_host_online() {
    for backend in backends() {
        tracer(&backend).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_wrong_ca_pin_refused() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;

        let args = enroll_args(&ts, "pin", with_pin(&token, &"00".repeat(32)));
        let paths = args.paths.clone();
        let error = lodestore_agent::enroll(args).await.unwrap_err();
        assert!(matches!(error, AgentError::TrustAnchorMismatch), "{error}");

        let state_empty = !paths.state_dir.exists()
            || std::fs::read_dir(&paths.state_dir)
                .unwrap()
                .next()
                .is_none();
        assert!(state_empty, "nothing may be written before the pin matches");
        assert!(!paths.config_dir.exists());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_token_single_use() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let token = api.join_token(&repo, json!([])).await;
        raw_enroll(&ts, &token, csr_with(CertificateParams::default()))
            .await
            .expect("the first use succeeds");
        let status = raw_enroll(&ts, &token, csr_with(CertificateParams::default()))
            .await
            .expect_err("the second use is refused");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        assert_eq!(
            status.metadata().get("lodestore-error").unwrap(),
            "token_invalid"
        );
        assert_eq!(count(&ts, "SELECT COUNT(*) AS n FROM agents").await, 1);

        let racing = api.join_token(&repo, json!([])).await;
        let (a, b) = tokio::join!(
            raw_enroll(&ts, &racing, csr_with(CertificateParams::default())),
            raw_enroll(&ts, &racing, csr_with(CertificateParams::default())),
        );
        assert_eq!(
            [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(),
            1,
            "[{}] exactly one concurrent Enroll may win",
            backend.name()
        );
        assert_eq!(count(&ts, "SELECT COUNT(*) AS n FROM agents").await, 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_forces_cert_fields() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;

        let mut params = CertificateParams::new(vec!["evil.example".to_string()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let response = raw_enroll(&ts, &token, csr_with(params)).await.unwrap();

        let (_, pem) = x509_parser::pem::parse_x509_pem(response.leaf_pem.as_bytes()).unwrap();
        let cert = pem.parse_x509().unwrap();
        let san = cert.subject_alternative_name().unwrap().unwrap();
        let names: Vec<String> = san
            .value
            .general_names
            .iter()
            .map(|n| format!("{n:?}"))
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(
            names[0].contains(&format!("spiffe://lodestore/agent/{}", response.agent_id)),
            "{names:?}"
        );
        assert!(!names[0].contains("evil.example"));

        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth);
        assert!(!eku.server_auth && !eku.any && !eku.code_signing && !eku.email_protection);
        assert!(eku.other.is_empty());
        assert!(!cert.is_ca());

        let not_before = cert.validity().not_before.timestamp();
        let not_after = cert.validity().not_after.timestamp();
        let expected = 90 * 86_400 + 5 * 60;
        assert!(
            (not_after - not_before - expected).abs() <= 60,
            "validity was {}s",
            not_after - not_before
        );
        assert_eq!(response.issued_at, not_before);
        assert_eq!(response.expires_at, not_after);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_requires_client_cert() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (channel, _) = pinned_channel(&ts, None).await;
        let hello = AgentMessage {
            body: Some(Body::Hello(Hello {
                versions: Some(Versions {
                    min: PROTOCOL_VERSION,
                    max: PROTOCOL_VERSION,
                }),
                agent_version: "test".to_string(),
                ..Default::default()
            })),
        };
        let status = AgentClient::new(channel)
            .session(tokio_stream::iter(vec![hello]))
            .await
            .expect_err("a session without a client certificate is refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        ts.stop().await;
    }
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_token_hash_only() {
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only test that logs");

    let mut secrets = Vec::new();
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;
        let secret = secret_of(&token);
        secrets.push(secret.clone());

        let raw_secret = BASE64URL_NOPAD.decode(secret.as_bytes()).unwrap();
        let stored: Vec<u8> = sqlx::query("SELECT token_hash FROM join_tokens")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("token_hash")
            .unwrap();
        assert_eq!(stored, Sha256::digest(&raw_secret).to_vec());

        lodestore_agent::enroll(enroll_args(&ts, "hash", token.clone()))
            .await
            .expect("enroll");

        let row =
            sqlx::query("SELECT id, repository_id, labels_json, used_by_agent_id FROM join_tokens")
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap();
        for column in ["id", "repository_id", "labels_json", "used_by_agent_id"] {
            let value: String = row.try_get(column).unwrap();
            assert!(!value.contains(&secret), "{column} holds the secret");
        }
        ts.stop().await;
    }

    let captured = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(!captured.is_empty(), "the log capture saw nothing");
    assert!(!captured.contains("lode1."), "a token reached the logs");
    for secret in secrets {
        assert!(
            !captured.contains(&secret),
            "a join secret reached the logs"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_agent_already_enrolled() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let first = api.join_token(&repo, json!([])).await;
        let enrolled = lodestore_agent::enroll(enroll_args(&ts, "again", first))
            .await
            .expect("first enroll");

        let second = api.join_token(&repo, json!([])).await;
        let error = lodestore_agent::enroll(enroll_args(&ts, "again", second))
            .await
            .expect_err("a registered host refuses another token");
        assert!(
            matches!(&error, AgentError::AlreadyEnrolled(id) if *id == enrolled.agent_id),
            "{error}"
        );
        assert_eq!(error.exit_code(), 3);
        assert_eq!(
            error.to_string(),
            format!("already registered as {}", enrolled.agent_id)
        );
        assert_eq!(count(&ts, "SELECT COUNT(*) AS n FROM agents").await, 1);
        assert_eq!(
            count(
                &ts,
                "SELECT COUNT(*) AS n FROM join_tokens WHERE used_at IS NULL"
            )
            .await,
            1
        );
        ts.stop().await;
    }
}
