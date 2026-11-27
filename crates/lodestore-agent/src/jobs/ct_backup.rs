//! A backup of one Proxmox VE container (ADR 0008). The agent starts vzdump
//! with a job token, the Lodestore provider hands the container over the
//! agent socket, and the agent walks the container's tree as the provider's
//! process sees it. The snapshot is written only when the provider reports
//! success and vzdump exits cleanly.

use std::collections::VecDeque;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lodestore_format::ct::{CtMeta, CtMode, config_lines};
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::source::{MAX_META_VALUE_LEN, Meta, SourceKey, SourceType};
use lodestore_format::writer::{
    BackupOptions, BackupProgress, CtBackup, Deadline, IssueEvent, PendingContainer, Writer,
};
use lodestore_proto::agent::v1::{JobEventLevel, JobFinished, JobSpec};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;

use super::runner::{
    backup_error, gc_locks_gate, interrupted, open_writer_with, read_write_keys, record_progress,
    report_issue, time_limit, written,
};
use super::throttle::Throttled;
use super::{JobCtx, distrusted, failed};
use crate::discovery::proxmox::{lookup_ct, parse_members};
use crate::discovery::{MAX_PVE_FILE_BYTES, read_bounded};
use crate::pve::install::plugin_health;
use crate::pve::socket::{BackupEvent, BackupSession, Info, PveSessions, Token};
use crate::pve::{STORAGE_ID, TOKEN_ENV};
use lodestore_format::store::Store;

const VMIDS: std::ops::RangeInclusive<u32> = 100..=999_999_999;
const SOCKET_ENV: &str = "LODESTORE_AGENT_SOCKET";
const MODE_WAIT: Duration = Duration::from_secs(10);
const TERM_WAIT: Duration = Duration::from_secs(30);
const OUTPUT_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);
const OUTPUT_EVENTS: usize = 500;
const TAIL_LINES: usize = 10;
const MAX_LINE: usize = 1024;

/// The Proxmox VE programs the agent runs; tests put wrappers in their place.
#[derive(Debug, Clone)]
pub struct PveCommands {
    pub vzdump: PathBuf,
    pub pct: PathBuf,
}

impl Default for PveCommands {
    fn default() -> Self {
        PveCommands {
            vzdump: PathBuf::from("/usr/bin/vzdump"),
            pct: PathBuf::from("/usr/sbin/pct"),
        }
    }
}

/// What container jobs need on a Proxmox VE node.
pub struct PveJobs {
    /// The sessions the agent socket serves the provider from.
    pub sessions: Arc<PveSessions>,
    pub commands: PveCommands,
    pub pve_root: PathBuf,
    pub perl_root: PathBuf,
    /// The agent socket, handed to the provider through vzdump's environment.
    pub socket: PathBuf,
}

/// The mode of a `backup mode: {mode}` line of vzdump's output.
pub fn parse_backup_mode(line: &str) -> Option<CtMode> {
    let (_, mode) = line.split_once("INFO: backup mode: ")?;
    CtMode::parse(mode.trim_end())
}

/// The excludes vzdump passed, matched as its tar matches them, and a
/// warning for each pattern that tar never matches.
pub fn translate_excludes(patterns: &[String]) -> (PathMatcher, Vec<String>) {
    let warnings = patterns
        .iter()
        .filter(|p| p.ends_with('/'))
        .map(|p| format!("the exclude pattern {p:?} ends with / and matches nothing, as in vzdump"))
        .collect();
    (PathMatcher::tar_excludes(patterns), warnings)
}

struct Target {
    vmid: u32,
    key: SourceKey,
    /// The `meta uid` recorded with the instance; empty for none.
    recorded_uid: String,
}

/// A job that ends before or instead of a snapshot.
type Refused = Box<JobFinished>;

fn refuse(code: &str, text: &str) -> Refused {
    Box::new(failed(code, text))
}

fn identity_changed(vmid: u32) -> Refused {
    refuse(
        "guest_identity_changed",
        &format!(
            "Container {vmid} is not the instance this job backs up: it was replaced or restored as another container. No snapshot was written; the next scan records it as a new instance."
        ),
    )
}

