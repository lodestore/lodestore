//! The restore job: the server describes what to restore as a stream of
//! plan items, the agent reads the chunk frames from the bucket with the keys
//! and links the server handed out, verifies them and writes them with the
//! same `Restorer` the command line uses. The agent never reads the bucket
//! with a key of its own and never deletes anything.

use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lodestore_format::Error;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, RepoId};
use lodestore_format::fs::restore::{Overwrite, RestoreSettings, Restorer};
use lodestore_format::keys::cdc_seed;
use lodestore_format::pb::{Node, NodeType};
use lodestore_format::reader::open_chunk;
use lodestore_format::tree::validate_name;
use lodestore_proto::agent::v1 as proto;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::plan_item::Kind as ItemKind;
use lodestore_proto::agent::v1::{
    ChunkLoc, ConflictPolicy, JobEventLevel, JobFinished, JobOutcome, JobPhaseKind,
    RestorePlanBatch, RestorePlanRequest, RestoreSpec,
};
use prost::Message;
use rustix::fs::{Access, access};
use tonic::transport::Channel;
use tonic::{Code, Streaming};
use zeroize::{Zeroize, Zeroizing};

use super::fetch::{FetchError, Fetchers, PackAccess, PackFetcher, Span, coalesce};
use super::{JobCtx, ProgressCell, failed, now_ms};
use crate::session::agent_channel;

const CANCEL_POLL: Duration = Duration::from_millis(50);
pub(super) const MAX_PLAN_MESSAGE: usize = 8 << 20;
const PLAN_PAUSES: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

#[derive(Debug)]
pub enum PlanError {
    /// The stream broke; opening it again may work.
    Transport(String),
    /// The server refuses or the plan cannot be used.
    Fatal(String),
}

/// Where plan batches come from. The executor does not care whether they
/// arrive over gRPC or from memory.
#[allow(async_fn_in_trait)]
pub trait PlanSource {
    async fn next(&mut self) -> Result<Option<RestorePlanBatch>, PlanError>;

    /// Starts the plan again from its first batch.
    async fn reopen(&mut self) -> Result<(), PlanError>;
}

pub struct GrpcPlan {
    channel: Channel,
    job_id: String,
    stream: Option<Streaming<RestorePlanBatch>>,
}

impl GrpcPlan {
    pub fn new(channel: Channel, job_id: &str) -> Self {
        GrpcPlan {
            channel,
            job_id: job_id.to_string(),
            stream: None,
        }
    }

    async fn open(&mut self) -> Result<&mut Streaming<RestorePlanBatch>, PlanError> {
        if self.stream.is_none() {
            let stream = AgentClient::new(self.channel.clone())
                .max_decoding_message_size(MAX_PLAN_MESSAGE)
                .restore_plan(RestorePlanRequest {
                    job_id: self.job_id.clone(),
                })
                .await
                .map_err(plan_error)?
                .into_inner();
            self.stream = Some(stream);
        }
        Ok(self.stream.as_mut().expect("opened above"))
    }
}

fn plan_error(status: tonic::Status) -> PlanError {
    match status.code() {
        Code::Unavailable
        | Code::Unknown
        | Code::Internal
        | Code::Aborted
        | Code::DeadlineExceeded
        | Code::Cancelled => PlanError::Transport(status.message().to_string()),
        _ => PlanError::Fatal(status.message().to_string()),
    }
}

impl PlanSource for GrpcPlan {
    async fn next(&mut self) -> Result<Option<RestorePlanBatch>, PlanError> {
        let stream = self.open().await?;
        stream.message().await.map_err(|s| {
            self.stream = None;
            plan_error(s)
        })
    }

    async fn reopen(&mut self) -> Result<(), PlanError> {
        self.stream = None;
        self.open().await.map(|_| ())
    }
}

/// How a restore is run beyond what the spec says.
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    /// Where an in-place restore writes; the agent always uses `/`.
    pub root: PathBuf,
    /// Waits before opening the plan again after it broke; one entry per retry.
    pub plan_pauses: Vec<Duration>,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        RestoreOptions {
            root: PathBuf::from("/"),
            plan_pauses: PLAN_PAUSES.to_vec(),
        }
    }
}

struct Abort {
    code: &'static str,
    message: String,
}

fn abort(code: &'static str, message: impl Into<String>) -> Abort {
    Abort {
        code,
        message: message.into(),
    }
}

enum Stop {
    Abort(Abort),
    Cancelled,
    Restart(String),
}

