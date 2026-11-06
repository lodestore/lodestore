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
async fn install_script_prepares_hooks_and_socket() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let body = reqwest::get(format!("{}/install.sh", ts.rest_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "groupadd --system lodestore",
        "hooks.d",
        "RuntimeDirectory=lodestore",
        "RuntimeDirectoryMode=0755",
    ] {
        assert!(body.contains(needle), "install.sh lacks {needle}");
    }
    assert!(!body.contains("RestrictNamespaces"));

    let mut bash = Command::new("bash")
        .arg("-n")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    bash.stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    assert!(
        bash.wait().unwrap().success(),
        "bash -n rejected the script"
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_script_trust_key_rejects_non_pub() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let body = reqwest::get(format!("{}/install.sh", ts.rest_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "--trust-key",
        "trusted-keys",
        "install -m 0644 -o root -g root",
        r#"rm -f "$UNIT" "$BIN" "$BIN.prev""#,
    ] {
        assert!(body.contains(needle), "install.sh lacks {needle}");
    }

    let own = ts.dir.path().join("own.key");
    std::fs::write(&own, "key").unwrap();
    let missing = ts.dir.path().join("missing.pub");
    for file in [own, missing] {
        let mut sh = Command::new("sh")
            .args(["-s", "--", "--trust-key"])
            .arg(&file)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        sh.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
        let out = sh.wait_with_output().unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("readable .pub file"), "{stderr}");
    }
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_script_allow_in_place_restore() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let body = reqwest::get(format!("{}/install.sh", ts.rest_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "--allow-in-place-restore) allow_in_place=1",
        "allow_in_place: true",
        "chmod 0600 \"$AGENT_YAML.new\"",
        "chown root:root \"$AGENT_YAML.new\"",
        "add \\\"restore:\\\" with \\\"allow_in_place: true\\\" to it by hand",
    ] {
        assert!(body.contains(needle), "install.sh lacks {needle}");
    }
    assert!(body.matches("--allow-in-place-restore").count() >= 2);

    let mut sh = Command::new("sh")
        .args(["-s", "--", "--allow-in-place-restore", "--bogus"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let out = sh.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown option --bogus"), "{stderr}");
    ts.stop().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_script_clears_a_pending_update_before_replacing_the_binary() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let body = reqwest::get(format!("{}/install.sh", ts.rest_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "'lodestore-agent-update-*.timer' 'lodestore-agent-update-*.service'",
        r#"rm -f "$STATE_DIR/update/pending.json" "$STATE_DIR/update/pending.rollback" "$BIN.prev""#,
    ] {
        assert!(body.contains(needle), "install.sh lacks {needle}");
    }
    let clear = body
        .find("\n  clear_pending_update\n")
        .expect("clears the pending update");
    let replace = body.find(r#"mv "$BIN.new" "$BIN""#).expect("replaces");
    assert!(clear < replace, "cleared before the binary is replaced");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_script_force_keeps_previous_agent_id() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let body = reqwest::get(format!("{}/install.sh", ts.rest_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("--previous-agent-id"));
    let read = body.find("prev=$(agent_id)").expect("reads the agent id");
    let stash = body
        .find("\n    stash_identity\n")
        .expect("stashes the identity");
    assert!(
        read < stash,
        "the id is read before the identity is stashed"
    );
    ts.stop().await;
}
