use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use data_encoding::HEXLOWER;
use lodestore_agent::Paths;
use lodestore_agent::update::apply::{install_files, prev_path, temp_files, temp_path};
use lodestore_agent::update::switch::{RecordingSwitcher, SwitchCall};
use lodestore_agent::update::{Pending, ROLLBACK_AFTER, UpdateOutcome, UpdateRuntime};
use lodestore_proto::agent::v1::UpdateAssign;
use lodestore_testkit::minisign::TestSigner;
use sha2::{Digest, Sha256};

const OLD: &[u8] = b"old agent";
const ID: &str = "0123abcd-4567-4000-8000-000000000000";

struct Host {
    _root: tempfile::TempDir,
    paths: Paths,
    exe: PathBuf,
    signer: TestSigner,
    switcher: Arc<RecordingSwitcher>,
}

impl Host {
    fn new() -> Host {
        Host::with_switcher(RecordingSwitcher::new())
    }

    fn with_switcher(switcher: RecordingSwitcher) -> Host {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: root.path().join("etc"),
            state_dir: root.path().join("state"),
        };
        let keys = paths.trusted_keys();
        std::fs::create_dir_all(&keys).unwrap();
        chmod(&keys, 0o755);
        let signer = TestSigner::generate();
        let key = keys.join("test.pub");
        std::fs::write(&key, signer.public_key_text()).unwrap();
        chmod(&key, 0o644);

        let bin_dir = root.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let exe = bin_dir.join("lodestore-agent");
        std::fs::write(&exe, OLD).unwrap();
        chmod(&exe, 0o755);
        Host {
            _root: root,
            paths,
            exe,
            signer,
            switcher: Arc::new(switcher),
        }
    }

    fn runtime(&self, version: &str) -> UpdateRuntime {
        UpdateRuntime {
            exe: self.exe.clone(),
            version: version.to_string(),
            switcher: self.switcher.clone(),
            enabled: true,
            flight: Default::default(),
        }
    }

    fn bin(&self) -> PathBuf {
        self.exe.with_file_name(".lodestore-agent.new.test")
    }

    fn download(&self, script: &str) -> PathBuf {
        let bin = self.bin();
        std::fs::write(&bin, script).unwrap();
        chmod(&bin, 0o644);
        bin
    }

    fn assign(&self, script: &str, version: &str) -> UpdateAssign {
        UpdateAssign {
            update_id: ID.to_string(),
            target_version: version.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            size: script.len() as u64,
            sha256: HEXLOWER.encode(&Sha256::digest(script.as_bytes())),
        }
    }

    fn sign(&self, script: &str, version: &str, arch: &str) -> Vec<u8> {
        self.signer
            .sign(
                script.as_bytes(),
                &format!("lodestore-agent {version} {arch}"),
            )
            .into_bytes()
    }

    async fn install(
        &self,
        runtime: &UpdateRuntime,
        assign: &UpdateAssign,
        sig: &[u8],
    ) -> UpdateOutcome {
        install_files(&self.paths, runtime, assign, &self.bin(), sig, &|| false).await
    }

    fn untouched(&self) {
        assert_eq!(std::fs::read(&self.exe).unwrap(), OLD);
        assert!(temp_files(&self.exe).is_empty());
        assert!(!prev_path(&self.exe).exists());
        assert!(!self.paths.update_dir().join("pending.json").exists());
    }
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn script(version: &str) -> String {
    format!("#!/bin/sh\necho \"lodestore-agent {version}\"\n")
}

fn rejected(outcome: UpdateOutcome) -> String {
    match outcome {
        UpdateOutcome::Rejected(reason) => reason,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn untrusted_key_is_rejected() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let stranger = TestSigner::generate();
    let sig = stranger
        .sign(
            body.as_bytes(),
            &format!("lodestore-agent 99.0.0 {}", std::env::consts::ARCH),
        )
        .into_bytes();
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
            .await,
    );
    assert!(reason.contains("key not trusted"), "{reason}");
    host.untouched();
}

