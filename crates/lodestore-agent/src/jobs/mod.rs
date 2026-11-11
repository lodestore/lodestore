//! Agent side of backup jobs: the hub that talks to the session, the job
//! store on disk and the runner that does the work on a thread of its own.
//! A job lives on disk from before `JobAccepted` until the server confirms
//! its result, so neither a dropped stream nor an agent restart loses it.

use std::collections::{HashMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, JobAccepted, JobCreated, JobEvent, JobEventLevel, JobEvents, JobFinished,
    JobOutcome, JobPhase, JobPhaseKind, JobProgress, JobRejected, JobReport, JobReportState,
    JobSpec, RestoreSpec,
};
use tokio::sync::Notify;

use crate::shared_channel::SharedChannel;
use crate::socket::StdinHeader;
use crate::{AgentError, NetOptions, Paths};

pub(crate) mod distrusted;
pub mod fetch;
pub(crate) mod own_shards;
pub mod priority;
pub mod restore;
pub mod runner;
mod store;
pub(crate) mod stream;
pub mod throttle;

use store::{ACCEPTED, FINISHED, JobRecord, JobStore, RUNNING};

pub const EVENT_CAP: u64 = 5000;
const EVENT_BATCH: usize = 256;
const SPEED_WINDOW: usize = 10;
const ETA_WINDOW: usize = 60;
const SAMPLE_TICK: Duration = Duration::from_secs(1);
const KIND_FILE: &str = "file";
const KIND_STDIN: &str = "stdin";
const KIND_RESTORE: &str = "restore";
const MAX_STDIN_JOBS: usize = 2;

