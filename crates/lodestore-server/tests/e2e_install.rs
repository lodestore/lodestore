mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use common::{Api, Backend, TestServer, wait_until};
use lodestore_testkit::connect_proxy::ConnectProxy;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const STATE_DIR: &str = "/var/lib/lodestore";
const CONF_DIR: &str = "/etc/lodestore";
const BIN: &str = "/usr/local/bin/lodestore-agent";
const UNIT: &str = "/etc/systemd/system/lodestore-agent.service";
const ONLINE: Duration = Duration::from_secs(30);

fn require_root_systemd() {
    let root = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| o.stdout == b"0\n")
        .unwrap_or(false);
    let systemd = Path::new("/run/systemd/system").is_dir();
    let curl = Command::new("curl").arg("--version").output().is_ok();
    assert!(
        root && systemd && curl,
        "e2e_install_systemd requires root and systemd and curl; run scripts/test-linux.sh --root"
    );
}

fn agent_binary() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let path = target.join("debug/lodestore-agent");
    assert!(
        path.is_file(),
        "{} is missing; build it with BINS=lodestore-agent",
        path.display()
    );
    path
}

async fn run(command: String) -> (i32, String) {
    tokio::task::spawn_blocking(move || {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{{ {command}\n}} 2>&1"))
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    })
    .await
    .unwrap()
}

fn install_command(url: &str, env: &str, args: &str) -> String {
    format!("curl -fsSL {url}/install.sh | {env} sh -s -- --insecure {args}")
}

fn mode(path: &str) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn local_agent_id() -> String {
    let identity: Value =
        serde_json::from_slice(&std::fs::read(format!("{STATE_DIR}/identity.json")).unwrap())
            .unwrap();
    identity["agent_id"].as_str().unwrap().to_string()
}

async fn host_status(api: &Api, id: &str) -> Option<String> {
    api.hosts()
        .await
        .iter()
        .find(|h| h["id"] == id)
        .map(|h| h["status"].as_str().unwrap().to_string())
}

async fn wait_status(api: &Api, id: &str, status: &str) -> bool {
    wait_until(ONLINE, || async {
        host_status(api, id).await.as_deref() == Some(status)
    })
    .await
}

async fn unit_restarts() -> String {
    let (_, show) = run("systemctl show -p NRestarts lodestore-agent".into()).await;
    show.trim().to_string()
}

fn sha256_of(path: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(std::fs::read(path).unwrap()))
}

