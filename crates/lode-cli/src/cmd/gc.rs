use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::server::{size, time};
use crate::cli::{GcArgs, GcCommand, UsageError};
use crate::client::{ApiFailure, Client, failure, seg};
use crate::output::Output;

const POLL: Duration = Duration::from_secs(2);
const POLL_RETRIES: u32 = 3;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const UNLOCK_WARNING: &str = "If one of these backups is still running, its snapshot can lose data and fail to restore. Unlock only backups you know were interrupted.";

#[derive(Clone, Serialize, Deserialize)]
struct Marker {
    host: Option<String>,
    started_at: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Progress {
    done: u64,
    total: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Cycle {
    id: String,
    state: String,
    #[serde(default)]
    dry_run: bool,
    delete_after: Option<i64>,
    progress: Option<Progress>,
    #[serde(default)]
    marked_packs: u64,
    #[serde(default)]
    marked_bytes: u64,
    #[serde(default)]
    blocked_by_writers: Vec<Marker>,
    #[serde(default)]
    waiting_for_restores: bool,
}

#[derive(Deserialize)]
struct Report {
    cycle_id: String,
    finished_at: i64,
    #[serde(default)]
    bytes_freed: u64,
    #[serde(default)]
    packs_deleted: u64,
    #[serde(default)]
    packs_repacked: u64,
    #[serde(default)]
    repack_transfer_bytes: u64,
    #[serde(default)]
    locked_bytes: u64,
    locked_until: Option<i64>,
}

#[derive(Deserialize)]
struct Event {
    kind: String,
    message: String,
}

#[derive(Deserialize)]
struct Schedule {
    #[serde(default)]
    next_runs: Vec<i64>,
}

#[derive(Deserialize)]
struct Status {
    enabled: bool,
    schedule: Option<Schedule>,
    cycle: Option<Cycle>,
    last_cleanup: Option<Report>,
    last_dry_run: Option<Report>,
    last_aborted_at: Option<i64>,
    #[serde(default)]
    events: Vec<Event>,
}

#[derive(Deserialize)]
struct Started {
    cycle_id: String,
}

#[derive(Deserialize)]
struct Unlocked {
    unlocked: u64,
}

#[derive(Deserialize)]
struct PolicyRepo {
    repository_id: String,
    repository_name: String,
}

enum Followed {
    Waiting(Cycle),
    Done(Value),
}

pub async fn run(client: &Client, out: Output, args: GcArgs) -> Result<u8> {
    match (args.command, args.repo) {
        (Some(GcCommand::Status { repo }), _) => status(client, out, &repo).await?,
        (Some(GcCommand::Unlock { repo, yes }), _) => unlock(client, out, &repo, yes).await?,
        (None, Some(repo)) if args.dry_run => dry_run(client, out, &repo).await?,
        (None, Some(repo)) => start(client, out, &repo).await?,
        (None, None) => return Err(UsageError("Name the repository to clean up.".into()).into()),
    }
    Ok(0)
}

fn gc_path(id: &str) -> String {
    format!("/api/v1/repositories/{}/gc", seg(id))
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiFailure>()
        .is_some_and(|f| f.status == StatusCode::NOT_FOUND.as_u16())
}

/// An id is used as given; any other text is a repository name, looked up
/// in the policies because the repository list is not open to API tokens.
async fn resolve(client: &Client, repo: &str) -> Result<String> {
    match client.get_json::<Value>(&gc_path(repo), &[]).await {
        Ok(_) => return Ok(repo.to_owned()),
        Err(err) if is_not_found(&err) => {}
        Err(err) => return Err(err),
    }
    let policies: Vec<PolicyRepo> = client
        .get_json("/api/v1/policies", &[])
        .await
        .map_err(|err| err.context("cannot look the repository name up"))?;
    let mut ids: Vec<_> = policies
        .into_iter()
        .filter(|p| p.repository_name == repo)
        .map(|p| p.repository_id)
        .collect();
    ids.dedup();
    match <[String; 1]>::try_from(ids) {
        Ok([id]) => Ok(id),
        Err(_) => Err(UsageError(format!(
            "Unknown repository {repo:?}. A name is found through the policies of the repository \
(needs policies:read); a repository without a policy must be given by its id."
        ))
        .into()),
    }
}

fn plural(n: u64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn day(ms: i64) -> String {
    let text = time(Some(ms));
    let part = |from, to| text.get(from..to).and_then(|s| s.parse::<usize>().ok());
    match (part(8, 10), part(5, 7), text.get(0..4)) {
        (Some(d), Some(m @ 1..=12), Some(y)) => format!("{d} {} {y}", MONTHS[m - 1]),
        _ => text,
    }
}

fn utc_time(ms: i64) -> String {
    format!("{} UTC", time(Some(ms)))
}

fn heading(cycle: &Cycle) -> &'static str {
    match (cycle.state.as_str(), cycle.dry_run) {
        (_, true) => "Dry run in progress",
        ("marking", _) => "Checking snapshots",
        ("repacking", _) => "Rewriting packs",
        ("waiting", _) if !cycle.blocked_by_writers.is_empty() => "Deletion is postponed",
        ("waiting", _) => "Marked for deletion",
        ("verifying", _) => "Checking again before deleting",
        _ => "Deleting unused data",
    }
}

fn show_progress(cycle: &Cycle, shown: &mut String) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let mut line = format!("{}...", heading(cycle));
    if let Some(p) = cycle.progress.as_ref().filter(|p| p.total > 0) {
        line.push_str(&format!(" {} of {}", p.done, p.total));
    }
    if line != *shown {
        eprintln!("{line}");
        *shown = line;
    }
}

/// A failure that says nothing about the cycle: the server could not be
/// reached or answered with a server error.
fn transient(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<ApiFailure>() {
        Some(f) => f.status >= 500,
        None => e.chain().any(|c| c.is::<reqwest::Error>()),
    }
}

/// Reads the status, trying again a few times when the server is briefly
/// unreachable: the cycle runs on the server whatever happens here.
async fn poll_status(client: &Client, path: &str) -> Result<Status> {
    let mut failures = 0;
    loop {
        match client.get_json(path, &[]).await {
            Err(e) if transient(&e) => {
                failures += 1;
                if failures > POLL_RETRIES {
                    return Err(e.context(
                        "Lost contact with the server. The cleanup continues there; check it with `lode gc status`",
                    ));
                }
                tokio::time::sleep(POLL).await;
            }
            other => return other,
        }
    }
}

/// Polls the cycle until it ends, or until it waits for its pause when
/// `stop_waiting` is set. A cycle that left the active state counts as done
/// only when it produced the last report of its kind.
async fn follow(
    client: &Client,
    id: &str,
    cycle_id: &str,
    dry_run: bool,
    out: Output,
    stop_waiting: bool,
    failed: &str,
) -> Result<Followed> {
    let path = gc_path(id);
    let mut shown = String::new();
    loop {
        let status = poll_status(client, &path).await?;
        if let Some(cycle) = status.cycle.filter(|c| c.id == cycle_id) {
            if stop_waiting && cycle.state == "waiting" {
                return Ok(Followed::Waiting(cycle));
            }
            if !out.json {
                show_progress(&cycle, &mut shown);
            }
        } else {
            let last = if dry_run {
                &status.last_dry_run
            } else {
                &status.last_cleanup
            };
            if last.as_ref().is_some_and(|r| r.cycle_id == cycle_id) {
                let report = format!("{path}/cycles/{}/report", seg(cycle_id));
                return Ok(Followed::Done(client.get_json(&report, &[]).await?));
            }
            let reason = status
                .events
                .iter()
                .rev()
                .find(|e| matches!(e.kind.as_str(), "failed" | "aborted"))
                .map_or("The server gave no reason.", |e| e.message.as_str());
            bail!("{failed}: {reason}");
        }
        tokio::time::sleep(POLL).await;
    }
}

fn facts(r: &Report, dry_run: bool) -> Vec<(&'static str, String)> {
    let (free, delete, repack) = if dry_run {
        ("Would free", "To delete", "To repack")
    } else {
        ("Freed", "Packs deleted", "Packs repacked")
    };
    let mut rows = vec![
        (free, size(r.bytes_freed)),
        (delete, plural(r.packs_deleted, "pack")),
        (repack, plural(r.packs_repacked, "pack")),
    ];
    if r.packs_repacked > 0 {
        rows.push(("Repack transfer", size(r.repack_transfer_bytes)));
    }
    if r.locked_bytes > 0 {
        let when = r.locked_until.map_or_else(
            || "release date unknown".to_owned(),
            |ms| format!("frees after {}", day(ms)),
        );
        rows.push((
            "Held by Object Lock",
            format!("{}, {when}", size(r.locked_bytes)),
        ));
    }
    rows
}

fn print_report(title: &str, report: &Report, dry_run: bool) {
    println!("{title}");
    for (label, value) in facts(report, dry_run) {
        println!("  {label:<21}{value}");
    }
}

fn report_of(value: Value) -> Result<Report> {
    Ok(serde_json::from_value(value)?)
}

async fn dry_run(client: &Client, out: Output, repo: &str) -> Result<()> {
    let id = resolve(client, repo).await?;
    let started: Started = client
        .post_json(&format!("{}/dry-run", gc_path(&id)), None::<&()>)
        .await?;
    let done = follow(
        client,
        &id,
        &started.cycle_id,
        true,
        out,
        false,
        "Dry run failed",
    )
    .await?;
    let Followed::Done(full) = done else {
        unreachable!("a dry run never stops at its pause");
    };
    if out.json {
        out.emit("gc_report", &full);
    } else {
        print_report(
            &format!("Dry run for repository {repo}"),
            &report_of(full)?,
            true,
        );
    }
    Ok(())
}

fn conflict(body: &[u8]) -> String {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let mut text = value["message"]
        .as_str()
        .unwrap_or("The server refused to start the cleanup.")
        .to_owned();
    for problem in value["problems"].as_array().into_iter().flatten() {
        text.push_str("\n- ");
        text.push_str(
            problem["message"]
                .as_str()
                .unwrap_or("A safety check failed."),
        );
        let hosts: Vec<_> = problem["hosts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| h["hostname"].as_str())
            .collect();
        if !hosts.is_empty() {
            text.push_str(&format!(" ({})", hosts.join(", ")));
        }
    }
    text
}

fn marker_line(marker: &Marker) -> String {
    format!(
        "{} · lode backup · started {}",
        marker.host.as_deref().unwrap_or("unknown host"),
        marker.started_at.map_or_else(|| "-".into(), utc_time),
    )
}

async fn start(client: &Client, out: Output, repo: &str) -> Result<()> {
    let id = resolve(client, repo).await?;
    let (code, body) = client
        .raw_post(&format!("{}/run", gc_path(&id)), &json!({}))
        .await?;
    let started: Started = match code {
        StatusCode::ACCEPTED => serde_json::from_slice(&body)?,
        StatusCode::CONFLICT => bail!("{}", conflict(&body)),
        _ => return Err(failure(code, &body).into()),
    };
    let done = follow(
        client,
        &id,
        &started.cycle_id,
        false,
        out,
        true,
        "Cleanup stopped",
    )
    .await?;
    match done {
        Followed::Waiting(cycle) if out.json => out.emit("gc_cycle", &cycle),
        Followed::Waiting(cycle) => {
            let after = cycle
                .delete_after
                .map_or_else(|| "the pause".into(), utc_time);
            println!(
                "Marked {} ({}). Deletion after {after}.",
                plural(cycle.marked_packs, "pack"),
                size(cycle.marked_bytes),
            );
        }
        Followed::Done(full) if out.json => out.emit("gc_report", &full),
        Followed::Done(full) => {
            print_report(
                &format!("Cleanup of repository {repo}"),
                &report_of(full)?,
                false,
            );
        }
    }
    Ok(())
}

fn print_cycle(cycle: &Cycle) {
    println!("{}", heading(cycle));
    let counter = cycle.progress.as_ref().filter(|p| p.total > 0);
    if let Some(p) = counter {
        println!("  {} of {}", p.done, p.total);
    }
    let pack_line = format!(
        "{} ({})",
        plural(cycle.marked_packs, "pack"),
        size(cycle.marked_bytes)
    );
    match cycle.state.as_str() {
        "waiting" if cycle.blocked_by_writers.is_empty() => {
            let after = cycle
                .delete_after
                .map_or_else(|| "the pause".into(), utc_time);
            println!("  {pack_line} will be deleted after {after}.");
            println!("  The pause lets running backups finish first.");
        }
        "waiting" => {
            println!(
                "  {} are marked. Deletion waits for the backups listed below.",
                plural(cycle.marked_packs, "pack")
            );
            for marker in &cycle.blocked_by_writers {
                println!("  {}", marker_line(marker));
            }
        }
        "verifying" => {
            println!("  Comparing the marked packs with the current snapshots.");
            if cycle.waiting_for_restores {
                println!("  Waiting for running restores to finish.");
            }
        }
        "sweeping" => println!("  This step can't be aborted."),
        _ => {}
    }
}

async fn status(client: &Client, out: Output, repo: &str) -> Result<()> {
    let id = resolve(client, repo).await?;
    let raw: Value = client.get_json(&gc_path(&id), &[]).await?;
    if out.json {
        out.emit("gc_status", &raw);
        return Ok(());
    }
    let status: Status = serde_json::from_value(raw)?;
    if let Some(cycle) = &status.cycle {
        print_cycle(cycle);
    } else if status.enabled {
        println!("Idle");
        match status.schedule.as_ref().and_then(|s| s.next_runs.first()) {
            Some(next) => println!("  Next run {}.", utc_time(*next)),
            None => println!("  No schedule. Run it by hand."),
        }
    } else {
        println!("Cleanup is off");
        println!("  Nothing is deleted from this repository.");
    }
    if status.cycle.is_none()
        && let Some(at) = status.last_aborted_at
    {
        println!(
            "  The last cycle was aborted {}. Nothing was deleted.",
            utc_time(at)
        );
    }
    for (title, report, dry) in [
        ("Last cleanup", status.last_cleanup, false),
        ("Last dry run", status.last_dry_run, true),
    ] {
        if let Some(report) = report {
            println!();
            print_report(
                &format!("{title}, finished {}", utc_time(report.finished_at)),
                &report,
                dry,
            );
        }
    }
    Ok(())
}

fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(UsageError("Use --yes to unlock without a prompt.".into()).into());
    }
    let mut err = std::io::stderr().lock();
    write!(err, "{question} [y/N] ")?;
    err.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

async fn unlock(client: &Client, out: Output, repo: &str, yes: bool) -> Result<()> {
    let id = resolve(client, repo).await?;
    let status: Status = client.get_json(&gc_path(&id), &[]).await?;
    let markers = status
        .cycle
        .map(|c| c.blocked_by_writers)
        .unwrap_or_default();
    if markers.is_empty() {
        if out.json {
            out.emit("gc_unlock", json!({ "unlocked": 0 }));
        } else {
            println!("There are no markers to unlock.");
        }
        return Ok(());
    }
    let mut hosts: Vec<&str> = markers
        .iter()
        .map(|m| m.host.as_deref().unwrap_or("unknown host"))
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    let n = markers.len() as u64;
    if !out.json {
        println!(
            "Cleanup stops waiting for: {}. {UNLOCK_WARNING}",
            hosts.join(", ")
        );
    }
    if !confirm(&format!("Unlock {}?", plural(n, "marker")), yes)? {
        bail!("Nothing was unlocked.");
    }
    let done: Unlocked = client
        .post_json(&format!("{}/unlock", gc_path(&id)), None::<&()>)
        .await?;
    if out.json {
        out.emit("gc_unlock", json!({ "unlocked": done.unlocked }));
    } else {
        println!(
            "Unlocked {}. Cleanup continues.",
            plural(done.unlocked, "marker")
        );
    }
    Ok(())
}
