mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use common::{Backend, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions, RunArgs, connect, tls};
use lodestore_format::keys::WriteKeys;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_proto::agent::v1::GetTrustAnchorRequest;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_server::config::{Config, ConfigArgs, Secret};
use lodestore_server::{Listeners, serve};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    config: ConfigArgs,
}

fn parse(args: &[&str]) -> anyhow::Result<Config> {
    let cli = Cli::try_parse_from(std::iter::once("lodestore-server").chain(args.iter().copied()))?;
    Config::from_args(cli.config)
}

fn write(dir: &Path, name: &str, text: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

#[test]
fn config_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "server.yaml",
        "listen: 127.0.0.1:9001\ngrpc:\n  public_address: lodestore.example:9443\n  extra_sans: [lodestore.example]\n",
    );

    let defaults = parse(&[]).unwrap();
    assert_eq!(defaults.listen.port(), 8080);
    assert!(defaults.grpc_public_address_defaulted);

    let from_file = parse(&["--config", &file]).unwrap();
    assert_eq!(from_file.listen.to_string(), "127.0.0.1:9001");
    assert_eq!(from_file.grpc.public_address, "lodestore.example:9443");
    assert_eq!(from_file.grpc.extra_sans, ["lodestore.example"]);
    assert!(!from_file.grpc_public_address_defaulted);

    unsafe { std::env::set_var("LODESTORE_LISTEN", "127.0.0.1:9002") };
    let from_env = parse(&["--config", &file]).unwrap();
    assert_eq!(from_env.listen.to_string(), "127.0.0.1:9002");

    let from_flag = parse(&["--config", &file, "--listen", "127.0.0.1:9003"]).unwrap();
    assert_eq!(from_flag.listen.to_string(), "127.0.0.1:9003");

    unsafe { std::env::set_var("LODESTORE_CONFIG", &file) };
    unsafe { std::env::remove_var("LODESTORE_LISTEN") };
    let config_from_env = parse(&[]).unwrap();
    assert_eq!(config_from_env.listen.to_string(), "127.0.0.1:9001");
    unsafe { std::env::remove_var("LODESTORE_CONFIG") };

    let unknown = write(dir.path(), "bad.yaml", "lisen: 127.0.0.1:9001\n");
    let error = parse(&["--config", &unknown]).unwrap_err();
    assert!(format!("{error:#}").contains("lisen"), "{error:#}");

    let nested = write(dir.path(), "nested.yaml", "grpc:\n  public_adress: x\n");
    let error = parse(&["--config", &nested]).unwrap_err();
    assert!(format!("{error:#}").contains("public_adress"), "{error:#}");

    let secret = write(dir.path(), "secret.yaml", "admin_password: nope\n");
    let error = parse(&["--config", &secret]).unwrap_err();
    assert!(format!("{error:#}").contains("admin_password"), "{error:#}");

    let missing = dir.path().join("absent.yaml").display().to_string();
    assert!(parse(&["--config", &missing]).is_err());
}

#[test]
fn config_secret_files() {
    let dir = tempfile::tempdir().unwrap();
    let password = write(dir.path(), "password", "s3cret-value\n");
    let url = write(dir.path(), "url", "sqlite:///tmp/x.db?mode=rwc\n\n");
    let key = write(dir.path(), "key", &"ab".repeat(32));

    let config = parse(&[
        "--admin-password-file",
        &password,
        "--database-url-file",
        &url,
        "--unlock-key-file",
        &key,
    ])
    .unwrap();
    assert_eq!(config.admin_password.unwrap().expose(), "s3cret-value");
    assert_eq!(config.database_url, "sqlite:///tmp/x.db?mode=rwc");
    assert_eq!(config.unlock_key_file.unwrap().display().to_string(), key);
    assert!(config.unlock_key.is_none());

    let (password, url, key) = (password.as_str(), url.as_str(), key.as_str());
    for args in [
        ["--admin-password", "x", "--admin-password-file", password],
        [
            "--database-url",
            "sqlite::memory:",
            "--database-url-file",
            url,
        ],
        ["--unlock-key", "x", "--unlock-key-file", key],
    ] {
        let error = parse(&args).unwrap_err();
        assert!(format!("{error:#}").contains("both set"), "{error:#}");
    }

    unsafe { std::env::set_var("LODESTORE_ADMIN_PASSWORD_FILE", password) };
    let from_env = parse(&[]).unwrap();
    assert_eq!(from_env.admin_password.unwrap().expose(), "s3cret-value");
}

#[test]
fn config_rejects_weak_admin_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let empty = write(dir.path(), "empty", "\n");
    let short = write(dir.path(), "short", "short-pass\n");
    let long_enough = write(dir.path(), "ok", "twelve-chars!\n");

    for file in [&empty, &short] {
        let error = parse(&["--admin-password-file", file]).unwrap_err();
        assert!(
            format!("{error:#}").contains("At least 12 characters"),
            "{error:#}"
        );
    }
    let error = parse(&[
        "--admin-password-file",
        &long_enough,
        "--admin-username",
        "",
    ])
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("Username must be"),
        "{error:#}"
    );
    parse(&["--admin-password-file", &long_enough]).unwrap();

    for url in ["lodestore.example", "https://h/sub"] {
        assert!(parse(&["--public-url", url]).is_err(), "{url}");
    }
    assert_eq!(
        parse(&["--public-url", "https://lode.example/"])
            .unwrap()
            .public_url,
        "https://lode.example"
    );
    assert!(parse(&["--trusted-proxies", "not-an-ip"]).is_err());
    assert!(parse(&["--trusted-proxies", "10.0.0.0/8,::1"]).is_ok());
}