impl From<Abort> for Stop {
    fn from(a: Abort) -> Self {
        Stop::Abort(a)
    }
}

fn io_abort(e: &std::io::Error, text: &str) -> Abort {
    let code = match e.kind() {
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => "restore_target_unwritable",
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => "restore_disk_full",
        _ => "restore_failed",
    };
    abort(code, format!("{text}: {e}"))
}

/// Resolves to `None` as soon as the flag is raised.
async fn unless_cancelled<T>(flag: &AtomicBool, work: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        out = work => Some(out),
        () = async {
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(CANCEL_POLL).await;
            }
        } => None,
    }
}

struct Prepared {
    snapshot: String,
    repo_id: RepoId,
    k_id: Zeroizing<[u8; 32]>,
    target: PathBuf,
    in_place: bool,
    overwrite: Overwrite,
    restore_owners: bool,
    stop_if_not_empty: bool,
}

impl Prepared {
    fn new(spec: &RestoreSpec, options: &RestoreOptions) -> Result<Self, Abort> {
        let invalid = |what: &str| {
            abort(
                "restore_failed",
                format!("the restore spec is invalid: {what}"),
            )
        };
        let repo_id = RepoId::try_from(spec.source_repo_id.as_slice())
            .map_err(|_| invalid("repository id"))?;
        let k_id = <[u8; 32]>::try_from(spec.k_id.as_slice())
            .map(Zeroizing::new)
            .map_err(|_| invalid("chunk key"))?;
        let policy = ConflictPolicy::try_from(spec.conflict).unwrap_or_default();
        let target = if spec.in_place {
            if policy == ConflictPolicy::StopIfNotEmpty {
                return Err(abort(
                    "restore_target_not_empty",
                    "a restore in place always writes into a non-empty target",
                ));
            }
            options.root.clone()
        } else {
            let path = PathBuf::from(&spec.destination);
            if !path.is_absolute() {
                return Err(invalid("the destination must be an absolute path"));
            }
            if path.components().any(|c| c == Component::ParentDir) {
                return Err(invalid("the destination must not contain \"..\""));
            }
            if path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::CurDir))
            {
                return Err(invalid("a restore into a new folder cannot write to \"/\""));
            }
            path
        };
        Ok(Prepared {
            snapshot: spec.snapshot_id.clone(),
            repo_id,
            k_id,
            target,
            in_place: spec.in_place,
            overwrite: match policy {
                ConflictPolicy::SkipExisting => Overwrite::Never,
                _ => Overwrite::IfChanged,
            },
            restore_owners: spec.restore_owners,
            stop_if_not_empty: policy == ConflictPolicy::StopIfNotEmpty,
        })
    }

    fn check_empty(&self) -> Result<(), Abort> {
        if !self.stop_if_not_empty {
            return Ok(());
        }
        match std::fs::read_dir(&self.target) {
            Ok(mut entries) => match entries.next() {
                Some(_) => Err(abort(
                    "restore_target_not_empty",
                    format!("{} is not empty", self.target.display()),
                )),
                None => Ok(()),
            },
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_abort(
                &e,
                &format!("cannot read {}", self.target.display()),
            )),
        }
    }

    fn settings(&self) -> RestoreSettings {
        RestoreSettings {
            overwrite: self.overwrite,
            verify: false,
            dry_run: false,
            k_id: self.k_id.clone(),
            seed: cdc_seed(&self.k_id),
            restore_owners: self.restore_owners,
        }
    }
}

fn unapplied_kind(what: &str) -> &'static str {
    match what {
        "owner" => "owner_not_applied",
        "times" => "time_not_applied",
        "hardlink" => "link_not_created",
        _ => "attributes_not_applied",
    }
}

/// One walk through the plan. A broken plan stream starts a new attempt with
/// a fresh `Restorer`; everything it did before stays and is recognised.
struct Attempt<'a, F> {
    ctx: &'a JobCtx,
    prepared: &'a Prepared,
    fetch: &'a F,
    cancel: Arc<AtomicBool>,
    cell: Arc<ProgressCell>,
    restorer: Restorer,
    packs: HashMap<u32, PackAccess>,
    pending: Option<(Node, Vec<ChunkLoc>)>,
    depth: usize,
    skip_depth: usize,
    errors: u64,
    warnings: u64,
}

