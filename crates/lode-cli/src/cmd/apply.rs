use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::{ApplyArgs, UsageError};
use crate::client::{Client, failure};
use crate::output::{Output, error_line};
use crate::profile::Profile;

const ENDPOINT: &str = "/api/v1/policies/apply";

#[derive(Serialize)]
struct Document {
    source: String,
    yaml: String,
}

#[derive(Serialize)]
struct Request<'a> {
    documents: &'a [Document],
    manager: &'a str,
    prune: bool,
    adopt: bool,
    dry_run: bool,
}

#[derive(Serialize, Deserialize)]
struct Field {
    path: String,
    #[serde(default)]
    from: Option<Value>,
    #[serde(default)]
    to: Option<Value>,
}

#[derive(Serialize, Deserialize)]
struct Change {
    action: String,
    name: String,
    #[serde(default, skip_serializing)]
    managed_by: Option<String>,
    #[serde(default)]
    adopted: bool,
    #[serde(default)]
    fields: Vec<Field>,
}

#[derive(Deserialize)]
struct Response {
    changes: Vec<Change>,
    unchanged: u64,
}

#[derive(Deserialize)]
struct Problem {
    source: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    column: Option<u64>,
    #[serde(default)]
    path: Option<String>,
    message: String,
}

#[derive(Deserialize)]
struct ProblemBody {
    errors: Vec<Problem>,
}

enum Outcome {
    Plan(Response),
    Invalid(Vec<Problem>),
}

pub async fn run(args: ApplyArgs, profile: Option<&Profile>, out: Output) -> Result<u8> {
    let documents = collect(&args.filename)?;
    let client = Client::resolve(args.server.server.clone(), profile)?;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let color = interactive && no_color_unset();

    let mut previewed = false;
    if prompt_needed(args.prune, args.dry_run, args.yes, interactive) {
        let preview = match send(&client, &documents, &args, true).await? {
            Outcome::Plan(plan) => plan,
            Outcome::Invalid(problems) => return Ok(report(&problems, out)),
        };
        let deletes = count(&preview, "delete");
        if deletes > 0 {
            if !out.json {
                print!("{}", render(&preview, true, color, true));
            }
            if !ask(deletes)? {
                bail!("Nothing was applied.");
            }
            previewed = true;
        }
    }

    let plan = match send(&client, &documents, &args, args.dry_run).await? {
        Outcome::Plan(plan) => plan,
        Outcome::Invalid(problems) => return Ok(report(&problems, out)),
    };
    if out.json {
        for change in &plan.changes {
            out.emit("change", change);
        }
        out.emit(
            "summary",
            json!({
                "create": count(&plan, "create"),
                "update": count(&plan, "update"),
                "delete": count(&plan, "delete"),
                "unchanged": plan.unchanged,
                "dry_run": args.dry_run,
            }),
        );
    } else {
        print!("{}", render(&plan, args.dry_run, color, !previewed));
    }
    if args.dry_run && !plan.changes.is_empty() {
        return Ok(crate::EXIT_DRIFT);
    }
    Ok(0)
}

fn no_color_unset() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Whether a real run that may delete policies asks first: only when a person
/// is at the terminal and did not pass `--yes`. Without a terminal `--prune`
/// itself is the consent.
pub(crate) fn prompt_needed(prune: bool, dry_run: bool, yes: bool, interactive: bool) -> bool {
    prune && !dry_run && !yes && interactive
}

