//! A restore of a whole Proxmox VE container (ADR 0008 § Restore). The
//! agent fills a staging folder with the snapshot's `/fs` and `/config` as
//! the container sees them, filters the snapshot's configuration, and starts
//! `pct restore` with a job token; the Lodestore provider hands Proxmox VE
//! the filtered configuration and the staging folder over the agent socket.
//! The agent never stops or removes a guest itself: a replace goes through
//! `pct restore --force` and only on the checks below.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lodestore_format::ct::{IdKind, IdMap, MAX_CT_CONFIG_BYTES, config_lines};
use lodestore_format::guest_config::{
    FilterInput, Filtered, filter_ct, guest_identity, needs_identity_mismatch_flag, same_guest,
};
use lodestore_proto::agent::v1::{
    ConfigLine, DroppedConfigKey, ExpectedGuest, GuestRestoreResult, GuestTarget, JobFinished,
    JobOutcome, RestoreSpec,
};
use rustix::fs::{AtFlags, FileType, FlockOperation, Mode, OFlags, flock, openat, statat};
use tokio::process::Command;

use super::JobCtx;
use super::ct_backup::{Output, PveJobs, Refused, SOCKET_ENV, VMIDS, refuse, terminate};
use super::fetch::PackFetcher;
use super::restore::{PlanSource, Prepared, RestoreOptions, execute_prepared, stopped};
use crate::config::{AgentSettings, DEFAULT_GUEST_STAGING_DIR};
use crate::discovery::proxmox::{
    CtTargetFacts, ScanRoots, bridges, ct_target_facts, node_storages, parse_members,
    read_main_section, vmid_used,
};
use crate::discovery::{MAX_PVE_FILE_BYTES, open_nofollow, read_bounded};
use crate::pve::install::plugin_health;
use crate::pve::{STORAGE_ID, TOKEN_ENV};

/// The default staging folder below the agent's state folder.
pub const STAGING_DIR: &str = DEFAULT_GUEST_STAGING_DIR;
const LOCK_FILE: &str = ".lock";
/// The cluster file system and its database: staging there would fill both.
const FORBIDDEN_STAGING: [&str; 2] = ["/etc/pve", "/var/lib/pve-cluster"];
const POLL: Duration = Duration::from_millis(50);

pub(super) async fn run<P: PlanSource, F: PackFetcher>(
    ctx: &JobCtx,
    spec: &RestoreSpec,
    guest: &GuestTarget,
    plan: &mut P,
    fetch: &F,
    options: &RestoreOptions,
) -> JobFinished {
    let mut finished = match ctx.pve() {
        Some(pve) => match restore(ctx, pve, spec, guest, plan, fetch, options).await {
            Ok(done) | Err(done) => *done,
        },
        None => *refuse(
            "restore_failed",
            "this agent is not set up for Proxmox VE containers",
        ),
    };
    if !finished.error_code.is_empty() {
        finished
            .error_detail
            .insert("vmid".into(), guest.vmid.to_string());
    }
    finished
}

/// What the checks before the staging folder found.
struct Checked {
    node: String,
    roots: ScanRoots,
}

