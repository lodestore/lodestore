mod common;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use common::{Backend, TestServer};
use lodestore_testkit::minisign::TestSigner;
use sha2::{Digest, Sha256};

const ARCH: &str = "x86_64";
const AGENT_FILE: &str = "lodestore-agent-linux-x86_64";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn require_tools() {
    let all = ["minisign", "curl", "sha256sum"].iter().all(|tool| {
        Command::new(tool)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    });
    assert!(
        all,
        "install_verify_ needs minisign, curl and sha256sum in PATH; run scripts/install-minisign.sh"
    );
}

fn agent_bytes() -> Vec<u8> {
    (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn write_key(dir: &Path, name: &str, signer: &TestSigner) -> PathBuf {
    let path = dir.join(format!("{name}.pub"));
    std::fs::write(&path, signer.public_key_text()).unwrap();
    path
}

#[derive(Default)]
struct Run {
    github: bool,
    no_minisign: bool,
    trust_keys: Vec<PathBuf>,
    remove_keys_after_snapshot: bool,
    releases_url: Option<String>,
}

struct Outcome {
    code: i32,
    stderr: String,
    installed: Option<Vec<u8>>,
}

async fn download(ts: &TestServer, run: Run) -> Outcome {
    require_tools();
    let body = reqwest::get(format!("{}/install.sh", ts.rest_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let body = body.trim_end();
    let (script, last) = body.rsplit_once('\n').unwrap();
    assert_eq!(last, "main \"$@\"", "the script no longer ends with main");

    let keys: String = run
        .trust_keys
        .iter()
        .map(|k| format!("{}\n", k.display()))
        .collect();
    let driver = format!(
        "{script}\n\
         tmp=$(mktemp -d)\n\
         trap 'rm -rf \"$tmp\"' EXIT\n\
         arch={ARCH}\n\
         github={github}\n\
         insecure=1\n\
         nl='\n'\n\
         trust_keys='{keys}'\n\
         snapshot_trust_keys\n\
         {remove_keys}\n\
         {no_minisign}\n\
         download_agent\n\
         cp \"$tmp/agent\" \"$OUT\"\n",
        github = u8::from(run.github),
        remove_keys = if run.remove_keys_after_snapshot {
            format!(
                "rm -f {}",
                run.trust_keys
                    .iter()
                    .map(|k| format!("'{}'", k.display()))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        } else {
            String::new()
        },
        no_minisign = if run.no_minisign {
            "have_minisign() { return 1; }"
        } else {
            ""
        },
    );
    let out = ts.dir.path().join("installed-agent");
    let mut command = Command::new("sh");
    command
        .env("OUT", &out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(url) = &run.releases_url {
        command.env("LODESTORE_RELEASES_URL", url);
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(driver.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    Outcome {
        code: result.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
        installed: std::fs::read(&out).ok(),
    }
}

fn put_agent(ts: &TestServer, bytes: &[u8], signature: Option<&str>) {
    std::fs::create_dir_all(&ts.config.agent_dir).unwrap();
    std::fs::write(ts.config.agent_dir.join(AGENT_FILE), bytes).unwrap();
    if let Some(signature) = signature {
        std::fs::write(
            ts.config.agent_dir.join(format!("{AGENT_FILE}.minisig")),
            signature,
        )
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_signed_by_trust_key_installs() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let signer = TestSigner::generate();
    let bytes = agent_bytes();
    put_agent(
        &ts,
        &bytes,
        Some(&signer.sign(&bytes, "lodestore-agent 0.2.0 x86_64")),
    );
    let key = write_key(ts.dir.path(), "own", &signer);
    let outcome = download(
        &ts,
        Run {
            trust_keys: vec![key],
            ..Run::default()
        },
    )
    .await;
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(outcome.installed.as_deref(), Some(bytes.as_slice()));
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_missing_signature_refused() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    put_agent(&ts, &agent_bytes(), None);
    let outcome = download(&ts, Run::default()).await;
    assert_eq!(outcome.code, 1, "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("nothing was installed"),
        "{}",
        outcome.stderr
    );
    assert!(outcome.installed.is_none());
    ts.stop().await;
}

async fn server_signed_with(comment: &str) -> (Outcome, Vec<u8>) {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let signer = TestSigner::generate();
    let bytes = agent_bytes();
    put_agent(&ts, &bytes, Some(&signer.sign(&bytes, comment)));
    let key = write_key(ts.dir.path(), "own", &signer);
    let outcome = download(
        &ts,
        Run {
            trust_keys: vec![key],
            ..Run::default()
        },
    )
    .await;
    ts.stop().await;
    (outcome, bytes)
}

fn assert_refused(outcome: &Outcome, needle: &str) {
    assert_eq!(outcome.code, 1, "{}", outcome.stderr);
    assert!(outcome.stderr.contains(needle), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("nothing was installed"),
        "{}",
        outcome.stderr
    );
    assert!(outcome.installed.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_tampered_binary_refused() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let signer = TestSigner::generate();
    let bytes = agent_bytes();
    let signature = signer.sign(&bytes, "lodestore-agent 0.2.0 x86_64");
    let mut tampered = bytes;
    tampered[0] ^= 1;
    put_agent(&ts, &tampered, Some(&signature));
    let key = write_key(ts.dir.path(), "own", &signer);
    let outcome = download(
        &ts,
        Run {
            trust_keys: vec![key],
            ..Run::default()
        },
    )
    .await;
    assert_refused(&outcome, "does not verify");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_untrusted_key_refused() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let stranger = TestSigner::generate();
    let trusted = TestSigner::generate();
    let bytes = agent_bytes();
    put_agent(
        &ts,
        &bytes,
        Some(&stranger.sign(&bytes, "lodestore-agent 0.2.0 x86_64")),
    );
    let key = write_key(ts.dir.path(), "trusted", &trusted);
    let outcome = download(
        &ts,
        Run {
            trust_keys: vec![key],
            ..Run::default()
        },
    )
    .await;
    assert_refused(&outcome, "does not verify");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_other_product_refused() {
    let (outcome, _) = server_signed_with("lodestore-server 0.2.0 x86_64").await;
    assert_refused(&outcome, "not lodestore-agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_other_arch_refused() {
    let (outcome, _) = server_signed_with("lodestore-agent 0.2.0 aarch64").await;
    assert_refused(&outcome, "not lodestore-agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_extra_field_refused() {
    let (outcome, _) = server_signed_with("lodestore-agent 0.2.0 x86_64 extra").await;
    assert_refused(&outcome, "not lodestore-agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_other_version_installs() {
    let (outcome, bytes) = server_signed_with("lodestore-agent 9.9.9 x86_64").await;
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(outcome.installed.as_deref(), Some(bytes.as_slice()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_second_trust_key_installs() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let first = TestSigner::generate();
    let second = TestSigner::generate();
    let bytes = agent_bytes();
    put_agent(
        &ts,
        &bytes,
        Some(&second.sign(&bytes, "lodestore-agent 0.2.0 x86_64")),
    );
    let keys = vec![
        write_key(ts.dir.path(), "first", &first),
        write_key(ts.dir.path(), "second", &second),
    ];
    let outcome = download(
        &ts,
        Run {
            trust_keys: keys,
            ..Run::default()
        },
    )
    .await;
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(outcome.installed.as_deref(), Some(bytes.as_slice()));
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_trust_key_is_read_once() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let signer = TestSigner::generate();
    let bytes = agent_bytes();
    put_agent(
        &ts,
        &bytes,
        Some(&signer.sign(&bytes, "lodestore-agent 0.2.0 x86_64")),
    );
    let key = write_key(ts.dir.path(), "own", &signer);
    let outcome = download(
        &ts,
        Run {
            trust_keys: vec![key],
            remove_keys_after_snapshot: true,
            ..Run::default()
        },
    )
    .await;
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(outcome.installed.as_deref(), Some(bytes.as_slice()));
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_trust_key_name_is_not_a_glob() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let trusted = TestSigner::generate();
    let neighbour = TestSigner::generate();
    let bytes = agent_bytes();
    put_agent(
        &ts,
        &bytes,
        Some(&neighbour.sign(&bytes, "lodestore-agent 0.2.0 x86_64")),
    );
    let literal = write_key(ts.dir.path(), "k*", &trusted);
    write_key(ts.dir.path(), "kB", &neighbour);
    let outcome = download(
        &ts,
        Run {
            trust_keys: vec![literal],
            ..Run::default()
        },
    )
    .await;
    assert_refused(&outcome, "does not verify");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_server_without_minisign_notes() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let bytes = agent_bytes();
    put_agent(&ts, &bytes, None);
    let outcome = download(
        &ts,
        Run {
            no_minisign: true,
            ..Run::default()
        },
    )
    .await;
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome
            .stderr
            .matches("note: minisign is not installed")
            .count(),
        1
    );
    assert_eq!(outcome.installed.as_deref(), Some(bytes.as_slice()));
    ts.stop().await;
}

struct Releases {
    files: HashMap<String, Vec<u8>>,
    hits: AtomicUsize,
}

async fn release_file(State(releases): State<Arc<Releases>>, uri: Uri) -> Response {
    releases.hits.fetch_add(1, Ordering::SeqCst);
    match releases.files.get(uri.path()) {
        Some(body) => body.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_releases(files: Vec<(&str, Vec<u8>)>) -> (String, Arc<Releases>) {
    let releases = Arc::new(Releases {
        files: files
            .into_iter()
            .map(|(name, body)| (format!("/v{VERSION}/{name}"), body))
            .collect(),
        hits: AtomicUsize::new(0),
    });
    let app = Router::new()
        .fallback(release_file)
        .with_state(releases.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, releases)
}

fn sums_for(bytes: &[u8]) -> Vec<u8> {
    format!(
        "{}  {AGENT_FILE}\n{}  lodestore-server-linux-x86_64\n",
        data_encoding::HEXLOWER.encode(&Sha256::digest(bytes)),
        "b".repeat(64)
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_github_without_minisign_refused_offline() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let (url, releases) = serve_releases(vec![]).await;
    let outcome = download(
        &ts,
        Run {
            github: true,
            no_minisign: true,
            releases_url: Some(url),
            ..Run::default()
        },
    )
    .await;
    assert_refused(&outcome, "needs minisign");
    assert!(outcome.stderr.contains("apt install minisign"));
    assert!(outcome.stderr.contains("dnf install minisign"));
    assert_eq!(releases.hits.load(Ordering::SeqCst), 0);
    ts.stop().await;
}

struct Github {
    outcome: Outcome,
    agent: Vec<u8>,
}

async fn github_with(sign_comment: &str, tamper_sums: bool, with_signature: bool) -> Github {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let signer = TestSigner::generate();
    let agent = agent_bytes();
    let sums = sums_for(&agent);
    let signature = signer.sign(&sums, sign_comment).into_bytes();
    let served_sums = if tamper_sums {
        String::from_utf8(sums)
            .unwrap()
            .replace('b', "c")
            .into_bytes()
    } else {
        sums
    };
    let mut files = vec![(AGENT_FILE, agent.clone()), ("SHA256SUMS", served_sums)];
    if with_signature {
        files.push(("SHA256SUMS.minisig", signature));
    }
    let (url, _) = serve_releases(files).await;
    let key = write_key(ts.dir.path(), "release", &signer);
    let outcome = download(
        &ts,
        Run {
            github: true,
            trust_keys: vec![key],
            releases_url: Some(url),
            ..Run::default()
        },
    )
    .await;
    ts.stop().await;
    Github { outcome, agent }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_github_signed_sums_installs() {
    let run = github_with(&format!("lodestore release {VERSION}"), false, true).await;
    assert_eq!(run.outcome.code, 0, "{}", run.outcome.stderr);
    assert_eq!(run.outcome.installed.as_deref(), Some(run.agent.as_slice()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_github_other_version_refused() {
    let run = github_with("lodestore release 0.0.1", false, true).await;
    assert_refused(&run.outcome, "not lodestore release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_github_tampered_sums_refused() {
    let run = github_with(&format!("lodestore release {VERSION}"), true, true).await;
    assert_refused(&run.outcome, "does not verify");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs minisign, curl and sha256sum"]
async fn install_verify_github_missing_signature_refused() {
    let run = github_with(&format!("lodestore release {VERSION}"), false, false).await;
    assert_refused(&run.outcome, "SHA256SUMS.minisig");
}
