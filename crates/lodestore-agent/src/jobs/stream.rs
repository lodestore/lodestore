//! A stdin job: the bytes a user sends over the local socket become one file
//! in a snapshot. Only a stream closed by the empty commit frame is kept.

use std::cell::Cell;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lodestore_format::Error;
use lodestore_format::index::hex32;
use lodestore_format::writer::{BackupOptions, BackupProgress};
use lodestore_proto::agent::v1::{
    JobEventLevel, JobFinished, JobOutcome, JobPhaseKind, JobSpec, JobStats,
};
use serde_json::{Value, json};

use super::own_shards;
use super::runner::{gc_locks_gate, job_deadline, open_writer, time_limit};
use super::{JobCtx, failed};

const MAX_FRAME: usize = 1 << 20;
const POLL: Duration = Duration::from_secs(1);
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
const WRITE_WAIT: Duration = Duration::from_secs(10);

pub(crate) fn send(mut stream: &UnixStream, reply: &Value) -> bool {
    let mut line = reply.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).is_ok()
}

pub(crate) fn reply_error(stream: &UnixStream, code: &str, message: &str) {
    send(
        stream,
        &json!({"v": 1, "type": "error", "code": code, "message": message}),
    );
}

/// Frames of `u32` big-endian length plus data, ended by a frame of length 0.
/// Anything else that ends the stream is an error, so a client that died or a
/// dump that failed can never pass for a complete stream.
struct Frames {
    inner: BufReader<UnixStream>,
    left: usize,
    done: bool,
    broken: bool,
    cancel: Arc<AtomicBool>,
}

impl Frames {
    fn read_waiting(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::ErrorKind::{Interrupted, TimedOut, WouldBlock};
        loop {
            match self.inner.read(buf) {
                Err(e) if matches!(e.kind(), WouldBlock | TimedOut) => {
                    if self.cancel.load(Ordering::Relaxed) {
                        return Err(std::io::Error::other("the job was cancelled"));
                    }
                }
                Err(e) if e.kind() == Interrupted => {}
                result => return result,
            }
        }
    }

    fn read_frame(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{
            Error,
            ErrorKind::{InvalidData, UnexpectedEof},
        };
        if self.left == 0 {
            let mut len = [0u8; 4];
            let mut got = 0;
            while got < len.len() {
                match self.read_waiting(&mut len[got..])? {
                    0 => {
                        return Err(Error::new(
                            UnexpectedEof,
                            "the stream ended before the commit frame",
                        ));
                    }
                    n => got += n,
                }
            }
            let len = u32::from_be_bytes(len) as usize;
            if len == 0 {
                self.done = true;
                return Ok(0);
            }
            if len > MAX_FRAME {
                return Err(Error::new(
                    InvalidData,
                    format!("a frame of {len} bytes is over the {MAX_FRAME} byte limit"),
                ));
            }
            self.left = len;
        }
        let want = buf.len().min(self.left);
        match self.read_waiting(&mut buf[..want])? {
            0 => Err(Error::new(UnexpectedEof, "the stream ended inside a frame")),
            n => {
                self.left -= n;
                Ok(n)
            }
        }
    }
}

impl Read for Frames {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        self.read_frame(buf).inspect_err(|_| self.broken = true)
    }
}

/// Runs one stdin job to the end: the result goes to the hub first, then to
/// the client, so a client that has its answer can rely on the hub having it.
pub(crate) fn run_stdin_job(ctx: JobCtx, stream: UnixStream, spec: JobSpec) {
    let _ = stream.set_read_timeout(Some(POLL));
    let _ = stream.set_write_timeout(Some(WRITE_WAIT));
    send(
        &stream,
        &json!({"v": 1, "type": "accepted", "job_id": ctx.job_id()}),
    );
    ctx.phase(JobPhaseKind::Backup, "");
    ctx.event(
        JobEventLevel::Info,
        "started",
        b"",
        "",
        "stream backup started",
    );
    let (finished, reply) = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(backup(&ctx, &stream, &spec)),
        Err(e) => {
            let text = format!("cannot start the backup: {e}");
            (
                failed("backup_failed", &text),
                error_reply("backup_failed", &text),
            )
        }
    };
    ctx.finish(finished);
    send(&stream, &reply);
}

fn error_reply(code: &str, message: &str) -> Value {
    json!({"v": 1, "type": "error", "code": code, "message": message})
}

