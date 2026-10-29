use std::path::Path;

use anyhow::Result;
use indicatif::HumanBytes;
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::restore::Unapplied;
use lodestore_format::reader::{
    Overwrite, RestoreFailure, RestoreOptions, RestoreProgress, parse_snapshot_path,
};
use serde::Serialize;

use crate::cli::{RestoreFlags, SecretArgs};
use crate::open::open_for_read;
use crate::output::{JsonPath, Output, Throttle, spinner};

const MAX_LISTED: usize = 50;
/// Exit code of a restore that finished but left entries out.
const EXIT_INCOMPLETE: u8 = 1;

#[derive(Serialize)]
struct ProgressJson<'a> {
    files_done: u64,
    bytes_done: u64,
    current_path: JsonPath<'a>,
}

#[derive(Serialize)]
struct UnappliedJson<'a> {
    path: JsonPath<'a>,
    what: &'a str,
    error: &'a str,
}

#[derive(Serialize)]
struct FailureJson<'a> {
    path: JsonPath<'a>,
    error: &'a str,
}

#[derive(Serialize)]
struct SummaryJson<'a> {
    files_restored: u64,
    bytes_restored: u64,
    files_skipped: u64,
    files_verified: u64,
    would_write: u64,
    would_skip: u64,
    dry_run: bool,
    unapplied: Vec<UnappliedJson<'a>>,
    errors: Vec<FailureJson<'a>>,
}

fn overwrite_mode(name: &str) -> Overwrite {
    match name {
        "always" => Overwrite::Always,
        "never" => Overwrite::Never,
        _ => Overwrite::IfChanged,
    }
}

pub async fn run(
    url: &str,
    args: &SecretArgs,
    out: Output,
    snapshot: &str,
    target: &Path,
    flags: RestoreFlags,
) -> Result<u8> {
    let include = if flags.include.is_empty() {
        None
    } else {
        Some(PathMatcher::new(&flags.include)?)
    };
    let mut reader = open_for_read(url, args).await?;
    let (snapshot, path) = parse_snapshot_path(snapshot);
    let entry = reader.resolve_snapshot(snapshot).await?;
    let throttle = Throttle::new();
    let bar = if out.json { None } else { spinner() };
    let progress = |p: &RestoreProgress| {
        if !throttle.ready() {
            return;
        }
        if out.json {
            out.emit(
                "restore_progress",
                ProgressJson {
                    files_done: p.files_done,
                    bytes_done: p.bytes_done,
                    current_path: JsonPath(&p.current),
                },
            );
        } else if let Some(bar) = &bar {
            bar.set_message(format!(
                "{} files, {}  {}",
                p.files_done,
                HumanBytes(p.bytes_done),
                String::from_utf8_lossy(&p.current)
            ));
        }
    };
    let report = reader
        .restore(
            &entry.id,
            target,
            &RestoreOptions {
                path: path.to_vec(),
                overwrite: overwrite_mode(&flags.overwrite),
                include,
                verify: flags.verify,
                dry_run: flags.dry_run,
                skip_owners: false,
            },
            &progress,
        )
        .await;
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    let report = report?;

    let code = if report.errors.is_empty() {
        0
    } else {
        EXIT_INCOMPLETE
    };
    if out.json {
        out.emit(
            "restore_summary",
            SummaryJson {
                files_restored: report.files,
                bytes_restored: report.bytes,
                files_skipped: report.skipped,
                files_verified: report.verified,
                would_write: report.would_write,
                would_skip: report.would_skip,
                dry_run: flags.dry_run,
                unapplied: report
                    .unapplied
                    .iter()
                    .map(|u| UnappliedJson {
                        path: JsonPath(&u.path),
                        what: &u.what,
                        error: &u.error,
                    })
                    .collect(),
                errors: report
                    .errors
                    .iter()
                    .map(|f| FailureJson {
                        path: JsonPath(&f.path),
                        error: &f.error,
                    })
                    .collect(),
            },
        );
        return Ok(code);
    }
    print_unapplied(&report.unapplied, &entry.snapshot.paths);
    print_failures(&report.errors);
    if flags.dry_run {
        println!(
            "dry run: would restore {} files and leave {} alone, nothing was changed",
            report.would_write, report.would_skip
        );
    } else {
        let mut line = format!(
            "restored {} files ({} bytes) to {}",
            report.files,
            report.bytes,
            target.display()
        );
        if report.skipped > 0 {
            line.push_str(&format!(", {} left as they were", report.skipped));
        }
        if report.verified > 0 {
            line.push_str(&format!(", {} verified", report.verified));
        }
        println!("{line}");
    }
    Ok(code)
}

fn print_failures(errors: &[RestoreFailure]) {
    if errors.is_empty() {
        return;
    }
    eprintln!("error: {} entries could not be restored:", errors.len());
    for f in errors.iter().take(MAX_LISTED) {
        eprintln!("  {}: {}", String::from_utf8_lossy(&f.path), f.error);
    }
    if errors.len() > MAX_LISTED {
        eprintln!("  ...and {} more", errors.len() - MAX_LISTED);
    }
}

/// The owner of a directory above a backed-up path is recorded as a bare
/// node, so a restore without root always reports it; it is one line, not one
/// per directory.
fn is_parent_owner(u: &Unapplied, roots: &[Vec<u8>]) -> bool {
    u.what == "owner"
        && roots.iter().any(|root| {
            let root = root.strip_prefix(b"/").unwrap_or(root);
            root.len() > u.path.len()
                && root.starts_with(&u.path)
                && (u.path.is_empty() || root[u.path.len()] == b'/')
        })
}

fn print_unapplied(unapplied: &[Unapplied], roots: &[Vec<u8>]) {
    let (parents, rest): (Vec<_>, Vec<_>) =
        unapplied.iter().partition(|u| is_parent_owner(u, roots));
    if !rest.is_empty() {
        eprintln!(
            "warning: {} metadata items could not be applied:",
            rest.len()
        );
        for u in rest.iter().take(MAX_LISTED) {
            eprintln!(
                "  {}: {}: {}",
                String::from_utf8_lossy(&u.path),
                u.what,
                u.error
            );
        }
        if rest.len() > MAX_LISTED {
            eprintln!("  ...and {} more", rest.len() - MAX_LISTED);
        }
    }
    if !parents.is_empty() {
        eprintln!(
            "note: the owner of {} parent directories above the backed-up paths was not applied (needs root)",
            parents.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(path: &str) -> Unapplied {
        Unapplied {
            path: path.as_bytes().to_vec(),
            what: "owner".into(),
            error: "EPERM".into(),
        }
    }

    #[test]
    fn only_strict_ancestors_of_a_root_count_as_parents() {
        let roots = vec![b"/tmp/x/src".to_vec()];
        assert!(is_parent_owner(&owner(""), &roots));
        assert!(is_parent_owner(&owner("tmp"), &roots));
        assert!(is_parent_owner(&owner("tmp/x"), &roots));
        assert!(!is_parent_owner(&owner("tmp/x/src"), &roots));
        assert!(!is_parent_owner(&owner("tmp/x/src/f"), &roots));
        assert!(!is_parent_owner(&owner("tmp/xy"), &roots));
        let mut mode = owner("tmp");
        mode.what = "mode".into();
        assert!(!is_parent_owner(&mode, &roots));
    }
}
