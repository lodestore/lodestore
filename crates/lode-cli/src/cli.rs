use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "lode",
    version,
    about = "Lodestore backup client",
    after_help = "The repository password or recovery key is read from the first of: \
--password-file, --password-command, LODE_PASSWORD_FILE, LODE_PASSWORD, or a prompt on a \
terminal; a --profile adds its password_file or password_command after LODE_PASSWORD. A value starting with lode-rk1- is taken as a recovery key. LODE_PASSWORD is visible \
to every process that can read this process's environment."
)]
pub struct Cli {
    /// Repository location: a folder path or s3:http(s)://host/bucket[/prefix]
    /// (credentials and region come from the standard AWS environment variables
    /// or shared config files). Falls back to LODE_REPOSITORY, then to the repository of
    /// --profile
    #[arg(short = 'r', long = "repo", global = true, value_name = "URL")]
    pub repo: Option<String>,

    /// Use the repository and password settings of this profile in
    /// $XDG_CONFIG_HOME/lode/config.toml (default ~/.config/lode/config.toml)
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Print machine-readable JSON (version 1) instead of text
    #[arg(long, global = true)]
    pub json: bool,

    #[command(flatten)]
    pub secret: SecretArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct SecretArgs {
    /// Read the password or recovery key from this file (one trailing newline is dropped)
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with = "password_command"
    )]
    pub password_file: Option<PathBuf>,

    /// Run CMD with `sh -c` and read the password or recovery key from its stdout
    #[arg(long, global = true, value_name = "CMD")]
    pub password_command: Option<String>,

    /// Back up with this writer-keys file (see `lode key export-writer`) instead of a
    /// password. Such keys can write and deduplicate but never read
    #[arg(long, global = true, value_name = "FILE")]
    pub writer_keys: Option<PathBuf>,

    #[arg(skip)]
    pub profile_password_file: Option<PathBuf>,

    #[arg(skip)]
    pub profile_password_command: Option<String>,
}

/// A recovery key that does not parse or fails its checksum; exit code 12.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UnlockError(pub String);

