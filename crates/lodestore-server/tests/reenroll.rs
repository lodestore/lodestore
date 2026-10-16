mod common;

use common::{Api, TestServer, backends, wait_until};
use data_encoding::BASE64URL_NOPAD;
use lodestore_agent::{EnrollArgs, NetOptions, connect, tls};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::{open_snapshot, seal_snapshot, snapshot_object_key};
use lodestore_format::store::{LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::{
    AgentMessage, EnrollRequest, GetTrustAnchorRequest, Hello, Versions,
};
use lodestore_server::grpc::registry::CAP_RESTORE;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

const PASSWORD: &[u8] = b"test-password";

fn args(ts: &TestServer, name: &str, token: String) -> EnrollArgs {
    EnrollArgs {
        token,
        paths: ts.agent_paths(name),
        hostname: Some(format!("host-{name}")),
        net: NetOptions::default(),
    }
}

async fn write_claimed_snapshot(ts: &TestServer, repo_name: &str, agent_id: &str) -> String {
    let store = LocalFsStore::new(ts.local_repo_root(repo_name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("file.txt"), "content").unwrap();
    let outcome = writer
        .backup(
            &[data],
            BackupOptions {
                hostname: Some(b"lost-host".to_vec()),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    let original = hex32(&outcome.snapshot_id);

    let keys = repo.read_keys();
    let bytes = std::fs::read(
        ts.local_repo_root(repo_name)
            .join(snapshot_object_key(&outcome.snapshot_id)),
    )
    .unwrap();
    let mut snapshot = open_snapshot(
        keys.hpke_private_key(),
        keys.repo_id(),
        &outcome.snapshot_id,
        &bytes,
    )
    .unwrap();
    snapshot.agent_id = agent_id.as_bytes().to_vec();
    let (forged, bytes) = seal_snapshot(
        &snapshot,
        keys.hpke_public_key(),
        keys.repo_id(),
        &mut Entropy::os(),
    )
    .unwrap();
    put_named(&store, &snapshot_object_key(&forged), bytes)
        .await
        .unwrap();
    std::fs::remove_file(
        ts.local_repo_root(repo_name)
            .join(format!("snapshots/{original}")),
    )
    .unwrap();
    assert_ne!(parse_hex32(&original).unwrap(), forged);
    hex32(&forged)
}

async fn sync(api: &Api, repo: &str) -> Value {
    let response = api
        .post(
            &format!("/api/v1/repositories/{repo}/catalog/sync"),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn host_snapshots(api: &Api, host: &str) -> Vec<Value> {
    let response = api.get(&format!("/api/v1/hosts/{host}/snapshots")).await;
    assert_eq!(response.status(), 200);
    let page: Value = response.json().await.unwrap();
    page["items"].as_array().unwrap().clone()
}

async fn pinned_channel(ts: &TestServer, client_cert: Option<(&str, &str)>) -> Channel {
    let address = ts.grpc_address();
    let bootstrap = connect::channel(
        &address,
        "127.0.0.1",
        tls::bootstrap_config().unwrap(),
        &NetOptions::default(),
    )
    .await
    .unwrap();
    let ca_pem = EnrollmentClient::new(bootstrap)
        .get_trust_anchor(GetTrustAnchorRequest {})
        .await
        .unwrap()
        .into_inner()
        .ca_pem;
    connect::channel(
        &address,
        "127.0.0.1",
        tls::pinned_config(&ca_pem, client_cert).unwrap(),
        &NetOptions::default(),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reenroll_keeps_agent_id_and_rebinds_snapshots() {
    for backend in backends() {
        let first = TestServer::start(&backend).await;
        let first_api = first.api().await;
        let repo = first.create_local_repository("main").await;
        let token = first_api.join_token(&repo, json!([])).await;
        let old_id = lodestore_agent::enroll(args(&first, "old", token))
            .await
            .expect("enroll")
            .agent_id;
        let snapshot = write_claimed_snapshot(&first, "main", &old_id).await;

        let second = TestServer::start(&backend).await;
        let second_api = second.api().await;
        let connected = second_api
            .post(
                "/api/v1/repositories/connect",
                &json!({
                    "draft": {
                        "name": "main",
                        "location": {
                            "backend": "local",
                            "options": { "path": first.local_repo_root("main").display().to_string() },
                        },
                    },
                    "password": "test-password",
                    "recovery_key": null,
                }),
            )
            .await;
        assert_eq!(connected.status(), 201, "[{}]", backend.name());
        let repo = connected.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(sync(&second_api, &repo).await["added"], 1);

        let token = second_api.join_token(&repo, json!([])).await;
        let new_id = lodestore_agent::enroll_with_previous(args(&second, "new", token), &old_id)
            .await
            .expect("re-enroll")
            .agent_id;
        assert_eq!(new_id, old_id, "[{}]", backend.name());

        let hosts = second_api.hosts().await;
        assert!(hosts.iter().any(|h| h["id"] == old_id.as_str()));
        let items = host_snapshots(&second_api, &old_id).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], snapshot);

        second.stop().await;
        first.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reenroll_taken_id_gets_new_id() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;
        let taken = lodestore_agent::enroll(args(&ts, "a", token))
            .await
            .expect("enroll")
            .agent_id;
        write_claimed_snapshot(&ts, "main", &taken).await;
        assert_eq!(sync(&api, &repo).await["added"], 1);
        assert_eq!(host_snapshots(&api, &taken).await.len(), 1);

        let token = api.join_token(&repo, json!([])).await;
        let other = lodestore_agent::enroll_with_previous(args(&ts, "b", token), &taken)
            .await
            .expect("re-enroll")
            .agent_id;
        assert_ne!(other, taken, "[{}]", backend.name());
        assert_eq!(host_snapshots(&api, &taken).await.len(), 1);
        assert!(host_snapshots(&api, &other).await.is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reenroll_rejects_bad_previous_id() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;
        let secret = {
            let body = token.strip_prefix("lode1.").unwrap();
            let payload: Value =
                serde_json::from_slice(&BASE64URL_NOPAD.decode(body.as_bytes()).unwrap()).unwrap();
            payload["s"].as_str().unwrap().to_string()
        };

        let channel = pinned_channel(&ts, None).await;

        for bad in ["../x".to_string(), "a".repeat(65)] {
            let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
            let csr = rcgen::CertificateParams::default()
                .serialize_request(&key)
                .unwrap();
            let status = EnrollmentClient::new(channel.clone())
                .enroll(EnrollRequest {
                    join_secret: secret.clone(),
                    csr_der: csr.der().to_vec(),
                    hostname: "raw-host".to_string(),
                    os: "linux".to_string(),
                    arch: "x86_64".to_string(),
                    agent_version: "test".to_string(),
                    versions: Some(Versions {
                        min: PROTOCOL_VERSION,
                        max: PROTOCOL_VERSION,
                    }),
                    previous_agent_id: bad,
                })
                .await
                .expect_err("a malformed previous id is refused");
            assert_eq!(
                status.code(),
                tonic::Code::InvalidArgument,
                "[{}]",
                backend.name()
            );
        }
        assert!(api.hosts().await.is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reenroll_capabilities_recorded_from_hello() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let mut sessions = Vec::new();
        for (name, caps) in [
            ("with", vec![CAP_RESTORE.to_string()]),
            ("without", Vec::new()),
        ] {
            let token = api.join_token(&repo, json!([])).await;
            let paths = ts.agent_paths(name);
            let id = lodestore_agent::enroll(args(&ts, name, token))
                .await
                .expect("enroll")
                .agent_id;
            let cert = std::fs::read_to_string(paths.cert()).unwrap();
            let key = std::fs::read_to_string(paths.key()).unwrap();
            let channel = pinned_channel(&ts, Some((cert.as_str(), key.as_str()))).await;
            let (tx, rx) = mpsc::channel(8);
            tx.send(AgentMessage {
                body: Some(Body::Hello(Hello {
                    versions: Some(Versions {
                        min: PROTOCOL_VERSION,
                        max: PROTOCOL_VERSION,
                    }),
                    agent_version: "test".to_string(),
                    jobs: Vec::new(),
                    capabilities: caps,
                })),
            })
            .await
            .unwrap();
            let inbound = AgentClient::new(channel)
                .session(ReceiverStream::new(rx))
                .await
                .expect("session")
                .into_inner();
            sessions.push((id, tx, inbound));
        }

        let registry = ts.state().registry.clone();
        let (with, without) = (sessions[0].0.clone(), sessions[1].0.clone());
        assert!(
            wait_until(Duration::from_secs(10), || async {
                registry.supports(&with, CAP_RESTORE)
            })
            .await,
            "[{}]",
            backend.name()
        );
        assert!(registry.is_online(&without));
        assert!(!registry.supports(&without, CAP_RESTORE));
        assert!(!registry.supports("unknown", CAP_RESTORE));

        drop(sessions);
        assert!(
            wait_until(Duration::from_secs(10), || async {
                !registry.supports(&with, CAP_RESTORE)
            })
            .await
        );
        ts.stop().await;
    }
}
