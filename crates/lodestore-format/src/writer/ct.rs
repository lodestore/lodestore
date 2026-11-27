//! Backs up a Proxmox VE container as a tree: the container's directory below
//! `/fs`, its configuration below `/config`; see [`crate::ct`] for the layout.

use std::os::fd::OwnedFd;

use rustix::fs::{FileType, Stat, fstat};

use super::{
    BackupOptions, BackupOutcome, BackupProgress, Deadline, Session, Writer, hostname, now,
};
use crate::chunker::Chunker;
use crate::crypto::{BLOB_TYPE_DATA, BlobId};
use crate::ct::{
    CONFIG_DIR, CONFIG_FILE, CtMeta, FIREWALL_FILE, FS_DIR, FS_PATH, IdMap, MAX_CT_CONFIG_BYTES,
};
use crate::error::{Error, Result};
use crate::fs::filter::PathMatcher;
use crate::fs::idmap::{Direction, OVERFLOW_ID, shift_node};
use crate::fs::walk::Walker;
use crate::fs::{conv, meta};
use crate::pb;
use crate::source::{self, Meta, SourceKey, SourceType};
use crate::store::BlobStore;

/// What a container backup records besides the tree.
#[derive(Debug, Clone)]
pub struct CtBackup {
    /// A `proxmox-ct` key.
    pub key: SourceKey,
    pub meta: CtMeta,
    /// Meta of the source the caller adds (`vmid`, `name`); no `ct_` key.
    pub source_meta: Meta,
    /// `pct.conf` as vzdump passed it.
    pub config: String,
    pub firewall: Option<String>,
    /// Patterns anchored at the root of the container.
    pub exclude: Option<PathMatcher>,
}

/// How the walk of a container differs from a file backup.
pub(super) struct CtWalk {
    pub(super) idmap: Option<IdMap>,
    pub(super) unmapped: u64,
}

impl CtWalk {
    /// Like the `tar` of vzdump, the walk leaves out every `lost+found` and
    /// every socket.
    pub(super) fn skipped(path: &[u8], st: &Stat) -> bool {
        path.ends_with(b"/lost+found") || FileType::from_raw_mode(st.st_mode) == FileType::Socket
    }

    /// The path an exclude pattern of the container sees: below `/fs`.
    pub(super) fn inner(path: &[u8]) -> &[u8] {
        path.get(FS_PATH.len()..).unwrap_or(path)
    }
}

/// A walked and uploaded container whose snapshot is not written yet. The
/// snapshot is written only by [`PendingContainer::commit`]; dropping it
/// writes none.
#[derive(Debug)]
pub struct PendingContainer {
    snapshot: pb::Snapshot,
    deadline: Option<Deadline>,
    root_tree: BlobId,
    time: pb::Timestamp,
    stats: pb::SnapshotStats,
    unmapped: u64,
}

impl PendingContainer {
    pub fn stats(&self) -> &pb::SnapshotStats {
        &self.stats
    }

    /// Ids outside the container's id map, stored as the overflow id.
    pub fn unmapped_ids(&self) -> u64 {
        self.unmapped
    }

    pub async fn commit<S: BlobStore>(self, writer: &mut Writer<S>) -> Result<BackupOutcome> {
        Deadline::check(self.deadline)?;
        let snapshot_id = writer.put_snapshot(&self.snapshot).await?;
        Ok(BackupOutcome {
            snapshot_id,
            root_tree: self.root_tree,
            time: self.time,
            stats: self.stats,
            incomplete: self.snapshot.incomplete,
            errors_total: self.snapshot.errors_total,
            warnings_total: self.snapshot.warnings_total,
        })
    }
}

fn check_config(name: &str, text: &str) -> Result<()> {
    if text.len() > MAX_CT_CONFIG_BYTES {
        return Err(Error::InvalidInput(format!(
            "{name} is larger than {MAX_CT_CONFIG_BYTES} bytes"
        )));
    }
    Ok(())
}

