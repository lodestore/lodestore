use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use indicatif::{HumanBytes, ProgressBar};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::{BackupFlags, UsageError};
use crate::output::{Output, spinner};

pub const DEFAULT_SOCKET: &str = "/run/lodestore/agent.sock";

const MAX_HEADER: usize = 4096;
const FRAME: usize = 64 * 1024;
const EXIT_INCOMPLETE: u8 = 3;

#[derive(Serialize)]
struct Header<'a> {
    v: u8,
    op: &'a str,
    name: &'a str,
    policy: Option<&'a str>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Summary {
    snapshot_id: String,
    incomplete: bool,
    errors_total: u64,
    stats: Stats,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Stats {
    files_new: u64,
    files_changed: u64,
    files_unchanged: u64,
    bytes_read: u64,
    bytes_added: u64,
}

#[derive(Debug)]
enum SendError {
    Source(std::io::Error),
    Sink(std::io::Error),
}

fn header_line(name: &str, policy: Option<&str>) -> Result<String> {
    let mut line = serde_json::to_string(&Header {
        v: 1,
        op: "backup_stdin",
        name,
        policy,
    })?;
    line.push('\n');
    if line.len() > MAX_HEADER {
        return Err(UsageError("--name and --policy are too long for the agent".into()).into());
    }
    Ok(line)
}

pub fn run(flags: &BackupFlags, out: Output) -> Result<u8> {
    let name = flags.name.as_deref().unwrap_or_default();
    if name.is_empty() {
        return Err(UsageError("--name must not be empty".into()).into());
    }
    if flags.parent.is_some() || flags.host.is_some() || !flags.tag.is_empty() {
        return Err(UsageError(
            "--parent, --host and --tag are not available when the agent takes the data".into(),
        )
        .into());
    }
    let header = header_line(name, flags.policy.as_deref())?;
    let socket = flags
        .agent_socket
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    let mut stream = UnixStream::connect(&socket)
        .map_err(|e| anyhow!("cannot reach the agent at {}: {e}", socket.display()))?;
    stream
        .write_all(header.as_bytes())
        .context("cannot send to the agent")?;
    let replies = stream.try_clone().context("cannot use the agent socket")?;
    let reader = std::thread::spawn(move || read_replies(replies, out));
    let sent = send_frames(&mut std::io::stdin().lock(), &mut stream);
    let reply = |reader: std::thread::JoinHandle<Result<u8>>| {
        reader
            .join()
            .unwrap_or_else(|_| Err(anyhow!("the reply reader failed")))
    };
    match sent {
        Ok(()) => reply(reader),
        Err(SendError::Source(e)) => {
            let _ = stream.shutdown(Shutdown::Both);
            let _ = reply(reader);
            Err(e).context("cannot read standard input")
        }
        Err(SendError::Sink(e)) => match reply(reader) {
            Err(agent) => Err(agent),
            Ok(_) => Err(e).context("cannot send to the agent"),
        },
    }
}

fn fill(src: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Sends `src` as frames of `u32` big-endian length plus data, then the empty
/// frame that commits the stream. A read error returns before that frame, so
/// the agent sees a broken stream and keeps no snapshot.
fn send_frames(src: &mut impl Read, dst: &mut impl Write) -> Result<(), SendError> {
    let mut buf = vec![0u8; FRAME];
    loop {
        let n = fill(src, &mut buf).map_err(SendError::Source)?;
        if n == 0 {
            break;
        }
        let len = u32::try_from(n).expect("a frame is at most 64 KiB");
        dst.write_all(&len.to_be_bytes()).map_err(SendError::Sink)?;
        dst.write_all(&buf[..n]).map_err(SendError::Sink)?;
    }
    dst.write_all(&0u32.to_be_bytes()).map_err(SendError::Sink)
}

fn print_line(line: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

fn read_replies(stream: UnixStream, out: Output) -> Result<u8> {
    let bar = if out.json { None } else { spinner() };
    let result = process(stream, out, bar.as_ref());
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    let summary = result?;
    if !out.json {
        println!("snapshot {} saved", summary.snapshot_id);
        let s = &summary.stats;
        println!(
            "files: {} new, {} changed, {} unchanged; {} read, {} added to the repository",
            s.files_new,
            s.files_changed,
            s.files_unchanged,
            HumanBytes(s.bytes_read),
            HumanBytes(s.bytes_added)
        );
        if summary.incomplete {
            eprintln!(
                "warning: {} errors; some files could not be read and the snapshot is incomplete",
                summary.errors_total
            );
        }
    }
    Ok(if summary.incomplete {
        EXIT_INCOMPLETE
    } else {
        0
    })
}

fn process(stream: UnixStream, out: Output, bar: Option<&ProgressBar>) -> Result<Summary> {
    for line in BufReader::new(stream).lines() {
        let line = line.context("cannot read the agent's reply")?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)
            .map_err(|e| anyhow!("the agent sent an unreadable reply: {e}"))?;
        match value["type"].as_str() {
            Some("accepted") if !out.json => {
                let id = &value["job_id"];
                eprintln!(
                    "queued as job {}",
                    id.as_str().map_or_else(|| id.to_string(), str::to_owned)
                );
            }
            Some("backup_progress") => {
                if out.json {
                    print_line(&line);
                } else if let Some(bar) = bar {
                    let n = |k: &str| value[k].as_u64().unwrap_or(0);
                    bar.set_message(format!(
                        "{} files, {} read, {} added",
                        n("files_done"),
                        HumanBytes(n("bytes_done")),
                        HumanBytes(n("bytes_added"))
                    ));
                }
            }
            Some("backup_summary") => {
                if out.json {
                    print_line(&line);
                }
                return serde_json::from_value(value)
                    .map_err(|e| anyhow!("the agent sent an unreadable summary: {e}"));
            }
            Some("error") => {
                let text = |k: &str| value[k].as_str().unwrap_or("unknown").to_owned();
                bail!("{} ({})", text("message"), text("code"));
            }
            _ => {}
        }
    }
    bail!("the agent closed the connection before the backup finished")
}

#[cfg(test)]
mod tests {
    use std::io::Error;

    use super::*;

    struct FailAfter(usize);

    impl Read for FailAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(Error::other("boom"));
            }
            let n = buf.len().min(self.0).min(1000);
            buf[..n].fill(7);
            self.0 -= n;
            Ok(n)
        }
    }

    #[test]
    fn stream_error_sends_no_commit_frame() {
        let mut sent = Vec::new();
        let err = send_frames(&mut FailAfter(100 * 1024), &mut sent).unwrap_err();
        assert!(matches!(err, SendError::Source(_)));
        assert_eq!(sent.len(), 4 + FRAME);
        assert_eq!(sent[..4], (FRAME as u32).to_be_bytes());
    }

    #[test]
    fn frames_end_with_the_empty_frame() {
        let mut sent = Vec::new();
        send_frames(&mut &vec![1u8; FRAME + 5][..], &mut sent).unwrap();
        assert_eq!(sent.len(), 4 + FRAME + 4 + 5 + 4);
        assert_eq!(sent[sent.len() - 4..], [0, 0, 0, 0]);
    }

    #[test]
    fn header_is_one_fixed_order_line() {
        assert_eq!(
            header_line("app.dump", None).unwrap(),
            "{\"v\":1,\"op\":\"backup_stdin\",\"name\":\"app.dump\",\"policy\":null}\n"
        );
        assert!(header_line(&"x".repeat(5000), None).is_err());
    }
}
