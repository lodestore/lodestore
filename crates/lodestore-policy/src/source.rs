//! `spec.source`: what a policy backs up. Absent means the files in `spec.paths`.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{PolicySpec, Problem};

const MAX_VMIDS: usize = 1000;
const MAX_TAGS: usize = 20;
const MAX_DATABASES: usize = 1000;
const MAX_NAME_BYTES: usize = 64;
const MIN_VMID: u32 = 100;
const MAX_VMID: u32 = 999_999_999;

const GUEST_RULE: &str = "Select guests with vmids, pool or tags.";
const VMID_RULE: &str = "Enter a VMID from 100 to 999999999.";
const POOL_RULE: &str =
    "Use up to 64 letters, digits, \"-\", \"_\" and \".\"; start with a letter or digit.";
const TAG_RULE: &str = "Use up to 64 letters, digits, \"-\", \"_\", \"+\" and \".\"; start with a letter, digit or \"_\".";
const DATABASE_RULE: &str = "Use a name of 1 to 64 bytes without control characters.";

/// The `type` selects the variant and the other fields of the block sit next to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Source {
    Files(FilesSource),
    ProxmoxVm(ProxmoxGuests),
    ProxmoxCt(ProxmoxGuests),
    Postgres(DatabaseSource),
    Mysql(DatabaseSource),
}

/// A newtype over an empty struct rather than a unit variant: a unit variant of
/// an internally tagged enum silently drops unknown fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FilesSource {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProxmoxGuests {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vmids: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DatabaseSource {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub databases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_databases: Vec<String>,
}

impl Source {
    pub fn type_name(&self) -> &'static str {
        match self {
            Source::Files(_) => "files",
            Source::ProxmoxVm(_) => "proxmox-vm",
            Source::ProxmoxCt(_) => "proxmox-ct",
            Source::Postgres(_) => "postgres",
            Source::Mysql(_) => "mysql",
        }
    }

    pub fn is_files(&self) -> bool {
        matches!(self, Source::Files(_))
    }
}

/// The source that decides what is backed up: `files` and no source are the same thing.
pub fn normalized(source: &Option<Source>) -> Option<&Source> {
    source.as_ref().filter(|s| !s.is_files())
}

/// Errors inside the buffered block carry no path and a made-up position; naming the
/// field here lets `yaml_problem` report `spec.source`.
pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Source>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<Source>::deserialize(deserializer)
        .map_err(|e| serde::de::Error::custom(format!("spec.source: {e}")))
}

pub(crate) fn validate_source(spec: &PolicySpec, out: &mut Vec<Problem>) {
    let Some(source) = normalized(&spec.source) else {
        return;
    };
    let ty = source.type_name();
    let forbidden = [
        ("spec.paths", !spec.paths.is_empty()),
        ("spec.exclude", !spec.exclude.is_empty()),
        ("spec.oneFileSystem", spec.one_file_system),
        ("spec.excludeCaches", spec.exclude_caches),
    ];
    for (path, set) in forbidden {
        if set {
            out.push(Problem::at(
                path,
                format!("Leave out {path} for a {ty} source."),
            ));
        }
    }
    match source {
        Source::Files(_) => {}
        Source::ProxmoxVm(g) | Source::ProxmoxCt(g) => validate_guests(g, out),
        Source::Postgres(d) | Source::Mysql(d) => validate_databases(d, out),
    }
}

fn validate_guests(guests: &ProxmoxGuests, out: &mut Vec<Problem>) {
    const AT: &str = "spec.source";
    if guests.vmids.is_empty() && guests.pool.is_none() && guests.tags.is_empty() {
        out.push(Problem::at(AT, GUEST_RULE));
    }
    if guests.vmids.len() > MAX_VMIDS {
        out.push(Problem::at(
            format!("{AT}.vmids"),
            format!("At most {MAX_VMIDS} guests are allowed."),
        ));
    } else {
        let mut seen = HashSet::new();
        for (i, id) in guests.vmids.iter().enumerate() {
            if !(MIN_VMID..=MAX_VMID).contains(id) {
                out.push(Problem::at(format!("{AT}.vmids[{i}]"), VMID_RULE));
            } else if !seen.insert(id) {
                out.push(Problem::at(
                    format!("{AT}.vmids[{i}]"),
                    "This VMID is listed twice.",
                ));
            }
        }
    }
    if let Some(pool) = &guests.pool
        && !word_ok(pool, |c| matches!(c, '_' | '.' | '-'), false)
    {
        out.push(Problem::at(format!("{AT}.pool"), POOL_RULE));
    }
    if guests.tags.len() > MAX_TAGS {
        out.push(Problem::at(
            format!("{AT}.tags"),
            format!("At most {MAX_TAGS} tags are allowed."),
        ));
        return;
    }
    let mut seen = HashSet::new();
    for (i, tag) in guests.tags.iter().enumerate() {
        if !word_ok(tag, |c| matches!(c, '_' | '+' | '.' | '-'), true) {
            out.push(Problem::at(format!("{AT}.tags[{i}]"), TAG_RULE));
        } else if !seen.insert(tag) {
            out.push(Problem::at(
                format!("{AT}.tags[{i}]"),
                "This tag is listed twice.",
            ));
        }
    }
}

fn word_ok(text: &str, extra: impl Fn(char) -> bool, underscore_first: bool) -> bool {
    let first_ok = |c: char| c.is_ascii_alphanumeric() || (underscore_first && c == '_');
    (1..=MAX_NAME_BYTES).contains(&text.len())
        && text.starts_with(first_ok)
        && text.chars().all(|c| c.is_ascii_alphanumeric() || extra(c))
}

fn validate_databases(db: &DatabaseSource, out: &mut Vec<Problem>) {
    const AT: &str = "spec.source";
    if !db.databases.is_empty() && !db.exclude_databases.is_empty() {
        out.push(Problem::at(
            format!("{AT}.excludeDatabases"),
            "excludeDatabases is used only when databases is empty.",
        ));
    }
    validate_names(&db.databases, &format!("{AT}.databases"), out);
    validate_names(
        &db.exclude_databases,
        &format!("{AT}.excludeDatabases"),
        out,
    );
}

fn validate_names(names: &[String], path: &str, out: &mut Vec<Problem>) {
    if names.len() > MAX_DATABASES {
        out.push(Problem::at(
            path,
            format!("At most {MAX_DATABASES} names are allowed."),
        ));
        return;
    }
    let mut seen = HashSet::new();
    for (i, name) in names.iter().enumerate() {
        if !(1..=MAX_NAME_BYTES).contains(&name.len()) || name.chars().any(char::is_control) {
            out.push(Problem::at(format!("{path}[{i}]"), DATABASE_RULE));
        } else if !seen.insert(name) {
            out.push(Problem::at(
                format!("{path}[{i}]"),
                "This name is listed twice.",
            ));
        }
    }
}
