fn json_string(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn join(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    }
}

/// The IAM policy a write-only agent key needs on AWS S3: add objects under
/// `packs/`, `index/` and `snapshots/`, read `config`, `index/` and `locks/`,
/// list `index/` and `locks/`, and nothing else, in particular no delete and
/// no write under `locks/`. A final statement
/// refuses any `PutObject` without `If-None-Match`, so a key holder cannot
/// replace an existing object.
pub fn agent_iam_policy_json(bucket: &str, prefix: &str) -> String {
    policy_json(bucket, prefix, true)
}

/// The same policy without the `s3:if-none-match` statement, for MinIO, which
/// rejects that condition key. There the key holder can still overwrite
/// objects; versioning or Object Lock is what protects them.
pub fn agent_minio_policy_json(bucket: &str, prefix: &str) -> String {
    policy_json(bucket, prefix, false)
}

fn policy_json(bucket: &str, prefix: &str, require_if_none_match: bool) -> String {
    let arn = |path: &str| json_string(&format!("arn:aws:s3:::{bucket}/{}", join(prefix, path)));
    let list = |s: &str| json_string(&join(prefix, s));
    let deny_unconditional = if require_if_none_match {
        format!(
            r#",
    {{
      "Effect": "Deny",
      "Action": ["s3:PutObject"],
      "Resource": [{}, {}, {}],
      "Condition": {{"Null": {{"s3:if-none-match": "true"}}}}
    }}"#,
            arn("packs/*"),
            arn("index/*"),
            arn("snapshots/*"),
        )
    } else {
        String::new()
    };
    format!(
        r#"{{
  "Version": "2012-10-17",
  "Statement": [
    {{
      "Effect": "Allow",
      "Action": ["s3:PutObject"],
      "Resource": [{packs}, {index_all}, {snapshots}]
    }},
    {{
      "Effect": "Allow",
      "Action": ["s3:GetObject"],
      "Resource": [{config}, {index_all}, {locks_all}]
    }},
    {{
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": [{bucket_arn}],
      "Condition": {{"StringLike": {{"s3:prefix": [{list_all}, {list_dir}, {locks_list_all}, {locks_list_dir}]}}}}
    }}{deny_unconditional}
  ]
}}
"#,
        packs = arn("packs/*"),
        index_all = arn("index/*"),
        snapshots = arn("snapshots/*"),
        config = arn("config"),
        locks_all = arn("locks/*"),
        bucket_arn = json_string(&format!("arn:aws:s3:::{bucket}")),
        list_all = list("index/*"),
        list_dir = list("index/"),
        locks_list_all = list("locks/*"),
        locks_list_dir = list("locks/"),
    )
}

/// `mc` commands that create the policy and a user bound to it.
pub fn agent_minio_commands(bucket: &str, prefix: &str) -> String {
    format!(
        "cat > lodestore-agent-policy.json <<'EOF'\n{}EOF\n\
         mc admin policy create ALIAS lodestore-agent lodestore-agent-policy.json\n\
         mc admin user add ALIAS AGENT_ACCESS_KEY AGENT_SECRET_KEY\n\
         mc admin policy attach ALIAS lodestore-agent --user AGENT_ACCESS_KEY\n",
        agent_minio_policy_json(bucket, prefix)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightsMechanism {
    Policy,
    Commands,
    Instruction,
    ServerIssued,
}

impl RightsMechanism {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::Commands => "commands",
            Self::Instruction => "instruction",
            Self::ServerIssued => "server_issued",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRights {
    pub mechanism: RightsMechanism,
    pub text: String,
    pub server_text: Option<String>,
    pub verified: bool,
    pub note: &'static str,
}

fn invalid(field: &str, why: &str) -> crate::Error {
    crate::Error::InvalidInput(format!("{field}: {why}"))
}

fn check_bucket(bucket: &str) -> crate::Result<()> {
    let ok = (3..=63).contains(&bucket.len())
        && bucket.starts_with(|c: char| c.is_ascii_alphanumeric())
        && bucket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "bucket",
            "3 to 63 characters from A-Z a-z 0-9 . _ -, starting with a letter or digit",
        ))
    }
}

fn check_prefix(prefix: &str) -> crate::Result<()> {
    let ok = prefix.len() <= 512
        && !prefix.starts_with('-')
        && prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        && !prefix.split('/').any(|segment| segment == "..");
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "prefix",
            "up to 512 characters from A-Z a-z 0-9 . _ / -, without \"..\" and a leading \"-\"",
        ))
    }
}

/// Rights text for one storage preset. Pure text over validated input: no
/// network calls and no server credentials; project and key ids stay
/// placeholders.
pub fn agent_rights(
    preset: &str,
    bucket: &str,
    prefix: &str,
    user: &str,
) -> crate::Result<AgentRights> {
    check_bucket(bucket)?;
    check_prefix(prefix)?;
    let _ = user;
    match preset {
        "google-cloud-storage" => Ok(gcs_rights(bucket)),
        _ => Err(crate::Error::InvalidInput("unknown preset".into())),
    }
}

fn gcs_rights(bucket: &str) -> AgentRights {
    let text = format!(
        "gcloud iam roles create lodestoreAgent --project=PROJECT_ID --title=\"Lodestore agent\" --permissions=storage.objects.create,storage.objects.get,storage.objects.list\n\
         gcloud iam service-accounts create lodestore-agent --project=PROJECT_ID\n\
         gcloud storage buckets add-iam-policy-binding gs://{bucket} --member=serviceAccount:lodestore-agent@PROJECT_ID.iam.gserviceaccount.com --role=projects/PROJECT_ID/roles/lodestoreAgent\n\
         gcloud iam service-accounts keys create agent.json --iam-account=lodestore-agent@PROJECT_ID.iam.gserviceaccount.com\n\
         base64 -w0 agent.json\n\
         # The base64 output is the value of agent_options.credential.\n"
    );
    let server_text = format!(
        "gcloud storage buckets add-iam-policy-binding gs://{bucket} --member=serviceAccount:SERVER_SA_EMAIL --role=roles/storage.legacyBucketReader\n"
    );
    AgentRights {
        mechanism: RightsMechanism::Commands,
        text,
        server_text: Some(server_text),
        verified: false,
        note: "Overwriting on GCS needs the delete permission, which this role lacks; checked only on an emulator without IAM.",
    }
}
