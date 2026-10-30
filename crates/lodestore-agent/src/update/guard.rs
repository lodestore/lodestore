use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use lodestore_proto::agent::v1::{UpdateState, UpdateStatus};
use serde::{Deserialize, Serialize};

use super::apply::{pending_path, prev_path, rollback_claim_path, temp_files};
use super::switch::{GuardCommand, Switcher};
use super::{Pending, ROLLBACK_AFTER, UpdateRuntime};
use crate::Paths;
use crate::identity::{sync_dir, write_json_file};

const RESULT_FILE: &str = "result.json";
const PREVIOUS_GONE: &str =
    "The previous binary is gone, so the update was not rolled back. Run install.sh on the host.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackResult {
    pub update_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    Continue,
    Restart,
}

fn result_path(paths: &Paths) -> std::path::PathBuf {
    paths.update_dir().join(RESULT_FILE)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| path.display().to_string())?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

fn read_pending(paths: &Paths) -> anyhow::Result<Option<Pending>> {
    read_json(&pending_path(paths))
}

enum Restored {
    Nothing,
    Back(Pending),
    Gone(Pending),
}

/// Past the rename the binary is back, so a failure here must not stop the
/// caller from restarting the service. The claim stays and the next start
/// records the result.
fn record_restore(
    paths: &Paths,
    exe: &Path,
    claim: &Path,
    result: &RollbackResult,
) -> anyhow::Result<()> {
    sync_dir(exe.parent().unwrap_or(Path::new("/")))?;
    write_json_file(&result_path(paths), result)?;
    std::fs::remove_file(claim)?;
    Ok(())
}

/// Moves `pending.json` aside, which claims the update: whoever does that or
/// removes the file first wins, so a rollback and a confirmation exclude each
/// other. Then puts `<exe>.prev` back and records why, without restarting
/// anything. A claim left by a crash is finished here.
fn restore(
    paths: &Paths,
    exe: &Path,
    reason: impl FnOnce(&Pending) -> String,
) -> anyhow::Result<Restored> {
    let claim = rollback_claim_path(paths);
    let resumed = claim.exists();
    if !resumed {
        match std::fs::rename(pending_path(paths), &claim) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Restored::Nothing),
            Err(e) => return Err(e).with_context(|| format!("claim {}", claim.display())),
        }
    }
    let Some(pending) = read_json::<Pending>(&claim)? else {
        return Ok(Restored::Nothing);
    };
    let prev = prev_path(exe);
    let back = match std::fs::rename(&prev, exe) {
        Ok(()) => true,
        // a crash after this rename leaves the claim and no .prev
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => resumed,
        Err(e) => return Err(e).with_context(|| format!("restore {}", prev.display())),
    };
    let result = RollbackResult {
        update_id: pending.update_id.clone(),
        reason: if back {
            reason(&pending)
        } else {
            PREVIOUS_GONE.to_string()
        },
    };
    if let Err(error) = record_restore(paths, exe, &claim, &result) {
        tracing::warn!(%error, "the rollback could not be recorded");
    }
    Ok(if back {
        Restored::Back(pending)
    } else {
        Restored::Gone(pending)
    })
}

/// Runs from `<exe>.prev` through the transient timer: returns the previous
/// binary and restarts the service. Does nothing when no update is pending.
pub fn rollback(
    paths: &Paths,
    exe: &Path,
    switcher: &dyn Switcher,
    reason: impl FnOnce(&Pending) -> String,
) -> anyhow::Result<bool> {
    match restore(paths, exe, reason)? {
        Restored::Back(_) => {
            switcher.restart_service()?;
            Ok(true)
        }
        Restored::Nothing | Restored::Gone(_) => Ok(false),
    }
}

