use anyhow::{Context, Result, bail};
use lodestore_format::check::packs::check_packs;
use lodestore_format::check::sample::{MAX_SAMPLE_NODES, SampleOptions, sample_restore};
use lodestore_format::check::structure::{StructureStep, check_structure};
use lodestore_format::check::{Finding, Note, shuffle};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::listed_packs;
use lodestore_format::reader::Reader;
use lodestore_policy::parse_size;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::gc::{POLL, POLL_RETRIES, resolve, transient};
use super::server::size;
use crate::cli::{CheckArgs, CheckLevel, SecretArgs, ServerCheckArgs, UsageError};
use crate::client::{Client, failure, seg};
use crate::open::open_repository;
use crate::output::{Output, spinner};

const MAX_LISTED: usize = 20;
const LABEL_WIDTH: usize = 18;
/// Exit code of a check that could not finish.
const EXIT_STOPPED: u8 = 1;
const DEFAULT_READ_LIMIT: u64 = 5 << 30;
const DEFAULT_MAX_DATA: u64 = 1 << 30;
const DEFAULT_FILES: u32 = 20;
const SIZE_RULE: &str = "Use a size such as 5GiB or 500MiB.";

fn size_flag(name: &str, text: Option<&str>, default: u64) -> Result<u64, UsageError> {
    match text {
        None => Ok(default),
        Some(text) => parse_size(text)
            .ok_or_else(|| UsageError(format!("{name} {text:?} is not a size. {SIZE_RULE}"))),
    }
}

pub(crate) enum Fact {
    Count(u64),
    Bytes(u64),
    Text(String),
}

impl Fact {
    fn text(&self) -> String {
        match self {
            Fact::Count(n) => count(*n),
            Fact::Bytes(n) => size(*n),
            Fact::Text(text) => text.clone(),
        }
    }