fn mode_mismatch(text: &str) -> Refused {
    refuse("ct_mode_mismatch", text)
}

/// The checks before vzdump starts: the job names a container instance, the
/// container is on this node, it is the instance the job names and the
/// plugin of this node is usable.
fn target(ctx: &JobCtx, pve: &PveJobs, spec: &JobSpec) -> Result<Target, Refused> {
    let invalid = |text: &str| refuse("source_invalid", text);
    let instance = spec
        .instance
        .as_ref()
        .ok_or_else(|| invalid("the job names no container instance"))?;
    let key = SourceKey::parse(&instance.key)
        .ok()
        .filter(|k| k.kind() == SourceType::ProxmoxCt)
        .ok_or_else(|| invalid("the job's instance key is not a proxmox-ct key"))?;
    let vmid = instance.vmid;
    if !VMIDS.contains(&vmid) {
        return Err(invalid("the job's VMID is not valid"));
    }
    let not_local = |text: String| refuse("guest_not_local", &text);
    let members = read_bounded(&pve.pve_root.join(".members"), MAX_PVE_FILE_BYTES)
        .map_err(|e| not_local(format!("this host is not a Proxmox VE node: .members: {e}")))?;
    let node = parse_members(&members).map_err(|e| not_local(e.0))?.node;
    let conf = pve
        .pve_root
        .join("nodes")
        .join(&node)
        .join("lxc")
        .join(format!("{vmid}.conf"));
    if !std::fs::symlink_metadata(&conf).is_ok_and(|m| m.is_file()) {
        return Err(not_local(format!(
            "Container {vmid} is not on this node ({node})."
        )));
    }
    let uuid = key.as_str().rsplit('/').next().unwrap_or_default();
    let recorded = lookup_ct(&ctx.paths().source_ids(), vmid)
        .map_err(|e| refuse("backup_failed", &e.0))?
        .filter(|id| id.uuid == uuid)
        .ok_or_else(|| identity_changed(vmid))?;
    if !plugin_health(&pve.pve_root, &pve.perl_root, &node).usable() {
        return Err(refuse(
            "guest_plugin_missing",
            &format!(
                "The Lodestore plugin on node {node} is missing or its files were changed: run lodestore-agent pve install on {node}."
            ),
        ));
    }
    Ok(Target {
        vmid,
        key,
        recorded_uid: recorded.pve_uid,
    })
}

pub(super) async fn run(ctx: &JobCtx, spec: &JobSpec, deadline: Deadline) -> JobFinished {
    let Some(pve) = ctx.pve() else {
        return failed(
            "source_invalid",
            "this agent is not set up for Proxmox VE containers",
        );
    };
    let target = match target(ctx, pve, spec) {
        Ok(target) => target,
        Err(refused) => return *refused,
    };
    let vmid = target.vmid;
    let mut finished = backup(ctx, pve, spec, target, deadline).await;
    if !finished.error_code.is_empty() {
        finished
            .error_detail
            .insert("vmid".into(), vmid.to_string());
    }
    finished
}