fn self_rollback(
    paths: &Paths,
    runtime: &UpdateRuntime,
    reason: impl FnOnce(&Pending) -> String,
) -> anyhow::Result<StartAction> {
    match restore(paths, &runtime.exe, reason)? {
        Restored::Back(pending) => {
            runtime.switcher.disarm(&pending.guard_unit);
            // Already the restored binary: restarting would loop while the result cannot be written.
            Ok(if runtime.version == pending.previous_version {
                StartAction::Continue
            } else {
                StartAction::Restart
            })
        }
        Restored::Gone(pending) => {
            runtime.switcher.disarm(&pending.guard_unit);
            Ok(StartAction::Continue)
        }
        Restored::Nothing => Ok(StartAction::Continue),
    }
}

/// Called once at process start. `Restart` means the previous binary is back
/// and the process must exit with code 0 so systemd starts it again.
pub fn on_start(
    paths: &Paths,
    runtime: &UpdateRuntime,
    now_ms: u64,
) -> anyhow::Result<StartAction> {
    for temp in temp_files(&runtime.exe) {
        if let Err(e) = std::fs::remove_file(temp) {
            tracing::warn!(error = %e, "could not remove a leftover update download");
        }
    }
    if rollback_claim_path(paths).exists() {
        return self_rollback(paths, runtime, |p| {
            format!(
                "The rollback was interrupted and finished at start. Running {} again.",
                p.previous_version
            )
        });
    }
    let Some(pending) = read_pending(paths)? else {
        return Ok(StartAction::Continue);
    };
    // someone replaced the binary by hand: there is nothing here to undo
    if pending.target_version != runtime.version {
        std::fs::remove_file(pending_path(paths))?;
        runtime.switcher.disarm(&pending.guard_unit);
        return Ok(StartAction::Continue);
    }
    if pending.deadline_ms <= now_ms {
        return self_rollback(paths, runtime, |p| {
            format!(
                "The host restarted during the update. Running {} again.",
                p.previous_version
            )
        });
    }
    if runtime.switcher.is_armed(&pending.guard_unit) {
        return Ok(StartAction::Continue);
    }
    let guard = GuardCommand {
        prev: prev_path(&runtime.exe),
        exe: runtime.exe.clone(),
        config_dir: paths.config_dir.clone(),
        state_dir: paths.state_dir.clone(),
    };
    let remaining = Duration::from_millis(pending.deadline_ms - now_ms).min(ROLLBACK_AFTER);
    match runtime.switcher.arm(&pending.guard_unit, &guard, remaining) {
        Ok(()) => Ok(StartAction::Continue),
        Err(e) => self_rollback(paths, runtime, |_| {
            format!("Could not schedule the rollback guard after a restart: {e}")
        }),
    }
}

/// Called after every `HelloAck`. Only the call that removes `pending.json`
/// confirms the update: a guard that claimed it first is restoring the old
/// binary and one that fires later finds nothing to do.
pub fn after_connect(paths: &Paths, runtime: &UpdateRuntime) -> Option<UpdateStatus> {
    match read_pending(paths) {
        Ok(Some(pending)) if pending.target_version == runtime.version => {
            match std::fs::remove_file(pending_path(paths)) {
                Ok(()) => {
                    let _ = sync_dir(&paths.update_dir());
                    runtime.switcher.disarm(&pending.guard_unit);
                    return Some(status(
                        pending.update_id,
                        UpdateState::Confirmed,
                        String::new(),
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, "could not remove pending.json"),
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "unreadable pending.json"),
    }
    rolled_back(paths).map(|r| status(r.update_id, UpdateState::RolledBack, r.reason))
}

fn status(update_id: String, state: UpdateState, reason: String) -> UpdateStatus {
    UpdateStatus {
        update_id,
        state: state.into(),
        reason,
    }
}

pub fn rolled_back(paths: &Paths) -> Option<RollbackResult> {
    read_json(&result_path(paths)).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "unreadable result.json");
        None
    })
}

/// Drops the rollback mark once the server has acknowledged it.
pub fn clear_rollback(paths: &Paths, update_id: &str) -> anyhow::Result<()> {
    if rolled_back(paths).is_some_and(|r| r.update_id == update_id) {
        std::fs::remove_file(result_path(paths))?;
    }
    Ok(())
}