#[tokio::test]
async fn modified_binary_fails_the_checksum() {
    let host = Host::new();
    let body = script("99.0.0");
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let assign = host.assign(&body, "99.0.0");
    host.download(&format!("{body}# tampered\n"));
    let assign = UpdateAssign {
        size: assign.size + 100,
        ..assign
    };
    let reason = rejected(host.install(&host.runtime("0.0.1"), &assign, &sig).await);
    assert_eq!(reason, "The download does not match its checksum.");
    host.untouched();
}

#[tokio::test]
async fn legacy_signature_is_rejected() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host
        .signer
        .sign_legacy(
            body.as_bytes(),
            &format!("lodestore-agent 99.0.0 {}", std::env::consts::ARCH),
        )
        .into_bytes();
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
            .await,
    );
    assert!(reason.contains("only prehashed"), "{reason}");
    host.untouched();
}

#[tokio::test]
async fn same_or_older_version_is_rejected() {
    let host = Host::new();
    let body = script("0.0.1");
    host.download(&body);
    let sig = host.sign(&body, "0.0.1", std::env::consts::ARCH);
    let assign = host.assign(&body, "0.0.1");
    let reason = rejected(host.install(&host.runtime("0.0.1"), &assign, &sig).await);
    assert_eq!(reason, "Version 0.0.1 is not newer than 0.0.1.");
    host.untouched();

    host.download(&body);
    let reason = rejected(host.install(&host.runtime("1.0.0"), &assign, &sig).await);
    assert_eq!(reason, "Version 0.0.1 is not newer than 1.0.0.");
    host.untouched();
}

#[tokio::test]
async fn signed_version_must_match_the_assignment() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "98.0.0"), &sig)
            .await,
    );
    assert!(reason.contains("not the requested"), "{reason}");
    host.untouched();
}

#[tokio::test]
async fn other_architecture_is_rejected() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", "pdp11");
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
            .await,
    );
    assert!(
        reason.starts_with("The binary is built for pdp11"),
        "{reason}"
    );
    host.untouched();
}

#[tokio::test]
async fn binary_reporting_another_version_is_rejected() {
    let host = Host::new();
    let body = script("98.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
            .await,
    );
    assert!(
        reason.starts_with("The new binary did not start"),
        "{reason}"
    );
    host.untouched();
}

#[tokio::test]
async fn binary_runs_with_an_empty_environment() {
    let host = Host::new();
    let body = "#!/bin/sh\nif [ -n \"${HOME:-}\" ]; then echo leaked; else echo \"lodestore-agent 99.0.0\"; fi\n";
    host.download(body);
    let sig = host.sign(body, "99.0.0", std::env::consts::ARCH);
    let outcome = host
        .install(&host.runtime("0.0.1"), &host.assign(body, "99.0.0"), &sig)
        .await;
    assert_eq!(outcome, UpdateOutcome::Switched);
}

#[tokio::test]
async fn download_larger_than_announced_is_rejected() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let assign = UpdateAssign {
        size: body.len() as u64 - 1,
        ..host.assign(&body, "99.0.0")
    };
    let reason = rejected(host.install(&host.runtime("0.0.1"), &assign, &sig).await);
    assert_eq!(reason, "The download is larger than announced.");
    host.untouched();
}

#[tokio::test]
async fn busy_host_is_left_alone() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let outcome = install_files(
        &host.paths,
        &host.runtime("0.0.1"),
        &host.assign(&body, "99.0.0"),
        &host.bin(),
        &sig,
        &|| true,
    )
    .await;
    assert_eq!(outcome, UpdateOutcome::Busy);
    host.untouched();
    assert!(host.switcher.calls().is_empty());
}