async fn info(ts: &TestServer) -> Value {
    let response = ts.api().await.get("/api/v1/server/info").await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_unlock_key_sources() {
    for backend in backends() {
        let name = backend.name();

        let ts = TestServer::start(&backend).await;
        assert_eq!(info(&ts).await["unlock_key_source"], "env", "[{name}]");
        ts.stop().await;

        let keys = tempfile::tempdir().unwrap();
        let key_path = keys.path().join("unlock.key");
        std::fs::write(&key_path, format!("{}\n", "09".repeat(32))).unwrap();
        let ts = TestServer::start_with(&backend, |c| {
            c.unlock_key = None;
            c.unlock_key_file = Some(key_path.clone());
        })
        .await;
        let body = info(&ts).await;
        assert_eq!(body["unlock_key_source"], "file", "[{name}]");
        assert_eq!(body["grpc_public_address"], ts.grpc_address());
        assert_eq!(body["public_url"], ts.rest_url());
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["agent_arches"], json!([]));
        let project_pub = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lodestore-agent/keys/project.pub"
        ))
        .unwrap();
        assert_eq!(
            body["project_key"],
            project_pub.lines().nth(1).unwrap(),
            "[{name}]"
        );
        assert!(
            !ts.config.data_dir.join("unlock.key").exists(),
            "[{name}] no key may be generated when one is supplied"
        );
        ts.stop().await;

        let ts = TestServer::start_with(&backend, |c| c.unlock_key = None).await;
        assert_eq!(info(&ts).await["unlock_key_source"], "data_dir", "[{name}]");
        let generated = ts.config.data_dir.join("unlock.key");
        let mode = std::fs::metadata(&generated).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "[{name}]");
        ts.stop().await;
    }
}

async fn trust_anchor_sha256(ts: &TestServer) -> String {
    let channel = connect::channel(
        &ts.grpc_address(),
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
    data_encoding::HEXLOWER.encode(&Sha256::digest(ca_pem.as_bytes()))
}

async fn restart_keeps_ca_and_secrets(backend: &Backend) {
    let name = backend.name();
    let mut ts = TestServer::start(backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;

    let first = ts.agent_paths("first");
    lodestore_agent::enroll(EnrollArgs {
        token: api.join_token(&repo, json!([])).await,
        paths: first.clone(),
        hostname: Some("first".to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    let shutdown = CancellationToken::new();
    let agent = tokio::spawn(lodestore_agent::run(RunArgs {
        paths: first,
        net: NetOptions::default(),
        heartbeat: Duration::from_secs(1),
        shutdown: shutdown.clone(),
    }));
    let online = || async { api.hosts().await.iter().any(|h| h["status"] == "online") };
    assert!(
        wait_until(Duration::from_secs(10), online).await,
        "[{name}] the host never became online"
    );

    let anchor = trust_anchor_sha256(&ts).await;
    ts.restart().await;
    assert_eq!(
        trust_anchor_sha256(&ts).await,
        anchor,
        "[{name}] the CA changed"
    );
    assert!(
        wait_until(Duration::from_secs(30), online).await,
        "[{name}] the host did not come back online after the restart"
    );

    let second = ts.agent_paths("second");
    lodestore_agent::enroll(EnrollArgs {
        token: api.join_token(&repo, json!([])).await,
        paths: second.clone(),
        hostname: Some("second".to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll after the restart");
    let text = std::fs::read_to_string(second.writer_keys()).unwrap();
    let keys = WriteKeys::from_file_text(&text).expect("a valid writer keys file");
    let config = LocalFsStore::new(ts.local_repo_root("main"))
        .get("config")
        .await
        .expect("repository config");
    keys.verify_config(&config)
        .expect("the writer keys still match the repository");

    shutdown.cancel();
    agent.await.unwrap().expect("agent stops cleanly");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_restart_keeps_ca_and_secrets() {
    for backend in backends() {
        restart_keeps_ca_and_secrets(&backend).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_wrong_unlock_key_refuses_start() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let mut config = ts.config.clone();
        config.unlock_key = Some(Secret::new("08".repeat(32)));
        let loopback = "127.0.0.1:0";
        let listeners = Listeners {
            rest: TcpListener::bind(loopback).await.unwrap(),
            grpc: TcpListener::bind(loopback).await.unwrap(),
        };
        let error = match serve(config, listeners).await {
            Ok(_) => panic!(
                "[{}] a wrong unlock key must not start the server",
                backend.name()
            ),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("does not match"),
            "[{}] {error:#}",
            backend.name()
        );
        ts.stop().await;
    }
}
