#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lodestore_agent::Paths;
use lodestore_agent::update::Pending;
use lodestore_agent::update::apply::{pending_path, prev_path};
use lodestore_agent::update::switch::{GuardCommand, Switcher, Systemd};

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

fn systemd_root() -> bool {
    let missing = if rustix::process::geteuid().as_raw() != 0 {
        "not running as root"
    } else if !Path::new("/run/systemd/system").is_dir() {
        "systemd is not PID 1 here"
    } else {
        return true;
    };
    assert!(std::env::var_os("CI").is_none(), "{missing}");
    eprintln!("skipped: {missing}");
    false
}

struct StopOnDrop(String);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        for suffix in ["timer", "service"] {
            let _ = std::process::Command::new("systemctl")
                .args(["stop", &format!("{}.{suffix}", self.0)])
                .status();
        }
        let _ = std::process::Command::new("systemctl")
            .args(["reset-failed", &format!("{}.service", self.0)])
            .status();
    }
}

#[test]
#[ignore = "needs root and systemd as PID 1"]
fn update_systemd_timer_returns_the_previous_binary() {
    if !systemd_root() {
        return;
    }
    let agent = agent_bin();
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let exe = root.path().join("lodestore-agent");
    std::fs::write(&exe, "new").unwrap();
    let prev = prev_path(&exe);
    std::fs::copy(&agent, &prev).unwrap();
    std::fs::set_permissions(&prev, std::fs::Permissions::from_mode(0o755)).unwrap();
    let paths = Paths {
        config_dir: root.path().join("etc"),
        state_dir: root.path().join("state"),
    };
    let id8 = format!("{:08x}", std::process::id());
    let unit = format!("lodestore-agent-update-{id8}");
    let _stop = StopOnDrop(unit.clone());
    lodestore_agent::identity::write_json_file(
        &pending_path(&paths),
        &Pending {
            update_id: format!("{id8}-0000-4000-8000-000000000000"),
            target_version: "0.0.2".into(),
            previous_version: "0.0.1".into(),
            deadline_ms: 0,
            guard_unit: unit.clone(),
        },
    )
    .unwrap();

    let guard = GuardCommand {
        prev: prev.clone(),
        exe: exe.clone(),
        config_dir: paths.config_dir.clone(),
        state_dir: paths.state_dir.clone(),
    };
    Systemd.arm(&unit, &guard, Duration::from_secs(2)).unwrap();
    assert!(Systemd.is_armed(&unit));

    let result = paths.update_dir().join("result.json");
    let started = Instant::now();
    while !result.exists() && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(result.exists(), "the timer did not fire within 20 s");
    assert_eq!(std::fs::read(&exe).unwrap(), std::fs::read(&agent).unwrap());
    assert!(!prev.exists());
    assert!(!pending_path(&paths).exists());
}

#[test]
#[ignore = "needs root and systemd as PID 1"]
fn update_systemd_disarmed_timer_never_fires() {
    if !systemd_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let unit = format!(
        "lodestore-agent-update-{:08x}",
        std::process::id() ^ 0x5a5a_5a5a
    );
    let _stop = StopOnDrop(unit.clone());
    let guard = GuardCommand {
        prev: "/bin/true".into(),
        exe: root.path().join("exe"),
        config_dir: root.path().join("etc"),
        state_dir: root.path().join("state"),
    };
    Systemd.arm(&unit, &guard, Duration::from_secs(60)).unwrap();
    assert!(Systemd.is_armed(&unit));
    Systemd.disarm(&unit);
    assert!(!Systemd.is_armed(&unit));
}

#[test]
#[ignore = "needs root and systemd as PID 1"]
fn update_systemd_disarm_stops_a_guard_that_already_runs() {
    if !systemd_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let guard_script = root.path().join("guard");
    std::fs::write(&guard_script, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&guard_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let unit = format!(
        "lodestore-agent-update-{:08x}",
        std::process::id() ^ 0x3c3c_3c3c
    );
    let _stop = StopOnDrop(unit.clone());
    let guard = GuardCommand {
        prev: guard_script,
        exe: root.path().join("exe"),
        config_dir: root.path().join("etc"),
        state_dir: root.path().join("state"),
    };
    let service = format!("{unit}.service");
    let service_state = || {
        let output = std::process::Command::new("systemctl")
            .args(["show", "--property=SubState", "--value", &service])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    Systemd.arm(&unit, &guard, Duration::from_secs(1)).unwrap();
    let started = Instant::now();
    while service_state() != "start" && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(service_state(), "start", "the guard did not start");
    Systemd.disarm(&unit);
    assert_ne!(service_state(), "start");
}

#[test]
#[ignore = "needs root and systemd as PID 1"]
fn update_systemd_rename_over_a_running_binary_is_allowed() {
    if !systemd_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let running = root.path().join("sleep");
    std::fs::copy("/bin/sleep", &running).unwrap();
    let mut child = std::process::Command::new(&running)
        .arg("30")
        .spawn()
        .unwrap();
    let replacement = root.path().join("replacement");
    std::fs::write(&replacement, "new").unwrap();
    let renamed = std::fs::rename(&replacement, &running);
    let _ = child.kill();
    let _ = child.wait();
    renamed.unwrap();
    assert_eq!(std::fs::read(&running).unwrap(), b"new");
}
