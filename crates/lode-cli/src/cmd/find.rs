use std::collections::BTreeMap;

use anyhow::{Result, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::gc::{POLL, POLL_RETRIES, transient};
use super::server::{table, time};
use crate::cli::{ServerFindArgs, UsageError};
use crate::client::{Client, failure, seg};
use crate::output::{Output, spinner};

#[derive(Deserialize)]
struct Started {
    id: String,
}

#[derive(Deserialize)]
struct RestoreHost {
    key: String,
    hostname: String,
    agent: bool,
    status: Option<String>,
}

#[derive(Deserialize)]
struct Snapshot {
    id: String,
    time_ms: i64,
}

#[derive(Deserialize)]
struct Version {
    last_ms: i64,
    snapshots: Vec<Snapshot>,
}

#[derive(Deserialize)]
struct Group {
    host_key: String,
    hostname: String,
    path: String,
    last_seen_ms: i64,
    versions: Vec<Version>,
}

impl Group {
    /// The newest snapshot of the newest version.
    fn newest_snapshot(&self) -> Option<&str> {
        let version = self.versions.iter().max_by_key(|v| v.last_ms)?;
        let snapshot = version.snapshots.iter().max_by_key(|s| s.time_ms)?;
        Some(&snapshot.id)
    }
}

#[derive(Deserialize)]
struct Progress {
    state: String,
    stop_reason: Option<String>,
    limit: u64,
    scanned: u64,
    total: u64,
    files: u64,
    started_at: i64,
    finished_at: Option<i64>,
    error: Option<String>,
    revision: u64,
    groups: Vec<Value>,
}

/// Text that came from a bucket or a host, with control characters replaced so
/// that a file name cannot steer the terminal.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:,=@%".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

fn duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..60 => format!("{secs} seconds"),
        60..120 => "1 minute".into(),
        _ => format!("{} minutes", secs / 60),
    }
}

fn valid_label(label: &str) -> bool {
    label
        .split_once('=')
        .is_some_and(|(key, _)| !key.is_empty())
}

fn host_keys(wanted: &[String], known: &[RestoreHost]) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for host in wanted {
        let mut matched = false;
        for k in known
            .iter()
            .filter(|k| k.key == *host || k.hostname == *host)
        {
            matched = true;
            if !keys.contains(&k.key) {
                keys.push(k.key.clone());
            }
        }
        if !matched {
            return Err(UsageError(format!(
                "No host matches {host:?}. See `lode server hosts list`."
            ))
            .into());
        }
    }
    Ok(keys)
}

/// Reads the search, trying again a few times when the server is briefly unreachable.
async fn poll(client: &Client, path: &str, after: u64) -> Result<Value> {
    let mut failures = 0;
    loop {
        match client.get_json(path, &[("after", after.to_string())]).await {
            Err(e) if transient(&e) => {
                failures += 1;
                if failures > POLL_RETRIES {
                    return Err(e.context("Lost contact with the server. Run the search again"));
                }
                tokio::time::sleep(POLL).await;
            }
            other => return other,
        }
    }
}

fn restore_hint(group: &Group, known: &[RestoreHost]) -> Option<String> {
    let snapshot = group.newest_snapshot()?;
    if group
        .path
        .chars()
        .any(|c| c.is_control() || c == '\u{fffd}')
        || group.hostname.chars().any(char::is_control)
    {
        // shortcut: a path or host name that is not plain text cannot be typed back; the web UI restores it
        return None;
    }
    let path = shell_word(&group.path);
    let online = known
        .iter()
        .any(|k| k.key == group.host_key && k.agent && k.status.as_deref() == Some("online"));
    Some(if online {
        format!(
            "lode server restore {snapshot} --host {} --path {path}",
            shell_word(&group.hostname)
        )
    } else {
        format!(
            "{} has no online agent. Restore to another host with lode server restore {snapshot} --host <HOST> --path {path}, or download it from the web UI.",
            clean(&group.hostname)
        )
    })
}