/// A wrong command line or configuration; exit code 2.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Repository management
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Back up files and directories into the repository
    Backup {
        #[arg(required_unless_present = "stdin", value_name = "PATHS")]
        paths: Vec<PathBuf>,
        #[command(flatten)]
        flags: BackupFlags,
    },
    /// List snapshots, oldest first
    Snapshots,
    /// Repository keys
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// List the files of a snapshot, or of one path inside it
    Ls {
        /// `latest`, a snapshot id or unique id prefix, optionally followed by
        /// `:<path>` to list only that path
        #[arg(value_name = "SNAPSHOT[:PATH]")]
        snapshot: String,
        /// Show type, mode, size and modification time
        #[arg(long)]
        long: bool,
    },
    /// Write one file of a snapshot to stdout
    Dump {
        /// `latest`, a snapshot id or unique id prefix
        snapshot: String,
        /// Absolute path of a regular file as recorded in the snapshot
        path: String,
    },
    /// Restore a snapshot below a target directory
    Restore {
        /// `latest`, a snapshot id or unique id prefix, optionally followed by
        /// `:<path>` to restore only that path directly into the target
        #[arg(value_name = "SNAPSHOT[:PATH]")]
        snapshot: String,
        /// Directory to restore into; files land at <DIR>/<original absolute path>
        #[arg(long, required = true, value_name = "DIR")]
        target: PathBuf,
        #[command(flatten)]
        flags: RestoreFlags,
    },
    /// Check the integrity of a repository without a server; exits 3 when it finds problems
    Check(CheckArgs),
    /// Manage a Lodestore server with an API token (LODESTORE_SERVER, LODESTORE_TOKEN)
    Server {
        #[command(flatten)]
        args: ServerArgs,
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Apply policy YAML files to a Lodestore server; exits 4 when --dry-run finds changes
    Apply(ApplyArgs),
    /// Free storage space of a repository on a Lodestore server (same as `lode server gc`)
    Gc {
        #[command(flatten)]
        server: ServerArgs,
        #[command(flatten)]
        args: GcArgs,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckLevel {
    Structure,
    Packs,
    Restore,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// structure lists the bucket and follows snapshots to their blobs; packs also downloads
    /// packs and verifies every blob; restore restores random files and compares their hashes
    #[arg(long, value_enum, default_value_t = CheckLevel::Structure)]
    pub level: CheckLevel,
    /// With --level packs: read at most this much pack data, for example 5GiB (default 5GiB)
    #[arg(long, value_name = "SIZE")]
    pub read_limit: Option<String>,
    /// With --level restore: restore this many files (1 to 1000, default 20)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub files: Option<u32>,
    /// With --level restore: stop when the restored files reach this size, for example 1GiB (default 1GiB)
    #[arg(long, value_name = "SIZE")]
    pub max_data: Option<String>,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct GcArgs {
    #[command(subcommand)]
    pub command: Option<GcCommand>,
    /// Repository name or id; a name is looked up in the policies (needs policies:read)
    #[arg(value_name = "REPO", required = true)]
    pub repo: Option<String>,
    /// Report how much space a cleanup would free and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum GcCommand {
    /// Show the state of the running cleanup and the last reports
    Status {
        /// Repository name or id
        repo: String,
    },
    /// Close the markers of backups that never finished, so deletion can go on
    Unlock {
        /// Repository name or id
        repo: String,
        /// Unlock without asking on a terminal
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct ApplyArgs {
    #[command(flatten)]
    pub server: ServerArgs,
    /// A policy file, or a folder searched for *.yaml and *.yml files
    #[arg(short = 'f', long = "filename", value_name = "PATH")]
    pub filename: PathBuf,
    /// Show what would change and change nothing; exits 4 when something would
    #[arg(long)]
    pub dry_run: bool,
    /// Also delete the policies of this manager that the input no longer contains
    #[arg(long)]
    pub prune: bool,
    /// Take over a policy of the same name that was created in the web interface
    #[arg(long)]
    pub adopt: bool,
    /// Name under which the applied policies are tracked
    #[arg(long, value_name = "NAME", default_value = "lode-apply")]
    pub manager: String,
    /// Delete with --prune without asking on a terminal
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ServerArgs {
    /// Server address, for example https://lodestore.example.com. Falls back to
    /// LODESTORE_SERVER, then to the server of --profile. The API token is read from
    /// LODESTORE_TOKEN, then from the token of --profile
    #[arg(long, global = true, value_name = "URL")]
    pub server: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// List hosts, show one, or set its labels
    Hosts {
        #[command(subcommand)]
        command: HostsCommand,
    },
    /// List, show, delete and run backup policies
    Policies {
        #[command(subcommand)]
        command: PoliciesCommand,
    },
    /// List jobs, read their logs, run, cancel and repeat them
    Jobs {
        #[command(subcommand)]
        command: JobsCommand,
    },
    /// List the snapshots the server knows
    Snapshots {
        #[command(subcommand)]
        command: ServerSnapshotsCommand,
    },
    /// Restore a snapshot to a host through the server
    Restore(ServerRestoreArgs),
    /// Free storage space of a repository: run a cleanup, a dry run, show or unlock it
    Gc(GcArgs),
    /// Run an integrity check of a repository on the server; exits 3 when it finds problems
    Check(ServerCheckArgs),
    /// Find a file by name or glob across the snapshots of every host
    Find(ServerFindArgs),
}

#[derive(Debug, Args)]
pub struct ServerFindArgs {
    /// A name fragment (nginx.conf), a glob for names (*.conf) or a glob for the whole path
    /// (/etc/nginx/**); case does not matter
    pub pattern: String,
    /// Search this host, by name or id; may be repeated; adds up with --label
    #[arg(long = "host", value_name = "HOST")]
    pub hosts: Vec<String>,
    /// Also search hosts with this label; may be repeated, a host must carry all of them
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServerCheckArgs {
    /// Repository name or id; a name is looked up in the policies (needs policies:read)
    pub repo: String,
    /// structure follows snapshots to their blobs; packs also downloads packs and verifies
    /// every blob; restore restores random files and compares their hashes
    #[arg(long, value_enum, default_value_t = CheckLevel::Structure)]
    pub level: CheckLevel,
}

#[derive(Debug, Subcommand)]
pub enum ServerSnapshotsCommand {
    /// List the snapshots of a host, newest first
    List {
        /// Host id or hostname
        #[arg(long, value_name = "HOST")]
        host: String,
        /// Stop after this many snapshots
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Mark a snapshot as suspicious; retention then neither counts nor forgets it
    Mark {
        /// Full snapshot id
        id: String,
        /// Why the snapshot looks suspicious (up to 200 characters)
        #[arg(long, value_name = "TEXT")]
        reason: String,
    },
    /// Clear the suspicious mark of a snapshot
    Clear {
        /// Full snapshot id
        id: String,
        /// Why the snapshot is fine (up to 200 characters)
        #[arg(long, value_name = "TEXT")]
        reason: String,
    },
    /// Delete a snapshot from the repository and the catalog; needs the snapshots:delete scope
    Delete {
        /// Full snapshot id
        id: String,
        /// Delete without asking
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictArg {
    OverwriteChanged,
    SkipExisting,
    StopIfNotEmpty,
}

#[derive(Debug, Args)]
pub struct ServerRestoreArgs {
    /// Full snapshot id
    pub snapshot: String,
    /// Host id or hostname to restore to
    #[arg(long, value_name = "HOST")]
    pub host: String,
    /// Restore only this absolute path of the snapshot; may be repeated
    #[arg(long, value_name = "PATH")]
    pub path: Vec<String>,
    /// Folder on the host to restore into (default /var/lodestore/restore/<date>-<id>)
    #[arg(long, value_name = "DIR", conflicts_with = "in_place")]
    pub target: Option<String>,
    /// Restore over the original files; needs --yes
    #[arg(long)]
    pub in_place: bool,
    /// Confirm --in-place
    #[arg(long)]
    pub yes: bool,
    /// What to do with a file that already exists
    #[arg(long, value_enum, default_value = "overwrite-changed")]
    pub conflict: ConflictArg,
    /// Do not restore file owners, setuid/setgid bits or file capabilities
    #[arg(long)]
    pub no_owners: bool,
    /// Restore a snapshot that nothing signed (never in place)
    #[arg(long)]
    pub accept_unverified: bool,
}

#[derive(Debug, Subcommand)]
pub enum PoliciesCommand {
    /// List all policies
    List,
    /// Print the YAML of a policy
    Get { name: String },
    /// Delete a policy; its snapshots stay in the repository
    Delete {
        name: String,
        /// Delete without asking
        #[arg(long)]
        yes: bool,
    },
    /// Queue a job on every host the policy selects
    Run { name: String },
    /// Show which snapshots the saved retention rules would forget; deletes nothing
    Retention { name: String },
}

#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    /// List jobs, active ones first
    List {
        /// queued, running, succeeded, warnings, failed, cancelled, interrupted or not_started
        #[arg(long, value_name = "STATE")]
        state: Option<String>,
        /// Only jobs of this policy
        #[arg(long, value_name = "NAME")]
        policy: Option<String>,
        /// Only jobs of this host, by id or hostname
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// backup, restore or download
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// One-based page number
        #[arg(long, value_name = "N")]
        page: Option<u64>,
    },
    /// Show one job
    Show { id: String },
    /// Print the log events of a job
    Logs {
        id: String,
        /// Keep printing new events until the job finishes
        #[arg(long)]
        follow: bool,
    },
    /// Queue policies on one host; all policies that apply when none is named
    Run {
        /// Host id or hostname
        #[arg(long, value_name = "HOST")]
        host: String,
        /// Policy to run; may be repeated
        #[arg(long, value_name = "NAME")]
        policy: Vec<String>,
    },
    /// Cancel a queued or running job
    Cancel { id: String },
    /// Queue the same policy on the same host again
    Rerun { id: String },
}

#[derive(Debug, Subcommand)]
pub enum HostsCommand {
    /// List all hosts
    List,
    /// Show one host
    Show {
        /// Host id or hostname
        #[arg(value_name = "HOST")]
        id: String,
    },
    /// Replace all labels of a host
    Label {
        /// Host id or hostname
        #[arg(value_name = "HOST")]
        id: String,
        #[arg(value_name = "KEY=VALUE", required_unless_present = "clear")]
        labels: Vec<String>,
        /// Remove all labels
        #[arg(long, conflicts_with = "labels")]
        clear: bool,
    },
}

#[derive(Debug, Args)]
pub struct RestoreFlags {
    /// What to do with a file that already exists in the target: replace it
    /// (`always`), keep it when its content is identical (`if-changed`), or
    /// leave it alone (`never`)
    #[arg(
        long,
        value_name = "MODE",
        default_value = "if-changed",
        value_parser = ["always", "if-changed", "never"]
    )]
    pub overwrite: String,

    /// Restore only paths matching this path or .gitignore-style pattern,
    /// anchored at /, together with the directories above them; may be repeated
    #[arg(long, value_name = "PATH|GLOB")]
    pub include: Vec<String>,

    /// Read every restored file back and compare it with the snapshot
    #[arg(long)]
    pub verify: bool,

    /// Show what would be restored; create and change nothing, not even the target
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct BackupFlags {
    /// Back up what is read from standard input as one file instead of paths;
    /// needs --name
    #[arg(long, requires = "name", conflicts_with = "paths")]
    pub stdin: bool,

    /// With --stdin: the file name the data is stored under (`/<NAME>` in the
    /// snapshot, which is tagged `stdin=<NAME>`)
    #[arg(long, value_name = "NAME", requires = "stdin")]
    pub name: Option<String>,

    /// With --stdin: add the tag `policy=<NAME>` to the snapshot
    #[arg(long, value_name = "NAME", requires = "stdin")]
    pub policy: Option<String>,

    /// With --stdin: hand the data to the local agent instead of writing to a
    /// repository; used by default when no repository is configured and the
    /// agent socket exists
    #[arg(long, requires = "stdin")]
    pub agent: bool,

    /// With --stdin: path of the agent socket (default /run/lodestore/agent.sock);
    /// implies --agent
    #[arg(long, value_name = "PATH", requires = "stdin")]
    pub agent_socket: Option<PathBuf>,

    /// Compare files with this snapshot (id or unique prefix) instead of the
    /// latest one of this host with the same paths. Not available with --writer-keys
    #[arg(long, value_name = "SNAPSHOT")]
    pub parent: Option<String>,

    /// Read every file again, even when the parent snapshot says it is unchanged
    #[arg(long)]
    pub force_rescan: bool,

    /// Host name to record in the snapshot instead of this machine's
    #[arg(long, value_name = "NAME")]
    pub host: Option<String>,

    /// Tag to put on the snapshot; may be repeated
    #[arg(long, value_name = "TAG")]
    pub tag: Vec<String>,

    /// Leave out paths matching this .gitignore-style pattern, anchored at /
    /// (`*.log`, `/srv/tmp`, `build/`, `!keep.log`, `**/node_modules`); may be repeated
    #[arg(long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Read exclude patterns from this file, one per line; may be repeated
    #[arg(long, value_name = "PATH")]
    pub exclude_file: Vec<PathBuf>,

    /// Do not descend into directories on another filesystem than the backed-up path;
    /// they are recorded as empty directories
    #[arg(long)]
    pub one_file_system: bool,

    /// Keep only the CACHEDIR.TAG file of directories that carry a valid one
    #[arg(long)]
    pub exclude_caches: bool,

    /// Skip the check that the credentials can read locks/ in the repository
    #[arg(long)]
    pub no_gc_check: bool,
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Create a new repository
    Init {
        /// Save the recovery key to this new file (mode 0600). Required when
        /// there is no terminal to show the key on
        #[arg(long, value_name = "PATH")]
        recovery_key_file: Option<PathBuf>,
    },
    /// Rebuild the index from the packs; needed after index/ objects were lost
    RebuildIndex,
}

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Write a key file that lets a host back up but not read
    ExportWriter {
        /// New file to create (mode 0600); an existing path is refused
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Add a password slot, or with --recovery another recovery key
    Add {
        /// Add a new recovery key instead of a password
        #[arg(long)]
        recovery: bool,
        /// With --recovery: save the new key to this new file (mode 0600).
        /// Required when there is no terminal to show the key on
        #[arg(long, value_name = "PATH", requires = "recovery")]
        recovery_key_file: Option<PathBuf>,
        /// Read the new password from this file instead of prompting
        #[arg(long, value_name = "PATH", conflicts_with = "recovery")]
        new_password_file: Option<PathBuf>,
    },
    /// Change the password this command was unlocked with; recovery keys stay valid
    Passwd {
        /// Read the new password from this file instead of prompting
        #[arg(long, value_name = "PATH")]
        new_password_file: Option<PathBuf>,
    },
}