fn ask(deletes: usize) -> Result<bool> {
    let mut err = std::io::stderr().lock();
    write!(err, "Delete {deletes} policies? [y/N] ")?;
    err.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

async fn send(
    client: &Client,
    documents: &[Document],
    args: &ApplyArgs,
    dry_run: bool,
) -> Result<Outcome> {
    let request = Request {
        documents,
        manager: &args.manager,
        prune: args.prune,
        adopt: args.adopt,
        dry_run,
    };
    let (status, body) = client.raw_post(ENDPOINT, &request).await?;
    if status.is_success() {
        let plan = serde_json::from_slice(&body)
            .context("the server answer is not what this lode expects")?;
        return Ok(Outcome::Plan(plan));
    }
    if status == StatusCode::UNPROCESSABLE_ENTITY
        && let Ok(found) = serde_json::from_slice::<ProblemBody>(&body)
        && !found.errors.is_empty()
    {
        return Ok(Outcome::Invalid(found.errors));
    }
    Err(failure(status, &body).into())
}

fn problem_text(p: &Problem) -> String {
    match (p.line, p.column, &p.path) {
        (Some(line), Some(column), _) => format!("{}:{line}:{column}: {}", p.source, p.message),
        (Some(line), None, _) => format!("{}:{line}: {}", p.source, p.message),
        (None, _, Some(path)) => format!("{}: {path}: {}", p.source, p.message),
        (None, _, None) => format!("{}: {}", p.source, p.message),
    }
}

fn report(problems: &[Problem], out: Output) -> u8 {
    for problem in problems {
        let text = problem_text(problem);
        if out.json {
            eprintln!("{}", error_line(crate::EXIT_USAGE, &text));
        } else {
            eprintln!("{text}");
        }
    }
    crate::EXIT_USAGE
}

fn count(plan: &Response, action: &str) -> usize {
    plan.changes.iter().filter(|c| c.action == action).count()
}

fn shown(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "(unset)".into(),
        Some(value) => value.to_string(),
    }
}

fn render(plan: &Response, dry_run: bool, color: bool, with_changes: bool) -> String {
    let mut text = String::new();
    if with_changes {
        let width = plan
            .changes
            .iter()
            .map(|c| "policy/".len() + c.name.len())
            .max()
            .unwrap_or(0)
            + 4;
        for change in &plan.changes {
            let (symbol, paint) = match change.action.as_str() {
                "create" => ("+", "32"),
                "delete" => ("-", "31"),
                _ => ("~", "33"),
            };
            let symbol = if color {
                format!("\x1b[{paint}m{symbol}\x1b[0m")
            } else {
                symbol.to_owned()
            };
            let label = format!("policy/{}", change.name);
            text.push_str(&format!("{symbol} {label:<width$}{}", change.action));
            match (change.action.as_str(), &change.managed_by) {
                ("delete", Some(owner)) => text.push_str(&format!("  (managed by {owner})")),
                ("update", _) if change.adopted => text.push_str("  (adopted)"),
                _ => {}
            }
            text.push('\n');
            let path_width = change
                .fields
                .iter()
                .map(|f| f.path.len())
                .max()
                .unwrap_or(0)
                + 4;
            for field in &change.fields {
                text.push_str(&format!(
                    "    {:<path_width$}{} -> {}\n",
                    field.path,
                    shown(field.from.as_ref()),
                    shown(field.to.as_ref()),
                ));
            }
        }
    }
    text.push_str(&format!(
        "{}: {} to create, {} to update, {} to delete, {} unchanged.\n",
        if dry_run { "Plan" } else { "Applied" },
        count(plan, "create"),
        count(plan, "update"),
        count(plan, "delete"),
        plan.unchanged,
    ));
    text
}

fn is_yaml(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let unreadable =
        |err: std::io::Error| UsageError(format!("cannot read {}: {err}", dir.display()));
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        if entry.file_type().map_err(unreadable)?.is_dir() {
            walk(&path, found)?;
        } else if is_yaml(&path) && path.is_file() {
            found.push(path);
        }
    }
    Ok(())
}

fn collect(root: &Path) -> Result<Vec<Document>> {
    let meta = fs::metadata(root)
        .map_err(|err| UsageError(format!("cannot read {}: {err}", root.display())))?;
    let (base, files) = if meta.is_dir() {
        let mut files = Vec::new();
        walk(root, &mut files)?;
        files.sort();
        if files.is_empty() {
            return Err(UsageError(format!(
                "No .yaml or .yml files found in {}.",
                root.display()
            ))
            .into());
        }
        (root, files)
    } else {
        (
            root.parent().unwrap_or(Path::new("")),
            vec![root.to_owned()],
        )
    };
    files
        .into_iter()
        .map(|path| {
            let yaml = fs::read_to_string(&path)
                .map_err(|err| UsageError(format!("cannot read {}: {err}", path.display())))?;
            let relative = path.strip_prefix(base).unwrap_or(&path);
            let source = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            Ok(Document { source, yaml })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_needed_only_on_tty() {
        assert!(prompt_needed(true, false, false, true));
        assert!(!prompt_needed(true, false, false, false));
        assert!(!prompt_needed(true, false, true, true));
        assert!(!prompt_needed(true, true, false, true));
        assert!(!prompt_needed(false, false, false, true));
    }
}