async fn restore<P: PlanSource, F: PackFetcher>(
    ctx: &JobCtx,
    pve: &PveJobs,
    spec: &RestoreSpec,
    guest: &GuestTarget,
    plan: &mut P,
    fetch: &F,
    options: &RestoreOptions,
) -> Result<Box<JobFinished>, Refused> {
    let settings = AgentSettings::load(ctx.paths()).restore;
    let checked = check(ctx, pve, spec, guest, settings.allow_guest_in_place)?;
    let base = staging_base(&settings.guest_staging_dir(ctx.paths()), &pve.pve_root)
        .map_err(|why| refuse("restore_staging_invalid", &why))?;
    let staging = Staging::create(&base).map_err(|e| {
        refuse(
            "restore_staging_invalid",
            &format!("cannot create a staging folder in {}: {e}", base.display()),
        )
    })?;
    let prepared = Prepared::staging(spec, staging.dir.clone(), options.space_reserve)
        .map_err(|a| refuse(a.code, &a.message))?;
    let mut finished = execute_prepared(ctx, &prepared, plan, fetch, options).await;
    let ok = [JobOutcome::Succeeded, JobOutcome::Warnings]
        .iter()
        .any(|o| *o as i32 == finished.outcome);
    if !ok {
        return Err(Box::new(finished));
    }
    // Warnings (owners as a regular user, skipped devices) leave a usable
    // copy; an entry that was not restored does not.
    if finished.errors_total > 0 {
        let mut refused = refuse(
            "restore_incomplete",
            &format!(
                "{} entries of the snapshot could not be restored into the staging folder. CT {} was not restored.",
                finished.errors_total, guest.vmid
            ),
        );
        refused.errors_total = finished.errors_total;
        refused.warnings_total = finished.warnings_total;
        refused.restore = finished.restore.take();
        return Err(refused);
    }
    let report = hand_over(ctx, pve, guest, &checked, &staging).await?;
    finished.restore.get_or_insert_with(Default::default).guest = Some(report);
    Ok(Box::new(finished))
}

/// The checks before anything is written: the target is on this node, the
/// VMID is free for a new container, or the container to replace may be
/// replaced, and the storage holds containers.
fn check(
    ctx: &JobCtx,
    pve: &PveJobs,
    spec: &RestoreSpec,
    guest: &GuestTarget,
    allow_replace: bool,
) -> Result<Checked, Refused> {
    if !spec.destination.is_empty() || spec.in_place {
        return Err(refuse(
            "restore_failed",
            "the restore spec is invalid: a container restore has no destination",
        ));
    }
    let vmid = guest.vmid;
    if !VMIDS.contains(&vmid) {
        return Err(refuse(
            "restore_failed",
            "the restore spec is invalid: the VMID is out of range",
        ));
    }
    let not_local = |text: String| refuse("guest_not_local", &text);
    let members = read_bounded(&pve.pve_root.join(".members"), MAX_PVE_FILE_BYTES)
        .map_err(|e| not_local(format!("this host is not a Proxmox VE node: .members: {e}")))?;
    let node = parse_members(&members).map_err(|e| not_local(e.0))?.node;
    if guest.node != node {
        return Err(not_local(format!(
            "The restore is for node {}, this is node {node}.",
            guest.node
        )));
    }
    if !plugin_health(&pve.pve_root, &pve.perl_root, &node).usable() {
        return Err(refuse(
            "guest_plugin_missing",
            &format!(
                "The Lodestore plugin on node {node} is missing or its files were changed: run lodestore-agent pve install on {node}."
            ),
        ));
    }
    let roots = pve.roots();
    let scan_failed = |e: crate::discovery::ScanFailed| refuse("restore_failed", &e.0);
    if guest.in_place {
        if !allow_replace {
            return Err(refuse(
                "guest_in_place_disabled",
                "Replacing containers is turned off on this node; set restore.allow_guest_in_place: true in agent.yaml.",
            ));
        }
        let facts = ct_target_facts(&roots, &ctx.paths().source_ids(), vmid)
            .map_err(scan_failed)?
            .ok_or_else(|| {
                refuse(
                    "guest_not_found",
                    &format!("CT {vmid} is not on node {node}."),
                )
            })?;
        check_live(&facts)?;
        if guest.expected.is_none() {
            return Err(target_changed(
                vmid,
                "the restore names no expected container",
            ));
        }
    } else if vmid_used(&roots, vmid).map_err(scan_failed)? || conf_path(pve, &node, vmid).exists()
    {
        return Err(refuse(
            "restore_vmid_taken",
            &format!("VMID {vmid} is taken."),
        ));
    }
    let holds_containers = node_storages(&roots, &node)
        .map_err(scan_failed)?
        .iter()
        .any(|s| s.id == guest.storage && s.content.iter().any(|c| c == "rootdir"));
    if !holds_containers {
        return Err(refuse(
            "restore_storage_invalid",
            &format!(
                "Storage {} on node {node} does not hold containers.",
                guest.storage
            ),
        ));
    }
    Ok(Checked { node, roots })
}