    fn json(&self) -> Value {
        match self {
            Fact::Count(n) | Fact::Bytes(n) => json!(n),
            Fact::Text(text) => json!(text),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Line {
    pub kind: String,
    pub key: String,
    pub detail: String,
}

impl From<&Finding> for Line {
    fn from(f: &Finding) -> Self {
        Line {
            kind: f.kind.as_str().into(),
            key: f.key.clone(),
            detail: f.detail.clone(),
        }
    }
}

impl From<&Note> for Line {
    fn from(n: &Note) -> Self {
        Line {
            kind: n.kind.as_str().into(),
            key: n.key.clone(),
            detail: n.detail.clone(),
        }
    }
}

/// What a check found, in the words of the web interface; `lode server check`
/// prints the report of the server the same way.
pub(crate) struct Report {
    pub level: CheckLevel,
    pub facts: Vec<(&'static str, Fact)>,
    pub findings: Vec<Line>,
    pub findings_total: u64,
    pub notes: Vec<Line>,
    pub stopped: Option<String>,
    /// Why a run that found nothing wrong also had nothing to look at.
    pub nothing_to_check: Option<&'static str>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Stopped,
    Failed,
    Passed,
    NothingToCheck,
}

impl Report {
    fn passed(&self) -> bool {
        self.stopped.is_none() && self.findings_total == 0
    }

    fn verdict(&self) -> Verdict {
        match (self.stopped.is_some(), self.passed()) {
            (true, _) => Verdict::Stopped,
            (false, true) if self.nothing_to_check.is_some() => Verdict::NothingToCheck,
            (false, true) => Verdict::Passed,
            (false, false) => Verdict::Failed,
        }
    }

    fn exit_code(&self) -> u8 {
        if self.stopped.is_some() {
            EXIT_STOPPED
        } else if self.passed() {
            0
        } else {
            crate::EXIT_CHECK_FAILED
        }
    }
}

fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn title(level: CheckLevel) -> &'static str {
    match level {
        CheckLevel::Structure => "Structure check",
        CheckLevel::Packs => "Pack reading check",
        CheckLevel::Restore => "Test restore check",
    }
}

fn level_name(level: CheckLevel) -> &'static str {
    match level {
        CheckLevel::Structure => "structure",
        CheckLevel::Packs => "packs",
        CheckLevel::Restore => "restore",
    }
}

fn json_key(label: &str) -> String {
    label.to_ascii_lowercase().replace(' ', "_")
}

/// Control characters of a name or a message, written out so that a file
/// name cannot steer the terminal of whoever reads the report.
fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

pub(crate) fn print_report(out: Output, repo: &str, mode: &str, report: &Report) {
    let verdict = report.verdict();
    let result = match verdict {
        Verdict::Stopped => "stopped",
        Verdict::Failed => "failed",
        Verdict::Passed => "passed",
        Verdict::NothingToCheck => "nothing_to_check",
    };
    if out.json {
        let facts: Map<String, Value> = report
            .facts
            .iter()
            .map(|(label, fact)| (json_key(label), fact.json()))
            .collect();
        out.emit(
            "check",
            json!({
                "level": level_name(report.level),
                "repository": repo,
                "mode": mode,
                "result": result,
                "stopped": report.stopped,
                "nothing_to_check": report.nothing_to_check,
                "facts": facts,
                "findings": report.findings,
                "findings_total": report.findings_total,
                "notes": report.notes,
            }),
        );
        return;
    }
    println!("{} for repository {repo} ({mode})", title(report.level));
    let verdict = match verdict {
        Verdict::Stopped => "Stopped",
        Verdict::Failed => "Failed",
        Verdict::Passed => "Passed",
        Verdict::NothingToCheck => "Nothing to check",
    };
    println!("  {:<LABEL_WIDTH$}{verdict}", "Result");
    for (label, fact) in &report.facts {
        println!("  {label:<LABEL_WIDTH$}{}", fact.text());
    }
    if let Some(why) = &report.stopped {
        println!("{}", safe(why));
    }
    if let Some(why) = report.nothing_to_check
        && report.passed()
    {
        println!("{why}");
    }
    if !report.findings.is_empty() {
        println!("Problems");
        for line in report.findings.iter().take(MAX_LISTED) {
            println!("  {}   {}", safe(&line.key), safe(&line.detail));
        }
        let more = report.findings_total.saturating_sub(MAX_LISTED as u64);
        if more > 0 {
            println!("  and {more} more.");
        }
    }
    if !report.notes.is_empty() {
        println!("Notes");
        for line in report.notes.iter().take(MAX_LISTED) {
            println!("  {}   {}", safe(&line.key), safe(&line.detail));
        }
        let more = report.notes.len().saturating_sub(MAX_LISTED);
        if more > 0 {
            println!("  and {more} more.");
        }
    }
}

pub async fn run(url: &str, args: &SecretArgs, out: Output, flags: CheckArgs) -> Result<u8> {
    let (store, repo) = open_repository(url, args).await?;
    let bar = (!out.json).then(spinner).flatten();
    let report = match flags.level {
        CheckLevel::Structure => {
            let found = check_structure(
                &store,
                repo.read_keys(),
                repo.config(),
                &|step, done, total| {
                    if let Some(bar) = &bar {
                        bar.set_message(match step {
                            StructureStep::Listing => "Listing the repository".to_owned(),
                            StructureStep::Indexes => "Reading the index".to_owned(),
                            StructureStep::Snapshots => {
                                format!("Checking snapshots ({done} of {total})")
                            }
                        });
                    }
                },
            )
            .await?;
            Report {
                level: CheckLevel::Structure,
                facts: vec![
                    ("Snapshots checked", Fact::Count(found.snapshots_checked)),
                    ("Trees checked", Fact::Count(found.trees_checked)),
                    ("Packs listed", Fact::Count(found.packs_listed)),
                    ("Missing packs", Fact::Count(found.missing_packs)),
                    ("Storage used", Fact::Bytes(found.physical_bytes)),
                ],
                findings: found.findings.iter().map(Line::from).collect(),
                findings_total: found.findings_total,
                notes: found.notes.iter().map(Line::from).collect(),
                stopped: None,
                nothing_to_check: None,
            }
        }
        CheckLevel::Packs => {
            let limit = size_flag(
                "--read-limit",
                flags.read_limit.as_deref(),
                DEFAULT_READ_LIMIT,
            )?;
            let mut packs: Vec<(BlobId, u64)> = listed_packs(&store).await?.into_iter().collect();
            shuffle(&mut packs, &mut Entropy::os());
            let found = check_packs(
                &store,
                repo.read_keys(),
                &packs,
                limit,
                &|done, total, bytes| {
                    if let Some(bar) = &bar {
                        bar.set_message(format!(
                            "Reading packs ({done} of {total}, {})",
                            size(bytes)
                        ));
                    }
                },
            )
            .await?;
            if let Some((key, error)) = found.read_errors.first() {
                bail!("reading {key}: {error}");
            }
            Report {
                level: CheckLevel::Packs,
                facts: vec![
                    ("Packs read", Fact::Count(found.packs_read)),
                    ("Data read", Fact::Bytes(found.bytes_read)),
                    ("Blobs verified", Fact::Count(found.blobs_verified)),
                    ("Damaged blobs", Fact::Count(found.damaged_blobs)),
                ],
                findings: found.findings.iter().map(Line::from).collect(),
                findings_total: found.findings_total,
                notes: Vec::new(),
                stopped: None,
                nothing_to_check: (found.packs_read == 0 && found.findings_total == 0)
                    .then_some("The repository has no packs."),
            }
        }
        CheckLevel::Restore => {
            let max_bytes = size_flag("--max-data", flags.max_data.as_deref(), DEFAULT_MAX_DATA)?;
            let files = flags.files.unwrap_or(DEFAULT_FILES);
            let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config())?;
            let listed = reader.list_snapshots().await?;
            let unopened: Vec<Line> = listed
                .skipped
                .iter()
                .map(|(key, why)| Line {
                    kind: "ignored_object".into(),
                    key: key.clone(),
                    detail: why.clone(),
                })
                .collect();
            let snapshots: Vec<BlobId> = listed.entries.into_iter().map(|e| e.id).collect();
            let scratch = tempfile::tempdir().context("cannot create a temporary folder")?;
            let opts = SampleOptions {
                files,
                max_bytes,
                scratch: scratch.path().to_path_buf(),
                max_nodes: MAX_SAMPLE_NODES,
            };
            let found = sample_restore(
                &mut reader,
                &snapshots,
                &opts,
                &mut Entropy::os(),
                &|done, total| {
                    if let Some(bar) = &bar {
                        bar.set_message(format!("Restoring files ({done} of {total})"));
                    }
                },
                &|| false,
            )
            .await?;
            Report {
                level: CheckLevel::Restore,
                facts: vec![
                    ("Snapshots sampled", Fact::Count(found.snapshots_sampled)),
                    ("Files restored", Fact::Count(found.files_restored)),
                    ("Data restored", Fact::Bytes(found.bytes_restored)),
                    ("Hash mismatches", Fact::Count(found.hash_mismatches)),
                    ("Skipped", Fact::Count(found.skipped)),
                ],
                findings_total: found.findings.len() as u64,
                findings: found.findings.iter().map(Line::from).collect(),
                stopped: if snapshots.is_empty() && !unopened.is_empty() {
                    Some("No snapshot could be opened, so nothing was tested.".to_owned())
                } else {
                    (found.stopped_at_limit && found.files_restored == 0)
                        .then(|| "Stopped at the size limit.".to_owned())
                },
                nothing_to_check: if snapshots.is_empty() {
                    Some("The repository has no snapshots.")
                } else if found.files_restored == 0 && !found.stopped_at_limit {
                    Some("No file was restored; the sampled snapshots hold no regular files.")
                } else {
                    None
                },
                notes: unopened,
            }
        }
    };
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    print_report(out, url, "direct", &report);
    Ok(report.exit_code())
}

#[derive(Deserialize)]
struct Started {
    run_id: String,
}

#[derive(Deserialize)]
struct Progress {
    done: u64,
    total: u64,
}

#[derive(Deserialize)]
struct StructureSummary {
    snapshots_checked: u64,
    trees_checked: u64,
    packs_listed: u64,
    missing_packs: u64,
}

#[derive(Deserialize)]
struct PacksSummary {
    packs_read: u64,
    bytes_read: u64,
    blobs_verified: u64,
    damaged_blobs: u64,
}

#[derive(Deserialize)]
struct RestoreSummary {
    snapshots_sampled: u64,
    files_restored: u64,
    bytes_restored: u64,
    hash_mismatches: u64,
    skipped: u64,
}

#[derive(Deserialize)]
struct Summary {
    structure: Option<StructureSummary>,
    packs: Option<PacksSummary>,
    restore: Option<RestoreSummary>,
}

#[derive(Deserialize)]
struct Coverage {
    checked_30d_pct: u64,
    never_checked: u64,
}

#[derive(Deserialize)]
struct Status {
    coverage: Option<Coverage>,
}

#[derive(Deserialize)]
struct Run {
    state: String,
    outcome: Option<String>,
    error: Option<String>,
    progress: Option<Progress>,
    #[serde(default)]
    findings_total: u64,
    summary: Summary,
}

#[derive(Deserialize)]
struct FullReport {
    run: Run,
    #[serde(default)]
    findings: Vec<Line>,
    #[serde(default)]
    notes: Vec<Line>,
}

/// Polls the run until it leaves the queue and the worker, trying again a
/// few times when the server is briefly unreachable: the check runs on the
/// server whatever happens here.
async fn follow(client: &Client, path: &str, out: Output) -> Result<Run> {
    let bar = (!out.json).then(spinner).flatten();
    let mut failures = 0;
    let run = loop {
        match client.get_json::<Run>(path, &[]).await {
            Ok(run) if matches!(run.state.as_str(), "queued" | "running") => {
                failures = 0;
                if let (Some(bar), Some(p)) = (&bar, &run.progress) {
                    bar.set_message(format!("Checking ({} of {})", p.done, p.total));
                }
            }
            Ok(run) => break run,
            Err(e) if transient(&e) => {
                failures += 1;
                if failures > POLL_RETRIES {
                    return Err(e.context(
                        "Lost contact with the server. The check continues there; see the Jobs page",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
        tokio::time::sleep(POLL).await;
    };
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    Ok(run)
}

fn server_nothing_to_check(summary: &Summary) -> Option<&'static str> {
    if let Some(p) = &summary.packs {
        (p.packs_read == 0).then_some("The repository has no packs.")
    } else if let Some(r) = &summary.restore {
        (r.files_restored == 0 && r.skipped == 0 && r.hash_mismatches == 0)
            .then_some("No file was restored.")
    } else {
        None
    }
}

fn server_facts(summary: &Summary) -> Vec<(&'static str, Fact)> {
    if let Some(s) = &summary.structure {
        vec![
            ("Snapshots checked", Fact::Count(s.snapshots_checked)),
            ("Trees checked", Fact::Count(s.trees_checked)),
            ("Packs listed", Fact::Count(s.packs_listed)),
            ("Missing packs", Fact::Count(s.missing_packs)),
        ]
    } else if let Some(p) = &summary.packs {
        vec![
            ("Packs read", Fact::Count(p.packs_read)),
            ("Data read", Fact::Bytes(p.bytes_read)),
            ("Blobs verified", Fact::Count(p.blobs_verified)),
            ("Damaged blobs", Fact::Count(p.damaged_blobs)),
        ]
    } else if let Some(r) = &summary.restore {
        vec![
            ("Snapshots sampled", Fact::Count(r.snapshots_sampled)),
            ("Files restored", Fact::Count(r.files_restored)),
            ("Data restored", Fact::Bytes(r.bytes_restored)),
            ("Hash mismatches", Fact::Count(r.hash_mismatches)),
            ("Skipped", Fact::Count(r.skipped)),
        ]
    } else {
        Vec::new()
    }
}

/// Runs a check on the server and prints its report like `lode check` does,
/// with `(server)` in place of `(direct)` on the first line.
pub async fn run_server(client: &Client, out: Output, args: ServerCheckArgs) -> Result<u8> {
    let id = resolve(client, &args.repo).await?;
    let base = format!("/api/v1/repositories/{}/checks", seg(&id));
    let (code, body) = client
        .raw_post(
            &format!("{base}/{}/run", level_name(args.level)),
            &json!({}),
        )
        .await?;
    let started: Started = match code {
        StatusCode::ACCEPTED => serde_json::from_slice(&body)?,
        StatusCode::CONFLICT => bail!("{}", failure(code, &body).message),
        _ => return Err(failure(code, &body).into()),
    };
    let run_path = format!("{base}/runs/{}", seg(&started.run_id));
    let run = follow(client, &run_path, out).await?;
    if run.state == "aborted" {
        bail!("The check was cancelled.");
    }
    let raw: Value = client.get_json(&format!("{run_path}/report"), &[]).await?;
    if out.json {
        out.emit("check_report", &raw);
    }
    let full: FullReport = serde_json::from_value(raw)?;
    let run = full.run;
    let did_not_finish = run.outcome.as_deref() == Some("stopped")
        || (run.state == "failed" && run.outcome.as_deref() != Some("problems"));
    let stopped = did_not_finish.then(|| {
        run.error
            .clone()
            .unwrap_or_else(|| "The check did not finish.".to_owned())
    });
    let mut facts = server_facts(&run.summary);
    if !out.json
        && args.level == CheckLevel::Packs
        && let Ok(Status {
            coverage: Some(c), ..
        }) = client.get_json(&base, &[]).await
    {
        facts.push((
            "Coverage",
            Fact::Text(format!(
                "{}% of packs checked in the last 30 days",
                c.checked_30d_pct
            )),
        ));
        facts.push((
            "Never checked",
            Fact::Text(format!("{} packs", count(c.never_checked))),
        ));
    }
    let report = Report {
        level: args.level,
        facts,
        findings: full.findings,
        findings_total: run.findings_total,
        notes: full.notes,
        nothing_to_check: server_nothing_to_check(&run.summary),
        stopped,
    };
    if !out.json {
        print_report(out, &args.repo, "server", &report);
    }
    Ok(report.exit_code())
}

#[cfg(test)]
mod tests {
    use super::safe;

    #[test]
    fn safe_writes_control_characters_out() {
        assert_eq!(safe("a\u{1b}[2Jb\nc\u{7}"), "a\\u{1b}[2Jb\\nc\\u{7}");
        assert_eq!(safe("plain/путь é.txt"), "plain/путь é.txt");
    }
}