async fn serve_releases(binary: Vec<u8>, sums: String) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let name = format!("lodestore-agent-linux-{}", std::env::consts::ARCH);
    let app = Router::new()
        .route(
            &format!("/v{version}/{name}"),
            get(move || async move { binary }),
        )
        .route(
            &format!("/v{version}/SHA256SUMS"),
            get(move || async move { sums }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("sh")
            .arg(&self.0)
            .args(["--uninstall", "--purge"])
            .output();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs root, systemd and curl"]
async fn e2e_install_systemd() {
    require_root_systemd();
    let ts = TestServer::start(&Backend::Sqlite).await;
    std::fs::create_dir_all(&ts.config.agent_dir).unwrap();
    std::fs::copy(
        agent_binary(),
        ts.config
            .agent_dir
            .join(format!("lodestore-agent-linux-{}", std::env::consts::ARCH)),
    )
    .unwrap();
    let url = ts.rest_url();
    let script = ts.dir.path().join("install.sh");
    std::fs::write(
        &script,
        reqwest::get(format!("{url}/install.sh"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    let _cleanup = Cleanup(script);

    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;

    let token = api.join_token(&repo, json!([])).await;
    let (code, output) = run(install_command(&url, "", &format!("--join {token}"))).await;
    assert_eq!(code, 0, "{output}");
    let agent_id = local_agent_id();
    assert!(
        output.contains(&format!("lodestore-agent is running as {agent_id}")),
        "{output}"
    );
    assert!(!output.contains(&token), "the join token was printed");

    let (_, active) = run("systemctl is-active lodestore-agent".into()).await;
    assert_eq!(active.trim(), "active");
    assert!(wait_status(&api, &agent_id, "online").await);

    assert_eq!(mode(CONF_DIR), 0o700);
    assert_eq!(mode(STATE_DIR), 0o700);
    assert_eq!(mode(&format!("{CONF_DIR}/agent.json")), 0o600);

    let (_, show) = run("systemctl show -p RestartPreventExitStatus lodestore-agent".into()).await;
    assert!(show.contains("77") && !show.contains("78"), "{show}");
    let unit = std::fs::read_to_string(UNIT).unwrap();
    assert!(!unit.contains("RestrictSUIDSGID"), "{unit}");
    assert!(Path::new(BIN).is_file());

    let again = run(install_command(&url, "", &format!("--join {token}"))).await;
    assert_eq!(again.0, 0, "{}", again.1);
    assert!(
        again
            .1
            .contains(&format!("already registered as {agent_id}")),
        "{}",
        again.1
    );
    assert_eq!(api.hosts().await.len(), 1);
    assert_eq!(local_agent_id(), agent_id);

    let second_token = api.join_token(&repo, json!([])).await;
    let proxy = "HTTPS_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost";
    let (code, output) = run(install_command(
        &url,
        proxy,
        &format!("--join {second_token} --force"),
    ))
    .await;
    assert_eq!(code, 0, "{output}");
    let second_id = local_agent_id();
    assert_ne!(second_id, agent_id);
    assert!(wait_status(&api, &second_id, "online").await);
    assert!(wait_status(&api, &agent_id, "offline").await);
    assert_eq!(api.hosts().await.len(), 2);
    assert_eq!(mode(&format!("{CONF_DIR}/agent.env")), 0o600);
    let env_file = std::fs::read_to_string(format!("{CONF_DIR}/agent.env")).unwrap();
    assert!(env_file.contains("HTTPS_PROXY=") && env_file.contains("NO_PROXY="));
    let (_, show) = run("systemctl show -p EnvironmentFiles lodestore-agent".into()).await;
    assert!(show.contains("/etc/lodestore/agent.env"), "{show}");

    let connect_proxy = ConnectProxy::start();
    let proxied_token = api.join_token(&repo, json!([])).await;
    let (code, output) = run(install_command(
        &url,
        &format!("HTTPS_PROXY={}", connect_proxy.url()),
        &format!("--join {proxied_token} --force"),
    ))
    .await;
    assert_eq!(code, 0, "{output}");
    let proxied_id = local_agent_id();
    assert_ne!(proxied_id, second_id);
    assert!(wait_status(&api, &proxied_id, "online").await);
    let env_file = std::fs::read_to_string(format!("{CONF_DIR}/agent.env")).unwrap();
    assert!(env_file.contains(&connect_proxy.url()), "{env_file}");
    assert!(!env_file.contains("NO_PROXY"), "{env_file}");
    assert!(
        connect_proxy.connects() > 0,
        "the agent did not go through the proxy"
    );

    let revoke = api
        .post(&format!("/api/v1/hosts/{proxied_id}/revoke"), &json!({}))
        .await;
    assert_eq!(revoke.status(), 204);
    assert!(
        wait_until(Duration::from_secs(15), || async {
            let (_, show) =
                run("systemctl show -p ActiveState,ExecMainStatus lodestore-agent".into()).await;
            show.contains("ActiveState=failed") && show.contains("ExecMainStatus=77")
        })
        .await,
        "the revoked agent did not stop with exit code 77"
    );
    let restarts = unit_restarts().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
        unit_restarts().await,
        restarts,
        "systemd restarted a revoked agent"
    );
    let (_, active) = run("systemctl is-active lodestore-agent".into()).await;
    assert_eq!(active.trim(), "failed");
    assert_eq!(
        host_status(&api, &proxied_id).await.as_deref(),
        Some("revoked")
    );
    assert_eq!(local_agent_id(), proxied_id);

    let mut release = std::fs::read(agent_binary()).unwrap();
    release.push(0);
    let release_sum = data_encoding::HEXLOWER.encode(&Sha256::digest(&release));
    let name = format!("lodestore-agent-linux-{}", std::env::consts::ARCH);
    let good = serve_releases(release.clone(), format!("{release_sum}  {name}\n")).await;
    let third_token = api.join_token(&repo, json!([])).await;
    let (code, output) = run(install_command(
        &url,
        &format!("LODESTORE_RELEASES_URL={good}"),
        &format!("--from-github --join {third_token} --force"),
    ))
    .await;
    assert_eq!(code, 0, "{output}");
    assert_eq!(sha256_of(BIN), release_sum);
    let third_id = local_agent_id();
    assert_ne!(third_id, proxied_id);
    assert!(wait_status(&api, &third_id, "online").await);
    let (_, active) = run("systemctl is-active lodestore-agent".into()).await;
    assert_eq!(active.trim(), "active");

    let before = sha256_of(BIN);
    let bad = serve_releases(release, format!("{}  {name}\n", "a".repeat(64))).await;
    let (code, output) = run(install_command(
        &url,
        &format!("LODESTORE_RELEASES_URL={bad}"),
        "--from-github --join unused --force",
    ))
    .await;
    assert_ne!(code, 0, "{output}");
    assert!(output.contains("checksum mismatch"), "{output}");
    assert_eq!(sha256_of(BIN), before);
    assert_eq!(local_agent_id(), third_id);

    let (code, output) = run(install_command(&url, "", "--join not-a-token --force")).await;
    assert_ne!(code, 0, "{output}");
    assert!(
        output.contains("previous registration is restored"),
        "{output}"
    );
    assert_eq!(local_agent_id(), third_id);
    assert!(
        std::fs::read_dir(STATE_DIR).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".old-")),
        "the backup directory was left behind"
    );
    let (_, active) = run("systemctl is-active lodestore-agent".into()).await;
    assert_eq!(active.trim(), "active");
    assert!(wait_status(&api, &third_id, "online").await);

    let (code, output) = run(install_command(&url, "", "--uninstall")).await;
    assert_eq!(code, 0, "{output}");
    assert!(!Path::new(BIN).exists() && !Path::new(UNIT).exists());
    let (_, active) = run("systemctl is-active lodestore-agent".into()).await;
    assert_ne!(active.trim(), "active");
    assert!(Path::new(CONF_DIR).is_dir());

    let (code, output) = run(install_command(&url, "", "--uninstall --purge")).await;
    assert_eq!(code, 0, "{output}");
    assert!(!Path::new(CONF_DIR).exists() && !Path::new(STATE_DIR).exists());
}
