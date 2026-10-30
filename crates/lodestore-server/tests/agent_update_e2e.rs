mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{AgentEnv, Backend, backends, wait_until};
use lodestore_agent::update::guard;
use lodestore_agent::update::switch::{RecordingSwitcher, SwitchCall, Switcher};
use lodestore_agent::update::{UpdateRuntime, apply};
use lodestore_testkit::minisign::TestSigner;
use sqlx::Row;

const OLD: &[u8] = b"old agent";
const TARGET: &str = "99.0.0";
const WAIT: Duration = Duration::from_secs(20);

struct Stand {
    env: AgentEnv,
    exe: PathBuf,
    body: String,
    switcher: Arc<RecordingSwitcher>,
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn script() -> String {
    format!("#!/bin/sh\necho \"lodestore-agent {TARGET}\"\n")
}

impl Stand {
    /// `trusted` signs nothing here, it only decides whose key the host trusts;
    /// `signer` signs the published binary.
    async fn new(backend: &Backend, trusted: &TestSigner, signer: &TestSigner) -> Stand {
        Stand::with_body(backend, trusted, signer, script()).await
    }

    async fn with_body(
        backend: &Backend,
        trusted: &TestSigner,
        signer: &TestSigner,
        body: String,
    ) -> Stand {
        let env = AgentEnv::enrolled(backend).await;
        let keys = env.paths.trusted_keys();
        std::fs::create_dir_all(&keys).unwrap();
        chmod(&keys, 0o755);
        let key = keys.join("test.pub");
        std::fs::write(&key, trusted.public_key_text()).unwrap();
        chmod(&key, 0o644);

        let arch = std::env::consts::ARCH;
        let agents = &env.ts().config.agent_dir;
        std::fs::create_dir_all(agents).unwrap();
        let file = agents.join(format!("lodestore-agent-linux-{arch}"));
        std::fs::write(&file, &body).unwrap();
        let comment = format!("lodestore-agent {TARGET} {arch}");
        std::fs::write(
            format!("{}.minisig", file.display()),
            signer.sign(body.as_bytes(), &comment),
        )
        .unwrap();

        let bin = env.work.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("lodestore-agent");
        std::fs::write(&exe, OLD).unwrap();
        chmod(&exe, 0o755);
        Stand {
            env,
            exe,
            body,
            switcher: Arc::new(RecordingSwitcher::new()),
        }
    }

    fn runtime(&self, version: &str) -> UpdateRuntime {
        let switcher: Arc<dyn Switcher> = self.switcher.clone();
        UpdateRuntime {
            exe: self.exe.clone(),
            version: version.to_string(),
            switcher,
            enabled: true,
            flight: Default::default(),
        }
    }

    async fn start_rollout(&self) {
        let token = self.env.ts().issue_token(&["agents:update"]).await;
        let response = common::bearer_client(&token)
            .post(format!("{}/api/v1/agent-updates", self.env.ts().rest_url()))
            .json(&serde_json::json!({ "hosts": [self.env.host] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 202);
    }

    async fn update(&self) -> Option<(String, Option<String>)> {
        sqlx::query("SELECT state, reason FROM agent_updates WHERE host_id = $1")
            .bind(&self.env.host)
            .fetch_optional(&self.env.ts().state().db.pool)
            .await
            .unwrap()
            .map(|row| (row.get("state"), row.get("reason")))
    }

    async fn wait_state(&self, want: &str) -> Option<String> {
        let reached = wait_until(WAIT, || async {
            self.update().await.is_some_and(|(state, _)| state == want)
        })
        .await;
        let (state, reason) = self.update().await.expect("an update row");
        assert!(
            reached,
            "the update is {state} ({reason:?}), expected {want}"
        );
        reason
    }

    async fn assigned_at(&self) -> Option<i64> {
        sqlx::query("SELECT assigned_at FROM agent_updates WHERE host_id = $1")
            .bind(&self.env.host)
            .fetch_one(&self.env.ts().state().db.pool)
            .await
            .unwrap()
            .get("assigned_at")
    }

    /// The download once verification has made it executable.
    fn verified_download(&self) -> Option<PathBuf> {
        std::fs::read_dir(self.exe.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with(".lodestore-agent.new")
            })
            .find(|path| {
                std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755)
            })
    }

    async fn online(&self) -> bool {
        self.env
            .api
            .hosts()
            .await
            .iter()
            .any(|h| h["id"] == self.env.host.as_str() && h["status"] == "online")
    }

    fn exe_is_new(&self) -> bool {
        std::fs::read(&self.exe).unwrap() == self.body.as_bytes()
    }

