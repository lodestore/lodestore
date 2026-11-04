use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use data_encoding::HEXLOWER;
use lodestore_format::Error;
use lodestore_format::crypto::BlobId;
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::walk::WalkOptions;
use lodestore_format::index::hex32;
use lodestore_format::keys::WriteKeys;
use lodestore_format::locks::DEFAULT_MAX_RUNTIME;
use lodestore_format::pb::IssueKind;
use lodestore_format::store::s3::StaticCredentials;
use lodestore_format::store::{RepoLocation, Store};
use lodestore_format::tree_cache::{CacheWriter, open_parent};
use lodestore_format::writer::{BackupOptions, BackupProgress, Deadline, IssueEvent, Writer};
use lodestore_proto::agent::v1::{
    Hook, JobEventLevel, JobFinished, JobOutcome, JobPhaseKind, JobSpec, JobStats,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::own_shards;
use super::priority;
use super::throttle::Throttled;
use super::{JobCtx, failed};
use crate::Paths;
use crate::config::AgentSettings;
use crate::hooks::{HookError, HookPhase, HookRun, HookSpec, run_hook};

const OUTPUT_CHUNK: usize = 8 * 1024;
const CANCEL_POLL: Duration = Duration::from_millis(50);

#[derive(Deserialize)]
struct RepositoryFile {
    backend: String,
    #[serde(default)]
    options: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct S3File {
    access_key_id: String,
    secret_access_key: String,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Opens the repository with the agent's own S3 key and writer keys. The
/// writer can only add objects, so nothing here can delete or replace data.
pub async fn open_writer(
    paths: &Paths,
    uploaded: Arc<AtomicU64>,
) -> Result<Writer<Throttled<Store>>, String> {
    let keys = read_write_keys(paths)?;
    open_writer_with(paths, keys, None, uploaded).await
}

fn read_write_keys(paths: &Paths) -> Result<WriteKeys, String> {
    let text = Zeroizing::new(
        std::fs::read_to_string(paths.writer_keys())
            .map_err(|e| format!("{}: {e}", paths.writer_keys().display()))?,
    );
    WriteKeys::from_file_text(&text).map_err(|e| format!("writer keys: {e}"))
}

async fn open_writer_with(
    paths: &Paths,
    keys: WriteKeys,
    rate: Option<u64>,
    uploaded: Arc<AtomicU64>,
) -> Result<Writer<Throttled<Store>>, String> {
    let repository: RepositoryFile = read_json(&paths.repository_json())?;
    let creds = match std::fs::read(paths.s3_credentials()) {
        Ok(bytes) => {
            let s3: S3File = serde_json::from_slice(&bytes)
                .map_err(|e| format!("{}: {e}", paths.s3_credentials().display()))?;
            Some(StaticCredentials {
                access_key_id: s3.access_key_id,
                secret_access_key: s3.secret_access_key,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", paths.s3_credentials().display())),
    };
    let location = RepoLocation {
        backend: repository.backend,
        options: repository.options,
    };
    let store = location
        .open(creds)
        .map_err(|e| format!("cannot open the repository: {e}"))?;
    Writer::connect_attested(
        Throttled::new(store, rate, uploaded),
        keys,
        &own_shards::load(paths),
    )
    .await
    .map_err(|e| format!("cannot open the repository: {e}"))
}

/// The job's time limit, counted from now; a spec without one gets the default.
pub(super) fn job_deadline(spec: &JobSpec) -> Deadline {
    Deadline::starting_now(match spec.max_runtime_s {
        0 => DEFAULT_MAX_RUNTIME,
        secs => Duration::from_secs(secs),
    })
}

pub(super) fn time_limit(limit_secs: u64) -> JobFinished {
    let limit = if limit_secs >= 3600 && limit_secs.is_multiple_of(3600) {
        format!("{}h", limit_secs / 3600)
    } else {
        format!("{limit_secs}s")
    };
    failed(
        "time_limit",
        &format!(
            "The backup ran longer than its time limit of {limit} and was stopped. No snapshot was written."
        ),
    )
}

/// Refuses a backup that cannot see `locks/` when the server needs it to.
pub(super) fn gc_locks_gate(denied: bool, required: bool) -> Option<JobFinished> {
    (denied && required).then(|| {
        failed(
            "gc_locks_unreadable",
            "The agent key cannot read locks/ in the repository, so this backup could reuse data a cleanup cycle is about to delete. Replace the agent key policy.",
        )
    })
}

fn issue_kind(kind: IssueKind) -> &'static str {
    match kind {
        IssueKind::Unreadable => "unreadable",
        IssueKind::Vanished => "vanished",
        IssueKind::ChangedDuringRead => "changed_during_read",
        IssueKind::UnsupportedType => "special_file",
        IssueKind::TooLarge => "too_large",
        IssueKind::Unspecified => "issue",
    }
}

/// Runs one file job to the end and reports the result through `ctx`: pre
/// hooks, the path check, the backup, then the post hooks whatever happened.
/// The backup does blocking I/O and its future is not `Send`, so it gets a
/// thread and a current-thread runtime of its own, apart from the gRPC session.
pub fn run_file_job(ctx: JobCtx, spec: JobSpec) {
    ctx.event(JobEventLevel::Info, "started", b"", "", "backup started");
    if let Some(limits) = &spec.limits {
        for warning in priority::apply(limits) {
            ctx.event(JobEventLevel::Warning, "priority_failed", b"", "", &warning);
        }
    }
    let finished = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(execute(&ctx, &spec)),
        Err(e) => failed("backup_failed", &format!("cannot start the backup: {e}")),
    };
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

fn hook_spec(hook: &Hook) -> HookSpec {
    let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
    HookSpec {
        script: some(&hook.script),
        command: some(&hook.command),
        args: hook.args.clone(),
        timeout_s: hook.timeout_s,
    }
}

fn backup_status(outcome: i32) -> &'static str {
    match JobOutcome::try_from(outcome) {
        Ok(JobOutcome::Succeeded) => "success",
        Ok(JobOutcome::Warnings) => "warnings",
        Ok(JobOutcome::Failed) => "failed",
        _ => "cancelled",
    }
}

fn interrupted(ctx: &JobCtx) -> JobFinished {
    let (outcome, code, text) = if ctx.cancelled() {
        (
            JobOutcome::Cancelled,
            "cancelled",
            "the backup was cancelled",
        )
    } else {
        (
            JobOutcome::Interrupted,
            "agent_stopped",
            "the agent stopped during the backup",
        )
    };
    JobFinished {
        outcome: outcome as i32,
        error_code: code.into(),
        error_message: text.into(),
        ..Default::default()
    }
}

fn watch_cancel(flag: Arc<AtomicBool>) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let token = CancellationToken::new();
    let watcher = tokio::spawn({
        let token = token.clone();
        async move {
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(CANCEL_POLL).await;
            }
            token.cancel();
        }
    });
    (token, watcher)
}

fn hook_failure(ctx: &JobCtx, err: &HookError) -> JobFinished {
    let detail: BTreeMap<String, String> = match err {
        HookError::Cancelled => return interrupted(ctx),
        HookError::Failed { script, code } => BTreeMap::from([
            ("script".into(), script.clone()),
            (
                "code".into(),
                code.map_or_else(|| "signal".to_string(), |c| c.to_string()),
            ),
        ]),
        HookError::Timeout { script, timeout_s } => BTreeMap::from([
            ("script".into(), script.clone()),
            ("timeout".into(), timeout_s.to_string()),
        ]),
        HookError::Missing { script } | HookError::Rejected { script, .. } => {
            BTreeMap::from([("script".into(), script.clone())])
        }
        HookError::InlineNotAllowed | HookError::Spawn(_) => BTreeMap::new(),
    };
    JobFinished {
        outcome: JobOutcome::Failed as i32,
        error_code: err.code().into(),
        error_message: err.to_string(),
        error_detail: detail.into_iter().collect(),
        ..Default::default()
    }
}

struct Hooks<'a> {
    ctx: &'a JobCtx,
    dir: PathBuf,
    allow_inline: bool,
    policy: &'a str,
}

impl Hooks<'_> {
    async fn run(
        &self,
        hook: &Hook,
        phase: HookPhase,
        status: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), HookError> {
        let ctx = self.ctx;
        let name = if hook.script.is_empty() {
            "inline"
        } else {
            hook.script.as_str()
        };
        ctx.phase(
            match phase {
                HookPhase::Pre => JobPhaseKind::PreHook,
                HookPhase::Post => JobPhaseKind::PostHook,
            },
            name,
        );
        ctx.event(
            JobEventLevel::Info,
            "hook_started",
            b"",
            name,
            &format!("running hook {name}"),
        );
        let run = HookRun {
            hooks_dir: &self.dir,
            allow_inline: self.allow_inline,
            policy: self.policy,
            job_id: ctx.job_id(),
            phase,
            backup_status: status,
        };
        let result = run_hook(&run, &hook_spec(hook), cancel).await;
        for chunk in result.output.trim_ascii_end().chunks(OUTPUT_CHUNK) {
            ctx.event(
                JobEventLevel::Info,
                "hook_output",
                b"",
                name,
                &String::from_utf8_lossy(chunk),
            );
        }
        if result.outcome.is_ok() {
            ctx.event(
                JobEventLevel::Info,
                "hook_finished",
                b"",
                name,
                &format!("hook {name} finished"),
            );
        }
        result.outcome
    }
}

