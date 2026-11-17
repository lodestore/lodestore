use std::fs;
use std::path::Path;

use anyhow::Result;
use lodestore_policy::{BackupPolicy, Problem, validate, yaml_problem};
use serde::{Deserialize, Serialize};

use crate::cli::{PolicyCommand, UsageError};
use crate::output::{Output, error_line};

const MAX_DOCUMENTS: usize = 1000;

#[derive(Serialize)]
struct FileReport {
    file: String,
    policies: usize,
    problems: Vec<Problem>,
}

#[derive(Serialize)]
struct Report {
    files: Vec<FileReport>,
}

pub fn run(out: Output, command: PolicyCommand) -> Result<u8> {
    match command {
        PolicyCommand::Validate { files } => validate_files(out, &files),
    }
}

fn validate_files(out: Output, files: &[impl AsRef<Path>]) -> Result<u8> {
    let mut report = Report { files: Vec::new() };
    for file in files {
        let path = file.as_ref();
        let text = fs::read_to_string(path)
            .map_err(|err| UsageError(format!("cannot read {}: {err}", path.display())))?;
        let (policies, problems) = check_documents(&text);
        report.files.push(FileReport {
            file: path.display().to_string(),
            policies,
            problems,
        });
    }
    let failed = report.files.iter().any(|f| !f.problems.is_empty());
    if out.json {
        out.emit("policy_validate", &report);
    }
    for file in &report.files {
        for problem in &file.problems {
            let text = problem_text(&file.file, problem);
            if out.json {
                eprintln!("{}", error_line(crate::EXIT_CHECK_FAILED, &text));
            } else {
                eprintln!("{text}");
            }
        }
        if !out.json && file.problems.is_empty() {
            let n = file.policies;
            let noun = if n == 1 { "policy is" } else { "policies are" };
            println!("{}: {n} {noun} valid", file.file);
        }
    }
    Ok(if failed { crate::EXIT_CHECK_FAILED } else { 0 })
}

/// Reads the documents the way the server does for `lode apply`; the count is the valid policies.
fn check_documents(text: &str) -> (usize, Vec<Problem>) {
    let mut valid = 0;
    let mut documents = 0;
    let mut problems: Vec<Problem> = Vec::new();
    for de in yaml_serde::Deserializer::from_str(text) {
        documents += 1;
        if documents > MAX_DOCUMENTS {
            problems.push(plain(format!(
                "At most {MAX_DOCUMENTS} documents are allowed."
            )));
            break;
        }
        match Option::<BackupPolicy>::deserialize(de) {
            Ok(None) => documents -= 1,
            Ok(Some(policy)) => {
                let found = validate(&policy);
                if found.is_empty() {
                    valid += 1;
                }
                problems.extend(found);
            }
            Err(e) => {
                let problem = yaml_problem(&e);
                if problems.last() == Some(&problem) {
                    break;
                }
                problems.push(problem);
            }
        }
    }
    if documents == 0 {
        problems.push(plain("No policy document found.".to_owned()));
    }
    (valid, problems)
}

fn plain(message: String) -> Problem {
    Problem {
        line: None,
        column: None,
        path: None,
        message,
    }
}

fn problem_text(file: &str, p: &Problem) -> String {
    let mut text = file.to_owned();
    if let Some(line) = p.line {
        text.push_str(&format!(":{line}"));
        if let Some(column) = p.column {
            text.push_str(&format!(":{column}"));
        }
    }
    text.push_str(": ");
    if let Some(path) = &p.path {
        text.push_str(path);
        text.push_str(": ");
    }
    text.push_str(&p.message);
    text
}
