use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
        #[arg(required = true, value_name = "PATHS")]
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