async fn backup(
    ctx: &JobCtx,
    pve: &PveJobs,
    spec: &JobSpec,
    target: Target,
    deadline: Deadline,
) -> JobFinished {
    let keys = match read_write_keys(ctx.paths()) {
        Ok(keys) => keys,
        Err(e) => return failed("repository_unavailable", &e),
    };
    distrusted::record_or_warn(ctx.paths(), &spec.distrusted_shards);
    let rate = spec.limits.as_ref().map(|l| l.upload_rate_bps);
    ctx.progress_cell()
        .upload_limit
        .store(rate.unwrap_or(0), Ordering::Relaxed);
    let uploaded = Arc::clone(&ctx.progress_cell().bytes_stored);
    let writer = match open_writer_with(ctx.paths(), keys, rate, uploaded).await {
        Ok(writer) => writer,
        Err(e) => return failed("repository_unavailable", &e),
    };
    if let Some(refused) = gc_locks_gate(writer.gc_locks_denied(), spec.gc_locks_required) {
        return refused;
    }

    let (token, session) = pve.sessions.register_backup(target.vmid);
    let mut command = Command::new(&pve.commands.vzdump);
    command
        .arg(target.vmid.to_string())
        .args(["--storage", STORAGE_ID, "--mode", "snapshot"])
        .args(["--remove", "0", "--protected", "0"])
        .env(TOKEN_ENV, token.expose())
        .env(SOCKET_ENV, &pve.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return failed(
                "vzdump_failed",
                &format!("cannot start {}: {e}", pve.commands.vzdump.display()),
            );
        }
    };
    let Some(pid) = child.id() else {
        return failed("vzdump_failed", "vzdump exited at once");
    };
    pve.sessions.set_spawned_pid(&token, pid);
    let output = Output::watch(ctx, &mut child, &token);
    Run {
        ctx,
        spec,
        target,
        deadline,
        writer,
        session,
        child,
        pid,
        output,
    }
    .drive()
    .await
}

/// What vzdump prints: each line becomes a job event (up to a limit, without
/// the token), the first `backup mode:` line is the mode and the last lines
/// of stderr explain a failure.
struct Output {
    mode: watch::Receiver<Option<CtMode>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

impl Output {
    fn watch(ctx: &JobCtx, child: &mut Child, token: &Token) -> Output {
        let (mode, seen) = watch::channel(None);
        let mode = Arc::new(mode);
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let events = Arc::new(AtomicUsize::new(0));
        let mut readers = Vec::new();
        let streams: [Option<Box<dyn AsyncRead + Send + Unpin>>; 2] = [
            child.stdout.take().map(|s| Box::new(s) as _),
            child.stderr.take().map(|s| Box::new(s) as _),
        ];
        for (stream, is_stderr) in streams.into_iter().zip([false, true]) {
            let Some(stream) = stream else { continue };
            let lines = Lines {
                ctx: ctx.clone(),
                token: token.expose().to_string(),
                mode: Arc::clone(&mode),
                tail: is_stderr.then(|| Arc::clone(&tail)),
                events: Arc::clone(&events),
            };
            readers.push(tokio::spawn(lines.read(stream)));
        }
        Output {
            mode: seen,
            tail,
            readers,
        }
    }

    /// The mode vzdump printed; it prints it before the provider is called,
    /// so a short wait covers a line still in the pipe.
    async fn mode(&mut self) -> Option<CtMode> {
        let seen = tokio::time::timeout(MODE_WAIT, self.mode.wait_for(Option::is_some)).await;
        seen.ok()?.ok().and_then(|m| *m)
    }

    async fn finish(&mut self) -> String {
        for reader in self.readers.drain(..) {
            let _ = tokio::time::timeout(OUTPUT_WAIT, reader).await;
        }
        let tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        tail.iter().cloned().collect::<Vec<_>>().join("; ")
    }
}

struct Lines {
    ctx: JobCtx,
    token: String,
    mode: Arc<watch::Sender<Option<CtMode>>>,
    tail: Option<Arc<Mutex<VecDeque<String>>>>,
    events: Arc<AtomicUsize>,
}

impl Lines {
    async fn read(self, stream: Box<dyn AsyncRead + Send + Unpin>) {
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            bytes.truncate(MAX_LINE);
            let text = String::from_utf8_lossy(&bytes);
            let line = text.trim_end().replace(&self.token, "[token]");
            if line.is_empty() {
                continue;
            }
            if let Some(mode) = parse_backup_mode(&line) {
                self.mode
                    .send_if_modified(|m| m.is_none() && m.replace(mode).is_none());
            }
            if let Some(tail) = &self.tail {
                let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line.clone());
            }
            if self.events.fetch_add(1, Ordering::Relaxed) < OUTPUT_EVENTS {
                self.ctx
                    .event(JobEventLevel::Info, "vzdump_output", b"", "", &line);
            }
        }
    }
}

enum Wake {
    Event(Option<BackupEvent>),
    Exited(io::Result<ExitStatus>),
    Stop,
}

