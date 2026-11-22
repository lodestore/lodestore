//! The storage probe and the agent must use the same location. The draft goes
//! through the real `open_draft` and `register`; the location the probe's agent
//! store was opened from is then compared with the material sent to the agent.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::OnceLock;

use lodestore_format::keys::MasterKey;
use lodestore_format::store::RepoLocation;
use lodestore_server::repos::checks::{
    agent_option_names, open_draft, open_parts, server_credential_names,
};
use lodestore_server::repos::db::{find_repository, server_access, writer_material};
use lodestore_server::repos::wizard::{Registration, register};
use lodestore_server::repos::{CheckResult, RepositoryDraft};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::json;
use zeroize::Zeroizing;

use crate::common::{Backend, TestServer};

const HOST_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
const PUBLISHED: [&str; 6] = [
    "endpoint",
    "bucket",
    "container",
    "root",
    "host_key",
    "publish",
];

#[derive(Debug, Clone)]
struct Case {
    backend: &'static str,
    public: BTreeMap<String, String>,
    server_secrets: BTreeMap<String, String>,
    agent: BTreeMap<String, String>,
}

fn private_keys() -> &'static [String; 2] {
    static KEYS: OnceLock<[String; 2]> = OnceLock::new();
    KEYS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        ["server", "agent"].map(|name| {
            let path = dir.path().join(name);
            let status = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&path)
                .status()
                .expect("ssh-keygen must be installed to run the agent options differential test");
            assert!(status.success(), "ssh-keygen failed");
            std::fs::read_to_string(&path).unwrap()
        })
    })
}

static REFUSING: OnceLock<String> = OnceLock::new();

/// A local endpoint that answers every request with 403, so that the
/// attestation upgrade of `register` fails at once instead of after retries.
fn refusing_endpoint() -> String {
    REFUSING.get().expect("the endpoint is up").clone()
}

async fn serve_refusals() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    url
}

fn word() -> impl Strategy<Value = String> {
    "[a-z0-9]{1,8}"
}

fn map(pairs: Vec<(&str, String)>) -> BTreeMap<String, String> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn azblob() -> impl Strategy<Value = Case> {
    (
        word(),
        word(),
        prop::option::of(word()),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(container, account, root, server_sas, agent_sas)| {
            let mut public = vec![
                ("container", format!("pub-{container}")),
                ("endpoint", refusing_endpoint()),
                ("account_name", format!("pub-{account}")),
            ];
            public.extend(root.map(|r| ("root", format!("/pub-{r}"))));
            let mut secrets = vec![("account_key", "c3J2LWFjY291bnQta2V5".to_string())];
            if server_sas {
                secrets.push(("sas_token", "sv=1&sig=srv-sas".to_string()));
            }
            let agent = if agent_sas {
                vec![("sas_token", "sv=1&sig=agt-sas".to_string())]
            } else {
                vec![]
            };
            Case {
                backend: "azblob",
                public: map(public),
                server_secrets: map(secrets),
                agent: map(agent),
            }
        })
}

fn gcs() -> impl Strategy<Value = Case> {
    (
        word(),
        prop::option::of(word()),
        prop::option::of(word()),
        0..3usize,
        any::<bool>(),
    )
        .prop_map(|(bucket, root, path, server_kind, agent_credential)| {
            let mut public = vec![
                ("bucket", format!("pub-{bucket}")),
                ("endpoint", refusing_endpoint()),
            ];
            public.extend(root.map(|r| ("root", format!("/pub-{r}"))));
            public.extend(path.map(|p| ("credential_path", format!("/pub-{p}"))));
            let credential = ("credential", "srv-credential".to_string());
            let token = ("token", "srv-token".to_string());
            let secrets = match server_kind {
                0 => vec![credential],
                1 => vec![token],
                _ => vec![credential, token],
            };
            let agent = if agent_credential {
                vec![("credential", "agt-credential".to_string())]
            } else {
                vec![]
            };
            Case {
                backend: "gcs",
                public: map(public),
                server_secrets: map(secrets),
                agent: map(agent),
            }
        })
}

fn sftp() -> impl Strategy<Value = Case> {
    (
        word(),
        prop::option::of(word()),
        any::<bool>(),
        prop::sample::select(vec!["hardlink", "direct"]),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(user, root, publish_set, publish, agent_user, agent_key)| {
                let [server_key, agent_key_text] = private_keys();
                let mut public = vec![
                    ("endpoint", "127.0.0.1:9".to_string()),
                    ("user", format!("pub-{user}")),
                    ("host_key", HOST_KEY.to_string()),
                ];
                public.extend(root.map(|r| ("root", format!("/pub-{r}"))));
                if publish_set {
                    public.push(("publish", publish.to_string()));
                }
                let mut agent = vec![];
                if agent_key {
                    agent.push(("key", agent_key_text.clone()));
                    if agent_user {
                        agent.push(("user", "agt-user".to_string()));
                    }
                }
                Case {
                    backend: "sftp",
                    public: map(public),
                    server_secrets: map(vec![("key", server_key.clone())]),
                    agent: map(agent),
                }
            },
        )
}