    async fn switch_and_exit(&mut self) {
        self.env.start_agent_with(self.runtime("0.0.1")).await;
        self.start_rollout().await;
        self.env.wait_agent_exit().await;
        assert!(self.exe_is_new());
        assert_eq!(std::fs::read(apply::prev_path(&self.exe)).unwrap(), OLD);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_installs_a_signed_binary_and_confirms_it() {
    for backend in backends() {
        let signer = TestSigner::generate();
        let mut stand = Stand::new(&backend, &signer, &signer).await;
        stand.switch_and_exit().await;
        assert!(
            stand
                .switcher
                .calls()
                .iter()
                .any(|c| matches!(c, SwitchCall::Arm { .. })),
            "[{}]",
            backend.name()
        );

        let runtime = stand.runtime(TARGET);
        stand.env.start_agent_with(runtime).await;
        stand.wait_state("succeeded").await;
        assert!(
            stand
                .switcher
                .calls()
                .iter()
                .any(|c| matches!(c, SwitchCall::Disarm { .. }))
        );
        let status = stand.env.json("/api/v1/agent-updates").await;
        let host = status["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["hostname"] == stand.env.hostname.as_str())
            .unwrap()
            .clone();
        assert!(host["state"].is_null(), "{host}");
        assert!(!stand.env.paths.update_dir().join("pending.json").exists());
        stand.env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_with_an_untrusted_signature_changes_nothing() {
    for backend in backends() {
        let (trusted, stranger) = (TestSigner::generate(), TestSigner::generate());
        let mut stand = Stand::new(&backend, &trusted, &stranger).await;
        stand.env.start_agent_with(stand.runtime("0.0.1")).await;
        stand.start_rollout().await;

        let reason = stand.wait_state("rolled_back").await;
        assert_eq!(
            reason.as_deref(),
            Some("Signature check failed: key not trusted on this host."),
            "[{}]",
            backend.name()
        );
        assert_eq!(std::fs::read(&stand.exe).unwrap(), OLD);
        assert!(apply::temp_files(&stand.exe).is_empty());
        assert!(!apply::prev_path(&stand.exe).exists());
        assert!(stand.online().await);
        stand.env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_rolled_back_by_the_guard_is_reported_once_the_old_agent_is_back() {
    for backend in backends() {
        let signer = TestSigner::generate();
        let mut stand = Stand::new(&backend, &signer, &signer).await;
        stand.switch_and_exit().await;

        let rolled = guard::rollback(&stand.env.paths, &stand.exe, &*stand.switcher, |p| {
            format!(
                "The new version did not connect within 2 min. Running {} again.",
                p.previous_version
            )
        })
        .unwrap();
        assert!(rolled);
        assert_eq!(std::fs::read(&stand.exe).unwrap(), OLD);

        let runtime = stand.runtime("0.0.1");
        stand.env.start_agent_with(runtime).await;
        let reason = stand.wait_state("rolled_back").await;
        assert_eq!(
            reason.as_deref(),
            Some("The new version did not connect within 2 min. Running 0.0.1 again."),
            "[{}]",
            backend.name()
        );
        assert!(stand.online().await);
        let mark = stand.env.paths.update_dir().join("result.json");
        assert!(
            wait_until(WAIT, || async { !mark.exists() }).await,
            "the server never acknowledged the rollback"
        );
        stand.env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_during_the_install_leaves_the_verified_file_alone() {
    use std::os::unix::fs::MetadataExt;

    let signer = TestSigner::generate();
    let body = format!("#!/bin/sh\nsleep 8\necho \"lodestore-agent {TARGET}\"\n");
    let mut stand = Stand::with_body(&Backend::Sqlite, &signer, &signer, body).await;
    stand.env.start_agent_with(stand.runtime("0.0.1")).await;
    stand.start_rollout().await;

    assert!(wait_until(WAIT, || async { stand.verified_download().is_some() }).await);
    let file = stand.verified_download().unwrap();
    let identity = |path: &Path| {
        let meta = std::fs::metadata(path).unwrap();
        (meta.ino(), meta.mtime_nsec(), meta.len())
    };
    let before = identity(&file);
    let first_assign = stand.assigned_at().await;

    stand.env.ts_mut().restart().await;
    let reassigned = wait_until(WAIT, || async {
        let at = stand.assigned_at().await;
        at.is_some() && at != first_assign
    })
    .await;
    assert!(reassigned, "the server never assigned the update again");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        identity(&file),
        before,
        "the second assignment touched the file being installed"
    );

    stand.env.wait_agent_exit().await;
    assert!(stand.exe_is_new());
    assert_eq!(std::fs::read(apply::prev_path(&stand.exe)).unwrap(), OLD);

    let runtime = stand.runtime(TARGET);
    stand.env.start_agent_with(runtime).await;
    stand.wait_state("succeeded").await;
    stand.env.stop().await;
}
