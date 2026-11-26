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
    let known = STORAGE_PRESETS
        .iter()
        .find(|p| p.id == preset)
        .ok_or_else(|| crate::Error::InvalidInput("unknown preset".into()))?;
    if bucket.is_empty() && !matches!(preset, "sftp" | "hetzner-storage-box" | "azure-blob") {
        return Err(invalid("bucket", "required"));
    }
    if !bucket.is_empty() {
        check_bucket(bucket)?;
    }
    check_prefix(prefix)?;
    if !user.is_empty() {
        check_user(user)?;
    }
    let mut rights = match preset {
        "aws-s3" => AgentRights::new(
            RightsMechanism::Policy,
            agent_iam_policy_json(bucket, prefix),
            "Denies any PutObject without If-None-Match, so keys cannot replace objects; checked only against the S3 emulator, not the real service.",
        ),
        "minio" => AgentRights::new(
            RightsMechanism::Commands,
            agent_minio_commands(bucket, prefix),
            "MinIO rejects the If-None-Match condition, so the key can overwrite; turn on versioning or Object Lock.",
        ),
        "wasabi" => AgentRights::new(
            RightsMechanism::Policy,
            agent_minio_policy_json(bucket, prefix),
            "The policy has no If-None-Match condition because Wasabi does not document one; turn on versioning or Object Lock.",
        ),
        "backblaze-b2" => b2_rights(bucket, prefix),
        "cloudflare-r2" => AgentRights::new(
            RightsMechanism::Instruction,
            format!(
                "R2 tokens cannot be limited to writing without deleting: Object Read & Write also deletes, and scopes are per bucket, not per prefix.\n\
                 1. Create an API token with Object Read & Write limited to the bucket {bucket}.\n\
                 2. Turn on a bucket lock rule for the repository prefix so stored objects cannot be deleted or replaced.\n\
                 3. Connect the repository; the check measures the guarantee you actually got.\n"
            ),
            "No token level writes without deleting; protection comes from the bucket lock, and the check reports the real level.",
        ),
        "hetzner-object-storage" => hetzner_rights(bucket, prefix),
        "hetzner-storage-box" => AgentRights::new(
            RightsMechanism::Instruction,
            "A Storage Box is managed SFTP and its sshd cannot be configured, so an agent account cannot be forbidden to delete.\n\
             1. Create a sub-account whose base directory is the repository directory.\n\
             2. Give the agent only that sub-account; the server keeps the main account.\n\
             3. Keep Storage Box snapshots so an overwritten or deleted object can be recovered.\n"
                .into(),
            "The level stays Host can overwrite or delete, as for any SFTP user that cannot be restricted by the host.",
        ),
        "azure-blob" => AgentRights::new(
            RightsMechanism::ServerIssued,
            "The server signs a short-lived SAS with read, create and list rights for each agent when the repository was created with the storage account key. Nothing needs to be created.\n\
             Without the account key, put a container SAS with the rcl permissions in agent_options.\n"
                .into(),
            "The server issues the SAS, so no commands are generated.",
        ),
        "google-cloud-storage" => gcs_rights(bucket),
        _ => sftp_rights(prefix, if user.is_empty() { "lodestore-agent" } else { user })?,
    };
    rights.verified = known.verified;
    Ok(rights)
}

impl AgentRights {
    fn new(mechanism: RightsMechanism, text: String, note: &'static str) -> Self {
        Self {
            mechanism,
            text,
            server_text: None,
            verified: false,
            note,
        }
    }
}

fn check_user(user: &str) -> crate::Result<()> {
    let mut chars = user.chars();
    let ok = user.len() <= 32
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "user",
            "up to 32 characters from a-z 0-9 _ -, starting with a letter or _",
        ))
    }
}

fn b2_rights(bucket: &str, prefix: &str) -> AgentRights {
    let prefix = prefix.trim_matches('/');
    let name_prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!(" --name-prefix {prefix}/")
    };
    AgentRights::new(
        RightsMechanism::Commands,
        format!(
            "b2 key create --bucket {bucket}{name_prefix} lodestore-agent listBuckets,listFiles,readFiles,writeFiles\n"
        ),
        "writeFiles lets the key hide a file, which keeps the old version; a lifecycle rule can still delete hidden versions.",
    )
}

