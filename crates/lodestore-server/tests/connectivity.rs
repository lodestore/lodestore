mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{Backend, TestServer, backends, wait_until};
use data_encoding::BASE64URL_NOPAD;
use lodestore_agent::{AgentError, EnrollArgs, NetOptions, Paths, RunArgs, connect, tls};
use lodestore_proto::agent::v1::GetTrustAnchorRequest;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_testkit::connect_proxy::ConnectProxy;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use x509_parser::extensions::GeneralName;

async fn enroll_and_run_online(backend: &Backend, net: NetOptions) {
    let ts = TestServer::start(backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let token = api.join_token(&repo, json!([])).await;

    let paths = ts.agent_paths("proxied");
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some("host-proxied".to_string()),
        net: net.clone(),
    })
    .await
    .expect("enroll");

    let shutdown = CancellationToken::new();
    let agent = tokio::spawn(lodestore_agent::run(RunArgs {
        paths,
        net,
        heartbeat: Duration::from_secs(1),
        shutdown: shutdown.clone(),
    }));
    let online = wait_until(Duration::from_secs(10), || async {
        api.hosts().await.iter().any(|h| h["status"] == "online")
    })
    .await;
    assert!(online, "[{}] the host never became online", backend.name());

    shutdown.cancel();
    agent.await.unwrap().expect("agent stops cleanly");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_enroll_and_session_through_connect() {
    for backend in backends() {
        let proxy = ConnectProxy::start();
        let net = NetOptions {
            https_proxy: Some(proxy.url()),
            ..NetOptions::default()
        };
        enroll_and_run_online(&backend, net).await;
        assert!(
            proxy.connects() >= 2,
            "[{}] only {} tunnels went through the proxy",
            backend.name(),
            proxy.connects()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_no_proxy_bypasses() {
    for backend in backends() {
        let proxy = ConnectProxy::start();
        let net = NetOptions {
            https_proxy: Some(proxy.url()),
            no_proxy: Some("127.0.0.1".to_string()),
            ..NetOptions::default()
        };
        enroll_and_run_online(&backend, net).await;
        assert_eq!(proxy.connects(), 0, "[{}]", backend.name());
    }
}

fn token_payload(token: &str) -> Value {
    let body = token.strip_prefix("lode1.").unwrap();
    serde_json::from_slice(&BASE64URL_NOPAD.decode(body.as_bytes()).unwrap()).unwrap()
}

fn state_is_empty(paths: &Paths) -> bool {
    !paths.config_dir.exists()
        && (!paths.state_dir.exists()
            || std::fs::read_dir(&paths.state_dir)
                .unwrap()
                .next()
                .is_none())
}

struct ExternalCert {
    _dir: TempDir,
    cert: PathBuf,
    key: PathBuf,
    bundle: PathBuf,
}

fn external_cert() -> ExternalCert {
    let dir = tempfile::tempdir().unwrap();
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let mut ca_params = CertificateParams::default();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "External Test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let mut leaf_params =
        CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    let write = |name: &str, text: String| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    ExternalCert {
        cert: write("server.crt", leaf_cert.pem()),
        key: write("server.key", leaf_key.serialize_pem()),
        bundle: write("ca.pem", ca_cert.pem()),
        _dir: dir,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_cert_trust_bundle() {
    for backend in backends() {
        let external = external_cert();
        let (cert, key) = (external.cert.clone(), external.key.clone());
        let ts = TestServer::start_with(&backend, |c| {
            c.grpc.public_address = format!("localhost:{}", c.grpc.listen.port());
            c.grpc.tls_cert_file = Some(cert);
            c.grpc.tls_key_file = Some(key);
        })
        .await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;
        assert_eq!(
            token_payload(&token).get("f"),
            Some(&Value::Null),
            "the token must carry no CA fingerprint"
        );

        let system = ts.agent_paths("system");
        let error = lodestore_agent::enroll(EnrollArgs {
            token: token.clone(),
            paths: system.clone(),
            hostname: None,
            net: NetOptions::default(),
        })
        .await
        .unwrap_err();
        let AgentError::Connect(message) = &error else {
            panic!(
                "[{}] expected a connection error, got {error}",
                backend.name()
            );
        };
        assert!(message.contains("UnknownIssuer"), "{message}");
        assert!(state_is_empty(&system), "nothing may be written on failure");

        let paths = ts.agent_paths("bundle");
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: paths.clone(),
            hostname: Some("host-bundle".to_string()),
            net: NetOptions {
                ca_bundle: Some(external.bundle.clone()),
                ..NetOptions::default()
            },
        })
        .await
        .expect("enroll with a CA bundle");
        let config: Value =
            serde_json::from_slice(&std::fs::read(paths.agent_json()).unwrap()).unwrap();
        assert_eq!(config["trust"]["mode"], "bundle");
        assert!(config["trust"]["ca_sha256"].is_null());

        let shutdown = CancellationToken::new();
        let agent = tokio::spawn(lodestore_agent::run(RunArgs {
            paths,
            net: NetOptions::default(),
            heartbeat: Duration::from_secs(1),
            shutdown: shutdown.clone(),
        }));
        let online = wait_until(Duration::from_secs(10), || async {
            api.hosts().await.iter().any(|h| h["status"] == "online")
        })
        .await;
        assert!(online, "[{}] the host never became online", backend.name());
        shutdown.cancel();
        agent.await.unwrap().expect("agent stops cleanly");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grpc_external_cert_reloaded_without_restart() {
    for backend in backends() {
        let first = external_cert();
        let second = external_cert();
        let (cert, key) = (first.cert.clone(), first.key.clone());
        let ts = TestServer::start_with(&backend, |c| {
            c.grpc.public_address = format!("localhost:{}", c.grpc.listen.port());
            c.grpc.tls_cert_file = Some(cert);
            c.grpc.tls_key_file = Some(key);
            c.grpc.cert_reload_secs = 1;
        })
        .await;
        let address = ts.grpc_address();
        let trusts = |bundle: &PathBuf| {
            let bundle = bundle.to_string_lossy().into_owned();
            let address = address.clone();
            async move {
                let tls = tls::bundle_config(&bundle, None).unwrap();
                connect::channel(&address, "localhost", tls, &NetOptions::default())
                    .await
                    .is_ok()
            }
        };
        assert!(trusts(&first.bundle).await);
        assert!(!trusts(&second.bundle).await);

        std::fs::copy(&second.cert, &first.cert).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            trusts(&first.bundle).await,
            "[{}] a certificate that does not match the key must keep the running one",
            backend.name()
        );
        std::fs::copy(&second.key, &first.key).unwrap();

        let rotated = wait_until(Duration::from_secs(15), || trusts(&second.bundle)).await;
        assert!(
            rotated,
            "[{}] the new certificate was not served",
            backend.name()
        );
        assert!(!trusts(&first.bundle).await);
        ts.stop().await;
    }
}

struct LeafInfo {
    serial: String,
    names: Vec<String>,
}

fn leaf_info(ts: &TestServer) -> LeafInfo {
    let pem = ts.state().pki.grpc_identity().0;
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    let names = cert
        .subject_alternative_name()
        .unwrap()
        .unwrap()
        .value
        .general_names
        .iter()
        .map(|n| match n {
            GeneralName::DNSName(name) => name.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    LeafInfo {
        serial: cert.tbs_certificate.serial.to_str_radix(16),
        names,
    }
}

async fn ca_fingerprint(address: &str) -> String {
    let channel = connect::channel(
        address,
        "127.0.0.1",
        tls::bootstrap_config().unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("bootstrap channel");
    let ca_pem = EnrollmentClient::new(channel)
        .get_trust_anchor(GetTrustAnchorRequest {})
        .await
        .expect("GetTrustAnchor")
        .into_inner()
        .ca_pem;
    tls::sha256_hex(&tls::ca_der(&ca_pem).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grpc_leaf_reissued_on_san_change() {
    for backend in backends() {
        let mut ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;
        let paths = ts.agent_paths("san");
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: paths.clone(),
            hostname: Some("host-san".to_string()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll");

        let shutdown = CancellationToken::new();
        let agent = tokio::spawn(lodestore_agent::run(RunArgs {
            paths,
            net: NetOptions::default(),
            heartbeat: Duration::from_secs(1),
            shutdown: shutdown.clone(),
        }));
        let online = wait_until(Duration::from_secs(10), || async {
            api.hosts().await.iter().any(|h| h["status"] == "online")
        })
        .await;
        assert!(online, "[{}] the host never became online", backend.name());

        let address = ts.grpc_address();
        let before = leaf_info(&ts);
        assert!(!before.names.contains(&"lodestore.internal".to_string()));
        let pin = ca_fingerprint(&address).await;

        ts.config.grpc.extra_sans = vec!["lodestore.internal".to_string()];
        ts.restart().await;
        let after = leaf_info(&ts);
        assert!(
            after.names.contains(&"lodestore.internal".to_string()),
            "{:?}",
            after.names
        );
        assert_ne!(before.serial, after.serial);
        assert_eq!(pin, ca_fingerprint(&address).await);

        let api = ts.api().await;
        let online = wait_until(Duration::from_secs(20), || async {
            api.hosts().await.iter().any(|h| h["status"] == "online")
        })
        .await;
        assert!(
            online,
            "[{}] the agent did not come back without re-registering",
            backend.name()
        );

        ts.restart().await;
        let again = leaf_info(&ts);
        assert_eq!(
            after.serial, again.serial,
            "an unchanged config keeps the leaf"
        );

        shutdown.cancel();
        agent.await.unwrap().expect("agent stops cleanly");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_has_no_listening_socket() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: /proc/net/tcp is only available on Linux");
        return;
    }
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api.join_token(&repo, json!([])).await;
        let paths = ts.agent_paths("quiet");
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: paths.clone(),
            hostname: Some("host-quiet".to_string()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll");
        let shutdown = CancellationToken::new();
        let agent = tokio::spawn(lodestore_agent::run(RunArgs {
            paths,
            net: NetOptions::default(),
            heartbeat: Duration::from_secs(1),
            shutdown: shutdown.clone(),
        }));
        let online = wait_until(Duration::from_secs(10), || async {
            api.hosts().await.iter().any(|h| h["status"] == "online")
        })
        .await;
        assert!(online, "[{}] the host never became online", backend.name());

        let known = [ts.config.listen.port(), ts.config.grpc.listen.port()];
        let mut stray = Vec::new();
        for table in ["/proc/self/net/tcp", "/proc/self/net/tcp6"] {
            for line in std::fs::read_to_string(table).unwrap().lines().skip(1) {
                let columns: Vec<&str> = line.split_whitespace().collect();
                if columns.get(3) != Some(&"0A") {
                    continue;
                }
                let port = u16::from_str_radix(columns[1].rsplit(':').next().unwrap(), 16).unwrap();
                if !known.contains(&port) {
                    stray.push(port);
                }
            }
        }
        assert!(stray.is_empty(), "unexpected listening ports: {stray:?}");

        shutdown.cancel();
        agent.await.unwrap().expect("agent stops cleanly");
        ts.stop().await;
    }
}
