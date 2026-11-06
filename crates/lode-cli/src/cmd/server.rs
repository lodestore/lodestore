use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use data_encoding::BASE64URL_NOPAD;
use lodestore_format::pb;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cli::{
    ConflictArg, HostsCommand, JobsCommand, PoliciesCommand, ServerCommand, ServerRestoreArgs,
    ServerSnapshotsCommand, UsageError,
};
use crate::client::{Client, seg};
use crate::output::{Output, utc};
use crate::profile::Profile;

pub async fn run(
    server: Option<String>,
    profile: Option<&Profile>,
    out: Output,
    command: ServerCommand,
) -> Result<u8> {
    let client = Client::resolve(server, profile)?;
    match command {
        ServerCommand::Hosts { command } => hosts(&client, out, command).await?,
        ServerCommand::Policies { command } => policies(&client, out, command).await?,
        ServerCommand::Jobs { command } => jobs(&client, out, command).await?,
        ServerCommand::Snapshots { command } => snapshots(&client, out, command).await?,
        ServerCommand::Restore(args) => restore(&client, out, args).await?,
        ServerCommand::Gc(args) => return super::gc::run(&client, out, args).await,
        ServerCommand::Check(args) => return super::check::run_server(&client, out, args).await,
        ServerCommand::Find(args) => return super::find::run_server(&client, out, args).await,
    }
    Ok(0)
}

pub(super) fn time(ms: Option<i64>) -> String {
    ms.map_or_else(
        || "-".into(),
        |ms| {
            utc(Some(&pb::Timestamp {
                seconds: ms.div_euclid(1000),
                nanos: 0,
            }))
        },
    )
}

pub(super) fn table(header: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let last = header.len() - 1;
    let print = |cells: &mut dyn Iterator<Item = &str>| {
        let mut line = String::new();
        for (i, cell) in cells.enumerate() {
            line.push_str(cell);
            if i < last {
                let pad = widths[i] - cell.chars().count() + 2;
                line.push_str(&" ".repeat(pad));
            }
        }
        println!("{}", line.trim_end());
    };
    print(&mut header.iter().copied());
    for row in rows {
        print(&mut row.iter().map(String::as_str));
    }
}

#[derive(Serialize, Deserialize)]
struct Label {
    key: String,
    value: String,
}