fn hetzner_rights(bucket: &str, prefix: &str) -> AgentRights {
    let arn = |path: &str| json_string(&format!("arn:aws:s3:::{bucket}/{}", join(prefix, path)));
    let objects = arn("*");
    let writable = [arn("packs/*"), arn("index/*"), arn("snapshots/*")].join(", ");
    let bucket_arn = json_string(&format!("arn:aws:s3:::{bucket}"));
    let principal = json_string("arn:aws:iam:::user/pAGENT_PROJECT_ID:AGENT_ACCESS_KEY");
    let policy = format!(
        r#"{{
  "Version": "2012-10-17",
  "Statement": [
    {{
      "Effect": "Allow",
      "Principal": {{"AWS": [{principal}]}},
      "Action": ["s3:PutObject"],
      "Resource": [{writable}]
    }},
    {{
      "Effect": "Allow",
      "Principal": {{"AWS": [{principal}]}},
      "Action": ["s3:GetObject", "s3:GetObjectVersion"],
      "Resource": [{objects}]
    }},
    {{
      "Effect": "Allow",
      "Principal": {{"AWS": [{principal}]}},
      "Action": ["s3:ListBucket"],
      "Resource": [{bucket_arn}]
    }}
  ]
}}
"#
    );
    let text = format!(
        "# Create the agent key in a second Hetzner project, then apply this policy with the bucket owner's key.\n\
         cat > lodestore-agent-policy.json <<'EOF'\n{policy}EOF\n\
         aws s3api put-bucket-policy --bucket {bucket} --policy file://lodestore-agent-policy.json\n"
    );
    AgentRights::new(
        RightsMechanism::Policy,
        text,
        "Not checked against the real service: the key lives in another project and the policy limits it to get and list, and to put under packs/, index/ and snapshots/, where it can still overwrite; also turn on Object Lock.",
    )
}

fn sftp_rights(root: &str, user: &str) -> crate::Result<AgentRights> {
    let root = root.trim_end_matches('/');
    if !root.starts_with('/') {
        return Err(invalid("prefix", "an absolute path is required for SFTP"));
    }
    let text = format!(
        "groupadd -f lode\n\
         useradd --create-home --shell /usr/sbin/nologin --groups lode {user}\n\
         usermod -aG lode SERVER_USER\n\
         install -d -m 0755 -o root -g root {root}\n\
         install -d -m 2775 -o SERVER_USER -g lode {root}/data\n\
         # Add the agent's public key to /home/{user}/.ssh/authorized_keys.\n\
         # Append to /etc/ssh/sshd_config, then reload sshd.\n\
         Match User {user}\n\
         \x20   ChrootDirectory {root}\n\
         \x20   ForceCommand internal-sftp -P {SFTP_DENY} -u 0222\n\
         \x20   AllowTcpForwarding no\n\
         \x20   X11Forwarding no\n\
         # The agent sees the repository as /data.\n"
    );
    Ok(AgentRights::new(
        RightsMechanism::Commands,
        text,
        "The level stays Host can overwrite or delete, because an SFTP user can keep appending to a file it holds open after it was published.",
    ))
}

const SFTP_DENY: &str =
    "remove,rmdir,rename,posix-rename,setstat,fsetstat,lsetstat,symlink,copy-data,mkdir";

#[derive(Debug, Clone, Copy)]
pub struct StoragePreset {
    pub id: &'static str,
    pub label: &'static str,
    pub backend: &'static str,
    pub endpoint_template: Option<&'static str>,
    pub region: Option<&'static str>,
    pub path_style: bool,
    pub provider: Option<&'static str>,
    pub mechanism: RightsMechanism,
    pub verified: bool,
    pub note: &'static str,
}

const fn preset(
    id: &'static str,
    label: &'static str,
    backend: &'static str,
    mechanism: RightsMechanism,
    note: &'static str,
) -> StoragePreset {
    StoragePreset {
        id,
        label,
        backend,
        endpoint_template: None,
        region: None,
        path_style: false,
        provider: None,
        mechanism,
        verified: false,
        note,
    }
}

