use std::io::Write;

use anyhow::Result;
use lodestore_format::reader::parse_snapshot_path;

use crate::cli::SecretArgs;
use crate::open::open_for_read;

pub async fn run(url: &str, args: &SecretArgs, snapshot: &str, path: &str) -> Result<()> {
    let mut reader = open_for_read(url, args).await?;
    let (snapshot, _) = parse_snapshot_path(snapshot);
    let entry = reader.resolve_snapshot(snapshot).await?;
    let mut out = std::io::stdout().lock();
    reader
        .dump_file(&entry.id, path.as_bytes(), &mut out)
        .await?;
    out.flush()?;
    Ok(())
}