fn reports_of(st: &State) -> Vec<JobReport> {
    let mut reports: Vec<JobReport> = st
        .jobs
        .values()
        .map(|e| JobReport {
            job_id: e.record.job_id.clone(),
            state: report_state(&e.record) as i32,
            last_event_seq: e.seq,
            finished: e.finished.clone(),
            created: e.created.clone(),
        })
        .collect();
    reports.sort_by(|a, b| a.job_id.cmp(&b.job_id));
    reports
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Counters the backup writes from its progress callback; the hub's sampler
/// reads them once a tick.
#[derive(Debug, Default)]
pub struct ProgressCell {
    pub files_done: AtomicU64,
    pub bytes_processed: AtomicU64,
    pub bytes_added: AtomicU64,
    pub files_total: AtomicU64,
    pub bytes_total: AtomicU64,
    pub scan_done: AtomicBool,
    pub bytes_downloaded: AtomicU64,
    pub bytes_stored: Arc<AtomicU64>,
    /// The upload limit of the job in bytes per second, 0 without one.
    pub upload_limit: AtomicU64,
}

#[derive(Debug, Default)]
struct JobShared {
    cancel: Arc<AtomicBool>,
    user_cancel: AtomicBool,
    progress: Arc<ProgressCell>,
}

struct Entry {
    record: JobRecord,
    finished: Option<JobFinished>,
    created: Option<JobCreated>,
    shared: Arc<JobShared>,
    seq: u64,
    forget: bool,
    phase: Option<JobPhase>,
    progress: Option<JobProgress>,
}

#[derive(Default)]
struct State {
    jobs: HashMap<String, Entry>,
    out: VecDeque<AgentMessage>,
}

pub struct JobHub {
    paths: Paths,
    agent_id: String,
    store: JobStore,
    state: Mutex<State>,
    notify: Notify,
    stopping: AtomicBool,
    tick: Duration,
    net: Mutex<NetOptions>,
    channel: Arc<SharedChannel>,
    stdin_max_runtime_s: AtomicU64,
}

/// What a job executor gets: report phases, events and the final result, and
/// poll for cancellation. Cheap to clone and `Send`.
#[derive(Clone)]
pub struct JobCtx {
    hub: Arc<JobHub>,
    job_id: Arc<str>,
    shared: Arc<JobShared>,
}

impl JobCtx {
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub fn paths(&self) -> &Paths {
        &self.hub.paths
    }

    pub fn agent_id(&self) -> &str {
        &self.hub.agent_id
    }

    /// Proxy settings the session was started with, for connections a job
    /// opens itself.
    pub fn channel(&self) -> Arc<SharedChannel> {
        self.hub.channel()
    }

    pub fn net(&self) -> NetOptions {
        self.hub
            .net
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// True once the server asked to cancel this job.
    pub fn cancelled(&self) -> bool {
        self.shared.user_cancel.load(Ordering::Relaxed)
    }

    /// True once the agent itself is stopping.
    pub fn shutting_down(&self) -> bool {
        self.hub.stopping.load(Ordering::Relaxed)
    }

    /// Set by a cancel request and by agent shutdown; this is the flag the
    /// backup watches.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.cancel)
    }

    pub fn progress_cell(&self) -> Arc<ProgressCell> {
        Arc::clone(&self.shared.progress)
    }

    pub fn phase(&self, kind: JobPhaseKind, script: &str) {
        self.hub.set_phase(
            &self.job_id,
            JobPhase {
                job_id: self.job_id.to_string(),
                phase: kind as i32,
                script: script.to_string(),
            },
        );
    }

    pub fn event(
        &self,
        level: JobEventLevel,
        kind: &str,
        path: &[u8],
        script: &str,
        message: &str,
    ) {
        self.hub
            .push_event(&self.job_id, level, kind, path, script, message);
    }

    pub fn finish(&self, finished: JobFinished) {
        self.hub.finish(&self.job_id, finished);
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn message(body: AgentBody) -> AgentMessage {
    AgentMessage { body: Some(body) }
}

fn report_state(record: &JobRecord) -> JobReportState {
    match record.state.as_str() {
        ACCEPTED => JobReportState::Accepted,
        RUNNING => JobReportState::Running,
        _ => JobReportState::Finished,
    }
}

impl JobHub {
    pub fn open(paths: &Paths, agent_id: &str) -> Result<Arc<JobHub>, AgentError> {
        Self::open_with_tick(paths, agent_id, SAMPLE_TICK)
    }

    pub fn open_with_tick(
        paths: &Paths,
        agent_id: &str,
        tick: Duration,
    ) -> Result<Arc<JobHub>, AgentError> {
        let store = JobStore::open(paths.jobs_dir())?;
        let mut jobs = HashMap::new();
        for mut record in store.load_all() {
            if !valid_id(&record.job_id) {
                continue;
            }
            let seq = store
                .read_events(&record.job_id)
                .last()
                .map_or(0, |e| e.seq)
                .max(record.last_event_seq);
            let mut finished = record.finished().filter(|_| record.state == FINISHED);
            if finished.is_none() {
                let interrupted = JobFinished {
                    job_id: record.job_id.clone(),
                    outcome: JobOutcome::Interrupted as i32,
                    error_code: "agent_restarted".into(),
                    error_message: "the agent stopped while the job was running".into(),
                    finished_at_ms: now_ms(),
                    last_event_seq: seq,
                    ..Default::default()
                };
                record.state = FINISHED.into();
                record.last_event_seq = seq;
                record.set_finished(&interrupted);
                if let Err(e) = store.save(&record) {
                    tracing::warn!(job_id = %record.job_id, error = %e, "cannot store the interrupted job");
                }
                finished = Some(interrupted);
            }
            jobs.insert(
                record.job_id.clone(),
                Entry {
                    created: record.created(),
                    finished,
                    record,
                    shared: Arc::default(),
                    seq,
                    forget: false,
                    phase: None,
                    progress: None,
                },
            );
        }
        Ok(Arc::new(JobHub {
            paths: paths.clone(),
            agent_id: agent_id.to_string(),
            store,
            state: Mutex::new(State {
                jobs,
                out: VecDeque::new(),
            }),
            notify: Notify::new(),
            stopping: AtomicBool::new(false),
            tick,
            net: Mutex::new(NetOptions::default()),
            channel: Arc::new(SharedChannel::new(paths)),
            stdin_max_runtime_s: AtomicU64::new(0),
        }))
    }

    /// Time limit of jobs taken from the local socket; 0, the start value,
    /// means `DEFAULT_MAX_RUNTIME`. Production leaves it at 0: the cleanup
    /// pause floor knows the policies' limits, not a value set on one agent,
    /// so a longer limit here would let a stream outlive the pause.
    pub fn set_stdin_max_runtime(&self, limit: Duration) {
        self.stdin_max_runtime_s
            .store(limit.as_secs(), Ordering::Relaxed);
    }

    /// The server channel jobs and update downloads share; the session loop keeps it current.
    pub fn channel(&self) -> Arc<SharedChannel> {
        Arc::clone(&self.channel)
    }

    pub fn set_net(&self, net: NetOptions) {
        self.channel.set_net(net.clone());
        *self.net.lock().unwrap_or_else(|e| e.into_inner()) = net;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, record: &JobRecord) {
        if let Err(e) = self.store.save(record) {
            tracing::warn!(job_id = %record.job_id, error = %e, "cannot store the job state");
        }
    }

    pub fn reports(&self) -> Vec<JobReport> {
        reports_of(&self.lock())
    }

    pub fn handle(self: &Arc<Self>, body: ServerBody) {
        match body {
            ServerBody::JobAssign(a) => match a.restore {
                Some(restore) => {
                    self.start_restore_with(a.job_id, restore, restore::run_restore_job);
                }
                None => self.assign(a.job_id, a.spec.unwrap_or_default()),
            },
            ServerBody::JobCancel(c) => self.cancel(&c.job_id),
            ServerBody::JobSync(s) => self.sync(&s.job_id, s.resend_events_from),
            ServerBody::JobFinishedAck(a) => self.drop_job(&a.job_id, false),
            ServerBody::JobForget(f) => self.drop_job(&f.job_id, true),
            _ => {}
        }
    }

    /// Everything to send now: queued messages in order, then the latest
    /// progress of each running job.
    pub fn outbound(&self) -> Vec<AgentMessage> {
        let mut st = self.lock();
        let mut out: Vec<AgentMessage> = Vec::new();
        for msg in st.out.drain(..) {
            if let Some(AgentBody::JobEvents(more)) = &msg.body
                && let Some(AgentMessage {
                    body: Some(AgentBody::JobEvents(last)),
                }) = out.last_mut()
                && last.job_id == more.job_id
                && last.events.len() + more.events.len() <= EVENT_BATCH
            {
                last.events.extend(more.events.iter().cloned());
                continue;
            }
            out.push(msg);
        }
        let mut progress: Vec<JobProgress> = st
            .jobs
            .values_mut()
            .filter_map(|e| e.progress.take())
            .collect();
        progress.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        out.extend(
            progress
                .into_iter()
                .map(|p| message(AgentBody::JobProgress(p))),
        );
        out
    }

    pub async fn changed(&self) {
        self.notify.notified().await;
    }

    /// A new session starts: whatever was queued for the old stream is
    /// dropped, results the server has not confirmed are queued again and the
    /// current phase is restated. The server asks for missing events itself.
    /// Drops everything queued for the previous session and returns the job
    /// reports for the new Hello. Both happen under one lock, so every message
    /// queued afterwards belongs to the new session and no job falls between.
    pub fn session_reset(&self) -> Vec<JobReport> {
        let mut st = self.lock();
        st.out
            .retain(|m| matches!(m.body, Some(AgentBody::JobRejected(_))));
        let mut ids: Vec<String> = st.jobs.keys().cloned().collect();
        ids.sort();
        for id in ids {
            let Some(e) = st.jobs.get_mut(&id) else {
                continue;
            };
            e.progress = None;
            let msg = match (&e.finished, &e.phase) {
                (Some(f), _) => message(AgentBody::JobFinished(f.clone())),
                (None, Some(p)) => message(AgentBody::JobPhase(p.clone())),
                (None, None) => continue,
            };
            st.out.push_back(msg);
        }
        let reports = reports_of(&st);
        drop(st);
        self.notify.notify_one();
        reports
    }

    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        for e in self.lock().jobs.values() {
            if e.finished.is_none() {
                e.shared.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// A backup or restore is running.
    pub fn busy(&self) -> bool {
        self.lock().jobs.values().any(|e| e.finished.is_none())
    }

    pub async fn wait_idle(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.busy() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn reject(&self, st: &mut State, job_id: &str, code: &str, text: &str) {
        st.out
            .push_back(message(AgentBody::JobRejected(JobRejected {
                job_id: job_id.to_string(),
                code: code.to_string(),
                message: text.to_string(),
            })));
    }

    /// Registers a job the server assigned: refuses it while another backup or
    /// restore runs, stores its record and queues `JobAccepted`. `None` when
    /// the job was refused or is already known.
    fn accept(
        self: &Arc<Self>,
        job_id: &str,
        kind: &str,
        spec: Option<&JobSpec>,
    ) -> Option<JobCtx> {
        let mut st = self.lock();
        if st.jobs.contains_key(job_id) {
            return None;
        }
        let refusal = if !valid_id(job_id) {
            Some(("invalid_job", "the job id is not valid".to_string()))
        } else if st.jobs.values().any(|e| {
            matches!(e.record.kind.as_str(), KIND_FILE | KIND_RESTORE) && e.finished.is_none()
        }) {
            Some(("busy", "another backup or restore is running".to_string()))
        } else {
            None
        };
        if let Some((code, text)) = refusal {
            self.reject(&mut st, job_id, code, &text);
            drop(st);
            self.notify.notify_one();
            return None;
        }
        let record = JobRecord {
            job_id: job_id.to_string(),
            kind: kind.into(),
            state: ACCEPTED.into(),
            finished: None,
            created: None,
            last_event_seq: 0,
        };
        let stored = match spec {
            Some(spec) => self.store.write_spec(job_id, spec),
            None => Ok(()),
        }
        .and_then(|()| self.store.save(&record));
        if let Err(e) = stored {
            self.store.remove(job_id);
            self.reject(&mut st, job_id, "store_failed", &e.to_string());
            drop(st);
            self.notify.notify_one();
            return None;
        }
        let shared = Arc::new(JobShared::default());
        st.jobs.insert(
            job_id.to_string(),
            Entry {
                record,
                finished: None,
                created: None,
                shared: Arc::clone(&shared),
                seq: 0,
                forget: false,
                phase: None,
                progress: None,
            },
        );
        st.out
            .push_back(message(AgentBody::JobAccepted(JobAccepted {
                job_id: job_id.to_string(),
                started_at_ms: now_ms(),
            })));
        drop(st);
        Some(JobCtx {
            hub: Arc::clone(self),
            job_id: job_id.into(),
            shared,
        })
    }

    fn spawn_task(
        self: &Arc<Self>,
        ctx: JobCtx,
        what: &'static str,
        run: impl FnOnce(JobCtx) + Send + 'static,
    ) {
        let spawned = std::thread::Builder::new()
            .name(format!("job-{}", ctx.job_id()))
            .spawn({
                let hub = Arc::clone(self);
                let ctx = ctx.clone();
                move || hub.run_task(ctx, what, run)
            });
        if let Err(e) = spawned {
            ctx.finish(failed("spawn_failed", &e.to_string()));
        }
        self.notify.notify_one();
    }

    fn assign(self: &Arc<Self>, job_id: String, spec: JobSpec) {
        if let Some(ctx) = self.accept(&job_id, KIND_FILE, Some(&spec)) {
            self.spawn_task(ctx, "backup", move |ctx| runner::run_file_job(ctx, spec));
        }
    }

    /// Starts a restore job with `run` as its executor. The agent passes
    /// `restore::run_restore_job`; tests pass their own. The spec is not kept
    /// on disk: it carries the repository's chunk id key.
    pub fn start_restore_with<R>(self: &Arc<Self>, job_id: String, spec: RestoreSpec, run: R)
    where
        R: FnOnce(JobCtx, RestoreSpec) + Send + 'static,
    {
        if let Some(ctx) = self.accept(&job_id, KIND_RESTORE, None) {
            self.spawn_task(ctx, "restore", move |ctx| run(ctx, spec));
        }
    }

    /// Takes a stream from the local socket. Such a job is the agent's own:
    /// it does not wait for the file-job queue and the server learns of it
    /// from `JobCreated`, now or in the next `Hello` if the server is away.
    pub fn submit_stdin(
        self: &Arc<Self>,
        header: StdinHeader,
        caller_uid: u32,
        caller_gid: u32,
        stream: UnixStream,
    ) {
        let mut st = self.lock();
        let running = st
            .jobs
            .values()
            .filter(|e| e.record.kind == KIND_STDIN && e.finished.is_none())
            .count();
        if running >= MAX_STDIN_JOBS {
            drop(st);
            stream::reply_error(
                &stream,
                "busy",
                &format!("{MAX_STDIN_JOBS} streams are already being written; try again later"),
            );
            return;
        }
        let job_id = uuid::Uuid::now_v7().to_string();
        let spec = JobSpec {
            policy_name: header.policy.clone().unwrap_or_default(),
            stdin_name: header.name,
            max_runtime_s: self.stdin_max_runtime_s.load(Ordering::Relaxed),
            gc_locks_required: true,
            tags: header
                .policy
                .map(|p| vec![format!("policy={p}")])
                .unwrap_or_default(),
            ..Default::default()
        };
        let created = JobCreated {
            job_id: job_id.clone(),
            spec: Some(spec.clone()),
            caller_uid,
            caller_gid,
            created_at_ms: now_ms(),
        };
        let mut record = JobRecord {
            job_id: job_id.clone(),
            kind: KIND_STDIN.into(),
            state: RUNNING.into(),
            finished: None,
            created: None,
            last_event_seq: 0,
        };
        record.set_created(&created);
        if let Err(e) = self.store.save(&record) {
            drop(st);
            self.store.remove(&job_id);
            stream::reply_error(
                &stream,
                "backup_failed",
                &format!("cannot store the job: {e}"),
            );
            return;
        }
        let shared = Arc::new(JobShared::default());
        st.jobs.insert(
            job_id.clone(),
            Entry {
                record,
                finished: None,
                created: Some(created.clone()),
                shared: Arc::clone(&shared),
                seq: 0,
                forget: false,
                phase: None,
                progress: None,
            },
        );
        st.out.push_back(message(AgentBody::JobCreated(created)));
        drop(st);

        let ctx = JobCtx {
            hub: Arc::clone(self),
            job_id: job_id.as_str().into(),
            shared,
        };
        let spawned = std::thread::Builder::new()
            .name(format!("job-{job_id}"))
            .spawn({
                let hub = Arc::clone(self);
                let ctx = ctx.clone();
                move || hub.run_stdin(ctx, stream, spec)
            });
        if let Err(e) = spawned {
            ctx.finish(failed("spawn_failed", &e.to_string()));
        }
        self.notify.notify_one();
    }

    fn run_stdin(self: Arc<Self>, ctx: JobCtx, stream: UnixStream, spec: JobSpec) {
        let (stop, stopped) = mpsc::channel::<()>();
        let sampler = std::thread::spawn({
            let ctx = ctx.clone();
            let tick = self.tick;
            move || sample_loop(&ctx, &stopped, tick)
        });
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            stream::run_stdin_job(ctx.clone(), stream, spec);
        }));
        drop(stop);
        let _ = sampler.join();
        if result.is_err() {
            ctx.finish(failed("internal_error", "the backup task panicked"));
        }
    }

    fn run_task(self: Arc<Self>, ctx: JobCtx, what: &str, run: impl FnOnce(JobCtx)) {
        self.set_state(ctx.job_id(), RUNNING);
        let (stop, stopped) = mpsc::channel::<()>();
        let sampler = std::thread::spawn({
            let ctx = ctx.clone();
            let tick = self.tick;
            move || sample_loop(&ctx, &stopped, tick)
        });
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| run(ctx.clone())));
        drop(stop);
        let _ = sampler.join();
        if result.is_err() {
            ctx.finish(failed(
                "internal_error",
                &format!("the {what} task panicked"),
            ));
        } else {
            ctx.finish(failed(
                "internal_error",
                &format!("the {what} task ended without a result"),
            ));
        }
    }

    fn set_state(&self, id: &str, state: &str) {
        let mut st = self.lock();
        if let Some(e) = st.jobs.get_mut(id)
            && e.finished.is_none()
        {
            e.record.state = state.into();
            self.persist(&e.record);
        }
    }

    fn cancel(&self, id: &str) {
        if let Some(e) = self.lock().jobs.get(id)
            && e.finished.is_none()
        {
            e.shared.user_cancel.store(true, Ordering::Relaxed);
            e.shared.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn drop_job(&self, id: &str, forget: bool) {
        let mut st = self.lock();
        let Some(e) = st.jobs.get_mut(id) else {
            return;
        };
        if e.finished.is_some() {
            st.jobs.remove(id);
            self.store.remove(id);
        } else if forget {
            e.forget = true;
            e.shared.user_cancel.store(true, Ordering::Relaxed);
            e.shared.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn sync(&self, id: &str, from: u64) {
        let mut st = self.lock();
        if !st.jobs.contains_key(id) {
            return;
        }
        let events: Vec<JobEvent> = self
            .store
            .read_events(id)
            .into_iter()
            .filter(|e| e.seq >= from)
            .collect();
        for batch in events.chunks(EVENT_BATCH) {
            st.out.push_back(message(AgentBody::JobEvents(JobEvents {
                job_id: id.to_string(),
                events: batch.to_vec(),
            })));
        }
        drop(st);
        self.notify.notify_one();
    }

    fn set_phase(&self, id: &str, phase: JobPhase) {
        let mut st = self.lock();
        let Some(e) = st.jobs.get_mut(id) else {
            return;
        };
        e.phase = Some(phase.clone());
        st.out.push_back(message(AgentBody::JobPhase(phase)));
        drop(st);
        self.notify.notify_one();
    }

    fn set_progress(&self, id: &str, progress: JobProgress) {
        let mut st = self.lock();
        if let Some(e) = st.jobs.get_mut(id)
            && e.finished.is_none()
        {
            e.progress = Some(progress);
            drop(st);
            self.notify.notify_one();
        }
    }

    fn push_event(
        &self,
        id: &str,
        level: JobEventLevel,
        kind: &str,
        path: &[u8],
        script: &str,
        text: &str,
    ) {
        let mut st = self.lock();
        let Some(e) = st.jobs.get_mut(id) else {
            return;
        };
        if e.seq >= EVENT_CAP {
            return;
        }
        e.seq += 1;
        let mut event = JobEvent {
            seq: e.seq,
            at_ms: now_ms(),
            level: level as i32,
            kind: kind.to_string(),
            path: String::from_utf8_lossy(path).into_owned(),
            script: script.to_string(),
            message: text.to_string(),
        };
        if e.seq == EVENT_CAP {
            event = JobEvent {
                level: JobEventLevel::Warning as i32,
                kind: "log_limit".into(),
                path: String::new(),
                script: String::new(),
                message: format!("the log reached {EVENT_CAP} lines; later events are not kept"),
                ..event
            };
        }
        if let Err(err) = self.store.append_event(id, &event) {
            tracing::warn!(job_id = id, error = %err, "cannot store a job event");
        }
        st.out.push_back(message(AgentBody::JobEvents(JobEvents {
            job_id: id.to_string(),
            events: vec![event],
        })));
        drop(st);
        self.notify.notify_one();
    }

    fn finish(&self, id: &str, mut finished: JobFinished) {
        let mut st = self.lock();
        let Some(e) = st.jobs.get_mut(id) else {
            return;
        };
        if e.finished.is_some() {
            return;
        }
        finished.job_id = id.to_string();
        finished.last_event_seq = e.seq;
        finished.finished_at_ms = now_ms();
        e.progress = None;
        if e.forget {
            st.jobs.remove(id);
            self.store.remove(id);
            return;
        }
        e.record.state = FINISHED.into();
        e.record.last_event_seq = e.seq;
        e.record.set_finished(&finished);
        self.persist(&e.record);
        e.finished = Some(finished.clone());
        let last = progress_of(id, &e.shared.progress, 0, 0, None);
        st.out.push_back(message(AgentBody::JobProgress(last)));
        st.out.push_back(message(AgentBody::JobFinished(finished)));
        drop(st);
        self.notify.notify_one();
    }
}

pub(crate) fn failed(code: &str, text: &str) -> JobFinished {
    JobFinished {
        outcome: JobOutcome::Failed as i32,
        error_code: code.into(),
        error_message: text.into(),
        ..Default::default()
    }
}

fn sample_loop(ctx: &JobCtx, stopped: &mpsc::Receiver<()>, tick: Duration) {
    let cell = ctx.progress_cell();
    let mut ring: VecDeque<(Instant, u64, u64)> = VecDeque::with_capacity(ETA_WINDOW + 1);
    ring.push_back((
        Instant::now(),
        cell.bytes_processed.load(Ordering::Relaxed),
        cell.bytes_stored.load(Ordering::Relaxed),
    ));
    while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(tick) {
        let processed = cell.bytes_processed.load(Ordering::Relaxed);
        ring.push_back((
            Instant::now(),
            processed,
            cell.bytes_stored.load(Ordering::Relaxed),
        ));
        if ring.len() > ETA_WINDOW {
            ring.pop_front();
        }
        let last = ring[ring.len() - 1];
        let per_sec = |first: (Instant, u64, u64), pick: fn(&(Instant, u64, u64)) -> u64| {
            let secs = last.0.duration_since(first.0).as_secs_f64();
            if secs > 0.0 {
                (pick(&last).saturating_sub(pick(&first)) as f64 / secs) as u64
            } else {
                0
            }
        };
        let recent = ring[ring.len().saturating_sub(SPEED_WINDOW + 1)];
        let speed = per_sec(recent, |s| s.1);
        let upload_speed = per_sec(recent, |s| s.2);
        // A throttled backup reads a pack, then waits for its upload slot, so
        // the read speed over a few seconds is often zero; the estimate looks
        // at the last minute. Deduplicated data reads far faster than the
        // limit allows new data to go out, so a limited job never estimates
        // above its limit.
        let steady = eta_speed(
            per_sec(ring[0], |s| s.1),
            cell.upload_limit.load(Ordering::Relaxed),
        );
        let scan_done = cell.scan_done.load(Ordering::Relaxed);
        let bytes_total = cell.bytes_total.load(Ordering::Relaxed);
        let eta_s = (scan_done && steady > 0)
            .then(|| bytes_total.saturating_sub(processed).div_ceil(steady));
        ctx.hub.set_progress(
            ctx.job_id(),
            progress_of(ctx.job_id(), &cell, speed, upload_speed, eta_s),
        );
    }
}

/// The read speed the time left is estimated from, capped by the upload
/// limit (0 when there is none or nothing was read).
fn eta_speed(read: u64, limit: u64) -> u64 {
    match (read, limit) {
        (0, limit) => limit,
        (read, 0) => read,
        (read, limit) => read.min(limit),
    }
}

/// A progress sample of the counters as they are now. The hub also sends one
/// when a job finishes, so a job shorter than a tick still reports its totals.
fn progress_of(
    job_id: &str,
    cell: &ProgressCell,
    speed_bps: u64,
    upload_speed_bps: u64,
    eta_s: Option<u64>,
) -> JobProgress {
    JobProgress {
        job_id: job_id.to_string(),
        files_done: cell.files_done.load(Ordering::Relaxed),
        bytes_done: cell.bytes_processed.load(Ordering::Relaxed),
        bytes_uploaded: cell.bytes_added.load(Ordering::Relaxed),
        files_total: cell.files_total.load(Ordering::Relaxed),
        bytes_total: cell.bytes_total.load(Ordering::Relaxed),
        scan_done: cell.scan_done.load(Ordering::Relaxed),
        speed_bps,
        eta_s,
        bytes_downloaded: cell.bytes_downloaded.load(Ordering::Relaxed),
        upload_speed_bps,
    }
}

#[cfg(test)]
mod tests {
    use super::eta_speed;

    #[test]
    fn eta_speed_never_exceeds_the_limit() {
        assert_eq!(eta_speed(500 << 20, 2 << 20), 2 << 20);
        assert_eq!(eta_speed(1 << 20, 2 << 20), 1 << 20);
        assert_eq!(eta_speed(0, 2 << 20), 2 << 20);
        assert_eq!(eta_speed(7, 0), 7);
        assert_eq!(eta_speed(0, 0), 0);
    }
}