async fn backup(ctx: &JobCtx, stream: &UnixStream, spec: &JobSpec) -> (JobFinished, Value) {
    let deadline = job_deadline(spec);
    let mut writer =
        match open_writer(ctx.paths(), Arc::clone(&ctx.progress_cell().bytes_stored)).await {
            Ok(writer) => writer,
            Err(e) => {
                ctx.event(JobEventLevel::Error, "repository_unavailable", b"", "", &e);
                return (
                    failed("repository_unavailable", &e),
                    error_reply("backup_failed", &e),
                );
            }
        };
    if let Some(refused) = gc_locks_gate(writer.gc_locks_denied(), spec.gc_locks_required) {
        ctx.event(
            JobEventLevel::Error,
            &refused.error_code,
            b"",
            "",
            &refused.error_message,
        );
        let reply = error_reply("backup_failed", &refused.error_message);
        return (refused, reply);
    }
    let Ok(inner) = stream.try_clone() else {
        let text = "cannot use the socket";
        return (
            failed("backup_failed", text),
            error_reply("backup_failed", text),
        );
    };
    let mut frames = Frames {
        inner: BufReader::with_capacity(64 * 1024, inner),
        left: 0,
        done: false,
        broken: false,
        cancel: ctx.cancel_flag(),
    };
    let cell = ctx.progress_cell();
    let last_sent: Cell<Option<Instant>> = Cell::new(None);
    let muted = Cell::new(false);
    let progress = |p: &BackupProgress| {
        cell.files_done.store(p.files_done, Ordering::Relaxed);
        cell.bytes_processed
            .store(p.bytes_processed, Ordering::Relaxed);
        cell.bytes_added.store(p.bytes_added, Ordering::Relaxed);
        if p.starting
            || muted.get()
            || last_sent
                .get()
                .is_some_and(|t| t.elapsed() < PROGRESS_EVERY)
        {
            return;
        }
        last_sent.set(Some(Instant::now()));
        let delivered = send(
            stream,
            &json!({
                "v": 1,
                "type": "backup_progress",
                "files_done": p.files_done,
                "bytes_done": p.bytes_done,
                "bytes_added": p.bytes_added,
                "current_path": String::from_utf8_lossy(&p.current),
                "files_total": 0,
                "bytes_total": 0,
                "scan_done": false,
                "bytes_processed": p.bytes_processed,
            }),
        );
        muted.set(!delivered);
    };
    let result = writer
        .backup_stream(
            spec.stdin_name.as_bytes(),
            &mut frames,
            BackupOptions {
                tags: spec.tags.clone(),
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
            let s = outcome.stats;
            let reply = json!({
                "v": 1,
                "type": "backup_summary",
                "snapshot_id": id,
                "incomplete": outcome.incomplete,
                "parent": null,
                "stats": {
                    "files_new": s.files_new,
                    "files_changed": s.files_changed,
                    "files_unchanged": s.files_unchanged,
                    "dirs": s.dirs,
                    "bytes_total": s.bytes_total,
                    "bytes_read": s.bytes_read,
                    "bytes_added": s.bytes_added,
                },
                "errors_total": outcome.errors_total,
                "warnings_total": outcome.warnings_total,
            });
            let finished = JobFinished {
                outcome: if outcome.errors_total + outcome.warnings_total > 0 {
                    JobOutcome::Warnings
                } else {
                    JobOutcome::Succeeded
                } as i32,
                snapshot_id: id,
                stats: Some(JobStats {
                    files_new: s.files_new,
                    files_changed: s.files_changed,
                    files_unchanged: s.files_unchanged,
                    dirs: s.dirs,
                    bytes_total: s.bytes_total,
                    bytes_read: s.bytes_read,
                    bytes_added: s.bytes_added,
                }),
                incomplete: outcome.incomplete,
                errors_total: outcome.errors_total,
                warnings_total: outcome.warnings_total,
                ..Default::default()
            };
            (finished, reply)
        }
        Err(e) if matches!(e, Error::Interrupted) || ctx.cancel_flag().load(Ordering::Relaxed) => {
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
            (
                JobFinished {
                    outcome: outcome as i32,
                    error_code: code.into(),
                    error_message: text.into(),
                    ..Default::default()
                },
                error_reply("backup_failed", text),
            )
        }
        Err(Error::DeadlineExceeded { limit_secs }) => {
            let finished = time_limit(limit_secs);
            ctx.event(
                JobEventLevel::Error,
                &finished.error_code,
                b"",
                "",
                &finished.error_message,
            );
            let reply = error_reply("backup_failed", &finished.error_message);
            (finished, reply)
        }
        Err(e) => {
            let (code, text) = if frames.broken {
                ("stream_incomplete", format!("no snapshot was written: {e}"))
            } else {
                ("backup_failed", e.to_string())
            };
            ctx.event(JobEventLevel::Error, code, b"", "", &text);
            (failed(code, &text), error_reply(code, &text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_reports_a_peer_that_is_gone() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(send(&a, &json!({"type": "x"})));
        drop(b);
        assert!(!send(&a, &json!({"type": "x"})));
    }
}