impl<'a, F: PackFetcher> Attempt<'a, F> {
    fn new(
        ctx: &'a JobCtx,
        prepared: &'a Prepared,
        fetch: &'a F,
        (errors, warnings): (u64, u64),
    ) -> Result<Self, Abort> {
        let target = prepared.target.display();
        let restorer =
            Restorer::open(&prepared.target, prepared.settings()).map_err(|e| match e {
                Error::Io(e) => io_abort(&e, &format!("cannot open {target}")),
                other => abort("restore_failed", other.to_string()),
            })?;
        // Entries that cannot be written only warn, so a new folder nothing
        // can be written to has to fail here. In place, parts of `/` may be
        // read-only while the rest is not.
        if !prepared.in_place {
            access(&prepared.target, Access::WRITE_OK)
                .map_err(|e| io_abort(&e.into(), &format!("cannot write to {target}")))?;
        }
        let cell = ctx.progress_cell();
        for counter in [
            &cell.files_done,
            &cell.bytes_processed,
            &cell.bytes_downloaded,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        Ok(Attempt {
            ctx,
            prepared,
            fetch,
            cancel: ctx.cancel_flag(),
            cell,
            restorer,
            packs: HashMap::new(),
            pending: None,
            depth: 0,
            skip_depth: 0,
            errors,
            warnings,
        })
    }

    async fn run<P: PlanSource>(&mut self, plan: &mut P) -> Result<(), Stop> {
        self.ctx.phase(JobPhaseKind::RestorePlan, "");
        let mut first = true;
        loop {
            let next = unless_cancelled(&self.cancel, plan.next()).await;
            let batch = match next {
                None => return Err(Stop::Cancelled),
                Some(Err(PlanError::Transport(why))) => return Err(Stop::Restart(why)),
                Some(Err(PlanError::Fatal(why))) => {
                    return Err(abort("restore_failed", why).into());
                }
                Some(Ok(None)) => return Err(Stop::Restart("the plan ended early".into())),
                Some(Ok(Some(batch))) => batch,
            };
            let last = batch.last;
            if first {
                first = false;
                if let Some(totals) = &batch.totals {
                    self.cell.files_total.store(totals.files, Ordering::Relaxed);
                    self.cell.bytes_total.store(totals.bytes, Ordering::Relaxed);
                }
                self.cell.scan_done.store(true, Ordering::Relaxed);
                self.ctx.phase(JobPhaseKind::RestoreFiles, "");
            }
            self.batch(batch).await?;
            if last {
                break;
            }
        }
        if self.pending.is_some() {
            return Err(abort("restore_failed", "the plan ended in the middle of a file").into());
        }
        if self.depth != 0 || self.skip_depth != 0 {
            return Err(abort("restore_failed", "the plan left folders open").into());
        }
        self.ctx.phase(JobPhaseKind::RestoreMetadata, "");
        self.flush();
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), Stop> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    async fn batch(&mut self, batch: RestorePlanBatch) -> Result<(), Stop> {
        for access in batch.packs {
            let access = PackAccess::from_proto(access).map_err(|m| abort("restore_failed", m))?;
            self.packs.insert(access.pack, access);
        }
        for item in batch.items {
            self.check_cancel()?;
            match item.kind {
                Some(ItemKind::Enter(e)) => self.enter(&e.node, e.ancestor)?,
                Some(ItemKind::Entry(e)) => {
                    if self.pending.is_some() {
                        return Err(abort("restore_failed", "the plan broke off a file").into());
                    }
                    let node = decode(&e.node)?;
                    if e.more {
                        self.pending = Some((node, e.chunks));
                    } else {
                        self.entry(node, e.chunks).await?;
                    }
                }
                Some(ItemKind::Chunks(c)) => {
                    let Some((_, locs)) = self.pending.as_mut() else {
                        return Err(
                            abort("restore_failed", "the plan sent chunks of no file").into()
                        );
                    };
                    locs.extend(c.chunks);
                    if !c.more {
                        let (node, locs) = self.pending.take().expect("checked above");
                        self.entry(node, locs).await?;
                    }
                }
                Some(ItemKind::Leave(_)) => self.leave(),
                None => {}
            }
        }
        Ok(())
    }

    fn enter(&mut self, raw: &[u8], ancestor: bool) -> Result<(), Stop> {
        if self.skip_depth > 0 {
            self.skip_depth += 1;
            return Ok(());
        }
        let node = decode(raw)?;
        if self.reject_name(&node) {
            self.skip_depth = 1;
            return Ok(());
        }
        let entered = if ancestor {
            self.restorer.enter_ancestor(&node)
        } else {
            self.restorer.enter_dir(&node)
        };
        match entered {
            Ok(()) => self.depth += 1,
            Err(e) => {
                self.classify(&node.name, e)?;
                self.skip_depth = 1;
            }
        }
        self.flush();
        Ok(())
    }

    fn leave(&mut self) {
        if self.skip_depth > 0 {
            self.skip_depth -= 1;
            return;
        }
        if self.depth > 0 {
            self.restorer.leave_dir();
            self.depth -= 1;
        }
        self.flush();
    }

    fn reject_name(&mut self, node: &Node) -> bool {
        if validate_name(&node.name).is_ok() {
            return false;
        }
        self.errors += 1;
        self.ctx.event(
            JobEventLevel::Warning,
            "unsafe_path_rejected",
            &self.restorer.path_of(&node.name),
            "",
            "the plan names an entry that cannot be restored safely; it was skipped",
        );
        true
    }

    /// A problem with one entry becomes a warning and the restore goes on;
    /// one that makes going on pointless ends the job.
    fn classify(&mut self, name: &[u8], err: Error) -> Result<(), Stop> {
        let path = self.restorer.path_of(name);
        let kind = match &err {
            Error::Io(e)
                if matches!(e.kind(), ErrorKind::StorageFull | ErrorKind::QuotaExceeded) =>
            {
                return Err(io_abort(e, &String::from_utf8_lossy(&path)).into());
            }
            Error::Io(_) => "restore_entry_failed",
            Error::Interrupted => return Err(Stop::Cancelled),
            Error::Corrupt { .. } | Error::Crypto => "restore_integrity",
            other => return Err(abort("restore_failed", other.to_string()).into()),
        };
        self.errors += 1;
        self.ctx.event(
            JobEventLevel::Warning,
            kind,
            &path,
            "",
            &format!("{err}; this entry was not restored"),
        );
        Ok(())
    }

    fn flush(&mut self) {
        for u in self.restorer.take_unapplied() {
            self.warnings += 1;
            self.ctx.event(
                JobEventLevel::Warning,
                unapplied_kind(&u.what),
                &u.path,
                "",
                &format!("{}: {}", u.what, u.error),
            );
        }
        for f in self.restorer.take_failures() {
            self.errors += 1;
            let kind = if f.error.contains("parent folder") {
                "unsafe_path_rejected"
            } else {
                "restore_conflict"
            };
            self.ctx
                .event(JobEventLevel::Warning, kind, &f.path, "", &f.error);
        }
    }

    async fn entry(&mut self, mut node: Node, locs: Vec<ChunkLoc>) -> Result<(), Stop> {
        if self.skip_depth > 0 || self.reject_name(&node) {
            return Ok(());
        }
        let is_file = node.r#type == NodeType::File as i32;
        if !is_file {
            if let Err(e) = self.restorer.create_other(&node) {
                self.classify(&node.name, e)?;
            }
            self.flush();
            return Ok(());
        }
        if locs.iter().any(|l| l.id.len() != 32 || l.enc_len == 0) {
            return Err(abort("restore_failed", "the plan has a malformed chunk").into());
        }
        node.content = locs.iter().map(|l| l.id.clone()).collect();
        let mut written = 0u64;
        let outcome = self.write_file(&node, &locs, &mut written).await;
        self.cell
            .bytes_processed
            .fetch_add(node.size.saturating_sub(written), Ordering::Relaxed);
        self.cell.files_done.fetch_add(1, Ordering::Relaxed);
        self.flush();
        outcome
    }

    async fn write_file(
        &mut self,
        node: &Node,
        locs: &[ChunkLoc],
        written: &mut u64,
    ) -> Result<(), Stop> {
        match self.restorer.link_if_seen(node) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => return self.classify(&node.name, e),
        }
        let mut sink = match self.restorer.begin_file(node) {
            Ok(Some(sink)) => sink,
            Ok(None) => return Ok(()),
            Err(e) => return self.classify(&node.name, e),
        };
        for span in coalesce(locs) {
            self.check_cancel()?;
            let data = self.read_span(&span).await?;
            let key = self.packs[&span.pack].key.clone();
            for loc in &locs[span.chunks.clone()] {
                let from = (u64::from(loc.offset) - span.start) as usize;
                let frame = &data[from..from + loc.enc_len as usize];
                let id = BlobId::try_from(loc.id.as_slice()).expect("length checked by the caller");
                let plain = match open_chunk(
                    &key,
                    &self.prepared.repo_id,
                    BLOB_TYPE_DATA,
                    &id,
                    frame,
                    loc.ulen,
                    &self.prepared.k_id,
                ) {
                    Ok(plain) => plain,
                    Err(e) => return self.classify(&node.name, e),
                };
                if let Err(e) = sink.write(&plain) {
                    return self.classify(&node.name, e);
                }
                *written += plain.len() as u64;
                self.cell
                    .bytes_processed
                    .fetch_add(plain.len() as u64, Ordering::Relaxed);
            }
        }
        match self.restorer.finish_file(sink, node) {
            Ok(()) => Ok(()),
            Err(e) => self.classify(&node.name, e),
        }
    }