impl<S: BlobStore> Writer<S> {
    /// Walks the container's directory `dir` (opened `O_DIRECTORY|O_NOFOLLOW`)
    /// and uploads its data, the tree and the configuration. The ids of the
    /// container's view are recorded; owner names are left empty.
    pub async fn backup_container(
        &mut self,
        dir: &OwnedFd,
        ct: CtBackup,
        mut options: BackupOptions<'_>,
        progress: &dyn Fn(&BackupProgress),
    ) -> Result<PendingContainer> {
        if ct.key.kind() != SourceType::ProxmoxCt {
            return Err(Error::InvalidInput(
                "a container backup has a proxmox-ct source key".into(),
            ));
        }
        if options.source.is_some() || options.walk.exclude.is_some() {
            return Err(Error::InvalidInput(
                "a container backup takes its source and its excludes from CtBackup".into(),
            ));
        }
        check_config("pct.conf", &ct.config)?;
        if let Some(fw) = &ct.firewall {
            check_config("pct.fw", fw)?;
        }
        let mut meta = ct.source_meta.clone();
        for (key, value) in ct.meta.to_meta()? {
            if meta.insert(key.clone(), value).is_some() {
                return Err(Error::InvalidInput(format!(
                    "the source meta of a container may not set {key}"
                )));
            }
        }
        let recorded = source::encode(&ct.key, &meta)?;
        let st = fstat(dir)?;
        if Walker::node_type(&st) != Some(pb::NodeType::Dir) {
            return Err(Error::InvalidInput(
                "the container is backed up from a directory".into(),
            ));
        }
        let started = options.time.unwrap_or_else(now);
        let parent_id = options.parent.as_ref().map(|p| p.id().to_vec());

        let mut run = Session::new(self, progress);
        run.apply(&mut options);
        run.walk.exclude.clone_from(&ct.exclude);
        run.ct = Some(CtWalk {
            idmap: ct.meta.idmap.clone(),
            unmapped: 0,
        });
        run.root_dev = conv(st.st_dev);
        let root_tree = match run.container_tree(dir, &st, &ct, started).await {
            Ok(root) => root,
            Err(e @ (Error::Interrupted | Error::DeadlineExceeded { .. })) => {
                run.wind_down().await?;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        run.wind_down().await?;

        let unmapped = run.ct.as_ref().map_or(0, |c| c.unmapped);
        if unmapped > 0 {
            run.issue(
                true,
                pb::IssueKind::Unspecified,
                FS_PATH,
                format!(
                    "{unmapped} owner ids are outside the container's id map and were stored as {OVERFLOW_ID}"
                ),
            );
        }
        let stats = run.stats;
        let snapshot = pb::Snapshot {
            time: Some(started),
            hostname: options.hostname.unwrap_or_else(hostname),
            paths: vec![FS_PATH.to_vec()],
            tags: options.tags,
            parent: parent_id.unwrap_or_default(),
            root_tree: root_tree.to_vec(),
            stats: Some(stats),
            incomplete: run.errors_total > 0,
            errors: run.errors,
            warnings: run.warnings,
            errors_total: run.errors_total,
            warnings_total: run.warnings_total,
            index_refs: run.refs.iter().map(|r| r.to_vec()).collect(),
            agent_id: options.agent_id.unwrap_or_default(),
            source: recorded,
            program_version: format!("lodestore-format {}", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        Ok(PendingContainer {
            snapshot,
            deadline: options.deadline,
            root_tree,
            time: started,
            stats,
            unmapped,
        })
    }
}

impl<S: BlobStore> Session<'_, '_, S> {
    async fn container_tree(
        &mut self,
        dir: &OwnedFd,
        st: &Stat,
        ct: &CtBackup,
        started: pb::Timestamp,
    ) -> Result<BlobId> {
        let children = self.backup_children(dir, FS_PATH).await?;
        let mut fs = Walker::node_of(FS_DIR, st, pb::NodeType::Dir, false);
        match meta::capture_xattrs(dir) {
            Ok(x) => fs.xattrs = x,
            Err(e) => self.xattr_issue(FS_PATH, e),
        }
        self.ct_node(&mut fs, FS_PATH);
        fs.subtrees = self.write_tree(children).await?;
        self.stats.dirs += 1;

        let mut files = vec![
            self.config_file(CONFIG_FILE, ct.config.as_bytes(), started)
                .await?,
        ];
        if let Some(fw) = &ct.firewall {
            files.push(
                self.config_file(FIREWALL_FILE, fw.as_bytes(), started)
                    .await?,
            );
        }
        let mut config = synthetic(CONFIG_DIR, pb::NodeType::Dir, 0o700, started);
        config.subtrees = self.write_tree(files).await?;
        self.stats.dirs += 1;
        self.write_root(vec![config, fs]).await
    }

    /// The node of an entry below `/fs` as the container sees it.
    /// The node of an entry below `/fs` as the container sees it: owners,
    /// ACLs and capabilities translated by the id map and no owner names. An
    /// ACL or capability xattr that does not parse is left out with a warning.
    pub(super) fn ct_node(&mut self, node: &mut pb::Node, path: &[u8]) {
        let Some(ct) = self.ct.as_mut() else {
            return;
        };
        node.user.clear();
        node.group.clear();
        let Some(map) = &ct.idmap else {
            return;
        };
        let dropped = shift_node(node, Direction::HostToCt, map, &mut ct.unmapped);
        for (name, e) in dropped {
            self.issue(
                true,
                pb::IssueKind::Unreadable,
                path,
                format!(
                    "extended attribute {} not captured: {e}",
                    String::from_utf8_lossy(&name)
                ),
            );
        }
    }

    async fn config_file(
        &mut self,
        name: &[u8],
        bytes: &[u8],
        started: pb::Timestamp,
    ) -> Result<pb::Node> {
        let mut node = synthetic(name, pb::NodeType::File, 0o600, started);
        for chunk in Chunker::new(bytes, &self.w.keys.k_id, self.w.keys.cdc_seed) {
            let chunk = chunk?;
            self.add_blob(BLOB_TYPE_DATA, chunk.id, &chunk.data).await?;
            node.content.push(chunk.id.to_vec());
        }
        node.size = bytes.len() as u64;
        self.stats.files_new += 1;
        self.stats.bytes_total += node.size;
        self.stats.bytes_read += node.size;
        Ok(node)
    }
}

/// A node the backup makes up: owned by root, with the time of the run.
fn synthetic(name: &[u8], ty: pb::NodeType, mode: u32, time: pb::Timestamp) -> pb::Node {
    pb::Node {
        name: name.to_vec(),
        r#type: ty as i32,
        mode,
        mtime: Some(time),
        ..Default::default()
    }
}