pub const STORAGE_PRESETS: &[StoragePreset] = &[
    preset(
        "aws-s3",
        "Amazon S3",
        "s3",
        RightsMechanism::Policy,
        "IAM policy without delete.",
    ),
    StoragePreset {
        path_style: true,
        verified: true,
        ..preset(
            "minio",
            "MinIO",
            "s3",
            RightsMechanism::Commands,
            "mc commands for a user without delete.",
        )
    },
    StoragePreset {
        endpoint_template: Some("https://s3.{region}.wasabisys.com"),
        ..preset(
            "wasabi",
            "Wasabi",
            "s3",
            RightsMechanism::Policy,
            "IAM policy without delete.",
        )
    },
    StoragePreset {
        endpoint_template: Some("https://s3.{region}.backblazeb2.com"),
        provider: Some("b2"),
        ..preset(
            "backblaze-b2",
            "Backblaze B2",
            "s3",
            RightsMechanism::Commands,
            "Application key without deleteFiles.",
        )
    },
    StoragePreset {
        endpoint_template: Some("https://{account_id}.r2.cloudflarestorage.com"),
        region: Some("auto"),
        ..preset(
            "cloudflare-r2",
            "Cloudflare R2",
            "s3",
            RightsMechanism::Instruction,
            "A token cannot be limited; use a bucket lock.",
        )
    },
    StoragePreset {
        endpoint_template: Some("https://{region}.your-objectstorage.com"),
        ..preset(
            "hetzner-object-storage",
            "Hetzner Object Storage",
            "s3",
            RightsMechanism::Policy,
            "Bucket policy for a key from another project.",
        )
    },
    StoragePreset {
        endpoint_template: Some("{user}.your-storagebox.de:23"),
        ..preset(
            "hetzner-storage-box",
            "Hetzner Storage Box",
            "sftp",
            RightsMechanism::Instruction,
            "A sub-account with the repository directory as its base.",
        )
    },
    preset(
        "azure-blob",
        "Azure Blob Storage",
        "azblob",
        RightsMechanism::ServerIssued,
        "The server issues a SAS without delete.",
    ),
    preset(
        "google-cloud-storage",
        "Google Cloud Storage",
        "gcs",
        RightsMechanism::Commands,
        "Custom role without delete.",
    ),
    StoragePreset {
        verified: true,
        ..preset(
            "sftp",
            "SFTP (OpenSSH)",
            "sftp",
            RightsMechanism::Commands,
            "Chrooted user with a deny list.",
        )
    },
];

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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn rights(preset: &str) -> AgentRights {
        agent_rights(preset, "repo-bucket", "lode", "").unwrap()
    }

    fn safe(s: &str, extra: &[char]) -> bool {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    }

    #[test]
    fn rights_gcs_role_has_no_delete_or_update() {
        let r = rights("google-cloud-storage");
        for word in ["delete", "update", "move"] {
            assert!(!r.text.contains(word), "{word}");
        }
    }

    #[test]
    fn rights_b2_key_has_no_delete_files() {
        let r = rights("backblaze-b2");
        assert!(r.text.contains(
            "--bucket repo-bucket --name-prefix lode/ lodestore-agent listBuckets,listFiles,readFiles,writeFiles\n"
        ));
        assert!(!r.text.contains("deleteFiles"));
        let none = agent_rights("backblaze-b2", "repo-bucket", "", "").unwrap();
        assert!(!none.text.contains("--name-prefix"));
    }

    #[test]
    fn rights_sftp_denies_every_destructive_request() {
        let r = agent_rights("sftp", "", "/srv/lodestore", "backup-agent").unwrap();
        let list = r.text.split("internal-sftp -P ").nth(1).unwrap();
        let list: Vec<&str> = list.split(' ').next().unwrap().split(',').collect();
        for op in [
            "remove",
            "rmdir",
            "rename",
            "posix-rename",
            "setstat",
            "fsetstat",
            "lsetstat",
            "symlink",
            "copy-data",
            "mkdir",
        ] {
            assert!(list.contains(&op), "{op}");
        }
        assert!(r.text.contains("-u 0222"));
        assert!(r.text.contains("ChrootDirectory /srv/lodestore"));
        assert!(r.text.contains("Match User backup-agent"));
        assert!(r.note.contains("overwrite or delete"));
        assert!(agent_rights("sftp", "", "relative", "").is_err());
    }

    #[test]
    fn rights_hetzner_policy_allows_only_put_get_list() {
        let r = rights("hetzner-object-storage");
        assert!(!r.verified);
        let json = r.text.split("<<'EOF'\n").nth(1).unwrap();
        let json = json.split("EOF\n").next().unwrap();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let allowed = [
            "s3:PutObject",
            "s3:GetObject",
            "s3:GetObjectVersion",
            "s3:ListBucket",
        ];
        for s in v["Statement"].as_array().unwrap() {
            assert_eq!(s["Effect"], "Allow");
            assert_eq!(
                s["Principal"]["AWS"][0],
                "arn:aws:iam:::user/pAGENT_PROJECT_ID:AGENT_ACCESS_KEY"
            );
            for a in s["Action"].as_array().unwrap() {
                assert!(allowed.contains(&a.as_str().unwrap()), "{a}");
                if a == "s3:PutObject" {
                    for r in s["Resource"].as_array().unwrap() {
                        let r = r.as_str().unwrap();
                        assert!(
                            ["/packs/*", "/index/*", "/snapshots/*"]
                                .iter()
                                .any(|p| r.ends_with(p)),
                            "{r}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rights_wasabi_policy_is_the_minio_policy() {
        assert_eq!(
            rights("wasabi").text,
            agent_minio_policy_json("repo-bucket", "lode")
        );
    }

    #[test]
    fn rights_r2_is_an_instruction() {
        assert_eq!(
            rights("cloudflare-r2").mechanism,
            RightsMechanism::Instruction
        );
    }

    #[test]
    fn rights_storage_box_is_an_instruction() {
        let r = agent_rights("hetzner-storage-box", "", "", "").unwrap();
        assert_eq!(r.mechanism, RightsMechanism::Instruction);
    }

    #[test]
    fn rights_azure_is_server_issued() {
        let r = agent_rights("azure-blob", "", "", "").unwrap();
        assert_eq!(r.mechanism, RightsMechanism::ServerIssued);
    }

    #[test]
    fn presets_ids_are_unique_and_every_preset_has_rights() {
        for (i, p) in STORAGE_PRESETS.iter().enumerate() {
            assert!(
                STORAGE_PRESETS[..i].iter().all(|q| q.id != p.id),
                "{}",
                p.id
            );
            let r = agent_rights(p.id, "repo-bucket", "/srv/lodestore", "").unwrap();
            assert_eq!(r.mechanism, p.mechanism, "{}", p.id);
            assert_eq!(r.verified, p.verified, "{}", p.id);
        }
    }

    #[test]
    fn rights_refuse_hostile_input() {
        for bad in [
            "a;rm -rf", "$(id)", "a'b", "a b", "a\nb", "a`b`", "a\"b", "-ab",
        ] {
            assert!(agent_rights("minio", bad, "", "").is_err(), "{bad:?}");
            assert!(agent_rights("minio", "bucket", bad, "").is_err(), "{bad:?}");
            assert!(agent_rights("sftp", "", "/srv/x", bad).is_err(), "{bad:?}");
        }
        assert!(agent_rights("minio", "bucket", "a/../b", "").is_err());
    }

    proptest! {
        #[test]
        fn rights_accept_only_safe_input(
            b in "[a-z0-9./;$ '`\"\\\\\\n_-]{0,12}",
            p in "[a-z0-9./;$ '`\"\\\\\\n_-]{0,12}",
            u in "[a-z0-9;$ '`\"\\\\\\n_-]{0,12}",
        ) {
            if let Ok(r) = agent_rights("minio", &b, &p, "") {
                prop_assert!(safe(&b, &['.', '_', '-']));
                prop_assert!(safe(&p, &['.', '_', '/', '-']));
                prop_assert!(!p.split('/').any(|s| s == ".."));
                prop_assert!(!r.text.contains(';'));
            }
            if agent_rights("sftp", "", "/srv/x", &u).is_ok() {
                prop_assert!(safe(&u, &['_', '-']));
            }
        }
    }
}