fn conf_path(pve: &PveJobs, node: &str, vmid: u32) -> PathBuf {
    pve.pve_root
        .join("nodes")
        .join(node)
        .join("lxc")
        .join(format!("{vmid}.conf"))
}

fn target_changed(vmid: u32, what: &str) -> Refused {
    refuse(
        "guest_target_changed",
        &format!(
            "CT {vmid} is not the container that was confirmed for the replace ({what}). Nothing was replaced; start the restore again."
        ),
    )
}

/// A container that runs or that HA manages is never replaced: the agent
/// does not stop it, and HA would start it right after the restore. Nor
/// is one with snapshots: `pct restore --force` deletes them and the
/// volumes only they use, which the confirmation does not show.
fn check_live(facts: &CtTargetFacts) -> Result<(), Refused> {
    let (vmid, name) = (facts.vmid, &facts.name);
    if facts.has_snapshots {
        return Err(refuse(
            "guest_target_has_snapshots",
            &format!(
                "CT {vmid} · {name} has snapshots, and replacing it would delete them. Remove its snapshots in Proxmox or restore as a new container."
            ),
        ));
    }
    if facts.running {
        return Err(refuse(
            "guest_running",
            &format!("CT {vmid} · {name} is running. Stop it in Proxmox, then try again."),
        ));
    }
    if facts.ha_managed {
        return Err(refuse(
            "guest_ha_managed",
            &format!(
                "CT {vmid} · {name} is managed by HA. Remove it from HA in Proxmox, then try again."
            ),
        ));
    }
    Ok(())
}

/// The fields in which the container differs from what the user confirmed.
fn differences(facts: &CtTargetFacts, expected: &ExpectedGuest) -> Vec<&'static str> {
    let volumes_same = !facts.volumes_truncated
        && facts.volumes.len() == expected.volumes.len()
        && facts
            .volumes
            .iter()
            .zip(&expected.volumes)
            .all(|((key, value), line)| *key == line.key && *value == line.value);
    [
        (facts.vmid == expected.vmid, "vmid"),
        (facts.name == expected.name, "name"),
        (facts.pve_uid == expected.pve_uid, "meta uid"),
        (facts.pve_rtime == expected.pve_rtime, "meta rtime"),
        (facts.instance_uuid == expected.instance_uuid, "instance"),
        (volumes_same, "volumes"),
    ]
    .into_iter()
    .filter_map(|(same, what)| (!same).then_some(what))
    .collect()
}

/// The configuration, the identity check and `pct restore`, once the
/// staging folder holds the snapshot.
async fn hand_over(
    ctx: &JobCtx,
    pve: &PveJobs,
    guest: &GuestTarget,
    checked: &Checked,
    staging: &Staging,
) -> Result<GuestRestoreResult, Refused> {
    let vmid = guest.vmid;
    let (config, firewall) = staged_config(&staging.dir)?;
    let target = if guest.in_place {
        Some(recheck_target(ctx, pve, guest, checked)?)
    } else {
        None
    };
    let mut cut = false;
    let bridges: BTreeSet<String> = bridges(&checked.roots, &mut cut).into_iter().collect();
    let filtered = filter_ct(&FilterInput {
        config: &config,
        firewall: firewall.as_deref(),
        target: target.as_ref().map(|(_, conf)| conf.as_slice()),
        bridges: Some(&bridges),
    })
    .map_err(|e| {
        refuse(
            "restore_config_invalid",
            &format!("the container configuration of the snapshot: {e}"),
        )
    })?;
    if !filtered.volumes.iter().any(|v| v.key == "rootfs") {
        return Err(refuse(
            "restore_config_invalid",
            "the container configuration of the snapshot has no usable rootfs",
        ));
    }
    let mismatch_flag = match &target {
        Some((facts, _)) => identity_flag(guest, &filtered, facts)?,
        None => false,
    };

    let mut args = vec![
        "restore".to_string(),
        vmid.to_string(),
        format!("{STORAGE_ID}:backup/restore/{}", staging.id),
    ];
    let replace = guest.in_place;
    let flag = if replace { "--force" } else { "--unique" };
    args.extend(
        [
            flag,
            "1",
            "--start",
            "0",
            "--unprivileged",
            "1",
            "--onboot",
            "0",
        ]
        .map(String::from),
    );
    args.extend(["--storage".to_string(), guest.storage.clone()]);
    if mismatch_flag {
        args.extend(["--allow-identity-mismatch", "1"].map(String::from));
    }
    pct_restore(ctx, pve, &args, &staging.id, &filtered, &staging.dir).await?;

    let created = std::fs::read(conf_path(pve, &checked.node, vmid))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| filtered.text.clone());
    let unprivileged = config_lines(&created).any(|(k, v)| k == "unprivileged" && v == "1");
    let root_host_uid = match IdMap::from_config(&created) {
        Ok(Some(map)) => map.to_host(IdKind::User, 0).unwrap_or(0),
        _ => 0,
    };
    Ok(GuestRestoreResult {
        vmid,
        node: checked.node.clone(),
        storage: guest.storage.clone(),
        replaced: replace,
        dropped: filtered
            .dropped
            .iter()
            .map(|d| DroppedConfigKey {
                key: d.key.clone(),
                value: d.value.clone(),
                reason: d.reason.as_str().to_string(),
            })
            .collect(),
        new_mac_nets: filtered
            .network
            .iter()
            .filter(|(_, value)| !value.split(',').any(|p| p.starts_with("hwaddr=")))
            .map(|(key, _)| key.clone())
            .collect(),
        unprivileged,
        root_host_uid,
        name: config_lines(&filtered.text)
            .find(|(k, _)| *k == "hostname")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default(),
        network: filtered
            .network
            .iter()
            .map(|(key, value)| ConfigLine {
                key: key.clone(),
                value: value.clone(),
            })
            .collect(),
    })
}