struct Run<'a> {
    ctx: &'a JobCtx,
    spec: &'a JobSpec,
    target: Target,
    deadline: Deadline,
    writer: Writer<Throttled<Store>>,
    session: BackupSession,
    child: Child,
    pid: u32,
    output: Output,
}

impl Run<'_> {
    fn stop_requested(&self) -> bool {
        self.ctx.cancel_flag().load(Ordering::Relaxed) || self.deadline.expired()
    }

    async fn next(&mut self, events_open: bool) -> Wake {
        let flag = self.ctx.cancel_flag();
        let deadline = self.deadline;
        tokio::select! {
            event = self.session.events.recv(), if events_open => Wake::Event(event),
            status = self.child.wait() => Wake::Exited(status),
            () = async {
                while !flag.load(Ordering::Relaxed) && !deadline.expired() {
                    tokio::time::sleep(POLL).await;
                }
            } => Wake::Stop,
        }
    }

    async fn drive(mut self) -> JobFinished {
        let mut refused: Option<JobFinished> = None;
        let mut opened: Option<(OwnedFd, Info)> = None;
        let mut pending: Option<PendingContainer> = None;
        let mut reported_success = false;
        let mut events_open = true;
        let status = loop {
            match self.next(events_open).await {
                Wake::Event(Some(BackupEvent::Open {
                    info,
                    peer_pid,
                    reply,
                })) => match self.open(&info, peer_pid) {
                    Ok(root) => {
                        opened = Some((root, info));
                        let _ = reply.send(Ok(()));
                    }
                    Err(f) => {
                        let _ = reply.send(Err(f.error_message.clone()));
                        refused.get_or_insert(*f);
                    }
                },
                Wake::Event(Some(BackupEvent::Data {
                    config,
                    excludes,
                    reply,
                })) => {
                    let result = match &opened {
                        Some((root, info)) => self.data(root, info, config, excludes).await,
                        None => Err(refuse("vzdump_failed", "the data came before the open")),
                    };
                    match result {
                        Ok(walked) => {
                            pending = Some(walked);
                            let _ = reply.send(Ok(()));
                        }
                        Err(f) => {
                            let _ = reply.send(Err(f.error_message.clone()));
                            refused.get_or_insert(*f);
                        }
                    }
                    if self.stop_requested() {
                        return self.stop().await;
                    }
                }
                Wake::Event(Some(BackupEvent::Finish {
                    success,
                    error,
                    reply,
                })) => {
                    let size = pending.as_ref().map(|p| p.stats().bytes_total);
                    match (success, size) {
                        (true, Some(size)) => {
                            reported_success = true;
                            let _ = reply.send(Ok(size));
                        }
                        _ => {
                            pending = None;
                            let _ = reply.send(Ok(0));
                            let text = error.unwrap_or_else(|| "the backup failed".into());
                            refused.get_or_insert_with(|| {
                                failed("vzdump_failed", &format!("vzdump: {text}"))
                            });
                        }
                    }
                }
                Wake::Event(Some(BackupEvent::Aborted)) => pending = None,
                Wake::Event(None) => events_open = false,
                Wake::Exited(status) => break status,
                Wake::Stop => return self.stop().await,
            }
        };
        let tail = self.output.finish().await;
        if let Some(refused) = refused {
            return refused;
        }
        match (status, pending) {
            (Ok(status), Some(pending)) if status.success() && reported_success => {
                match pending.commit(&mut self.writer).await {
                    Ok(outcome) => written(self.ctx, &outcome),
                    Err(e) => backup_error(self.ctx, e),
                }
            }
            (Ok(status), _) => failed(
                "vzdump_failed",
                &format!("vzdump failed ({status}), no snapshot was written: {tail}"),
            ),
            (Err(e), _) => failed("vzdump_failed", &format!("waiting for vzdump: {e}")),
        }
    }

    /// Stops vzdump and everything it started: SIGTERM to its process
    /// group, SIGKILL if it is still there after a while.
    async fn stop(mut self) -> JobFinished {
        use rustix::process::{Pid, Signal, kill_process_group};
        let group = i32::try_from(self.pid).ok().and_then(Pid::from_raw);
        if let Some(group) = group {
            let _ = kill_process_group(group, Signal::TERM);
        }
        if tokio::time::timeout(TERM_WAIT, self.child.wait())
            .await
            .is_err()
        {
            if let Some(group) = group {
                let _ = kill_process_group(group, Signal::KILL);
            }
            let _ = self.child.wait().await;
        }
        self.output.finish().await;
        if !self.ctx.cancel_flag().load(Ordering::Relaxed) && self.deadline.expired() {
            time_limit(self.deadline.limit().as_secs())
        } else {
            interrupted(self.ctx)
        }
    }

    /// The open of the provider: the container's `meta uid` must be the one
    /// recorded with the instance (ADR 0008 § Identity), and the directory
    /// is opened once, through the root of the peer's process, while the
    /// peer waits for this answer.
    fn open(&self, info: &Info, peer_pid: u32) -> Result<OwnedFd, Refused> {
        if info.guest_uid.as_deref().unwrap_or_default() != self.target.recorded_uid {
            return Err(identity_changed(self.target.vmid));
        }
        open_container(peer_pid, &info.directory).map_err(|e| {
            refuse(
                "source_invalid",
                &format!(
                    "cannot open the container directory {} of vzdump: {e}",
                    info.directory
                ),
            )
        })
    }

    async fn data(
        &mut self,
        root: &OwnedFd,
        info: &Info,
        config: Vec<u8>,
        excludes: Vec<String>,
    ) -> Result<PendingContainer, Refused> {
        let config = String::from_utf8(config)
            .map_err(|_| refuse("source_invalid", "the container configuration is not UTF-8"))?;
        let mode = self
            .output
            .mode()
            .await
            .ok_or_else(|| mode_mismatch("vzdump did not report the backup mode"))?;
        check_shape(mode, info, self.target.vmid)?;
        check_sources(root, &info.sources)?;
        let meta = CtMeta::from_config(mode, &config).map_err(|e| {
            refuse(
                "source_invalid",
                &format!("the container configuration: {e}"),
            )
        })?;
        if let Some(Err(e)) = meta.idmap.as_ref().map(|m| m.canonical()) {
            return Err(refuse("ct_idmap_unsupported", &e.to_string()));
        }
        if !is_dir_at(root, "etc") {
            return Err(refuse(
                "ct_empty_rootfs",
                "The root of the container that vzdump handed over has no /etc, so it is empty or not mounted. No snapshot was written.",
            ));
        }
        let (exclude, warnings) = translate_excludes(&excludes);
        for warning in warnings {
            self.ctx
                .event(JobEventLevel::Warning, "exclude_ignored", b"", "", &warning);
        }
        let mut source_meta = Meta::from([("vmid".to_string(), self.target.vmid.to_string())]);
        if let Some((_, name)) = config_lines(&config).find(|(k, _)| *k == "hostname")
            && name.len() <= MAX_META_VALUE_LEN
        {
            source_meta.insert("name".into(), name.to_string());
        }
        let ct = CtBackup {
            key: self.target.key.clone(),
            meta,
            source_meta,
            config,
            firewall: info.firewall_config.clone().filter(|f| !f.is_empty()),
            exclude: Some(exclude),
        };
        let ctx = self.ctx;
        let cell = ctx.progress_cell();
        let progress = move |p: &BackupProgress| record_progress(&cell, p);
        let options = BackupOptions {
            tags: self.spec.tags.clone(),
            prescan: true,
            on_issue: Some(Box::new(|issue: &IssueEvent<'_>| report_issue(ctx, issue))),
            agent_id: Some(ctx.agent_id().as_bytes().to_vec()),
            cancel: ctx.cancel_flag(),
            deadline: Some(self.deadline),
            ..Default::default()
        };
        self.writer
            .backup_container(root, ct, options, &progress)
            .await
            .map_err(|e| Box::new(backup_error(ctx, e)))
    }
}

/// The directory vzdump named, opened through `/proc/{pid}/root`: in
/// snapshot and stop mode the container is mounted only in the mount
/// namespace of that process. No component is a symlink.
fn open_container(pid: u32, directory: &str) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir = rustix::fs::open(format!("/proc/{pid}/root"), flags, Mode::empty())?;
    for part in directory.split('/').filter(|p| !p.is_empty() && *p != ".") {
        dir = openat(&dir, part, flags | OFlags::NOFOLLOW, Mode::empty())?;
    }
    Ok(dir)
}

