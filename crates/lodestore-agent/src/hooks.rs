use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const HOOK_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const DEFAULT_TIMEOUT_S: u32 = 3600;
const KILL_GRACE: Duration = Duration::from_secs(10);
const READER_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_CAP: usize = 64 * 1024;
const LIST_CAP: usize = 256;
const NAME_MAX: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPhase {
    Pre,
    Post,
}

impl HookPhase {
    fn as_str(self) -> &'static str {
        match self {
            HookPhase::Pre => "pre",
            HookPhase::Post => "post",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HookRun<'a> {
    pub hooks_dir: &'a Path,
    pub allow_inline: bool,
    pub policy: &'a str,
    pub job_id: &'a str,
    pub phase: HookPhase,
    pub backup_status: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
pub struct HookSpec {
    pub script: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub timeout_s: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("hook {script} rejected: {reason}")]
    Rejected { script: String, reason: String },
    #[error("hook script {script} is not installed")]
    Missing { script: String },
    #[error("inline hook commands are turned off on this host")]
    InlineNotAllowed,
    #[error("hook {script} timed out after {timeout_s} s")]
    Timeout { script: String, timeout_s: u32 },
    #[error("hook {script} failed with {}", exit_text(*code))]
    Failed { script: String, code: Option<i32> },
    #[error("hook was cancelled")]
    Cancelled,
    #[error("hook could not be started: {0}")]
    Spawn(String),
}

fn exit_text(code: Option<i32>) -> String {
    code.map_or_else(
        || "a signal".to_string(),
        |code| format!("exit code {code}"),
    )
}

impl HookError {
    pub fn code(&self) -> &'static str {
        match self {
            HookError::Rejected { .. } => "hook_rejected",
            HookError::Missing { .. } => "hook_missing",
            HookError::InlineNotAllowed => "inline_not_allowed",
            HookError::Timeout { .. } => "hook_timeout",
            HookError::Failed { .. } => "pre_hook_failed",
            HookError::Cancelled => "cancelled",
            HookError::Spawn(_) => "hook_failed_to_start",
        }
    }
}

#[derive(Debug)]
pub struct HookResult {
    pub outcome: Result<(), HookError>,
    pub exit_code: Option<i32>,
    pub output: Vec<u8>,
    pub truncated: bool,
}

impl HookResult {
    fn failed(err: HookError) -> HookResult {
        HookResult {
            outcome: Err(err),
            exit_code: None,
            output: Vec::new(),
            truncated: false,
        }
    }
}

pub(crate) fn unsafe_reason(meta: &Metadata) -> Option<&'static str> {
    if meta.uid() != rustix::process::geteuid().as_raw() {
        Some("not owned by the agent user")
    } else if meta.mode() & 0o022 != 0 {
        Some("writable by group or others")
    } else {
        None
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= NAME_MAX
        && !name.contains("..")
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn rejected(script: &str, reason: impl Into<String>) -> HookError {
    HookError::Rejected {
        script: script.to_string(),
        reason: reason.into(),
    }
}

pub fn check_script(hooks_dir: &Path, name: &str) -> Result<PathBuf, HookError> {
    if !valid_name(name) {
        return Err(rejected(name, "invalid script name"));
    }
    let dir = std::fs::symlink_metadata(hooks_dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HookError::Missing {
                script: name.to_string(),
            }
        } else {
            rejected(name, format!("hooks directory: {e}"))
        }
    })?;
    if !dir.is_dir() {
        return Err(rejected(name, "hooks directory is not a directory"));
    }
    if let Some(reason) = unsafe_reason(&dir) {
        return Err(rejected(name, format!("hooks directory is {reason}")));
    }
    let path = hooks_dir.join(name);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HookError::Missing {
                script: name.to_string(),
            }
        } else {
            rejected(name, e.to_string())
        }
    })?;
    if !meta.is_file() {
        return Err(rejected(name, "not a regular file"));
    }
    if let Some(reason) = unsafe_reason(&meta) {
        return Err(rejected(name, reason));
    }
    if meta.mode() & 0o100 == 0 {
        return Err(rejected(name, "not executable"));
    }
    Ok(path)
}

pub fn list_scripts(hooks_dir: &Path) -> Vec<(String, bool)> {
    let Ok(entries) = std::fs::read_dir(hooks_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| valid_name(name))
        .collect();
    names.sort();
    names.truncate(LIST_CAP);
    names
        .into_iter()
        .map(|name| {
            let ok = check_script(hooks_dir, &name).is_ok();
            (name, ok)
        })
        .collect()
}