/// `config/pct.conf` and `config/pct.fw` of the staged snapshot, and its
/// `fs` a real folder: nothing on the way may be a symlink, or Proxmox VE
/// would read the node's files instead.
fn staged_config(dir: &Path) -> Result<(Vec<u8>, Option<Vec<u8>>), Refused> {
    let not_ct = |what: String| {
        refuse(
            "restore_snapshot_not_ct",
            &format!("the snapshot is not of a container: {what}"),
        )
    };
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(dir, flags | OFlags::DIRECTORY, Mode::empty()).map_err(|e| {
        refuse(
            "restore_failed",
            &format!("cannot open the staging folder: {e}"),
        )
    })?;
    let fs = openat(&root, "fs", flags | OFlags::DIRECTORY, Mode::empty())
        .map_err(|e| not_ct(format!("/fs is not a folder: {e}")))?;
    let has_etc = statat(&fs, "etc", AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory);
    if !has_etc {
        return Err(refuse(
            "restore_incomplete",
            "The container root of the snapshot has no /etc, so it was empty or not mounted when it was backed up. Nothing was restored.",
        ));
    }
    let config_dir = openat(&root, "config", flags | OFlags::DIRECTORY, Mode::empty())
        .map_err(|e| not_ct(format!("/config is not a folder: {e}")))?;
    let read = |name: &str| -> io::Result<Vec<u8>> {
        let fd = openat(&config_dir, name, flags, Mode::empty())?;
        let mut bytes = Vec::new();
        std::fs::File::from(fd)
            .take(MAX_CT_CONFIG_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_CT_CONFIG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("{name} is larger than {MAX_CT_CONFIG_BYTES} bytes"),
            ));
        }
        Ok(bytes)
    };
    let config = read("pct.conf").map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => not_ct("it has no /config/pct.conf".into()),
        _ => refuse(
            "restore_config_invalid",
            &format!("cannot read /config/pct.conf of the snapshot: {e}"),
        ),
    })?;
    let firewall = match read("pct.fw") {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(refuse(
                "restore_config_invalid",
                &format!("cannot read /config/pct.fw of the snapshot: {e}"),
            ));
        }
    };
    Ok((config, firewall))
}