    // ponytail: ranges are read one after another; prefetching the next span
    // while the current one is written would hide latency on slow links.
    async fn read_span(&mut self, span: &Span) -> Result<Vec<u8>, Stop> {
        let mut refreshed = false;
        loop {
            let Some(access) = self.packs.get(&span.pack) else {
                return Err(abort(
                    "restore_failed",
                    "the plan names a pack it did not describe",
                )
                .into());
            };
            if !refreshed && access.expiring(now_ms()) {
                self.refresh(span.pack).await?;
                refreshed = true;
                continue;
            }
            let read =
                unless_cancelled(&self.cancel, self.fetch.fetch(access, span.start, span.len))
                    .await
                    .ok_or(Stop::Cancelled)?;
            match read {
                Ok(bytes) if bytes.len() as u64 != span.len => {
                    return Err(abort(
                        "restore_failed",
                        format!(
                            "pack {} returned {} bytes, expected {}",
                            span.pack,
                            bytes.len(),
                            span.len
                        ),
                    )
                    .into());
                }
                Ok(bytes) => {
                    self.cell
                        .bytes_downloaded
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    return Ok(bytes);
                }
                Err(FetchError::Forbidden) if !refreshed => {
                    self.refresh(span.pack).await?;
                    refreshed = true;
                }
                Err(e) => return Err(fetch_abort(&e, span.pack).into()),
            }
        }
    }

