//! The `BackupPolicy` object: one strict YAML document shared by the API,
//! the UI and `lode apply`.

use std::collections::BTreeMap;

use lodestore_format::fs::filter::PathMatcher;
use serde::{Deserialize, Serialize};

pub const API_VERSION: &str = "lodestore.io/v1alpha1";
pub const KIND: &str = "BackupPolicy";

const DEFAULT_TIMEOUT_SECS: u32 = 3600;
const MAX_TIMEOUT_SECS: u64 = 24 * 3600;
const MAX_NAME_LEN: usize = 63;
const MAX_LABELS: usize = 20;
const MAX_HOSTS: usize = 1000;
const MAX_ARGS: usize = 64;

const PATH_RULE: &str = "Enter an absolute path that starts with \"/\".";
const SCRIPT_RULE: &str = "Enter a file name from /etc/lodestore/hooks.d, without \"/\".";
const TIMEOUT_RULE: &str = "Use a duration such as 90s, 30m or 2h.";
const LABEL_RULE: &str = "Keys and values: up to 63 characters, letters, digits, \"-\", \"_\" and \".\"; key is required.";

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BackupPolicy {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: PolicySpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Metadata {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PolicySpec {
    pub repository: String,
    #[serde(default)]
    pub selector: Selector,
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub one_file_system: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub exclude_caches: bool,
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Selector {
    #[serde(default)]
    pub match_labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
}

impl Selector {
    /// The one place host matching lives. A host list selects exactly those
    /// hosts by agent ID. Otherwise every required label must be present with
    /// the same value, and an empty selector matches any host.
    pub fn matches<'a>(
        &self,
        host_id: &str,
        labels: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> bool {
        if let Some(hosts) = &self.hosts {
            return hosts.iter().any(|h| h == host_id);
        }
        let labels: Vec<_> = labels.into_iter().collect();
        self.match_labels
            .iter()
            .all(|(k, v)| labels.iter().any(|(lk, lv)| lk == k && lv == v))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Hooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<Hook>,
}

impl Hooks {
    fn is_empty(&self) -> bool {
        self.before.is_empty() && self.after.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Hook {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

impl Hook {
    pub fn timeout_secs(&self) -> u32 {
        self.timeout
            .as_deref()
            .and_then(parse_timeout)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
    }
}

/// An error with the location of the offending value. `line`/`column` are
/// 1-based and only set for errors that come from the YAML text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Problem {
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub path: Option<String>,
    pub message: String,
}

impl Problem {
    fn at(path: impl Into<String>, message: impl Into<String>) -> Self {
        Problem {
            line: None,
            column: None,
            path: Some(path.into()),
            message: message.into(),
        }
    }
}

pub fn parse_yaml(text: &str) -> Result<BackupPolicy, Vec<Problem>> {
    let policy: BackupPolicy = yaml_serde::from_str(text).map_err(|e| vec![yaml_problem(&e)])?;
    let problems = validate(&policy);
    if problems.is_empty() {
        Ok(policy)
    } else {
        Err(problems)
    }
}

pub fn to_yaml(policy: &BackupPolicy) -> String {
    yaml_serde::to_string(policy).expect("BackupPolicy always serializes")
}

fn yaml_problem(error: &yaml_serde::Error) -> Problem {
    let (line, column) = match error.location() {
        Some(l) => (Some(l.line()), Some(l.column())),
        None => (None, None),
    };
    let mut message = error.to_string();
    if let (Some(l), Some(c)) = (line, column) {
        let tail = format!(" at line {l} column {c}");
        if let Some(head) = message.strip_suffix(&tail) {
            message = head.to_string();
        }
    }
    let mut path = None;
    if let Some((head, rest)) = message.split_once(": ")
        && head.starts_with(|c: char| c.is_ascii_alphabetic())
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '[' | ']' | '_' | '-'))
    {
        path = Some(head.to_string());
        message = rest.to_string();
    }
    Problem {
        line,
        column,
        path,
        message,
    }
}

fn parse_timeout(text: &str) -> Option<u32> {
    let unit = match text.chars().last()? {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return None,
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs = digits.parse::<u64>().ok()?.checked_mul(unit)?;
    if (1..=MAX_TIMEOUT_SECS).contains(&secs) {
        u32::try_from(secs).ok()
    } else {
        None
    }
}

pub fn validate(policy: &BackupPolicy) -> Vec<Problem> {
    let mut out = Vec::new();
    if policy.api_version != API_VERSION {
        out.push(Problem::at(
            "apiVersion",
            format!("Use apiVersion \"{API_VERSION}\"."),
        ));
    }
    if policy.kind != KIND {
        out.push(Problem::at("kind", format!("Use kind \"{KIND}\".")));
    }
    if !name_ok(&policy.metadata.name) {
        out.push(Problem::at(
            "metadata.name",
            "Use up to 63 lowercase letters, digits and \"-\"; start and end with a letter or digit.",
        ));
    }
    let spec = &policy.spec;
    if spec.repository.is_empty() {
        out.push(Problem::at("spec.repository", "Enter a repository name."));
    }
    validate_labels(&spec.selector, &mut out);
    validate_hosts(&spec.selector, &mut out);
    validate_paths(spec, &mut out);
    for (i, pattern) in spec.exclude.iter().enumerate() {
        if let Err(e) = PathMatcher::new([pattern]) {
            out.push(Problem::at(format!("spec.exclude[{i}]"), e.to_string()));
        }
    }
    for (kind, hooks) in [("before", &spec.hooks.before), ("after", &spec.hooks.after)] {
        for (i, hook) in hooks.iter().enumerate() {
            validate_hook(&format!("spec.hooks.{kind}[{i}]"), hook, &mut out);
        }
    }
    out
}

fn name_ok(name: &str) -> bool {
    let edge = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=MAX_NAME_LEN).contains(&name.len())
        && name.starts_with(edge)
        && name.ends_with(edge)
        && name.chars().all(|c| edge(c) || c == '-')
}

fn validate_hosts(selector: &Selector, out: &mut Vec<Problem>) {
    const PATH: &str = "spec.selector.hosts";
    let Some(hosts) = &selector.hosts else {
        return;
    };
    if hosts.is_empty() {
        out.push(Problem::at(PATH, "Pick at least one host."));
        return;
    }
    if !selector.match_labels.is_empty() {
        out.push(Problem::at(
            "spec.selector",
            "Select hosts either by labels or by a host list, not both.",
        ));
    }
    if hosts.len() > MAX_HOSTS {
        out.push(Problem::at(
            PATH,
            format!("At most {MAX_HOSTS} hosts are allowed."),
        ));
        return;
    }
    let mut seen = std::collections::HashSet::new();
    for (i, id) in hosts.iter().enumerate() {
        let id_ok = (1..=64).contains(&id.len())
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !id_ok {
            out.push(Problem::at(
                format!("{PATH}[{i}]"),
                "Enter a host agent ID.",
            ));
            return;
        }
        if !seen.insert(id) {
            out.push(Problem::at(
                format!("{PATH}[{i}]"),
                "This host is listed twice.",
            ));
            return;
        }
    }
}

fn validate_labels(selector: &Selector, out: &mut Vec<Problem>) {
    const PATH: &str = "spec.selector.matchLabels";
    let labels = &selector.match_labels;
    if labels.len() > MAX_LABELS {
        out.push(Problem::at(
            PATH,
            format!("{LABEL_RULE} At most {MAX_LABELS} labels are allowed."),
        ));
    }
    let charset = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
    for (key, value) in labels {
        let key_ok =
            (1..=63).contains(&key.len()) && key.chars().all(|c| c.is_ascii() && charset(c));
        let value_ok = value.chars().count() <= 63 && value.chars().all(charset);
        if !key_ok || !value_ok {
            out.push(Problem::at(PATH, format!("{LABEL_RULE} Check \"{key}\".")));
        }
    }
}

fn validate_paths(spec: &PolicySpec, out: &mut Vec<Problem>) {
    if spec.paths.is_empty() {
        out.push(Problem::at(
            "spec.paths",
            "Add at least one path to back up.",
        ));
    }
    for (i, path) in spec.paths.iter().enumerate() {
        let at = format!("spec.paths[{i}]");
        if !path.starts_with('/') || path.contains('\0') {
            out.push(Problem::at(at, PATH_RULE));
        } else if spec.paths[..i].contains(path) {
            out.push(Problem::at(at, "Remove the duplicate path."));
        }
    }
}

fn validate_hook(at: &str, hook: &Hook, out: &mut Vec<Problem>) {
    match (&hook.script, &hook.command) {
        (Some(_), Some(_)) => out.push(Problem::at(
            at,
            "Set either \"script\" or \"command\", not both.",
        )),
        (None, None) => out.push(Problem::at(at, "Set \"script\" or \"command\".")),
        (Some(script), None) if !script_ok(script) => {
            out.push(Problem::at(format!("{at}.script"), SCRIPT_RULE));
        }
        (None, Some(command)) if command.is_empty() || command.contains('\0') => {
            out.push(Problem::at(format!("{at}.command"), "Enter a command."));
        }
        _ => {}
    }
    if hook.args.len() > MAX_ARGS || hook.args.iter().any(|a| a.contains('\0')) {
        out.push(Problem::at(
            format!("{at}.args"),
            format!("Use at most {MAX_ARGS} arguments without NUL characters."),
        ));
    }
    if let Some(timeout) = &hook.timeout
        && parse_timeout(timeout).is_none()
    {
        out.push(Problem::at(format!("{at}.timeout"), TIMEOUT_RULE));
    }
}

fn script_ok(script: &str) -> bool {
    script.starts_with(|c: char| c.is_ascii_alphanumeric())
        && script.len() <= 255
        && script
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !script.contains("..")
}