#[derive(Default)]
struct Tail {
    buf: Vec<u8>,
    truncated: bool,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > OUTPUT_CAP {
            let excess = self.buf.len() - OUTPUT_CAP;
            self.buf.drain(..excess);
            self.truncated = true;
        }
    }
}

fn read_into(
    mut stream: impl AsyncRead + Unpin + Send + 'static,
    tail: Arc<Mutex<Tail>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stream.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            if let Ok(mut tail) = tail.lock() {
                tail.push(&chunk[..n]);
            }
        }
    })
}

async fn terminate(child: &mut tokio::process::Child, pgid: Pid) {
    let _ = kill_process_group(pgid, Signal::TERM);
    let deadline = tokio::time::Instant::now() + KILL_GRACE;
    loop {
        let _ = child.try_wait();
        if matches!(test_kill_process_group(pgid), Err(Errno::SRCH)) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = kill_process_group(pgid, Signal::KILL);
    let _ = child.wait().await;
}

pub async fn run_hook(
    run: &HookRun<'_>,
    hook: &HookSpec,
    cancel: &CancellationToken,
) -> HookResult {
    let inline = hook.command.as_deref().filter(|c| !c.is_empty());
    let (label, mut cmd) = match (hook.script.as_deref(), inline) {
        (Some(script), None) => match check_script(run.hooks_dir, script) {
            Ok(path) => {
                let mut cmd = Command::new(path);
                cmd.args(&hook.args);
                (script.to_string(), cmd)
            }
            Err(e) => return HookResult::failed(e),
        },
        (None, Some(command)) => {
            if !run.allow_inline {
                return HookResult::failed(HookError::InlineNotAllowed);
            }
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(command).arg("lodestore-hook");
            cmd.args(&hook.args);
            ("inline".to_string(), cmd)
        }
        _ => {
            return HookResult::failed(rejected(
                hook.script.as_deref().unwrap_or("inline"),
                "exactly one of script and command is required",
            ));
        }
    };

    cmd.env_clear()
        .env("PATH", HOOK_PATH)
        .env("LODESTORE_POLICY", run.policy)
        .env("LODESTORE_JOB_ID", run.job_id)
        .env("LODESTORE_HOOK_PHASE", run.phase.as_str())
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(status) = run.backup_status {
        cmd.env("LODESTORE_BACKUP_STATUS", status);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return HookResult::failed(HookError::Spawn(e.to_string())),
    };
    let pgid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    let tail = Arc::new(Mutex::new(Tail::default()));
    let mut readers: Vec<_> = [
        child.stdout.take().map(|s| read_into(s, tail.clone())),
        child.stderr.take().map(|s| read_into(s, tail.clone())),
    ]
    .into_iter()
    .flatten()
    .collect();

    let timeout_s = if hook.timeout_s == 0 {
        DEFAULT_TIMEOUT_S
    } else {
        hook.timeout_s
    };
    let mut exit_code = None;
    let outcome = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => {
                exit_code = status.code();
                if status.success() {
                    Ok(())
                } else {
                    Err(HookError::Failed { script: label, code: exit_code })
                }
            }
            Err(e) => Err(HookError::Spawn(e.to_string())),
        },
        _ = tokio::time::sleep(Duration::from_secs(u64::from(timeout_s))) => {
            if let Some(pgid) = pgid {
                terminate(&mut child, pgid).await;
            }
            Err(HookError::Timeout { script: label, timeout_s })
        }
        _ = cancel.cancelled() => {
            if let Some(pgid) = pgid {
                terminate(&mut child, pgid).await;
            }
            Err(HookError::Cancelled)
        }
    };

    // A descendant that outlives the hook and keeps the pipes open must not
    // hold the job up; whatever was written by then is the output.
    let _ = tokio::time::timeout(READER_GRACE, async {
        for reader in &mut readers {
            let _ = reader.await;
        }
    })
    .await;
    for reader in &readers {
        reader.abort();
    }
    let tail = tail
        .lock()
        .map(|mut t| std::mem::take(&mut *t))
        .unwrap_or_default();
    HookResult {
        outcome,
        exit_code,
        output: tail.buf,
        truncated: tail.truncated,
    }
}