    async fn refresh(&mut self, pack: u32) -> Result<(), Stop> {
        let fresh = unless_cancelled(&self.cancel, self.fetch.refresh(&[pack]))
            .await
            .ok_or(Stop::Cancelled)?
            .map_err(|e| fetch_abort(&e, pack))?;
        for access in fresh {
            self.packs.insert(access.pack, access);
        }
        Ok(())
    }

    fn finished(&mut self, outcome: JobOutcome) -> JobFinished {
        let stats = self.restorer.take_stats();
        JobFinished {
            outcome: outcome as i32,
            restore: Some(proto::RestoreStats {
                created: stats.created,
                overwritten: stats.overwritten,
                skipped: stats.skipped,
                files: stats.created + stats.overwritten,
                bytes: stats.bytes,
            }),
            errors_total: self.errors,
            warnings_total: self.warnings,
            ..Default::default()
        }
    }
}

fn fetch_abort(e: &FetchError, pack: u32) -> Abort {
    match e {
        FetchError::Forbidden => abort(
            "restore_access_denied",
            format!("storage refused access to pack {pack}"),
        ),
        FetchError::NotFound => abort(
            "restore_data_missing",
            format!("pack {pack} is missing from storage"),
        ),
        FetchError::Failed(why) => {
            abort("restore_failed", format!("cannot read pack {pack}: {why}"))
        }
    }
}

fn decode(raw: &[u8]) -> Result<Node, Stop> {
    Node::decode(raw)
        .map_err(|_| abort("restore_failed", "the plan has an unreadable entry").into())
}

fn stopped(ctx: &JobCtx, mut finished: JobFinished) -> JobFinished {
    let (outcome, code, text) = if ctx.cancelled() {
        (
            JobOutcome::Cancelled,
            "cancelled",
            "the restore was cancelled",
        )
    } else {
        (
            JobOutcome::Interrupted,
            "agent_stopped",
            "the agent stopped during the restore",
        )
    };
    finished.outcome = outcome as i32;
    finished.error_code = code.into();
    finished.error_message = text.into();
    finished
}

/// Restores what the plan describes and returns the result for the server.
pub async fn execute<P: PlanSource, F: PackFetcher>(
    ctx: &JobCtx,
    spec: &RestoreSpec,
    plan: &mut P,
    fetch: &F,
) -> JobFinished {
    execute_with(ctx, spec, plan, fetch, &RestoreOptions::default()).await
}

