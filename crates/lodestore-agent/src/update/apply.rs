use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use data_encoding::HEXLOWER;
use lodestore_proto::agent::v1::UpdateAssign;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use super::minisign::{Digest512, Signature, TrustedRelease, verify};
use super::switch::GuardCommand;
use super::trust::trusted_keys;
use super::{MAX_UPDATE_BYTES, Pending, ROLLBACK_AFTER, UpdateOutcome, UpdateRuntime};
use crate::Paths;
use crate::identity::{sync_dir, write_json_file};

const PENDING_FILE: &str = "pending.json";
const ROLLBACK_CLAIM_FILE: &str = "pending.rollback";
const TEMP_NAME: &str = ".lodestore-agent.new";
const UNIT_ID_LEN: usize = 8;
const CHUNK: usize = 64 * 1024;

/// A name no other download used, for a file created with `create_new`.
pub fn temp_path(exe: &Path) -> Result<PathBuf, getrandom::Error> {
    let mut id = [0u8; 8];
    getrandom::fill(&mut id)?;
    Ok(exe.with_file_name(format!("{TEMP_NAME}.{}", HEXLOWER.encode(&id))))
}

/// Every download file next to `exe`.
pub fn temp_files(exe: &Path) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(exe.parent().unwrap_or(Path::new("/"))) else {
        return Vec::new();
    };
    dir.flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_NAME))
        .map(|entry| entry.path())
        .collect()
}

pub fn prev_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".prev");
    exe.with_file_name(name)
}

pub fn pending_path(paths: &Paths) -> PathBuf {
    paths.update_dir().join(PENDING_FILE)
}

pub fn rollback_claim_path(paths: &Paths) -> PathBuf {
    paths.update_dir().join(ROLLBACK_CLAIM_FILE)
}

pub async fn install_files(
    paths: &Paths,
    runtime: &UpdateRuntime,
    assign: &UpdateAssign,
    bin: &Path,
    sig: &[u8],
    busy: &(dyn Fn() -> bool + Sync),
) -> UpdateOutcome {
    let outcome = run(paths, runtime, assign, bin, sig, busy).await;
    if outcome != UpdateOutcome::Switched {
        let _ = tokio::fs::remove_file(bin).await;
    }
    outcome
}

async fn run(
    paths: &Paths,
    runtime: &UpdateRuntime,
    assign: &UpdateAssign,
    bin: &Path,
    sig: &[u8],
    busy: &(dyn Fn() -> bool + Sync),
) -> UpdateOutcome {
    if !runtime.enabled {
        return reject("Self-update is not available on this host.");
    }
    let Some(unit) = guard_unit(&assign.update_id) else {
        return reject("The update id is malformed.");
    };
    if bin.parent() != runtime.exe.parent() {
        return reject("The downloaded file is not next to the agent binary.");
    }
    if let Err(reason) = check(paths, runtime, assign, bin, sig).await {
        return UpdateOutcome::Rejected(reason);
    }
    if busy() {
        return UpdateOutcome::Busy;
    }
    switch(paths, runtime, assign, bin, unit).await
}

fn reject(reason: &str) -> UpdateOutcome {
    UpdateOutcome::Rejected(reason.to_string())
}

fn guard_unit(update_id: &str) -> Option<String> {
    let id = update_id.get(..UNIT_ID_LEN)?;
    id.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        .then(|| format!("lodestore-agent-update-{id}"))
}

async fn check(
    paths: &Paths,
    runtime: &UpdateRuntime,
    assign: &UpdateAssign,
    bin: &Path,
    sig: &[u8],
) -> Result<(), String> {
    let limit = assign.size.min(MAX_UPDATE_BYTES);
    let (sha256, blake2) = {
        let bin = bin.to_path_buf();
        tokio::task::spawn_blocking(move || hash_file(&bin, limit))
            .await
            .map_err(|e| e.to_string())??
    };
    if !sha256.eq_ignore_ascii_case(&assign.sha256) {
        return Err("The download does not match its checksum.".into());
    }

    let text = std::str::from_utf8(sig)
        .map_err(|_| "Signature check failed: signature file is not text.".to_string())?;
    let signature = Signature::parse(text).map_err(|e| e.to_string())?;
    let comment = verify(&trusted_keys(paths), &blake2, &signature).map_err(|e| e.to_string())?;
    let release = TrustedRelease::parse(comment).map_err(|e| e.to_string())?;

    if release.version != assign.target_version {
        return Err(format!(
            "The signed version {} is not the requested {}.",
            release.version, assign.target_version
        ));
    }
    if release.arch != std::env::consts::ARCH {
        return Err(format!(
            "The binary is built for {}, this host is {}.",
            release.arch,
            std::env::consts::ARCH
        ));
    }
    let new = semver::Version::parse(&release.version)
        .map_err(|_| format!("Version {} is not a valid version.", release.version))?;
    let current = semver::Version::parse(&runtime.version).map_err(|_| {
        format!(
            "Running version {} is not a valid version.",
            runtime.version
        )
    })?;
    if new <= current {
        return Err(format!("Version {new} is not newer than {current}."));
    }

    run_version(bin, &release.version).await
}

