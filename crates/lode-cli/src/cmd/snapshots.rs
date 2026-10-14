use anyhow::Result;
use lodestore_format::index::hex32;
use serde::Serialize;

use crate::cli::SecretArgs;
use crate::open::open_for_read;
use crate::output::{JsonPath, Output, StatsJson, hex, rfc3339, utc, warn_skipped};

#[derive(Serialize)]
struct SnapshotJson<'a> {
    id: String,
    short_id: String,
    time: String,
    hostname: JsonPath<'a>,
    paths: Vec<JsonPath<'a>>,
    tags: &'a [String],
    incomplete: bool,
    parent: Option<String>,
    stats: StatsJson,
}

#[derive(Serialize)]
struct SnapshotsJson<'a> {
    snapshots: Vec<SnapshotJson<'a>>,
}

pub async fn run(url: &str, args: &SecretArgs, out: Output) -> Result<()> {
    let reader = open_for_read(url, args).await?;
    let list = reader.list_snapshots().await?;
    warn_skipped(&list.skipped);
    let entries = list.entries;
    if out.json {
        let snapshots = entries
            .iter()
            .map(|e| {
                let snap = &e.snapshot;
                let id = hex32(&e.id);
                SnapshotJson {
                    short_id: id[..8].to_owned(),
                    id,
                    time: rfc3339(snap.time.as_ref()),
                    hostname: JsonPath(&snap.hostname),
                    paths: snap.paths.iter().map(|p| JsonPath(p)).collect(),
                    tags: &snap.tags,
                    incomplete: snap.incomplete,
                    parent: Some(hex(&snap.parent)).filter(|p| !p.is_empty()),
                    stats: snap.stats.as_ref().into(),
                }
            })
            .collect();
        out.emit("snapshots", SnapshotsJson { snapshots });
        return Ok(());
    }
    for entry in entries {
        let snap = &entry.snapshot;
        let paths: Vec<_> = snap
            .paths
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        println!(
            "{}  {}  {}  {}",
            &hex32(&entry.id)[..8],
            utc(snap.time.as_ref()),
            String::from_utf8_lossy(&snap.hostname),
            paths.join(",")
        );
    }
    Ok(())
}