fn missing_path(paths: &[String]) -> Option<JobFinished> {
    paths.iter().find_map(|path| {
        let e = std::fs::symlink_metadata(Path::new(path)).err()?;
        matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
        .then(|| JobFinished {
            outcome: JobOutcome::Failed as i32,
            error_code: "path_missing".into(),
            error_message: format!("{path} does not exist"),
            error_detail: [("path".to_string(), path.clone())].into(),
            ..Default::default()
        })
    })
}

async fn execute(ctx: &JobCtx, spec: &JobSpec) -> JobFinished {
    let deadline = job_deadline(spec);
    let settings = AgentSettings::load(ctx.paths());
    let hooks = Hooks {
        ctx,
        dir: ctx.paths().hooks_dir(),
        allow_inline: settings.hooks.allow_inline,
        policy: &spec.policy_name,
    };
    let (cancel, watcher) = watch_cancel(ctx.cancel_flag());

    let mut stopped = None;
    for hook in &spec.before {
        if let Err(e) = hooks.run(hook, HookPhase::Pre, None, &cancel).await {
            stopped = Some(hook_failure(ctx, &e));
            break;
        }
    }
    if stopped.is_none() {
        stopped = missing_path(&spec.paths);
    }
    let mut finished = match stopped {
        Some(finished) => finished,
        None => {
            ctx.phase(JobPhaseKind::Backup, "");
            backup(ctx, spec, deadline).await
        }
    };
    watcher.abort();

    let status = backup_status(finished.outcome);
    let never = CancellationToken::new();
    let mut post_failed = 0;
    for hook in &spec.after {
        if let Err(e) = hooks.run(hook, HookPhase::Post, Some(status), &never).await {
            post_failed += 1;
            ctx.event(
                JobEventLevel::Warning,
                "post_hook_failed",
                b"",
                &hook.script,
                &e.to_string(),
            );
        }
    }
    if post_failed > 0 {
        finished.warnings_total += post_failed;
        if finished.outcome == JobOutcome::Succeeded as i32 {
            finished.outcome = JobOutcome::Warnings as i32;
        }
    }
    finished
}