fn is_dir_at(dir: &OwnedFd, name: &str) -> bool {
    statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory)
}

/// The shape of `$info` for the mode vzdump reported: suspend hands over a
/// copy in `vzdumptmp{pid}_{vmid}` with the one source `.`, snapshot and
/// stop the mounted volumes with `./` first and `./{mp}` for each mount point.
fn check_shape(mode: CtMode, info: &Info, vmid: u32) -> Result<(), Refused> {
    let base = info.directory.trim_end_matches('/').rsplit('/').next();
    let copy = base
        .and_then(|b| b.strip_prefix("vzdumptmp"))
        .and_then(|b| b.strip_suffix(&format!("_{vmid}")))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()));
    let shape_ok = match mode {
        CtMode::Suspend => copy && info.sources == ["."],
        CtMode::Snapshot | CtMode::Stop => {
            !copy
                && info.sources.first().is_some_and(|s| s == "./")
                && info.sources.iter().all(|s| s.starts_with("./"))
        }
    };
    if shape_ok {
        Ok(())
    } else {
        Err(mode_mismatch(&format!(
            "vzdump reported {} mode, but handed over {} with sources {:?}",
            mode.as_str(),
            info.directory,
            info.sources
        )))
    }
}

/// Each mount point vzdump backs up is a directory inside the container's
/// directory, reached without a symlink. The walk of the root covers them.
fn check_sources(root: &OwnedFd, sources: &[String]) -> Result<(), Refused> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    for source in sources {
        let mut dir: Option<OwnedFd> = None;
        for part in source.split('/').filter(|p| !p.is_empty() && *p != ".") {
            let next = openat(dir.as_ref().unwrap_or(root), part, flags, Mode::empty());
            dir = Some(next.map_err(|e| {
                refuse(
                    "source_invalid",
                    &format!(
                        "the mount point {source} is not a directory inside the container: {e}"
                    ),
                )
            })?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_mode_lines() {
        assert_eq!(
            parse_backup_mode("INFO: backup mode: snapshot"),
            Some(CtMode::Snapshot)
        );
        assert_eq!(
            parse_backup_mode("INFO: backup mode: suspend\n"),
            Some(CtMode::Suspend)
        );
        assert_eq!(parse_backup_mode("INFO: backup mode: stopx"), None);
        assert_eq!(parse_backup_mode("backup mode: stop"), None);
    }

    fn info(dir: &str, sources: &[&str]) -> Info {
        Info {
            directory: dir.into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            backup_user_id: None,
            bandwidth_limit: None,
            firewall_config: None,
            guest_uid: None,
            guest_rtime: None,
            cluster_id: None,
        }
    }

    #[test]
    fn shape_follows_the_mode() {
        let snap = info("/mnt/vzsnap0", &["./", "./data"]);
        let copy = info("/var/tmp/vzdumptmp4242_200/", &["."]);
        assert!(check_shape(CtMode::Snapshot, &snap, 200).is_ok());
        assert!(check_shape(CtMode::Stop, &snap, 200).is_ok());
        assert!(check_shape(CtMode::Suspend, &copy, 200).is_ok());
        assert!(check_shape(CtMode::Suspend, &snap, 200).is_err());
        assert!(check_shape(CtMode::Snapshot, &copy, 200).is_err());
        assert!(
            check_shape(
                CtMode::Suspend,
                &info("/var/tmp/vzdumptmp1_201", &["."]),
                200
            )
            .is_err()
        );
        assert!(check_shape(CtMode::Snapshot, &info("/mnt/vzsnap0", &["."]), 200).is_err());
    }
}
