use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardCommand {
    pub prev: PathBuf,
    pub exe: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

pub trait Switcher: Send + Sync {
    fn arm(&self, unit: &str, guard: &GuardCommand, after: Duration) -> anyhow::Result<()>;
    fn disarm(&self, unit: &str);
    fn is_armed(&self, unit: &str) -> bool;
    fn restart_service(&self) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchCall {
    Arm {
        unit: String,
        guard: GuardCommand,
        after: Duration,
    },
    Disarm {
        unit: String,
    },
    RestartService,
}

#[derive(Debug, Default)]
pub struct RecordingSwitcher {
    calls: Mutex<Vec<SwitchCall>>,
    arm_error: Mutex<Option<String>>,
    armed: AtomicBool,
}

impl RecordingSwitcher {
    pub fn new() -> RecordingSwitcher {
        RecordingSwitcher::default()
    }

    pub fn failing_arm(message: &str) -> RecordingSwitcher {
        let switcher = RecordingSwitcher::default();
        *lock(&switcher.arm_error) = Some(message.to_string());
        switcher
    }

    pub fn armed(armed: bool) -> RecordingSwitcher {
        let switcher = RecordingSwitcher::default();
        switcher.armed.store(armed, Ordering::SeqCst);
        switcher
    }

    pub fn calls(&self) -> Vec<SwitchCall> {
        lock(&self.calls).clone()
    }

    fn record(&self, call: SwitchCall) {
        lock(&self.calls).push(call);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Switcher for RecordingSwitcher {
    fn arm(&self, unit: &str, guard: &GuardCommand, after: Duration) -> anyhow::Result<()> {
        self.record(SwitchCall::Arm {
            unit: unit.to_string(),
            guard: guard.clone(),
            after,
        });
        match lock(&self.arm_error).clone() {
            Some(message) => Err(anyhow::anyhow!(message)),
            None => Ok(()),
        }
    }

    fn disarm(&self, unit: &str) {
        self.record(SwitchCall::Disarm {
            unit: unit.to_string(),
        });
    }

    fn is_armed(&self, _unit: &str) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    fn restart_service(&self) -> anyhow::Result<()> {
        self.record(SwitchCall::RestartService);
        Ok(())
    }
}

const SERVICE: &str = "lodestore-agent.service";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Real systemd calls. Every program runs with an argv vector, never a shell.
pub struct Systemd;

impl Systemd {
    pub fn arm_args(unit: &str, guard: &GuardCommand, after: Duration) -> Vec<OsString> {
        let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
        vec![
            format!("--unit={unit}").into(),
            format!("--on-active={}s", secs.max(1)).into(),
            "--timer-property=AccuracySec=1s".into(),
            "--property=Type=oneshot".into(),
            "--collect".into(),
            guard.prev.clone().into(),
            "update-rollback".into(),
            "--config-dir".into(),
            guard.config_dir.clone().into(),
            "--state-dir".into(),
            guard.state_dir.clone().into(),
            "--exe".into(),
            guard.exe.clone().into(),
        ]
    }
}

fn run(program: &str, args: &[OsString]) -> anyhow::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("{program}: {e}"))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > CALL_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{program} did not finish in {} s", CALL_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(pipe) = child.stderr.take() {
        let _ = pipe.take(4096).read_to_string(&mut stderr);
    }
    anyhow::bail!("{program} failed ({status}): {}", stderr.trim())
}

impl Switcher for Systemd {
    fn arm(&self, unit: &str, guard: &GuardCommand, after: Duration) -> anyhow::Result<()> {
        run("systemd-run", &Systemd::arm_args(unit, guard, after))
    }

    fn disarm(&self, unit: &str) {
        for suffix in ["timer", "service"] {
            let args = ["stop".into(), format!("{unit}.{suffix}").into()];
            if let Err(e) = run("systemctl", &args) {
                tracing::warn!(error = %e, unit, "could not stop the rollback guard");
            }
        }
    }

    fn is_armed(&self, unit: &str) -> bool {
        let args = [
            "is-active".into(),
            "--quiet".into(),
            format!("{unit}.timer").into(),
        ];
        run("systemctl", &args).is_ok()
    }

    fn restart_service(&self) -> anyhow::Result<()> {
        run("systemctl", &["restart".into(), SERVICE.into()])
    }
}
