use std::io::Write;

use anyhow::Result;
use lodestore_format::Error;
use lodestore_format::pb::{self, NodeType};
use lodestore_format::reader::parse_snapshot_path;
use serde::Serialize;

use crate::cli::SecretArgs;
use crate::open::open_for_read;
use crate::output::{JsonPath, Output, rfc3339, utc};

#[derive(Serialize)]
struct NodeJson<'a> {
    path: JsonPath<'a>,
    node_type: &'static str,
    size: u64,
    mode: u32,
    mtime: String,
    link_target: Option<JsonPath<'a>>,
}

fn type_name(node: &pb::Node) -> &'static str {
    match NodeType::try_from(node.r#type).unwrap_or_default() {
        NodeType::File => "file",
        NodeType::Dir => "dir",
        NodeType::Symlink => "symlink",
        NodeType::Fifo => "fifo",
        NodeType::Chardev => "chardev",
        NodeType::Blockdev => "blockdev",
        NodeType::Unspecified => "unknown",
    }
}

fn type_char(node: &pb::Node) -> char {
    match NodeType::try_from(node.r#type).unwrap_or_default() {
        NodeType::File => '-',
        NodeType::Dir => 'd',
        NodeType::Symlink => 'l',
        NodeType::Fifo => 'p',
        NodeType::Chardev => 'c',
        NodeType::Blockdev => 'b',
        NodeType::Unspecified => '?',
    }
}

pub async fn run(
    url: &str,
    args: &SecretArgs,
    out: Output,
    snapshot: &str,
    long: bool,
) -> Result<()> {
    let mut reader = open_for_read(url, args).await?;
    let (snapshot, path) = parse_snapshot_path(snapshot);
    let entry = super::resolve_spec(&reader, snapshot).await?;
    let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
    let mut visit = |path: &[u8], node: &pb::Node| -> Result<(), Error> {
        if out.json {
            let line = NodeJson {
                path: JsonPath(path),
                node_type: type_name(node),
                size: node.size,
                mode: node.mode,
                mtime: rfc3339(node.mtime.as_ref()),
                link_target: (node.r#type == NodeType::Symlink as i32)
                    .then(|| JsonPath(&node.link_target)),
            };
            out.emit_to(&mut stdout, "node", line)?;
        } else if long {
            write!(
                stdout,
                "{}{:04o} {:>12} {}  ",
                type_char(node),
                node.mode,
                node.size,
                utc(node.mtime.as_ref())
            )?;
            stdout.write_all(path)?;
            writeln!(stdout)?;
        } else {
            stdout.write_all(path)?;
            writeln!(stdout)?;
        }
        Ok(())
    };
    let walked = reader.walk_subtree(&entry.id, path, &mut visit).await;
    match walked {
        // `lode ls | head` closes the pipe; that is not a failure.
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
        other => other?,
    }
    match stdout.flush() {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}