/// Right before `pct` starts: the container to replace is still the one the
/// user confirmed, read by the same code that reported it, and still stopped
/// and outside HA. Returns it with the main section of its configuration.
fn recheck_target(
    ctx: &JobCtx,
    pve: &PveJobs,
    guest: &GuestTarget,
    checked: &Checked,
) -> Result<(CtTargetFacts, Vec<u8>), Refused> {
    let vmid = guest.vmid;
    let expected = guest
        .expected
        .as_ref()
        .ok_or_else(|| target_changed(vmid, "the restore names no expected container"))?;
    let facts = ct_target_facts(&checked.roots, &ctx.paths().source_ids(), vmid)
        .map_err(|e| refuse("restore_failed", &e.0))?
        .ok_or_else(|| target_changed(vmid, "it is gone"))?;
    let changed = differences(&facts, expected);
    if !changed.is_empty() {
        return Err(target_changed(
            vmid,
            &format!("changed: {}", changed.join(", ")),
        ));
    }
    check_live(&facts)?;
    let conf = open_nofollow(&conf_path(pve, &checked.node, vmid))
        .and_then(|f| read_main_section(io::BufReader::new(f), MAX_CT_CONFIG_BYTES as u64))
        .map_err(|e| target_changed(vmid, &format!("its configuration cannot be read: {e}")))?;
    Ok((facts, conf.into_bytes()))
}

/// The identity rule shared with the server (D-18): the same guest needs no
/// confirmation; another guest, or one without a uid, needs the user's
/// confirmation of exactly the pair of uids the agent sees now. Returns
/// whether `pct` needs `--allow-identity-mismatch`.
fn identity_flag(
    guest: &GuestTarget,
    filtered: &Filtered,
    facts: &CtTargetFacts,
) -> Result<bool, Refused> {
    let snapshot = guest_identity(filtered.text.as_bytes()).map(|i| i.uid);
    let snapshot = snapshot.as_deref();
    let target = Some(facts.pve_uid.as_str()).filter(|u| !u.is_empty());
    if !same_guest(snapshot, target) {
        let confirmed = guest.confirm_other_guest.as_ref().is_some_and(|c| {
            c.snapshot_pve_uid == snapshot.unwrap_or_default()
                && c.target_pve_uid == target.unwrap_or_default()
        });
        if !confirmed {
            return Err(refuse(
                "guest_identity_mismatch",
                &format!(
                    "The snapshot is of another container than CT {} · {}. Confirm replacing it with this snapshot, then try again.",
                    facts.vmid, facts.name
                ),
            ));
        }
    }
    Ok(needs_identity_mismatch_flag(snapshot, target))
}

/// Runs `pct` with `args` in its own process group and the job token; the
/// provider gets exactly the filtered configuration and `{staging}/fs`.
async fn pct_restore(
    ctx: &JobCtx,
    pve: &PveJobs,
    args: &[String],
    restore_id: &str,
    filtered: &Filtered,
    staging: &Path,
) -> Result<(), Refused> {
    let (token, _session) = pve.sessions.register_restore(
        restore_id,
        filtered.text.clone().into_bytes(),
        staging.join("fs"),
    );
    let mut command = Command::new(&pve.commands.pct);
    command
        .args(args)
        .env(TOKEN_ENV, token.expose())
        .env(SOCKET_ENV, &pve.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| {
        refuse(
            "pct_restore_failed",
            &format!("cannot start {}: {e}", pve.commands.pct.display()),
        )
    })?;
    let pid = child
        .id()
        .ok_or_else(|| refuse("pct_restore_failed", "pct exited at once"))?;
    pve.sessions.set_spawned_pid(&token, pid);
    let mut output = Output::watch(ctx, &mut child, &token, "pct_output");
    let flag = ctx.cancel_flag();
    let status = tokio::select! {
        status = child.wait() => Some(status),
        () = async {
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(POLL).await;
            }
        } => None,
    };
    let Some(status) = status else {
        terminate(&mut child, pid).await;
        output.finish().await;
        return Err(Box::new(stopped(ctx, JobFinished::default())));
    };
    let tail = output.finish().await;
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(refuse(
            "pct_restore_failed",
            &format!("pct restore failed ({status}): {tail}"),
        )),
        Err(e) => Err(refuse(
            "pct_restore_failed",
            &format!("waiting for pct: {e}"),
        )),
    }
}