#[tokio::test]
async fn successful_update_switches_and_keeps_the_previous_binary() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let outcome = host
        .install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
        .await;
    assert_eq!(outcome, UpdateOutcome::Switched);

    assert_eq!(std::fs::read_to_string(&host.exe).unwrap(), body);
    let mode = std::fs::metadata(&host.exe).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    assert_eq!(std::fs::read(prev_path(&host.exe)).unwrap(), OLD);
    assert!(temp_files(&host.exe).is_empty());

    let pending: Pending = serde_json::from_slice(
        &std::fs::read(host.paths.update_dir().join("pending.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pending.update_id, ID);
    assert_eq!(pending.target_version, "99.0.0");
    assert_eq!(pending.previous_version, "0.0.1");
    assert_eq!(pending.guard_unit, "lodestore-agent-update-0123abcd");
    assert!(pending.deadline_ms > 0);

    let calls = host.switcher.calls();
    assert_eq!(calls.len(), 1);
    let Some(SwitchCall::Arm { unit, guard, after }) = calls.first() else {
        panic!("expected arm, got {calls:?}");
    };
    assert_eq!(unit, "lodestore-agent-update-0123abcd");
    assert_eq!(guard.prev, prev_path(&host.exe));
    assert_eq!(guard.exe, host.exe);
    assert_eq!(*after, ROLLBACK_AFTER);
}

#[tokio::test]
async fn stale_previous_binary_is_replaced() {
    let host = Host::new();
    std::fs::write(prev_path(&host.exe), b"two versions ago").unwrap();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let outcome = host
        .install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
        .await;
    assert_eq!(outcome, UpdateOutcome::Switched);
    assert_eq!(std::fs::read(prev_path(&host.exe)).unwrap(), OLD);
}

#[tokio::test]
async fn unconfirmed_update_keeps_its_previous_binary() {
    let host = Host::new();
    let pending = host.paths.update_dir().join("pending.json");
    std::fs::create_dir_all(host.paths.update_dir()).unwrap();
    std::fs::write(&pending, "unconfirmed").unwrap();
    std::fs::write(prev_path(&host.exe), b"the version to return to").unwrap();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
            .await,
    );
    assert_eq!(reason, "An earlier update has not finished yet.");
    assert_eq!(std::fs::read(&host.exe).unwrap(), OLD);
    assert_eq!(
        std::fs::read(prev_path(&host.exe)).unwrap(),
        b"the version to return to"
    );
    assert_eq!(std::fs::read(&pending).unwrap(), b"unconfirmed");
    assert!(temp_files(&host.exe).is_empty());
    assert!(host.switcher.calls().is_empty());
}

#[tokio::test]
async fn running_rollback_blocks_a_new_switch() {
    let host = Host::new();
    std::fs::create_dir_all(host.paths.update_dir()).unwrap();
    std::fs::write(host.paths.update_dir().join("pending.rollback"), "{}").unwrap();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
            .await,
    );
    assert_eq!(reason, "An earlier update has not finished yet.");
    assert_eq!(std::fs::read(&host.exe).unwrap(), OLD);
}

#[tokio::test]
async fn failed_guard_restores_the_old_binary() {
    let host = Host::with_switcher(RecordingSwitcher::failing_arm("no systemd"));
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    let reason = rejected(
        host.install(&host.runtime("0.0.1"), &host.assign(&body, "99.0.0"), &sig)
            .await,
    );
    assert_eq!(reason, "Could not schedule the rollback guard: no systemd");
    host.untouched();
}

#[tokio::test]
async fn malformed_update_id_is_rejected() {
    let host = Host::new();
    let body = script("99.0.0");
    host.download(&body);
    let sig = host.sign(&body, "99.0.0", std::env::consts::ARCH);
    for id in ["../../etc/passwd", "zzzzzzzz-1", "abc"] {
        let assign = UpdateAssign {
            update_id: id.to_string(),
            ..host.assign(&body, "99.0.0")
        };
        let outcome = host.install(&host.runtime("0.0.1"), &assign, &sig).await;
        assert!(matches!(outcome, UpdateOutcome::Rejected(_)), "{id}");
        host.download(&body);
    }
    std::fs::remove_file(host.bin()).unwrap();
    host.untouched();
}

#[test]
fn every_download_gets_its_own_name() {
    let host = Host::new();
    let first = temp_path(&host.exe).unwrap();
    assert_ne!(first, temp_path(&host.exe).unwrap());
    assert_eq!(first.parent(), host.exe.parent());
    std::fs::write(&first, "partial").unwrap();
    std::fs::write(host.exe.with_file_name("lodestore-agent.new"), "keep").unwrap();
    assert_eq!(temp_files(&host.exe), vec![first]);
}
