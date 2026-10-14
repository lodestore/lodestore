mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use common::{Backend, TestServer, backends};
use sha2::{Digest, Sha256};

fn agent_bytes() -> Vec<u8> {
    (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_script_served_with_server_url() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let response = reqwest::get(format!("{}/install.sh", ts.rest_url()))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "text/x-shellscript; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = response.text().await.unwrap();
        assert!(body.contains(&format!("SERVER_URL='{}'", ts.config.public_url)));
        assert!(body.contains(&format!("VERSION='{}'", env!("CARGO_PKG_VERSION"))));
        assert!(!body.contains("@@"));

        let mut sh = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        sh.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
        assert!(sh.wait().unwrap().success(), "sh -n rejected the script");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_script_refuses_plain_http_without_insecure() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let body = reqwest::get(format!("{}/install.sh", ts.rest_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("SERVER_URL='http://"));
    let mut sh = Command::new("sh")
        .args(["-s", "--", "--join", "token"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let out = sh.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plain HTTP") && stderr.contains("--insecure"),
        "{stderr}"
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_agent_binary_and_checksum() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let bytes = agent_bytes();
        std::fs::create_dir_all(&ts.config.agent_dir).unwrap();
        std::fs::write(
            ts.config.agent_dir.join("lodestore-agent-linux-x86_64"),
            &bytes,
        )
        .unwrap();

        let url = format!("{}/install/agent/x86_64", ts.rest_url());
        let served = reqwest::get(&url).await.unwrap();
        assert_eq!(served.status(), 200);
        assert_eq!(
            served.headers()["content-length"],
            bytes.len().to_string().as_str()
        );
        assert_eq!(served.bytes().await.unwrap().as_ref(), bytes.as_slice());

        let sum = reqwest::get(format!("{url}.sha256")).await.unwrap();
        assert_eq!(sum.status(), 200);
        assert_eq!(
            sum.text().await.unwrap(),
            data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes))
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_missing_binary_404() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        for suffix in ["", ".sha256"] {
            let response = reqwest::get(format!("{}/install/agent/aarch64{suffix}", ts.rest_url()))
                .await
                .unwrap();
            assert_eq!(response.status(), 404);
            assert!(response.text().await.unwrap().contains("use --from-github"));
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_unknown_arch_404() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        for path in ["mips", "mips.sha256", "..%2Fetc"] {
            let response = reqwest::get(format!("{}/install/agent/{path}", ts.rest_url()))
                .await
                .unwrap();
            assert_eq!(response.status(), 404, "{path}");
        }
        ts.stop().await;
    }
}