fn cache_dir(paths: &Paths, keys: &WriteKeys, spec: &JobSpec) -> PathBuf {
    let mut sorted: Vec<&str> = spec.paths.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let digest = Sha256::digest(sorted.join("\0"));
    let policy: String = spec
        .policy_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let policy = if policy.is_empty() {
        "adhoc"
    } else {
        policy.as_str()
    };
    paths
        .cache_dir()
        .join(HEXLOWER.encode(&keys.repo_id))
        .join(format!("{policy}-{}", &HEXLOWER.encode(&digest)[..16]))
}

async fn backup(ctx: &JobCtx, spec: &JobSpec, deadline: Deadline) -> JobFinished {
    let keys = match read_write_keys(ctx.paths()) {
        Ok(keys) => keys,
        Err(e) => return failed("repository_unavailable", &e),
    };
    let dir = cache_dir(ctx.paths(), &keys, spec);
    let parent = open_parent(&dir, &keys).await;
    let repo_id = keys.repo_id;
    let rate = spec.limits.as_ref().map(|l| l.upload_rate_bps);
    ctx.progress_cell()
        .upload_limit
        .store(rate.unwrap_or(0), Ordering::Relaxed);
    let uploaded = Arc::clone(&ctx.progress_cell().bytes_stored);
    let mut writer = match open_writer_with(ctx.paths(), keys, rate, uploaded).await {
        Ok(writer) => writer,
        Err(e) => return failed("repository_unavailable", &e),
    };
    if let Some(refused) = gc_locks_gate(writer.gc_locks_denied(), spec.gc_locks_required) {
        return refused;
    }
    let exclude = if spec.exclude.is_empty() {
        None
    } else {
        match PathMatcher::new(&spec.exclude) {
            Ok(matcher) => Some(matcher),
            Err(e) => return failed("invalid_policy", &e.to_string()),
        }
    };
    let cache_failed = |e: &dyn std::fmt::Display| {
        ctx.event(
            JobEventLevel::Warning,
            "cache_failed",
            b"",
            "",
            &format!("the local tree cache is not used this time: {e}"),
        );
    };
    let cache = Rc::new(RefCell::new(match CacheWriter::create(&dir) {
        Ok(writer) => Some(writer),
        Err(e) => {
            cache_failed(&e);
            None
        }
    }));
    let tree_sink = {
        let cache = Rc::clone(&cache);
        let cache_failed = &cache_failed;
        move |id: &BlobId, bytes: &[u8]| {
            let mut slot = cache.borrow_mut();
            if let Some(Err(e)) = slot.as_mut().map(|w| w.put(id, bytes)) {
                cache_failed(&e);
                *slot = None;
            }
        }
    };
    let cell = ctx.progress_cell();
    let progress = move |p: &BackupProgress| {
        cell.files_done.store(p.files_done, Ordering::Relaxed);
        cell.bytes_processed
            .store(p.bytes_processed, Ordering::Relaxed);
        cell.bytes_added.store(p.bytes_added, Ordering::Relaxed);
        cell.files_total.store(p.files_total, Ordering::Relaxed);
        cell.bytes_total.store(p.bytes_total, Ordering::Relaxed);
        cell.scan_done.store(p.scan_done, Ordering::Relaxed);
    };
    let on_issue = |issue: &IssueEvent<'_>| {
        let level = if issue.warning {
            JobEventLevel::Warning
        } else {
            JobEventLevel::Error
        };
        ctx.event(level, issue_kind(issue.kind), issue.path, "", issue.message);
    };
    let paths: Vec<PathBuf> = spec.paths.iter().map(PathBuf::from).collect();
    let result = writer
        .backup(
            &paths,
            BackupOptions {
                tags: spec.tags.clone(),
                walk: WalkOptions {
                    exclude,
                    one_file_system: spec.one_file_system,
                    exclude_caches: spec.exclude_caches,
                },
                parent,
                prescan: true,
                on_issue: Some(Box::new(on_issue)),
                tree_sink: Some(Box::new(tree_sink)),
                agent_id: Some(ctx.agent_id().as_bytes().to_vec()),
                cancel: ctx.cancel_flag(),
                deadline: Some(deadline),
                ..Default::default()
            },
            &progress,
        )
        .await;
    own_shards::record_written(ctx.paths(), &writer);
    match result {
        Ok(outcome) => {
            let id = hex32(&outcome.snapshot_id);
            ctx.event(
                JobEventLevel::Info,
                "snapshot_written",
                b"",
                "",
                &format!("snapshot {id} written"),
            );
            let cache = cache.borrow_mut().take();
            if let Some(Err(e)) = cache.map(|w| {
                w.commit(
                    &repo_id,
                    &outcome.snapshot_id,
                    &outcome.time,
                    &outcome.root_tree,
                )
            }) {
                cache_failed(&e);
            }
            let stats = outcome.stats;
            JobFinished {
                outcome: if outcome.errors_total + outcome.warnings_total > 0 {
                    JobOutcome::Warnings
                } else {
                    JobOutcome::Succeeded
                } as i32,
                snapshot_id: id,
                stats: Some(JobStats {
                    files_new: stats.files_new,
                    files_changed: stats.files_changed,
                    files_unchanged: stats.files_unchanged,
                    dirs: stats.dirs,
                    bytes_total: stats.bytes_total,
                    bytes_read: stats.bytes_read,
                    bytes_added: stats.bytes_added,
                }),
                incomplete: outcome.incomplete,
                errors_total: outcome.errors_total,
                warnings_total: outcome.warnings_total,
                ..Default::default()
            }
        }
        Err(Error::Interrupted) => interrupted(ctx),
        Err(Error::DeadlineExceeded { limit_secs }) => time_limit(limit_secs),
        Err(e) => failed("backup_failed", &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_gc_locks_gate() {
        assert!(gc_locks_gate(true, true).is_some_and(|f| f.error_code == "gc_locks_unreadable"));
        assert!(gc_locks_gate(true, false).is_none());
        assert!(gc_locks_gate(false, true).is_none());
        assert!(gc_locks_gate(false, false).is_none());
    }
}