fn hash_file(path: &Path, limit: u64) -> Result<(String, [u8; 64]), String> {
    let io = |e: std::io::Error| format!("The download cannot be read: {e}");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err("The download is not a regular file.".into());
    }
    if meta.len() > limit {
        return Err("The download is larger than announced.".into());
    }
    let mut sha = Sha256::new();
    let mut blake = Digest512::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = file
            .by_ref()
            .take(limit + 1 - total)
            .read(&mut buf)
            .map_err(io)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            return Err("The download is larger than announced.".into());
        }
        let chunk = buf.get(..n).unwrap_or_default();
        sha.update(chunk);
        blake.update(chunk);
    }
    Ok((HEXLOWER.encode(&sha.finalize()), blake.finalize()))
}

async fn run_version(bin: &Path, version: &str) -> Result<(), String> {
    let fail = |why: String| format!("The new binary did not start: {why}");
    std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| fail(e.to_string()))?;
    let output = tokio::time::timeout(
        super::VERSION_TIMEOUT,
        Command::new(bin)
            .arg("version")
            .env_clear()
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| fail("it did not answer in time".into()))?
    .map_err(|e| fail(e.to_string()))?;
    if !output.status.success() {
        return Err(fail(format!("exit status {}", output.status)));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    if printed.trim_end() != format!("lodestore-agent {version}") {
        return Err(fail("it reported a different version".into()));
    }
    Ok(())
}

async fn switch(
    paths: &Paths,
    runtime: &UpdateRuntime,
    assign: &UpdateAssign,
    bin: &Path,
    unit: String,
) -> UpdateOutcome {
    let exe = &runtime.exe;
    let prev = prev_path(exe);
    let pending = pending_path(paths);
    if pending.exists() || rollback_claim_path(paths).exists() {
        return reject("An earlier update has not finished yet.");
    }
    let fail = |what: &str, e: &dyn std::fmt::Display| {
        UpdateOutcome::Rejected(format!("Could not {what}: {e}"))
    };

    let record = Pending {
        update_id: assign.update_id.clone(),
        target_version: assign.target_version.clone(),
        previous_version: runtime.version.clone(),
        deadline_ms: now_ms() + ROLLBACK_AFTER.as_millis() as u64,
        guard_unit: unit.clone(),
    };
    if let Err(e) = write_json_file(&pending, &record) {
        return fail("record the update", &e);
    }
    let undo_pending = || {
        let _ = std::fs::remove_file(&pending);
    };

    if let Err(e) = remove_if_exists(&prev).and_then(|()| std::fs::hard_link(exe, &prev)) {
        undo_pending();
        return fail("keep the previous binary", &e);
    }
    let switched = std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))
        .and_then(|()| std::fs::rename(bin, exe))
        .and_then(|()| sync_parent(exe));
    if let Err(e) = switched {
        let _ = std::fs::rename(&prev, exe);
        undo_pending();
        return fail("replace the agent binary", &e);
    }

    let guard = GuardCommand {
        prev: prev.clone(),
        exe: exe.clone(),
        config_dir: paths.config_dir.clone(),
        state_dir: paths.state_dir.clone(),
    };
    let switcher = runtime.switcher.clone();
    let armed = tokio::task::spawn_blocking(move || switcher.arm(&unit, &guard, ROLLBACK_AFTER))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
    if let Err(e) = armed {
        let _ = std::fs::rename(&prev, exe);
        let _ = sync_parent(exe);
        undo_pending();
        return UpdateOutcome::Rejected(format!("Could not schedule the rollback guard: {e}"));
    }
    UpdateOutcome::Switched
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn sync_parent(exe: &Path) -> std::io::Result<()> {
    sync_dir(exe.parent().unwrap_or(Path::new("/")))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