fn labels_text(labels: &[Label]) -> String {
    if labels.is_empty() {
        return "-".into();
    }
    let parts: Vec<_> = labels
        .iter()
        .map(|l| format!("{}={}", l.key, l.value))
        .collect();
    parts.join(",")
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

#[derive(Serialize, Deserialize)]
struct Host {
    id: String,
    hostname: String,
    status: String,
    repository_id: String,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    last_backup_at: Option<i64>,
}

#[derive(Deserialize)]
struct RestoreHost {
    host_id: Option<String>,
    repository_name: Option<String>,
    latest_snapshot_ms: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct HostDetail {
    id: String,
    hostname: String,
    status: String,
    version: String,
    os: String,
    arch: String,
    #[serde(default)]
    distro: Option<String>,
    #[serde(default)]
    labels: Vec<Label>,
    repository_id: String,
    #[serde(default)]
    repository_name: String,
    #[serde(default)]
    kernel: Option<String>,
    #[serde(default)]
    last_seen_at: Option<i64>,
    cert_not_after: i64,
    enrolled_at: i64,
}

/// Turns a hostname into the id of its host that is not revoked. An id, an
/// unknown name, or a token that may not list hosts passes the text through,
/// and the server answers for it.
async fn resolve_host(client: &Client, host: &str) -> Result<String> {
    let Ok(hosts) = client.get_json::<Vec<Host>>("/api/v1/hosts", &[]).await else {
        return Ok(host.to_owned());
    };
    if hosts.iter().any(|h| h.id == host) {
        return Ok(host.to_owned());
    }
    let mut named = hosts
        .into_iter()
        .filter(|h| h.hostname == host && h.status != "revoked");
    match (named.next(), named.next()) {
        (Some(only), None) => Ok(only.id),
        (None, _) => Ok(host.to_owned()),
        (Some(_), Some(_)) => Err(UsageError(format!(
            "Several hosts are named {host:?}. Use the id from `lode server hosts list`."
        ))
        .into()),
    }
}

async fn hosts(client: &Client, out: Output, command: HostsCommand) -> Result<()> {
    match command {
        HostsCommand::List => {
            let mut hosts: Vec<Host> = client.get_json("/api/v1/hosts", &[]).await?;
            // The repository name and last backup come from the restore view;
            // a token without its scope still gets the host list.
            if let Ok(known) = client
                .get_json::<Vec<RestoreHost>>("/api/v1/restore/hosts", &[])
                .await
            {
                for host in &mut hosts {
                    if let Some(k) = known
                        .iter()
                        .find(|k| k.host_id.as_deref() == Some(&host.id))
                    {
                        host.repository.clone_from(&k.repository_name);
                        host.last_backup_at = k.latest_snapshot_ms;
                    }
                }
            }
            if out.json {
                for host in &hosts {
                    out.emit("host", host);
                }
                return Ok(());
            }
            let rows: Vec<_> = hosts
                .iter()
                .map(|h| {
                    vec![
                        h.hostname.clone(),
                        h.id.clone(),
                        capitalized(&h.status),
                        h.repository
                            .clone()
                            .unwrap_or_else(|| h.repository_id.clone()),
                        labels_text(&h.labels),
                        time(h.last_backup_at),
                    ]
                })
                .collect();
            table(
                &[
                    "HOST",
                    "ID",
                    "STATUS",
                    "REPOSITORY",
                    "LABELS",
                    "LAST BACKUP",
                ],
                &rows,
            );
        }
        HostsCommand::Show { id } => {
            let id = resolve_host(client, &id).await?;
            let host: HostDetail = client
                .get_json(&format!("/api/v1/hosts/{}", seg(&id)), &[])
                .await?;
            print_host(out, &host);
        }
        HostsCommand::Label { id, labels, clear } => {
            let labels: Vec<Label> = if clear {
                Vec::new()
            } else {
                labels
                    .iter()
                    .map(|text| {
                        text.split_once('=')
                            .map(|(key, value)| Label {
                                key: key.into(),
                                value: value.into(),
                            })
                            .ok_or_else(|| UsageError(format!("{text:?} is not KEY=VALUE")))
                    })
                    .collect::<Result<_, _>>()?
            };
            let id = resolve_host(client, &id).await?;
            let host: HostDetail = client
                .put_json(
                    &format!("/api/v1/hosts/{}/labels", seg(&id)),
                    &json!({ "labels": labels }),
                )
                .await?;
            print_host(out, &host);
        }
    }
    Ok(())
}

fn print_host(out: Output, host: &HostDetail) {
    if out.json {
        out.emit("host", host);
        return;
    }
    let lines = [
        ("Host", host.hostname.clone()),
        ("Id", host.id.clone()),
        ("Status", capitalized(&host.status)),
        ("Repository", host.repository_name.clone()),
        ("Labels", labels_text(&host.labels)),
        ("Last seen", time(host.last_seen_at)),
        ("Agent", host.version.clone()),
        (
            "System",
            format!(
                "{} {} {}",
                host.os,
                host.arch,
                host.distro.as_deref().unwrap_or("")
            )
            .trim_end()
            .to_owned(),
        ),
    ];
    for (name, value) in lines {
        println!("{:<12}{value}", format!("{name}:"));
    }
}

fn state_word(state: &str) -> String {
    match state {
        "warnings" => "Completed with warnings".into(),
        "not_started" => "Not started".into(),
        other => capitalized(other),
    }
}

fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(UsageError("Use --yes to delete without a prompt.".into()).into());
    }
    let mut err = std::io::stderr().lock();
    write!(err, "{question} [y/N] ")?;
    err.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[derive(Serialize, Deserialize)]
struct JobBrief {
    id: String,
    state: String,
    queued_at: i64,
}

#[derive(Serialize, Deserialize)]
struct Policy {
    name: String,
    repository_id: String,
    repository_name: String,
    hosts: i64,
    #[serde(default)]
    last_job: Option<JobBrief>,
}

#[derive(Serialize, Deserialize)]
struct PolicyDetail {
    name: String,
    revision: i64,
    yaml: String,
}

#[derive(Serialize, Deserialize)]
struct QueuedJob {
    id: String,
    host_id: String,
    hostname: String,
    created: bool,
}

#[derive(Deserialize)]
struct RunResult {
    jobs: Vec<QueuedJob>,
}

fn print_queued(out: Output, jobs: &[QueuedJob]) {
    for job in jobs {
        if out.json {
            out.emit("queued", job);
        } else if job.created {
            println!("Queued job {} on {}", job.id, job.hostname);
        } else {
            println!("Job {} on {} is already queued", job.id, job.hostname);
        }
    }
}

async fn policies(client: &Client, out: Output, command: PoliciesCommand) -> Result<()> {
    match command {
        PoliciesCommand::List => {
            let policies: Vec<Policy> = client.get_json("/api/v1/policies", &[]).await?;
            if out.json {
                for policy in &policies {
                    out.emit("policy", policy);
                }
                return Ok(());
            }
            let rows: Vec<_> = policies
                .iter()
                .map(|p| {
                    vec![
                        p.name.clone(),
                        p.repository_name.clone(),
                        p.hosts.to_string(),
                        p.last_job.as_ref().map_or_else(
                            || "-".into(),
                            |j| format!("{} {}", time(Some(j.queued_at)), state_word(&j.state)),
                        ),
                    ]
                })
                .collect();
            table(&["NAME", "REPOSITORY", "HOSTS", "LAST RUN"], &rows);
        }
        PoliciesCommand::Get { name } => {
            let policy: PolicyDetail = client
                .get_json(&format!("/api/v1/policies/{}", seg(&name)), &[])
                .await?;
            if out.json {
                out.emit("policy", &policy);
            } else {
                print!("{}", policy.yaml);
            }
        }
        PoliciesCommand::Delete { name, yes } => {
            if !confirm(&format!("Delete policy {name}?"), yes)? {
                return Ok(());
            }
            client
                .delete(&format!("/api/v1/policies/{}", seg(&name)))
                .await?;
            if out.json {
                out.emit("policy", json!({ "name": name, "deleted": true }));
            } else {
                println!("Policy {name} deleted.");
            }
        }
        PoliciesCommand::Retention { name } => retention(client, out, &name).await?,
        PoliciesCommand::Run { name } => {
            let result: RunResult = client
                .post_json(&format!("/api/v1/policies/{}/run", seg(&name)), None::<&()>)
                .await?;
            if result.jobs.is_empty() && !out.json {
                println!("No host matches policy {name}.");
            }
            print_queued(out, &result.jobs);
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct PreviewSnapshot {
    id: String,
    time: i64,
}

#[derive(Serialize, Deserialize)]
struct HostPreview {
    host_id: String,
    hostname: String,
    total: u64,
    forget: u64,
    snapshots: Vec<PreviewSnapshot>,
}

#[derive(Deserialize)]
struct RetentionPreview {
    total: u64,
    forget: u64,
    suspicious: u64,
    truncated: u64,
    hosts: Vec<HostPreview>,
}

async fn retention(client: &Client, out: Output, name: &str) -> Result<()> {
    let detail: serde_json::Value = client
        .get_json(&format!("/api/v1/policies/{}", seg(name)), &[])
        .await?;
    let spec = &detail["policy"]["spec"];
    let rules = &spec["retention"];
    if rules.is_null() {
        if !out.json {
            println!("Policy {name} has no retention rules.");
        }
        return Ok(());
    }
    let mut body = json!({ "retention": rules });
    if let Some(zone) = spec["schedule"]["timezone"].as_str() {
        body["timezone"] = json!(zone);
    }
    let preview: RetentionPreview = client
        .post_json(
            &format!("/api/v1/policies/{}/retention-preview", seg(name)),
            Some(&body),
        )
        .await?;
    if out.json {
        for host in &preview.hosts {
            out.emit("retention_host", host);
        }
        out.emit(
            "retention_summary",
            json!({
                "total": preview.total,
                "forget": preview.forget,
                "suspicious": preview.suspicious,
                "truncated": preview.truncated,
            }),
        );
        return Ok(());
    }
    if preview.forget == 0 {
        println!("Nothing would be forgotten right now.");
    } else {
        println!(
            "Would forget {} of {} snapshots on {} hosts.",
            preview.forget,
            preview.total,
            preview.hosts.len()
        );
    }
    if preview.suspicious > 0 {
        println!(
            "{} suspicious snapshots are not counted and are never forgotten.",
            preview.suspicious
        );
    }
    for host in preview.hosts.iter().filter(|h| h.forget > 0) {
        println!();
        println!(
            "{} (forget {} of {})",
            host.hostname, host.forget, host.total
        );
        for snapshot in &host.snapshots {
            println!("  {}  {}", snapshot.id, time(Some(snapshot.time)));
        }
    }
    if preview.truncated > 0 {
        println!();
        println!("{} more snapshots are not listed.", preview.truncated);
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Job {
    id: String,
    kind: String,
    host_id: String,
    hostname: String,
    #[serde(default)]
    policy_name: Option<String>,
    state: String,
    queued_at: i64,
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    finished_at: Option<i64>,
    #[serde(default)]
    snapshot_id: Option<String>,
    #[serde(default)]
    warnings_total: u64,
    #[serde(default)]
    errors_total: u64,
    #[serde(default)]
    error_message: Option<String>,
}

#[derive(Deserialize)]
struct JobPage {
    items: Vec<Job>,
    total: u64,
    page: u64,
    page_size: u64,
}

#[derive(Serialize, Deserialize)]
struct Event {
    seq: i64,
    at: i64,
    level: String,
    kind: String,
    #[serde(default)]
    path: Option<String>,
    message: String,
}

#[derive(Deserialize)]
struct EventPage {
    events: Vec<Event>,
    last_seq: i64,
}

const FOLLOW_INTERVAL: Duration = Duration::from_secs(2);

async fn jobs(client: &Client, out: Output, command: JobsCommand) -> Result<()> {
    match command {
        JobsCommand::List {
            state,
            policy,
            host,
            kind,
            page,
        } => {
            let host = match host {
                Some(host) => Some(resolve_host(client, &host).await?),
                None => None,
            };
            let query: Vec<(&str, String)> = [
                ("status", state),
                ("policy", policy),
                ("host_id", host),
                ("kind", kind),
                ("page", page.map(|p| p.to_string())),
            ]
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| (key, v)))
            .collect();
            let page: JobPage = client.get_json("/api/v1/jobs", &query).await?;
            if out.json {
                for job in &page.items {
                    out.emit("job", job);
                }
                return Ok(());
            }
            let rows: Vec<_> = page
                .items
                .iter()
                .map(|j| {
                    vec![
                        j.id.clone(),
                        j.hostname.clone(),
                        j.policy_name.clone().unwrap_or_else(|| "-".into()),
                        state_word(&j.state),
                        time(Some(j.queued_at)),
                    ]
                })
                .collect();
            table(&["JOB", "HOST", "POLICY", "STATE", "QUEUED"], &rows);
            if page.total > page.page_size {
                eprintln!(
                    "Page {} of {}. Use --page to see more.",
                    page.page,
                    page.total.div_ceil(page.page_size.max(1))
                );
            }
        }
        JobsCommand::Show { id } => {
            let job: Job = client
                .get_json(&format!("/api/v1/jobs/{}", seg(&id)), &[])
                .await?;
            print_job(out, &job);
        }
        JobsCommand::Logs { id, follow } => {
            let mut after = 0;
            loop {
                after = print_events(client, out, &id, after).await?;
                if !follow {
                    break;
                }
                let job: Job = client
                    .get_json(&format!("/api/v1/jobs/{}", seg(&id)), &[])
                    .await?;
                if !matches!(job.state.as_str(), "queued" | "running") {
                    print_events(client, out, &id, after).await?;
                    break;
                }
                tokio::time::sleep(FOLLOW_INTERVAL).await;
            }
        }
        JobsCommand::Run { host, policy } => {
            let host = resolve_host(client, &host).await?;
            let body = (!policy.is_empty()).then(|| json!({ "policies": policy }));
            let result: RunResult = client
                .post_json(&format!("/api/v1/hosts/{}/run", seg(&host)), body.as_ref())
                .await?;
            print_queued(out, &result.jobs);
        }
        JobsCommand::Cancel { id } => {
            let job: Job = client
                .post_json(&format!("/api/v1/jobs/{}/cancel", seg(&id)), None::<&()>)
                .await?;
            if out.json {
                out.emit("job", &job);
            } else if job.state == "cancelled" {
                println!("Job {} cancelled.", job.id);
            } else {
                println!("Cancel requested for job {}.", job.id);
            }
        }
        JobsCommand::Rerun { id } => {
            let job: QueuedJob = client
                .post_json(&format!("/api/v1/jobs/{}/rerun", seg(&id)), None::<&()>)
                .await?;
            print_queued(out, &[job]);
        }
    }
    Ok(())
}

async fn print_events(client: &Client, out: Output, id: &str, after: i64) -> Result<i64> {
    let page: EventPage = client
        .get_json(
            &format!("/api/v1/jobs/{}/events", seg(id)),
            &[("after", after.to_string())],
        )
        .await?;
    for event in &page.events {
        if out.json {
            out.emit("event", event);
            continue;
        }
        let message = match &event.path {
            Some(path) => format!("{path}: {}", event.message),
            None => event.message.clone(),
        };
        println!("{}  {}  {message}", time(Some(event.at)), event.level);
    }
    Ok(page.last_seq.max(after))
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SnapshotStats {
    files_new: u64,
    files_changed: u64,
    files_unchanged: u64,
    bytes_total: u64,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    id: String,
    time: i64,
    hostname: String,
    #[serde(default)]
    policy_name: Option<String>,
    #[serde(default)]
    stdin_name: Option<String>,
    #[serde(default)]
    incomplete: bool,
    #[serde(default)]
    suspicious: Option<serde_json::Value>,
    #[serde(default)]
    policy_deleted: bool,
    #[serde(default)]
    stats: SnapshotStats,
}

#[derive(Deserialize)]
struct SnapshotPage {
    items: Vec<Snapshot>,
    #[serde(default)]
    next: Option<String>,
}

const SNAPSHOT_PAGE: usize = 200;

pub(super) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn id8(id: &str) -> String {
    id.chars().take(8).collect()
}

fn full_id(id: &str) -> Result<(), UsageError> {
    if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(UsageError(
            "The snapshot must be a full 64-character id; see lode server snapshots list.".into(),
        ))
    }
}

fn status_words(snapshot: &Snapshot) -> String {
    let mut words = Vec::new();
    if snapshot.suspicious.is_some() {
        words.push("Suspicious");
    }
    if snapshot.incomplete {
        words.push("Incomplete");
    }
    if snapshot.policy_deleted {
        words.push("Policy deleted");
    }
    if words.is_empty() {
        "Complete".into()
    } else {
        words.join(", ")
    }
}

#[derive(Deserialize)]
struct DeletedSnapshot {
    id: String,
    storage_kept: bool,
    #[serde(default)]
    reason: Option<String>,
}

async fn mark_snapshot(
    client: &Client,
    out: Output,
    id: &str,
    suspicious: bool,
    reason: String,
) -> Result<()> {
    full_id(id)?;
    let snapshot: Snapshot = client
        .put_json(
            &format!("/api/v1/snapshots/{}/suspicious", seg(id)),
            &json!({ "suspicious": suspicious, "reason": reason }),
        )
        .await?;
    if out.json {
        out.emit("snapshot", &snapshot);
    } else if suspicious {
        println!("Snapshot {} marked as suspicious.", id8(id));
    } else {
        println!("Suspicious mark cleared on snapshot {}.", id8(id));
    }
    Ok(())
}

async fn delete_snapshot(client: &Client, out: Output, id: &str, yes: bool) -> Result<()> {
    full_id(id)?;
    if !yes && std::io::stdin().is_terminal() {
        eprintln!(
            "It is removed from the repository, from Restore and from lode. This cannot be \
undone. Storage space is freed later by garbage collection."
        );
    }
    if !confirm(&format!("Delete snapshot {}?", id8(id)), yes)? {
        return Ok(());
    }
    let deleted: DeletedSnapshot = client
        .delete_json(&format!("/api/v1/snapshots/{}", seg(id)))
        .await?;
    if out.json {
        out.emit(
            "snapshot",
            json!({
                "id": deleted.id,
                "deleted": true,
                "storage_kept": deleted.storage_kept,
                "reason": deleted.reason,
            }),
        );
    } else if !deleted.storage_kept {
        println!("Snapshot {} deleted.", id8(id));
    }
    if deleted.storage_kept {
        eprintln!(
            "warning: Snapshot {} was removed from the catalog, but storage kept the file because of Object Lock. It stays out of Restore.",
            id8(id)
        );
    }
    Ok(())
}

async fn snapshots(client: &Client, out: Output, command: ServerSnapshotsCommand) -> Result<()> {
    let (host, limit) = match command {
        ServerSnapshotsCommand::List { host, limit } => (host, limit),
        ServerSnapshotsCommand::Mark { id, reason } => {
            return mark_snapshot(client, out, &id, true, reason).await;
        }
        ServerSnapshotsCommand::Clear { id, reason } => {
            return mark_snapshot(client, out, &id, false, reason).await;
        }
        ServerSnapshotsCommand::Delete { id, yes } => {
            return delete_snapshot(client, out, &id, yes).await;
        }
    };
    let host = resolve_host(client, &host).await?;
    let mut found: Vec<Snapshot> = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let wanted = limit.map_or(SNAPSHOT_PAGE, |l| {
            (l - found.len().min(l)).clamp(1, SNAPSHOT_PAGE)
        });
        let mut query = vec![("host", host.clone()), ("limit", wanted.to_string())];
        query.extend(before.take().map(|cursor| ("before", cursor)));
        let page: SnapshotPage = client.get_json("/api/v1/restore/snapshots", &query).await?;
        found.extend(page.items);
        before = page.next;
        if before.is_none() || limit.is_some_and(|l| found.len() >= l) {
            break;
        }
    }
    if let Some(limit) = limit {
        found.truncate(limit);
    }
    if out.json {
        for snapshot in &found {
            out.emit("snapshot", snapshot);
        }
        return Ok(());
    }
    let rows: Vec<_> = found
        .iter()
        .map(|s| {
            let stats = &s.stats;
            vec![
                s.id.clone(),
                time(Some(s.time)),
                match (&s.policy_name, &s.stdin_name) {
                    (Some(policy), _) => policy.clone(),
                    (None, Some(name)) => format!("CLI \u{b7} {name}"),
                    (None, None) => "-".into(),
                },
                (stats.files_new + stats.files_changed + stats.files_unchanged).to_string(),
                size(stats.bytes_total),
                status_words(s),
            ]
        })
        .collect();
    table(
        &["SNAPSHOT", "TIME", "POLICY", "FILES", "SIZE", "STATUS"],
        &rows,
    );
    Ok(())
}

#[derive(Serialize)]
struct RestoreRequest<'a> {
    snapshot_id: &'a str,
    items: Vec<String>,
    target_host_id: &'a str,
    destination: String,
    in_place: bool,
    conflict: ConflictArg,
    restore_owners: bool,
    accept_unverified: bool,
}

#[derive(Serialize, Deserialize)]
struct RestoreCreated {
    job_id: String,
}

async fn restore(client: &Client, out: Output, args: ServerRestoreArgs) -> Result<()> {
    if args.in_place && !args.yes {
        return Err(UsageError("Use --yes to restore in place.".into()).into());
    }
    let id = args.snapshot.as_str();
    full_id(id)?;
    if let Some(bad) = args
        .path
        .iter()
        .chain(&args.target)
        .find(|p| !p.starts_with('/'))
    {
        return Err(UsageError(format!("{bad:?} is not an absolute path")).into());
    }
    let destination = match (&args.target, args.in_place) {
        (Some(target), _) => target.clone(),
        (None, true) => "/".into(),
        (None, false) => {
            let snapshot: Snapshot = client
                .get_json(&format!("/api/v1/snapshots/{}", seg(id)), &[])
                .await?;
            format!(
                "/var/lodestore/restore/{}-{}",
                &time(Some(snapshot.time))[..10],
                &id[..8]
            )
        }
    };
    let items = if args.path.is_empty() {
        vec!["/".to_owned()]
    } else {
        args.path.clone()
    };
    let target_host_id = resolve_host(client, &args.host).await?;
    let request = RestoreRequest {
        snapshot_id: id,
        items: items
            .iter()
            .map(|p| BASE64URL_NOPAD.encode(p.as_bytes()))
            .collect(),
        target_host_id: &target_host_id,
        destination,
        in_place: args.in_place,
        conflict: args.conflict,
        restore_owners: !args.no_owners,
        accept_unverified: args.accept_unverified,
    };
    let created: RestoreCreated = client.post_json("/api/v1/restores", Some(&request)).await?;
    if out.json {
        out.emit("restore", &created);
    } else {
        println!("Restore queued: job {}", created.job_id);
    }
    Ok(())
}

fn print_job(out: Output, job: &Job) {
    if out.json {
        out.emit("job", job);
        return;
    }
    let mut lines = vec![
        ("Job", job.id.clone()),
        ("Kind", capitalized(&job.kind)),
        ("Host", job.hostname.clone()),
        (
            "Policy",
            job.policy_name.clone().unwrap_or_else(|| "-".into()),
        ),
        ("State", state_word(&job.state)),
        ("Queued", time(Some(job.queued_at))),
        ("Started", time(job.started_at)),
        ("Finished", time(job.finished_at)),
        ("Warnings", job.warnings_total.to_string()),
        ("Errors", job.errors_total.to_string()),
    ];
    if let Some(snapshot) = &job.snapshot_id {
        lines.push(("Snapshot", snapshot.clone()));
    }
    if let Some(message) = &job.error_message {
        lines.push(("Error", message.clone()));
    }
    for (name, value) in lines {
        println!("{:<10}{value}", format!("{name}:"));
    }
}