async fn run_case(ts: &TestServer, n: usize, case: Case) {
    let mut options = case.public.clone();
    options.extend(case.server_secrets.clone());
    let mut draft = json!({
        "name": format!("differential-{n}"),
        "location": { "backend": case.backend, "options": options },
    });
    if !case.agent.is_empty() {
        draft["agent_options"] = json!(case.agent);
    }
    let draft: RepositoryDraft = serde_json::from_value(draft).unwrap();
    let opened = match open_draft(&draft) {
        Ok(opened) => opened,
        Err(error) => panic!("the draft was refused: {error:?}\n{case:?}"),
    };
    let state = ts.state();
    let summary = register(
        state,
        Registration {
            name: draft.name.clone(),
            opened,
            master: MasterKey::from_bytes([7; 32]),
            format_repo_id: *uuid::Uuid::now_v7().as_bytes(),
            config_hash: [1; 32],
            server_secret: Zeroizing::new(vec![9; 32]),
            checks: CheckResult {
                existing_repository: false,
                blocking: false,
                agent_key_state: "unchecked".into(),
                guarantee_level: "unchecked".into(),
                guarantee_notes: vec![],
                checks: vec![],
                host_key: None,
            },
        },
    )
    .await
    .expect("register");
    let id = summary.base.id;

    let access = server_access(state, &id).await.unwrap();
    let probed = open_parts(
        access.location.clone(),
        access.server_creds.clone(),
        access.agent_creds.clone(),
        access.agent_options.clone(),
    )
    .unwrap()
    .agent_location
    .expect("the agent store of this backend has a location");
    let material = writer_material(state, &id).await.unwrap();
    assert_eq!(probed.backend, material.location.backend, "{case:?}");
    assert_eq!(probed.options, material.location.options, "{case:?}");
    let sent = &probed.options;

    let agent_names = agent_option_names(case.backend);
    assert!(case.agent.keys().all(|k| agent_names.contains(&k.as_str())));
    if !case.agent.is_empty() {
        for name in server_credential_names(case.backend) {
            if let Some(value) = sent.get(*name) {
                assert!(
                    !case.server_secrets.values().any(|s| s == value),
                    "{name} still carries the server's value: {case:?}"
                );
            }
        }
        for (name, value) in &case.agent {
            assert_eq!(sent.get(name), Some(value), "{case:?}");
        }
    } else {
        assert_eq!(sent, &access.location.for_agent().options, "{case:?}");
    }

    for name in PUBLISHED {
        assert_eq!(sent.get(name), case.public.get(name), "{name}: {case:?}");
    }
    if case.backend == "gcs" {
        assert!(!sent.contains_key("credential_path"), "{case:?}");
        assert_eq!(sent["disable_vm_metadata"], "true");
        assert_eq!(sent["disable_config_load"], "true");
    }

    let stored = find_repository(state, &id).await.unwrap().unwrap();
    let stored_json = serde_json::to_string(&stored.options).unwrap();
    let server_side: RepoLocation = access.location;
    let server_side_json = serde_json::to_string(&server_side.options).unwrap();
    for (name, value) in &case.agent {
        let is_public_too = case.public.get(name) == Some(value);
        if !is_public_too {
            assert!(
                !stored_json.contains(value.as_str()),
                "{name} reached location_json"
            );
            assert!(
                !server_side_json.contains(value.as_str()),
                "{name} reached the server's secret"
            );
        }
    }
}

#[test]
fn probe_and_agent_locations_agree() {
    private_keys();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    REFUSING.set(rt.block_on(serve_refusals())).unwrap();
    let ts = rt.block_on(TestServer::start(&Backend::Sqlite));
    let mut runner = TestRunner::new(Config {
        cases: 32,
        failure_persistence: None,
        ..Config::default()
    });
    let n = std::cell::Cell::new(0);
    let result = runner.run(&prop_oneof![azblob(), gcs(), sftp()], |case| {
        n.set(n.get() + 1);
        rt.block_on(run_case(&ts, n.get(), case));
        Ok(())
    });
    rt.block_on(ts.stop());
    result.unwrap();
}

#[test]
fn sftp_agent_user_without_a_key_is_refused() {
    let [server_key, _] = private_keys();
    let draft: RepositoryDraft = serde_json::from_value(json!({
        "name": "user-only",
        "location": { "backend": "sftp", "options": {
            "endpoint": "127.0.0.1:9",
            "user": "srv",
            "key": server_key,
            "host_key": HOST_KEY,
        } },
        "agent_options": { "user": "agt-user" },
    }))
    .unwrap();
    let error = open_draft(&draft).err().expect("refused");
    assert!(format!("{error:?}").contains("`key`"), "{error:?}");
}