/// Searches every host for a file, waits for the end of the search and prints
/// the matches with a command that restores the first one.
pub async fn run_server(client: &Client, out: Output, args: ServerFindArgs) -> Result<u8> {
    if let Some(bad) = args.labels.iter().find(|l| !valid_label(l)) {
        return Err(UsageError(format!("The label {bad:?} must look like KEY=VALUE.")).into());
    }
    let known: Vec<RestoreHost> = if args.hosts.is_empty() {
        client
            .get_json("/api/v1/restore/hosts", &[])
            .await
            .unwrap_or_default()
    } else {
        client.get_json("/api/v1/restore/hosts", &[]).await?
    };
    let hosts = host_keys(&args.hosts, &known)?;
    let (code, body) = client
        .raw_post(
            "/api/v1/find",
            &json!({"pattern": args.pattern, "hosts": hosts, "labels": args.labels}),
        )
        .await?;
    let started: Started = match code {
        StatusCode::ACCEPTED => serde_json::from_slice(&body)?,
        StatusCode::UNPROCESSABLE_ENTITY => {
            let message = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v["errors"][0]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "The pattern is not valid.".into());
            return Err(UsageError(message).into());
        }
        _ => return Err(failure(code, &body).into()),
    };
    let path = format!("/api/v1/find/{}", seg(&started.id));
    let bar = (!out.json).then(spinner).flatten();
    let mut groups: BTreeMap<u64, Value> = BTreeMap::new();
    let mut after = 0;
    let (raw, progress) = loop {
        let raw = poll(client, &path, after).await?;
        let progress: Progress = serde_json::from_value(raw.clone())?;
        for group in &progress.groups {
            if let Some(index) = group["index"].as_u64() {
                groups.insert(index, group.clone());
            }
        }
        after = progress.revision;
        if progress.state != "running" {
            break (raw, progress);
        }
        if let Some(bar) = &bar {
            bar.set_message(format!(
                "Searching ({} of {} snapshots, {} files found)",
                progress.scanned, progress.total, progress.files
            ));
        }
        tokio::time::sleep(POLL).await;
    };
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    match progress.state.as_str() {
        "failed" => bail!(
            "{}",
            progress
                .error
                .as_deref()
                .map_or_else(|| "The search failed.".to_owned(), clean)
        ),
        "cancelled" => bail!("The search was cancelled."),
        _ => {}
    }
    if progress.state == "incomplete" {
        match progress.stop_reason.as_deref() {
            Some("timeout") => eprintln!(
                "Stopped after {}. The result is incomplete; narrow the search with --host or --label.",
                duration(progress.finished_at.unwrap_or(progress.started_at) - progress.started_at)
            ),
            _ => eprintln!(
                "Stopped at {} matches. The result is incomplete; narrow the search with --host, --label or a longer pattern.",
                progress.limit
            ),
        }
    }
    if out.json {
        let mut session = raw;
        session["groups"] = Value::Array(groups.into_values().collect());
        out.emit("find_session", session);
        return Ok(0);
    }
    let groups: Vec<Group> = groups
        .into_values()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()?;
    if groups.is_empty() {
        println!("No files match \"{}\".", clean(&args.pattern));
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|g| {
            vec![
                clean(&g.hostname),
                clean(&g.path),
                g.versions.len().to_string(),
                time(Some(g.last_seen_ms)),
                g.newest_snapshot().unwrap_or("-").to_owned(),
            ]
        })
        .collect();
    table(
        &["HOST", "PATH", "VERSIONS", "LAST SEEN", "SNAPSHOT"],
        &rows,
    );
    if let Some(hint) = groups.first().and_then(|g| restore_hint(g, &known)) {
        println!("\nTo restore the newest version of the first match:\n  {hint}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(hostname: &str) -> Group {
        serde_json::from_value(json!({
            "host_key": "agent-1",
            "hostname": hostname,
            "path": "/etc/hosts",
            "last_seen_ms": 1,
            "versions": [{ "last_ms": 1, "snapshots": [{ "id": "ab", "time_ms": 1 }] }],
        }))
        .unwrap()
    }

    #[test]
    fn restore_hint_skips_a_host_name_with_control_characters() {
        let known = [RestoreHost {
            key: "agent-1".into(),
            hostname: "web".into(),
            agent: true,
            status: Some("online".into()),
        }];
        assert!(restore_hint(&group("web"), &known).is_some());
        assert!(restore_hint(&group("web\u{1b}[2J"), &known).is_none());
    }
}