/// `{base}/{id}`, 0700, with a `.lock` held for as long as the restore
/// runs. The lock's descriptor is inherited by `pct`, so a `pct` left over
/// from an agent that died keeps its folder; the folder is removed when the
/// restore ends, however it ends.
struct Staging {
    id: String,
    dir: PathBuf,
    _lock: OwnedFd,
}

impl Staging {
    fn create(base: &Path) -> io::Result<Staging> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(base)?;
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
        let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let dir = base.join(&id);
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let lock = rustix::fs::open(
            dir.join(LOCK_FILE),
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .and_then(|fd| flock(&fd, FlockOperation::LockExclusive).map(|()| fd));
        match lock {
            Ok(lock) => Ok(Staging {
                id,
                dir,
                _lock: lock,
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                Err(e.into())
            }
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.dir.display(), "cannot remove the staging folder: {e}");
        }
    }
}

fn is_restore_id(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The staging folder `base` as it resolves on this node: an absolute path
/// without `.` and `..`, not `/`, and not inside the cluster file system,
/// its database or `pve_root`.
pub fn staging_base(base: &Path, pve_root: &Path) -> Result<PathBuf, String> {
    let shown = base.display();
    let bytes = base.as_os_str().as_encoded_bytes();
    let plain = bytes.len() > 1
        && bytes[0] == b'/'
        && bytes[1..]
            .split(|b| *b == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b"..");
    if !plain {
        return Err(format!(
            "restore.guest_staging_dir {shown} must be an absolute path below / without \".\" or \"..\""
        ));
    }
    let resolved = resolve(base);
    let forbidden = FORBIDDEN_STAGING
        .iter()
        .map(PathBuf::from)
        .chain([pve_root.to_path_buf()]);
    for root in forbidden {
        if base.starts_with(&root) || resolved.starts_with(resolve(&root)) {
            return Err(format!(
                "restore.guest_staging_dir {shown} is inside {}; choose a folder on a local file system",
                root.display()
            ));
        }
    }
    Ok(resolved)
}

/// `path` with its longest existing part resolved through symlinks.
fn resolve(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut here = path;
    loop {
        if let Ok(real) = here.canonicalize() {
            return rest.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (here.parent(), here.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                here = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Removes the staging folders under `base` a restore left behind: those
/// whose `.lock` no live process holds. Only folders named like a restore
/// id are touched, and nothing is followed through a symlink.
pub fn clean_stale_staging(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let name = entry.file_name();
        if !is_dir || !name.to_str().is_some_and(is_restore_id) {
            continue;
        }
        let dir = entry.path();
        let lock = rustix::fs::open(
            dir.join(LOCK_FILE),
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        );
        let free = match lock {
            Ok(fd) => flock(&fd, FlockOperation::NonBlockingLockExclusive).is_ok(),
            Err(e) => e == rustix::io::Errno::NOENT,
        };
        if free && let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), "cannot remove a stale staging folder: {e}");
        }
    }
}

/// Cleans the configured staging folder at agent start; a setting that is
/// not valid is left alone, as a restore would refuse it.
pub(crate) fn clean_stale_staging_at_start(paths: &crate::Paths, pve_root: &Path) {
    let base = AgentSettings::load(paths).restore.guest_staging_dir(paths);
    if let Ok(base) = staging_base(&base, pve_root) {
        clean_stale_staging(&base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_base_rules() {
        let pve = Path::new("/nonexistent-pve");
        assert!(staging_base(Path::new("/var/lib/lodestore/pve-staging"), pve).is_ok());
        for bad in [
            "/etc/pve/x",
            "/etc/pve",
            "/var/lib/pve-cluster/db",
            "/nonexistent-pve/a",
            "rel",
            "/a/../b",
            "/a/./b",
            "/",
        ] {
            assert!(staging_base(Path::new(bad), pve).is_err(), "{bad}");
        }
    }

    #[test]
    fn restore_ids() {
        assert!(is_restore_id(&"ab".repeat(16)));
        assert!(!is_restore_id(&"AB".repeat(16)));
        assert!(!is_restore_id("lost+found"));
    }
}
