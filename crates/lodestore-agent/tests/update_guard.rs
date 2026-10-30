use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lodestore_agent::Paths;
use lodestore_agent::update::apply::{pending_path, prev_path, temp_files, temp_path};
use lodestore_agent::update::guard::{
    RollbackResult, StartAction, after_connect, clear_rollback, on_start, rollback, rolled_back,
};
use lodestore_agent::update::switch::{
    GuardCommand, RecordingSwitcher, SwitchCall, Switcher, Systemd,
};
use lodestore_agent::update::{Pending, ROLLBACK_AFTER, UpdateRuntime};
use lodestore_proto::agent::v1::UpdateState;

const ID: &str = "0123abcd-4567-4000-8000-000000000000";
const UNIT: &str = "lodestore-agent-update-0123abcd";

fn agent_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let bin = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("lodestore-agent");
    assert!(
        bin.is_file(),
        "{} is missing; build the agent first",
        bin.display()
    );
    bin
}

struct Host {
    _root: tempfile::TempDir,
    paths: Paths,
    exe: PathBuf,
    switcher: Arc<RecordingSwitcher>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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
        std::fs::create_dir_all(paths.update_dir()).unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("lodestore-agent");
        std::fs::write(&exe, "new").unwrap();
        std::fs::write(prev_path(&exe), "old").unwrap();
        Host {
            _root: root,
            paths,
            exe,
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

    fn pending(&self, deadline_ms: u64) {
        let record = Pending {
            update_id: ID.to_string(),
            target_version: "0.0.2".to_string(),
            previous_version: "0.0.1".to_string(),
            deadline_ms,
            guard_unit: UNIT.to_string(),
        };
        std::fs::write(
            pending_path(&self.paths),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }

    fn result_path(&self) -> PathBuf {
        self.paths.update_dir().join("result.json")
    }

    fn claim_path(&self) -> PathBuf {
        self.paths.update_dir().join("pending.rollback")
    }

    fn claimed(&self) {
        std::fs::rename(pending_path(&self.paths), self.claim_path()).unwrap();
    }

    fn rolled_back_files(&self) {
        assert_eq!(std::fs::read(&self.exe).unwrap(), b"old");
        assert!(!prev_path(&self.exe).exists());
        assert!(!pending_path(&self.paths).exists());
        assert!(!self.claim_path().exists());
    }
}

fn reason(p: &Pending) -> String {
    format!(
        "The new version did not connect within 2 min. Running {} again.",
        p.previous_version
    )
}

#[test]
fn rollback_returns_the_previous_binary() {
    let host = Host::new();
    host.pending(now_ms());
    assert!(rollback(&host.paths, &host.exe, &*host.switcher, reason).unwrap());
    host.rolled_back_files();
    assert_eq!(
        rolled_back(&host.paths).unwrap(),
        RollbackResult {
            update_id: ID.to_string(),
            reason: "The new version did not connect within 2 min. Running 0.0.1 again.".into(),
        }
    );
    assert_eq!(host.switcher.calls(), vec![SwitchCall::RestartService]);
}

#[test]
fn rollback_without_pending_changes_nothing() {
    let host = Host::new();
    assert!(!rollback(&host.paths, &host.exe, &*host.switcher, reason).unwrap());
    assert_eq!(std::fs::read(&host.exe).unwrap(), b"new");
    assert!(prev_path(&host.exe).exists());
    assert!(!host.result_path().exists());
    assert!(host.switcher.calls().is_empty());
}

#[test]
fn expired_pending_rolls_back_at_start() {
    let host = Host::new();
    host.pending(now_ms() - 1000);
    std::fs::write(temp_path(&host.exe).unwrap(), "stale").unwrap();
    let action = on_start(&host.paths, &host.runtime("0.0.2"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Restart);
    host.rolled_back_files();
    assert!(temp_files(&host.exe).is_empty());
    assert_eq!(
        rolled_back(&host.paths).unwrap().reason,
        "The host restarted during the update. Running 0.0.1 again."
    );
    assert!(!host.switcher.calls().contains(&SwitchCall::RestartService));
}

#[test]
fn leftover_downloads_are_removed_without_pending() {
    let host = Host::new();
    std::fs::write(temp_path(&host.exe).unwrap(), "stale").unwrap();
    std::fs::write(temp_path(&host.exe).unwrap(), "stale").unwrap();
    let action = on_start(&host.paths, &host.runtime("0.0.1"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Continue);
    assert!(temp_files(&host.exe).is_empty());
    assert_eq!(std::fs::read(&host.exe).unwrap(), b"new");
}

#[test]
fn binary_installed_by_hand_is_not_rolled_back() {
    for offset in [-1000, 50_000] {
        let host = Host::new();
        let now = now_ms();
        host.pending(now.checked_add_signed(offset).unwrap());
        let action = on_start(&host.paths, &host.runtime("0.0.9"), now).unwrap();
        assert_eq!(action, StartAction::Continue);
        assert_eq!(std::fs::read(&host.exe).unwrap(), b"new");
        assert!(prev_path(&host.exe).exists());
        assert!(!pending_path(&host.paths).exists());
        assert!(rolled_back(&host.paths).is_none());
        assert_eq!(
            host.switcher.calls(),
            vec![SwitchCall::Disarm {
                unit: UNIT.to_string()
            }]
        );
    }
}

#[test]
fn on_start_rearms_guard_after_reboot() {
    let host = Host::with_switcher(RecordingSwitcher::armed(false));
    let now = now_ms();
    host.pending(now + 50_000);
    let action = on_start(&host.paths, &host.runtime("0.0.2"), now).unwrap();
    assert_eq!(action, StartAction::Continue);
    let calls = host.switcher.calls();
    let [SwitchCall::Arm { unit, guard, after }] = calls.as_slice() else {
        panic!("expected one arm call, got {calls:?}");
    };
    assert_eq!(unit, UNIT);
    assert!(after.abs_diff(Duration::from_secs(50)) <= Duration::from_secs(1));
    assert_eq!(
        guard,
        &GuardCommand {
            prev: prev_path(&host.exe),
            exe: host.exe.clone(),
            config_dir: host.paths.config_dir.clone(),
            state_dir: host.paths.state_dir.clone(),
        }
    );
    assert!(pending_path(&host.paths).exists());
}

#[test]
fn rearmed_guard_never_waits_past_the_rollback_window() {
    let host = Host::with_switcher(RecordingSwitcher::armed(false));
    let now = now_ms();
    host.pending(now + 3_600_000);
    on_start(&host.paths, &host.runtime("0.0.2"), now).unwrap();
    let calls = host.switcher.calls();
    let [SwitchCall::Arm { after, .. }] = calls.as_slice() else {
        panic!("expected one arm call, got {calls:?}");
    };
    assert_eq!(*after, ROLLBACK_AFTER);
}

#[test]
fn on_start_keeps_running_guard() {
    let host = Host::with_switcher(RecordingSwitcher::armed(true));
    let now = now_ms();
    host.pending(now + 50_000);
    let action = on_start(&host.paths, &host.runtime("0.0.2"), now).unwrap();
    assert_eq!(action, StartAction::Continue);
    assert!(host.switcher.calls().is_empty());
}

#[test]
fn on_start_rolls_back_when_rearm_fails() {
    let host = Host::with_switcher(RecordingSwitcher::failing_arm("no systemd"));
    let now = now_ms();
    host.pending(now + 50_000);
    let action = on_start(&host.paths, &host.runtime("0.0.2"), now).unwrap();
    assert_eq!(action, StartAction::Restart);
    host.rolled_back_files();
    let reason = rolled_back(&host.paths).unwrap().reason;
    assert!(
        reason.starts_with("Could not schedule the rollback guard after a restart"),
        "{reason}"
    );
}

#[test]
fn after_connect_confirms_and_stops_the_guard() {
    let host = Host::new();
    host.pending(now_ms() + 60_000);
    let status = after_connect(&host.paths, &host.runtime("0.0.2")).unwrap();
    assert_eq!(status.update_id, ID);
    assert_eq!(status.state, UpdateState::Confirmed as i32);
    assert_eq!(
        host.switcher.calls(),
        vec![SwitchCall::Disarm {
            unit: UNIT.to_string()
        }]
    );
    assert!(!pending_path(&host.paths).exists());
    assert!(after_connect(&host.paths, &host.runtime("0.0.2")).is_none());
}

struct GuardFiresDuringDisarm {
    paths: Paths,
    exe: PathBuf,
}

impl Switcher for GuardFiresDuringDisarm {
    fn arm(&self, _: &str, _: &GuardCommand, _: Duration) -> anyhow::Result<()> {
        Ok(())
    }

    fn disarm(&self, _: &str) {
        rollback(&self.paths, &self.exe, &RecordingSwitcher::new(), reason).unwrap();
    }

    fn is_armed(&self, _: &str) -> bool {
        true
    }

    fn restart_service(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn confirmed_update_keeps_its_binary_when_the_guard_fires_late() {
    let host = Host::new();
    host.pending(now_ms() + 60_000);
    let switcher = GuardFiresDuringDisarm {
        paths: host.paths.clone(),
        exe: host.exe.clone(),
    };
    let runtime = UpdateRuntime {
        switcher: Arc::new(switcher),
        ..host.runtime("0.0.2")
    };
    let status = after_connect(&host.paths, &runtime).unwrap();
    assert_eq!(status.state, UpdateState::Confirmed as i32);
    assert_eq!(std::fs::read(&host.exe).unwrap(), b"new");
    assert!(prev_path(&host.exe).exists());
    assert!(!host.result_path().exists());
}

#[test]
fn rollback_claims_the_update_before_it_touches_the_binary() {
    let host = Host::new();
    host.pending(now_ms());
    let prev = prev_path(&host.exe);
    std::fs::remove_file(&prev).unwrap();
    std::fs::create_dir(&prev).unwrap();

    assert!(rollback(&host.paths, &host.exe, &*host.switcher, reason).is_err());
    assert!(host.claim_path().exists());
    assert!(after_connect(&host.paths, &host.runtime("0.0.2")).is_none());
    assert_eq!(std::fs::read(&host.exe).unwrap(), b"new");
    assert!(host.switcher.calls().is_empty());
}

#[test]
fn interrupted_rollback_is_finished_at_start() {
    let host = Host::new();
    host.pending(now_ms() + 60_000);
    host.claimed();
    let action = on_start(&host.paths, &host.runtime("0.0.2"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Restart);
    host.rolled_back_files();
    assert_eq!(
        rolled_back(&host.paths).unwrap().reason,
        "The rollback was interrupted and finished at start. Running 0.0.1 again."
    );
}

#[test]
fn crash_after_the_rename_still_records_the_rollback() {
    let host = Host::new();
    host.pending(now_ms() + 60_000);
    host.claimed();
    std::fs::rename(prev_path(&host.exe), &host.exe).unwrap();
    let action = on_start(&host.paths, &host.runtime("0.0.1"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Continue);
    host.rolled_back_files();
    assert_eq!(
        rolled_back(&host.paths).unwrap().reason,
        "The rollback was interrupted and finished at start. Running 0.0.1 again."
    );
}

#[test]
fn rollback_restarts_the_service_when_the_result_cannot_be_written() {
    let host = Host::new();
    host.pending(now_ms());
    std::fs::create_dir(host.result_path()).unwrap();

    assert!(rollback(&host.paths, &host.exe, &*host.switcher, reason).unwrap());
    assert_eq!(std::fs::read(&host.exe).unwrap(), b"old");
    assert_eq!(host.switcher.calls(), vec![SwitchCall::RestartService]);
    assert!(host.claim_path().exists());

    std::fs::remove_dir(host.result_path()).unwrap();
    let action = on_start(&host.paths, &host.runtime("0.0.1"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Continue);
    host.rolled_back_files();
    assert_eq!(rolled_back(&host.paths).unwrap().update_id, ID);
}

#[test]
fn start_does_not_loop_while_the_result_cannot_be_written() {
    let host = Host::new();
    host.pending(now_ms() - 1000);
    std::fs::create_dir(host.result_path()).unwrap();

    let action = on_start(&host.paths, &host.runtime("0.0.2"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Restart);
    let action = on_start(&host.paths, &host.runtime("0.0.1"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Continue);

    assert_eq!(std::fs::read(&host.exe).unwrap(), b"old");
    assert!(host.claim_path().exists());
}

const PREVIOUS_GONE: &str =
    "The previous binary is gone, so the update was not rolled back. Run install.sh on the host.";

#[test]
fn guard_reports_a_missing_previous_binary() {
    let host = Host::new();
    host.pending(now_ms());
    std::fs::remove_file(prev_path(&host.exe)).unwrap();
    assert!(!rollback(&host.paths, &host.exe, &*host.switcher, reason).unwrap());
    assert_eq!(std::fs::read(&host.exe).unwrap(), b"new");
    assert_eq!(rolled_back(&host.paths).unwrap().reason, PREVIOUS_GONE);
    assert!(!pending_path(&host.paths).exists());
    assert!(!host.claim_path().exists());
    assert!(host.switcher.calls().is_empty());
}

#[test]
fn start_reports_a_missing_previous_binary_and_keeps_running() {
    let host = Host::new();
    host.pending(now_ms() - 1000);
    std::fs::remove_file(prev_path(&host.exe)).unwrap();
    let action = on_start(&host.paths, &host.runtime("0.0.2"), now_ms()).unwrap();
    assert_eq!(action, StartAction::Continue);
    assert_eq!(std::fs::read(&host.exe).unwrap(), b"new");
    assert_eq!(rolled_back(&host.paths).unwrap().reason, PREVIOUS_GONE);
    assert_eq!(
        host.switcher.calls(),
        vec![SwitchCall::Disarm {
            unit: UNIT.to_string()
        }]
    );
}

#[test]
fn after_connect_reports_the_rollback_until_cleared() {
    let host = Host::new();
    host.pending(now_ms());
    rollback(&host.paths, &host.exe, &*host.switcher, reason).unwrap();
    for _ in 0..2 {
        let status = after_connect(&host.paths, &host.runtime("0.0.1")).unwrap();
        assert_eq!(status.state, UpdateState::RolledBack as i32);
        assert!(status.reason.contains("did not connect within 2 min"));
    }
    clear_rollback(&host.paths, "ffffffff-0000-4000-8000-000000000000").unwrap();
    assert!(host.result_path().exists());
    clear_rollback(&host.paths, ID).unwrap();
    assert!(!host.result_path().exists());
    assert!(after_connect(&host.paths, &host.runtime("0.0.1")).is_none());
}

#[test]
fn arm_args_are_an_argv_vector_for_the_old_binary() {
    let guard = GuardCommand {
        prev: "/usr/local/bin/lodestore-agent.prev".into(),
        exe: "/usr/local/bin/lodestore-agent".into(),
        config_dir: "/etc/lodestore".into(),
        state_dir: "/var/lib/lodestore".into(),
    };
    let args = Systemd::arm_args(UNIT, &guard, Duration::from_secs(120));
    let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        args,
        [
            "--unit=lodestore-agent-update-0123abcd",
            "--on-active=120s",
            "--timer-property=AccuracySec=1s",
            "--property=Type=oneshot",
            "--collect",
            "/usr/local/bin/lodestore-agent.prev",
            "update-rollback",
            "--config-dir",
            "/etc/lodestore",
            "--state-dir",
            "/var/lib/lodestore",
            "--exe",
            "/usr/local/bin/lodestore-agent",
        ]
    );
}

#[test]
fn update_rollback_subcommand_without_pending_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("exe");
    std::fs::write(&exe, "current").unwrap();
    std::fs::write(prev_path(&exe), "prev").unwrap();
    let status = std::process::Command::new(agent_bin())
        .arg("update-rollback")
        .arg("--config-dir")
        .arg(dir.path())
        .arg("--state-dir")
        .arg(dir.path())
        .arg("--exe")
        .arg(&exe)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&exe).unwrap(), b"current");
    assert!(prev_path(&exe).exists());
}

#[test]
fn detect_is_disabled_outside_systemd() {
    if std::env::var_os("INVOCATION_ID").is_some() {
        return;
    }
    assert!(!UpdateRuntime::detect().enabled);
}