pub async fn execute_with<P: PlanSource, F: PackFetcher>(
    ctx: &JobCtx,
    spec: &RestoreSpec,
    plan: &mut P,
    fetch: &F,
    options: &RestoreOptions,
) -> JobFinished {
    let prepared = match Prepared::new(spec, options).and_then(|p| p.check_empty().map(|()| p)) {
        Ok(p) => p,
        Err(a) => return failed(a.code, &a.message),
    };
    tracing::info!(job_id = ctx.job_id(), snapshot = %prepared.snapshot, "restore started");
    let mut restarts = 0;
    let mut broken: Option<String> = None;
    let mut carried = (0, 0);
    loop {
        if let Some(why) = broken.take() {
            let Some(&pause) = options.plan_pauses.get(restarts) else {
                return failed(
                    "restore_failed",
                    &format!("the plan stream broke too many times: {why}"),
                );
            };
            restarts += 1;
            ctx.event(
                JobEventLevel::Warning,
                "plan_restarted",
                b"",
                "",
                &format!("the plan stream broke ({why}); opening it again"),
            );
            let flag = ctx.cancel_flag();
            if unless_cancelled(&flag, tokio::time::sleep(pause))
                .await
                .is_none()
            {
                return stopped(ctx, JobFinished::default());
            }
            match plan.reopen().await {
                Ok(()) => {}
                Err(PlanError::Transport(why)) => {
                    broken = Some(why);
                    continue;
                }
                Err(PlanError::Fatal(why)) => return failed("restore_failed", &why),
            }
        }
        let mut attempt = match Attempt::new(ctx, &prepared, fetch, carried) {
            Ok(a) => a,
            Err(a) => return failed(a.code, &a.message),
        };
        match attempt.run(plan).await {
            Ok(()) => {
                let outcome = if attempt.errors + attempt.warnings > 0 {
                    JobOutcome::Warnings
                } else {
                    JobOutcome::Succeeded
                };
                return attempt.finished(outcome);
            }
            Err(Stop::Abort(a)) => {
                let mut finished = attempt.finished(JobOutcome::Failed);
                finished.error_code = a.code.into();
                finished.error_message = a.message;
                return finished;
            }
            Err(Stop::Cancelled) => {
                let finished = attempt.finished(JobOutcome::Cancelled);
                return stopped(ctx, finished);
            }
            Err(Stop::Restart(why)) => {
                carried = (attempt.errors, attempt.warnings);
                broken = Some(why);
            }
        }
    }
}

fn report(ctx: &JobCtx, finished: JobFinished) {
    if !finished.error_message.is_empty() {
        let level = if finished.outcome == JobOutcome::Failed as i32 {
            JobEventLevel::Error
        } else {
            JobEventLevel::Warning
        };
        ctx.event(
            level,
            &finished.error_code,
            b"",
            "",
            &finished.error_message,
        );
    }
    ctx.finish(finished);
}

/// Runs a restore with the given plan and fetcher on a thread-local runtime
/// and reports the result through `ctx`.
pub fn run_restore_with<P: PlanSource, F: PackFetcher>(
    ctx: &JobCtx,
    mut spec: RestoreSpec,
    mut plan: P,
    fetch: F,
    options: &RestoreOptions,
) {
    ctx.event(JobEventLevel::Info, "started", b"", "", "restore started");
    let finished = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(execute_with(ctx, &spec, &mut plan, &fetch, options)),
        Err(e) => failed("restore_failed", &format!("cannot start the restore: {e}")),
    };
    spec.k_id.zeroize();
    report(ctx, finished);
}

/// The executor the agent runs for a `JobAssign` with a restore.
pub fn run_restore_job(ctx: JobCtx, mut spec: RestoreSpec) {
    ctx.event(JobEventLevel::Info, "started", b"", "", "restore started");
    let options = RestoreOptions::default();
    let finished = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(async {
            let net = ctx.net();
            let channel = match agent_channel(ctx.paths(), &net).await {
                Ok(c) => c,
                Err(e) => {
                    return failed("restore_failed", &format!("cannot reach the server: {e}"));
                }
            };
            let fetch = match Fetchers::new(channel.clone(), ctx.job_id(), &net) {
                Ok(f) => f,
                Err(e) => return failed("restore_failed", &e.to_string()),
            };
            let mut plan = GrpcPlan::new(channel, ctx.job_id());
            execute_with(&ctx, &spec, &mut plan, &fetch, &options).await
        }),
        Err(e) => failed("restore_failed", &format!("cannot start the restore: {e}")),
    };
    spec.k_id.zeroize();
    report(&ctx, finished);
}
