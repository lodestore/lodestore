#[allow(dead_code)]
use crate::common;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TestAgent, local_agent};
use lodestore_agent::jobs::JobHub;
use lodestore_agent::jobs::fetch::{FetchError, HttpFetcher, PackAccess, PackFetcher};
use lodestore_agent::jobs::restore::{PlanError, PlanSource, RestoreOptions, run_restore_with};
use lodestore_agent::socket::StdinHeader;
use lodestore_format::crypto::BlobId;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb::{Node, NodeType};
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::selection::Selection;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::tree::{PlanStep, SelectionWalk};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::plan_item::Kind;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, ChunkLoc, ConflictPolicy, Hook, JobAssign, JobCancel, JobEvent, JobFinished,
    JobOutcome, JobSpec, PlanChunks, PlanEnter, PlanEntry, PlanItem, PlanLeave, RestorePlanBatch,
    RestoreSpec, RestoreTotals,
};
use lodestore_proto::agent::v1::{DatabaseTarget, GuestTarget, restore_spec::Target};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

const WAIT: Duration = Duration::from_secs(60);
const PAUSE: Duration = Duration::from_millis(10);

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn write_file(path: &Path, data: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn is_root() -> bool {
    rustix::process::getuid().is_root()
}

struct PackKey {
    pack: u32,
    pack_id: BlobId,
    key: [u8; 32],
}

struct Planned {
    batches: Vec<RestorePlanBatch>,
    packs: Vec<PackKey>,
}

struct Fx {
    _tmp: tempfile::TempDir,
    agent: TestAgent,
    root: PathBuf,
    src: PathBuf,
    snapshot: BlobId,
    next_job: AtomicUsize,
}

impl Fx {
    async fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let agent = local_agent(&root).await;
        let src = root.join("src");
        write_file(&src.join("a/x"), b"content of a/x", 0o640);
        write_file(&src.join("a/y"), b"content of a/y", 0o644);
        symlink("x", src.join("a/sym")).unwrap();
        write_file(&src.join("b/z"), b"content of b/z", 0o644);
        write_file(&src.join("b/h1"), b"shared by two names", 0o600);
        fs::hard_link(src.join("b/h1"), src.join("b/h2")).unwrap();
        write_file(&src.join("c/deep/w"), &noise(3 << 20, 5), 0o600);

        let store = LocalFsStore::new(agent.repo_root.clone());
        let repo = Repository::open(&store, agent.password.as_bytes())
            .await
            .unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
        let outcome = writer
            .backup(
                std::slice::from_ref(&src),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
        Fx {
            _tmp: tmp,
            agent,
            root,
            src,
            snapshot: outcome.snapshot_id,
            next_job: AtomicUsize::new(0),
        }
    }

    fn job_id(&self) -> String {
        format!("restore-{}", self.next_job.fetch_add(1, Ordering::Relaxed))
    }

    fn hub(&self) -> Arc<JobHub> {
        JobHub::open_with_tick(&self.agent.paths, &self.agent.agent_id, PAUSE).unwrap()
    }

    fn store(&self) -> LocalFsStore {
        LocalFsStore::new(self.agent.repo_root.clone())
    }

    fn abs(&self, rel: &str) -> Vec<u8> {
        let mut p = self.src.as_os_str().as_encoded_bytes().to_vec();
        if !rel.is_empty() {
            p.push(b'/');
            p.extend_from_slice(rel.as_bytes());
        }
        p
    }

    /// Where a backed-up path lands below `target`.
    fn restored(&self, target: &Path, rel: &str) -> PathBuf {
        let mut p = target.join(self.src.strip_prefix("/").unwrap());
        if !rel.is_empty() {
            p.push(rel);
        }
        p
    }

    async fn reader(&self) -> (Reader<LocalFsStore>, [u8; 16], [u8; 32]) {
        let store = self.store();
        let repo = Repository::open(&store, self.agent.password.as_bytes())
            .await
            .unwrap();
        let reader = Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap();
        let ids = (*reader.keys().repo_id(), *reader.keys().k_id());
        (reader, ids.0, ids.1)
    }

    fn spec(
        &self,
        destination: &Path,
        conflict: ConflictPolicy,
        in_place: bool,
        owners: bool,
        k_id: [u8; 32],
        repo_id: [u8; 16],
    ) -> RestoreSpec {
        RestoreSpec {
            snapshot_id: "snapshot".into(),
            source_repo_id: repo_id.to_vec(),
            k_id: k_id.to_vec(),
            destination: destination.to_string_lossy().into_owned(),
            in_place,
            conflict: conflict as i32,
            restore_owners: owners,
            target: None,
            ..Default::default()
        }
    }

    /// What the server would send for these paths, split into batches of
    /// `per_batch` items and files with more than two chunks split across items.
    async fn plan(&self, rels: &[&str], per_batch: usize, url: Option<(&str, &str)>) -> Planned {
        let selection = Selection::from_paths(rels.iter().map(|r| self.abs(r))).unwrap();
        let (mut reader, _, _) = self.reader().await;
        let mut walk = SelectionWalk::start(&mut reader, &self.snapshot, &selection)
            .await
            .unwrap();
        let mut packs: Vec<PackKey> = Vec::new();
        let mut index: HashMap<BlobId, u32> = HashMap::new();
        let mut items: Vec<PlanItem> = Vec::new();
        let (mut files, mut bytes) = (0u64, 0u64);
        while let Some(step) = walk.next(&mut reader).await.unwrap() {
            let kind = match step {
                PlanStep::Enter { mut node, ancestor } => {
                    node.subtrees.clear();
                    Kind::Enter(PlanEnter {
                        node: node.encode_to_vec(),
                        ancestor,
                    })
                }
                PlanStep::Leave => Kind::Leave(PlanLeave {}),
                PlanStep::Entry(mut node) => {
                    let mut locs = Vec::new();
                    if node.r#type == NodeType::File as i32 {
                        files += 1;
                        bytes += node.size;
                        let found = reader.chunk_locations(&node).await.unwrap();
                        for (id, loc) in node.content.iter().zip(&found) {
                            let pack = match index.get(&loc.pack_id) {
                                Some(p) => *p,
                                None => {
                                    let pack = packs.len() as u32;
                                    let key = reader.pack_key(&loc.pack_id).await.unwrap();
                                    packs.push(PackKey {
                                        pack,
                                        pack_id: loc.pack_id,
                                        key: *key,
                                    });
                                    index.insert(loc.pack_id, pack);
                                    pack
                                }
                            };
                            locs.push(ChunkLoc {
                                pack,
                                offset: loc.offset,
                                enc_len: loc.enc_len,
                                ulen: loc.ulen,
                                id: id.clone(),
                            });
                        }
                    }
                    node.content.clear();
                    node.subtrees.clear();
                    let encoded = node.encode_to_vec();
                    let mut rest = locs.split_off(locs.len().min(2));
                    let more = !rest.is_empty();
                    items.push(PlanItem {
                        kind: Some(Kind::Entry(PlanEntry {
                            node: encoded,
                            chunks: locs,
                            more,
                        })),
                    });
                    while !rest.is_empty() {
                        let tail = rest.split_off(rest.len().min(2));
                        let more = !tail.is_empty();
                        items.push(PlanItem {
                            kind: Some(Kind::Chunks(PlanChunks { chunks: rest, more })),
                        });
                        rest = tail;
                    }
                    continue;
                }
            };
            items.push(PlanItem { kind: Some(kind) });
        }
        let table = packs
            .iter()
            .map(|p| lodestore_proto::agent::v1::PackAccess {
                pack: p.pack,
                pack_id: p.pack_id.to_vec(),
                key: p.key.to_vec(),
                url: url
                    .map(|(base, token)| {
                        format!("{base}/{}?t={token}", pack_object_key(&p.pack_id))
                    })
                    .unwrap_or_default(),
                expires_at_ms: if url.is_some() { i64::MAX / 2 } else { 0 },
            })
            .collect::<Vec<_>>();
        let mut batches: Vec<RestorePlanBatch> = items
            .chunks(per_batch.max(1))
            .map(|chunk| RestorePlanBatch {
                items: chunk.to_vec(),
                ..Default::default()
            })
            .collect();
        if batches.is_empty() {
            batches.push(RestorePlanBatch::default());
        }
        batches[0].totals = Some(RestoreTotals { files, bytes });
        batches[0].packs = table;
        batches.last_mut().expect("one batch at least").last = true;
        Planned { batches, packs }
    }
}

struct MemPlan {
    batches: Vec<RestorePlanBatch>,
    pos: usize,
    breaks: VecDeque<usize>,
}

impl MemPlan {
    fn new(planned: &Planned) -> Self {
        MemPlan {
            batches: planned.batches.clone(),
            pos: 0,
            breaks: VecDeque::new(),
        }
    }

    fn breaking(mut self, at: &[usize]) -> Self {
        self.breaks = at.iter().copied().collect();
        self
    }

    fn map_nodes(mut self, f: impl Fn(&mut Node)) -> Self {
        for item in self.batches.iter_mut().flat_map(|b| b.items.iter_mut()) {
            let raw = match item.kind.as_mut() {
                Some(Kind::Enter(e)) => &mut e.node,
                Some(Kind::Entry(e)) => &mut e.node,
                _ => continue,
            };
            let mut node = Node::decode(raw.as_slice()).unwrap();
            f(&mut node);
            *raw = node.encode_to_vec();
        }
        self
    }
}

impl PlanSource for MemPlan {
    async fn next(&mut self) -> Result<Option<RestorePlanBatch>, PlanError> {
        if self.breaks.front() == Some(&self.pos) {
            self.breaks.pop_front();
            return Err(PlanError::Transport("test break".into()));
        }
        let batch = self.batches.get(self.pos).cloned();
        self.pos += 1;
        Ok(batch)
    }

    async fn reopen(&mut self) -> Result<(), PlanError> {
        self.pos = 0;
        Ok(())
    }
}

type Hook1 = Box<dyn Fn(usize) + Send + Sync>;

struct StoreFetcher {
    store: LocalFsStore,
    calls: AtomicUsize,
    hook: Option<Hook1>,
    missing: bool,
}

impl StoreFetcher {
    fn new(fx: &Fx) -> Self {
        StoreFetcher {
            store: fx.store(),
            calls: AtomicUsize::new(0),
            hook: None,
            missing: false,
        }
    }
}

impl PackFetcher for StoreFetcher {
    async fn fetch(
        &self,
        access: &PackAccess,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FetchError> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(hook) = &self.hook {
            hook(n);
        }
        if self.missing {
            return Err(FetchError::NotFound);
        }
        let id = BlobId::try_from(access.pack_id.as_slice()).unwrap();
        self.store
            .get_range(&pack_object_key(&id), offset..offset + len)
            .await
            .map(|b| b.to_vec())
            .map_err(|e| FetchError::Failed(e.to_string()))
    }

    async fn refresh(&self, _: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        Err(FetchError::Failed("no refresh in this test".into()))
    }
}

fn options() -> RestoreOptions {
    RestoreOptions {
        plan_pauses: vec![PAUSE; 3],
        roots: vec![std::env::temp_dir().canonicalize().unwrap()],
        ..Default::default()
    }
}

fn finished_of<'a>(msgs: &'a [AgentMessage], id: &str) -> Option<&'a JobFinished> {
    msgs.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobFinished(f)) if f.job_id == id => Some(f),
        _ => None,
    })
}

fn events(msgs: &[AgentMessage]) -> Vec<&JobEvent> {
    msgs.iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobEvents(e)) => Some(e.events.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn rejected(msgs: &[AgentMessage], id: &str) -> Option<String> {
    msgs.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobRejected(r)) if r.job_id == id => Some(r.code.clone()),
        _ => None,
    })
}

fn accepted(msgs: &[AgentMessage], id: &str) -> bool {
    msgs.iter()
        .any(|m| matches!(&m.body, Some(AgentBody::JobAccepted(a)) if a.job_id == id))
}

async fn wait_for(hub: &Arc<JobHub>, id: &str) -> Vec<AgentMessage> {
    let mut all = Vec::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        all.extend(hub.outbound());
        if finished_of(&all, id).is_some() {
            return all;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{id} did not finish"
        );
        tokio::select! {
            () = hub.changed() => {}
            () = tokio::time::sleep(PAUSE) => {}
        }
    }
}

async fn run<F: PackFetcher + Send + 'static>(
    hub: &Arc<JobHub>,
    id: &str,
    spec: RestoreSpec,
    plan: MemPlan,
    fetch: F,
    options: RestoreOptions,
) -> Vec<AgentMessage> {
    hub.start_restore_with(id.into(), spec, move |ctx, spec| {
        run_restore_with(&ctx, spec, plan, fetch, &options);
    });
    wait_for(hub, id).await
}

fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".lode-restore-")
        {
            out.push(path.clone());
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            out.extend(leftovers(&path));
        }
    }
    out
}

fn same_file(fx: &Fx, target: &Path, rel: &str) {
    let (a, b) = (fx.src.join(rel), fx.restored(target, rel));
    assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap(), "{rel}");
    assert_eq!(
        fs::metadata(&a).unwrap().permissions().mode() & 0o7777,
        fs::metadata(&b).unwrap().permissions().mode() & 0o7777,
        "{rel}"
    );
}

struct Ids {
    repo_id: [u8; 16],
    k_id: [u8; 32],
}

async fn ids(fx: &Fx) -> Ids {
    let (_, repo_id, k_id) = fx.reader().await;
    Ids { repo_id, k_id }
}

async fn restore(
    fx: &Fx,
    hub: &Arc<JobHub>,
    rels: &[&str],
    dest: &Path,
    conflict: ConflictPolicy,
    owners: bool,
) -> (String, Vec<AgentMessage>) {
    let ids = ids(fx).await;
    let planned = fx.plan(rels, 4, None).await;
    let id = fx.job_id();
    let spec = fx.spec(dest, conflict, false, owners, ids.k_id, ids.repo_id);
    let msgs = run(
        hub,
        &id,
        spec,
        MemPlan::new(&planned),
        StoreFetcher::new(fx),
        options(),
    )
    .await;
    (id, msgs)
}

#[tokio::test]
async fn restore_runner_restores_plan_from_local_repo() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let dest = fx.root.join("out");
    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a", "b", "c/deep/w"],
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
    )
    .await;

    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    let stats = f.restore.clone().unwrap();
    assert_eq!(stats.files, 6);
    assert_eq!(stats.created, 6);
    assert_eq!(stats.overwritten + stats.skipped, 0);
    assert_eq!(f.errors_total + f.warnings_total, 0);

    for rel in ["a/x", "a/y", "b/z", "b/h1", "b/h2", "c/deep/w"] {
        same_file(&fx, &dest, rel);
    }
    assert_eq!(
        fs::read_link(fx.restored(&dest, "a/sym")).unwrap(),
        PathBuf::from("x")
    );
    assert_eq!(
        fs::metadata(fx.restored(&dest, "b/h1")).unwrap().ino(),
        fs::metadata(fx.restored(&dest, "b/h2")).unwrap().ino()
    );
    assert!(leftovers(&dest).is_empty());
    assert!(events(&msgs).iter().any(|e| e.kind == "started"));
}

#[tokio::test]
async fn restore_runner_short_job_sends_final_progress() {
    let fx = Fx::new().await;
    let hub = JobHub::open_with_tick(
        &fx.agent.paths,
        &fx.agent.agent_id,
        Duration::from_secs(3600),
    )
    .unwrap();
    let dest = fx.root.join("out");
    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a/x", "b/z"],
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
    )
    .await;
    let progress = msgs
        .iter()
        .rev()
        .find_map(|m| match &m.body {
            Some(AgentBody::JobProgress(p)) if p.job_id == id => Some(p),
            _ => None,
        })
        .expect("a progress sample at the end");
    assert_eq!((progress.files_done, progress.files_total), (2, 2));
    assert!(progress.bytes_downloaded > 0);
    let at = |pick: fn(&AgentBody) -> bool| {
        msgs.iter()
            .position(|m| m.body.as_ref().is_some_and(pick))
            .unwrap()
    };
    assert!(
        at(|b| matches!(b, AgentBody::JobProgress(_)))
            < at(|b| matches!(b, AgentBody::JobFinished(_)))
    );
}

#[tokio::test]
async fn restore_runner_busy_rules() {
    let fx = Fx::new().await;
    let agent = &fx.agent;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let dest = fx.root.join("out");

    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    hub.submit_stdin(
        StdinHeader {
            v: 1,
            op: "backup_stdin".into(),
            name: "app.dump".into(),
            policy: None,
        },
        0,
        0,
        server,
    );

    struct Gate(Arc<tokio::sync::Notify>);
    impl PlanSource for Gate {
        async fn next(&mut self) -> Result<Option<RestorePlanBatch>, PlanError> {
            self.0.notified().await;
            Ok(Some(RestorePlanBatch {
                last: true,
                ..Default::default()
            }))
        }
        async fn reopen(&mut self) -> Result<(), PlanError> {
            Ok(())
        }
    }
    let gate = Arc::new(tokio::sync::Notify::new());
    let spec = |_: ()| {
        fx.spec(
            &dest,
            ConflictPolicy::OverwriteChanged,
            false,
            false,
            ids.k_id,
            ids.repo_id,
        )
    };

    {
        let gate = Arc::clone(&gate);
        let options = options();
        hub.start_restore_with("r1".into(), spec(()), move |ctx, spec| {
            run_restore_with(&ctx, spec, Gate(gate), NoFetch, &options);
        });
    }
    let mut seen = hub.outbound();
    assert!(
        accepted(&seen, "r1"),
        "a stdin job must not block a restore: {seen:?}"
    );

    hub.handle(ServerBody::JobAssign(JobAssign {
        job_id: "b1".into(),
        spec: Some(JobSpec {
            paths: vec![fx.src.to_string_lossy().into_owned()],
            ..Default::default()
        }),
        restore: None,
    }));
    hub.start_restore_with("r2".into(), spec(()), |_, _| {});
    seen.extend(hub.outbound());
    assert_eq!(rejected(&seen, "b1").as_deref(), Some("busy"));
    assert_eq!(rejected(&seen, "r2").as_deref(), Some("busy"));

    gate.notify_one();
    seen.extend(wait_for(&hub, "r1").await);
    let f = finished_of(&seen, "r1").unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{seen:?}");

    let hooks = agent.paths.hooks_dir();
    fs::create_dir_all(&hooks).unwrap();
    fs::set_permissions(&hooks, fs::Permissions::from_mode(0o700)).unwrap();
    let slow = hooks.join("slow");
    fs::write(&slow, "#!/bin/sh\nsleep 30\n").unwrap();
    fs::set_permissions(&slow, fs::Permissions::from_mode(0o700)).unwrap();
    hub.handle(ServerBody::JobAssign(JobAssign {
        job_id: "b2".into(),
        spec: Some(JobSpec {
            paths: vec![fx.src.to_string_lossy().into_owned()],
            before: vec![Hook {
                script: "slow".into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        restore: None,
    }));
    hub.start_restore_with("r3".into(), spec(()), |_, _| {});
    let mut seen = hub.outbound();
    assert!(accepted(&seen, "b2"), "{seen:?}");
    assert_eq!(rejected(&seen, "r3").as_deref(), Some("busy"));

    hub.handle(ServerBody::JobCancel(JobCancel {
        job_id: "b2".into(),
    }));
    seen.extend(wait_for(&hub, "b2").await);
    drop(client);
    hub.shutdown();
    hub.wait_idle(Duration::from_secs(20)).await;
}

struct NoFetch;

impl PackFetcher for NoFetch {
    async fn fetch(&self, _: &PackAccess, _: u64, _: u64) -> Result<Vec<u8>, FetchError> {
        Err(FetchError::Failed("unused".into()))
    }
    async fn refresh(&self, _: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        Err(FetchError::Failed("unused".into()))
    }
}

#[tokio::test]
async fn restore_runner_new_folder_drops_setuid_but_keeps_owner() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let plan = || {
        MemPlan::new(&planned).map_nodes(move |n| {
            n.uid = uid;
            n.gid = gid;
            if n.r#type == NodeType::File as i32 {
                n.mode = 0o6755;
                n.xattrs.push(lodestore_format::pb::Xattr {
                    name: b"security.capability".to_vec(),
                    value: [&[0, 0, 0, 2][..], &[0; 16]].concat(),
                });
            }
        })
    };
    let mode_of = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;

    let dest = fx.root.join("fresh");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
        true,
        ids.k_id,
        ids.repo_id,
    );
    let opts = RestoreOptions {
        roots: vec![fx.root.clone()],
        ..options()
    };
    let msgs = run(&hub, &id, spec, plan(), StoreFetcher::new(&fx), opts).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    let restored = fx.restored(&dest, "a/x");
    assert_eq!(mode_of(&restored), 0o755);
    assert_eq!(fs::metadata(&restored).unwrap().uid(), uid);

    let target = fx.root.join("in-place");
    let id = fx.job_id();
    let spec = fx.spec(
        Path::new("/"),
        ConflictPolicy::OverwriteChanged,
        true,
        true,
        ids.k_id,
        ids.repo_id,
    );
    let options = RestoreOptions {
        root: target.clone(),
        allow_in_place: true,
        ..options()
    };
    let msgs = run(&hub, &id, spec, plan(), StoreFetcher::new(&fx), options).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert!(f.outcome != JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(mode_of(&fx.restored(&target, "a/x")), 0o6755);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root -p lodestore-agent -E 'test(/^restore::/)'"]
async fn restore_runner_root_new_folder_gives_foreign_owner_without_privileges() {
    use lodestore_format::pb::Xattr;
    use rustix::fs::{XattrFlags, getxattr, lsetxattr};

    assert!(is_root(), "run through scripts/test-linux.sh --root");
    const OWNER: u32 = 54321;
    const CAP: &str = "security.capability";
    let value = [&[0, 0, 0, 2][..], &[0; 16]].concat();

    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let plan = || {
        let value = value.clone();
        MemPlan::new(&planned).map_nodes(move |n| {
            if n.r#type == NodeType::File as i32 {
                n.uid = OWNER;
                n.gid = OWNER;
                n.mode = 0o6755;
                n.xattrs.push(Xattr {
                    name: CAP.as_bytes().to_vec(),
                    value: value.clone(),
                });
            }
        })
    };
    let probe = fx.root.join("probe");
    fs::write(&probe, b"").unwrap();
    if lsetxattr(&probe, CAP, &value, XattrFlags::empty()).is_err() {
        panic!("this file system cannot hold {CAP}");
    }
    let has_cap = |p: &Path| getxattr(p, CAP, &mut [0u8; 64]).is_ok();

    let dest = fx.root.join("fresh");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
        true,
        ids.k_id,
        ids.repo_id,
    );
    let opts = RestoreOptions {
        roots: vec![fx.root.clone()],
        ..options()
    };
    let msgs = run(&hub, &id, spec, plan(), StoreFetcher::new(&fx), opts).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    let restored = fx.restored(&dest, "a/x");
    let meta = fs::metadata(&restored).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (OWNER, OWNER));
    assert_eq!(meta.permissions().mode() & 0o7777, 0o755);
    assert!(!has_cap(&restored), "{CAP} must not be applied");

    let target = fx.root.join("in-place");
    let id = fx.job_id();
    let spec = fx.spec(
        Path::new("/"),
        ConflictPolicy::OverwriteChanged,
        true,
        true,
        ids.k_id,
        ids.repo_id,
    );
    let options = RestoreOptions {
        root: target.clone(),
        allow_in_place: true,
        ..options()
    };
    let msgs = run(&hub, &id, spec, plan(), StoreFetcher::new(&fx), options).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    let restored = fx.restored(&target, "a/x");
    let meta = fs::metadata(&restored).unwrap();
    assert_eq!(meta.uid(), OWNER);
    assert_eq!(meta.permissions().mode() & 0o7777, 0o6755);
    assert!(has_cap(&restored), "in place keeps {CAP}");
}

const CT_IDMAP: &str = "u:0:100000:65536,g:0:100000:65536";

/// Files of a container snapshot: `a/x` owned by the container's root, `a/y`
/// by its uid 33 and `b/z` setuid with a capability.
fn ct_plan(planned: &Planned) -> MemPlan {
    MemPlan::new(planned).map_nodes(|n| {
        let (id, privileged) = match n.name.as_slice() {
            b"y" => (33, false),
            b"z" => (0, true),
            _ => (0, false),
        };
        n.uid = id;
        n.gid = id;
        if privileged {
            n.mode = 0o4755;
            n.xattrs.push(lodestore_format::pb::Xattr {
                name: b"security.capability".to_vec(),
                value: [&[0, 0, 0, 2][..], &[0; 16]].concat(),
            });
        }
    })
}

/// Runs a files restore of a container snapshot into `dest` and returns
/// the job's end.
async fn ct_restore_job(
    fx: &Fx,
    dest: &Path,
    tweak: impl FnOnce(&mut RestoreSpec),
    opts: RestoreOptions,
) -> JobFinished {
    ct_restore_run(fx, dest, tweak, opts, &[]).await.0
}

/// Like [`ct_restore_job`], with `extra` entries at the top of the plan.
async fn ct_restore_run(
    fx: &Fx,
    dest: &Path,
    tweak: impl FnOnce(&mut RestoreSpec),
    opts: RestoreOptions,
    extra: &[Node],
) -> (JobFinished, Vec<AgentMessage>) {
    let hub = fx.hub();
    let ids = ids(fx).await;
    let planned = fx.plan(&["a/x", "a/y", "b/z"], 4, None).await;
    let id = fx.job_id();
    let mut spec = RestoreSpec {
        ct_idmap: CT_IDMAP.into(),
        ..fx.spec(
            dest,
            ConflictPolicy::OverwriteChanged,
            false,
            true,
            ids.k_id,
            ids.repo_id,
        )
    };
    tweak(&mut spec);
    let mut plan = ct_plan(&planned);
    let entries = extra.iter().map(|n| PlanItem {
        kind: Some(Kind::Entry(PlanEntry {
            node: n.encode_to_vec(),
            chunks: Vec::new(),
            more: false,
        })),
    });
    plan.batches[0].items.splice(0..0, entries);
    let msgs = run(&hub, &id, spec, plan, StoreFetcher::new(fx), opts).await;
    (finished_of(&msgs, &id).unwrap().clone(), msgs)
}

fn in_root(fx: &Fx) -> RestoreOptions {
    RestoreOptions {
        roots: vec![fx.root.clone()],
        ..options()
    }
}

async fn assert_refused(fx: &Fx, tweak: impl FnOnce(&mut RestoreSpec), text: &str) {
    let dest = fx.root.join("refused");
    let f = ct_restore_job(fx, &dest, tweak, in_root(fx)).await;
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{f:?}");
    assert_eq!(f.error_code, "restore_failed", "{f:?}");
    assert_eq!(f.error_message, text, "{f:?}");
    assert!(!dest.exists(), "nothing is written");
}

#[tokio::test]
async fn restore_ct_idmap_invalid_refused() {
    let fx = Fx::new().await;
    let text = "the container idmap of the snapshot is invalid";
    assert_refused(&fx, |s| s.ct_idmap = "u:0:100000".into(), text).await;
    let long = vec!["u:0:100000:1"; 30].join(",");
    assert!(long.len() > 256);
    assert_refused(&fx, |s| s.ct_idmap = long, text).await;
}

#[tokio::test]
async fn restore_ct_idmap_maps_root_refused() {
    let fx = Fx::new().await;
    let text = "the container idmap of the snapshot maps a container id to root on the node";
    for map in [
        "u:0:0:65536,g:0:100000:65536",
        "u:0:100000:65536,g:1000:0:1",
    ] {
        assert_refused(&fx, |s| s.ct_idmap = map.into(), text).await;
    }
}

#[tokio::test]
async fn restore_ct_idmap_in_place_refused() {
    let fx = Fx::new().await;
    let target = fx.root.join("in-place");
    let opts = RestoreOptions {
        root: target.clone(),
        allow_in_place: true,
        ..options()
    };
    let f = ct_restore_job(&fx, Path::new("/"), |s| s.in_place = true, opts).await;
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{f:?}");
    assert_eq!(f.error_code, "restore_failed", "{f:?}");
    assert_eq!(
        f.error_message,
        "a container snapshot cannot be restored in place"
    );
    assert!(!target.exists(), "nothing is written");
}

#[tokio::test]
async fn restore_ct_idmap_without_owners() {
    let fx = Fx::new().await;
    let dest = fx.root.join("fresh");
    let f = ct_restore_job(&fx, &dest, |s| s.restore_owners = false, in_root(&fx)).await;
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{f:?}");
    assert_eq!(f.errors_total + f.warnings_total, 0, "{f:?}");
    let me = rustix::process::geteuid().as_raw();
    for rel in ["a/x", "a/y", "b/z"] {
        let meta = fs::metadata(fx.restored(&dest, rel)).unwrap();
        assert_eq!(meta.uid(), me, "{rel}: no owner is applied");
    }
    assert_eq!(
        fs::metadata(fx.restored(&dest, "b/z"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7000,
        0
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root -p lodestore-agent pve_root_"]
async fn pve_root_file_shift_owners() {
    use rustix::fs::getxattr;

    assert!(is_root(), "run through scripts/test-linux.sh --root");
    let fx = Fx::new().await;
    let dest = fx.root.join("fresh");
    let f = ct_restore_job(&fx, &dest, |_| {}, in_root(&fx)).await;
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{f:?}");
    let owner = |rel: &str| {
        let meta = fs::metadata(fx.restored(&dest, rel)).unwrap();
        (meta.uid(), meta.gid())
    };
    assert_eq!(owner("a/x"), (100_000, 100_000));
    assert_eq!(owner("a/y"), (100_033, 100_033));
    assert_eq!(owner("b/z"), (100_000, 100_000));
    assert_eq!(owner("a"), (100_000, 100_000), "folders are shifted too");
    let z = fx.restored(&dest, "b/z");
    assert_eq!(
        fs::metadata(&z).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert!(getxattr(&z, "security.capability", &mut [0u8; 64]).is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root -p lodestore-agent pve_root_"]
async fn pve_root_file_shift_privileged_ct() {
    assert!(is_root(), "run through scripts/test-linux.sh --root");
    let fx = Fx::new().await;
    let dest = fx.root.join("fresh");
    let f = ct_restore_job(&fx, &dest, |s| s.ct_idmap.clear(), in_root(&fx)).await;
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{f:?}");
    let uid = |rel: &str| fs::metadata(fx.restored(&dest, rel)).unwrap().uid();
    assert_eq!(uid("a/x"), 0);
    assert_eq!(uid("a/y"), 33);
}

/// A block device, a character device, a FIFO and a node type this build
/// does not know, the way a socket would reach it.
#[cfg(target_os = "linux")]
fn specials() -> Vec<Node> {
    let node = |name: &str, ty: i32, rdev: u64| Node {
        name: name.as_bytes().to_vec(),
        r#type: ty,
        mode: 0o666,
        rdev,
        ..Default::default()
    };
    vec![
        node("sda", NodeType::Blockdev as i32, rustix::fs::makedev(8, 0)),
        node("null", NodeType::Chardev as i32, rustix::fs::makedev(1, 3)),
        node("fifo", NodeType::Fifo as i32, 0),
        node("sock", 7, 0),
    ]
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root -p lodestore-agent pve_root_"]
async fn pve_root_file_shift_skips_devices() {
    use std::os::unix::fs::FileTypeExt;

    assert!(is_root(), "run through scripts/test-linux.sh --root");
    let fx = Fx::new().await;
    let names = ["sda", "null", "fifo", "sock"];
    for owners in [true, false] {
        let dest = fx.root.join(format!("guest-{owners}"));
        let guest = |s: &mut RestoreSpec| {
            s.guest_snapshot = true;
            s.restore_owners = owners;
        };
        let (f, msgs) = ct_restore_run(&fx, &dest, guest, in_root(&fx), &specials()).await;
        assert_eq!(f.outcome, JobOutcome::Warnings as i32, "{f:?}");
        assert_eq!(f.restore.as_ref().unwrap().special_skipped, 4, "{f:?}");
        let mut reported: Vec<_> = events(&msgs)
            .iter()
            .filter(|e| e.kind == "special_file")
            .map(|e| e.path.clone())
            .collect();
        reported.sort();
        let mut expected: Vec<_> = names.iter().map(|n| n.to_string()).collect();
        expected.sort();
        assert_eq!(reported, expected, "{msgs:?}");
        for name in names {
            assert!(fs::symlink_metadata(dest.join(name)).is_err(), "{name}");
        }
        assert!(fx.restored(&dest, "a/x").is_file());
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    let dest = fx.root.join("host");
    let (f, msgs) = ct_restore_run(
        &fx,
        &dest,
        |s| s.ct_idmap.clear(),
        in_root(&fx),
        &specials(),
    )
    .await;
    assert_ne!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.restore.as_ref().unwrap().special_skipped, 0);
    let ty = |name: &str| fs::symlink_metadata(dest.join(name)).unwrap().file_type();
    assert!(ty("sda").is_block_device());
    assert!(ty("null").is_char_device());
    assert!(ty("fifo").is_fifo());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root; run scripts/test-linux.sh --root -p lodestore-agent pve_root_"]
fn pve_root_space_reserve_mid_write() {
    use lodestore_format::fs::restore::{Overwrite, RestoreSettings, Restorer, SpaceReserve};
    use std::process::Command;

    struct Unmount(PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let _ = Command::new("umount").arg(&self.0).status();
        }
    }

    assert!(is_root(), "run through scripts/test-linux.sh --root");
    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("small");
    fs::create_dir(&mnt).unwrap();
    let mounted = Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=64m", "tmpfs"])
        .arg(&mnt)
        .status()
        .unwrap();
    assert!(mounted.success(), "cannot mount a tmpfs");
    let _unmount = Unmount(mnt.clone());

    let settings = RestoreSettings {
        overwrite: Overwrite::IfChanged,
        verify: false,
        dry_run: false,
        k_id: Zeroizing::new([0; 32]),
        seed: 0,
        restore_owners: false,
        keep_privileges: false,
    };
    let node = Node {
        name: b"big".to_vec(),
        r#type: NodeType::File as i32,
        size: 100 << 20,
        mode: 0o644,
        ..Default::default()
    };
    let piece = vec![7u8; 1 << 20];
    let free = || {
        let st = rustix::fs::statvfs(&mnt).unwrap();
        st.f_bavail * st.f_frsize
    };
    // Writes the file piece by piece until the first error; returns how much
    // went in, the error and the free space at that moment.
    let fill = |restorer: &mut Restorer| {
        let mut sink = restorer.begin_file(&node).unwrap().unwrap();
        let mut written = 0u64;
        while written < node.size {
            if let Err(e) = sink.write(&piece) {
                return (written, Some(e), free());
            }
            written += piece.len() as u64;
        }
        (written, None, free())
    };

    let reserve = SpaceReserve {
        min_bytes: 8 << 20,
        percent: 0,
    };
    let mut restorer = Restorer::open(&mnt, settings.clone())
        .unwrap()
        .with_space_reserve(reserve);
    let (written, err, left) = fill(&mut restorer);
    assert!(
        matches!(err, Some(lodestore_format::Error::NoSpace { .. })),
        "{err:?}"
    );
    assert!(written > 0 && written < 64 << 20, "{written}");
    assert!(left >= (8 << 20) - (64 << 10), "{left} bytes left");
    assert_eq!(
        fs::read_dir(&mnt).unwrap().count(),
        0,
        "the part is removed"
    );

    let mut plain = Restorer::open(&mnt, settings).unwrap();
    let (_, err, _) = fill(&mut plain);
    assert!(
        matches!(&err, Some(lodestore_format::Error::Io(e)) if e.kind() == std::io::ErrorKind::StorageFull),
        "{err:?}"
    );
}

#[tokio::test]
async fn restore_guest_snapshot_no_space() {
    use lodestore_format::fs::restore::SpaceReserve;

    let fx = Fx::new().await;
    let fresh = fx.root.join("fresh");
    let opts = || RestoreOptions {
        space_reserve: SpaceReserve {
            min_bytes: u64::MAX / 2,
            percent: 0,
        },
        ..in_root(&fx)
    };
    let guest = |s: &mut RestoreSpec| s.guest_snapshot = true;
    for existed in [false, true] {
        if existed {
            fs::create_dir(&fresh).unwrap();
        }
        let f = ct_restore_job(&fx, &fresh.join("deeper"), guest, opts()).await;
        assert_eq!(f.outcome, JobOutcome::Failed as i32, "{f:?}");
        assert_eq!(f.error_code, "restore_no_space", "{f:?}");
        assert!(f.error_message.contains("must stay free"), "{f:?}");
        assert_eq!(
            fresh.exists(),
            existed,
            "only what the restore created is removed"
        );
        assert!(!fresh.join("deeper").exists());
    }
}

#[tokio::test]
async fn restore_runner_symlinked_target_is_not_read() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let roots = fx.root.join("r");
    let elsewhere = fx.root.join("elsewhere");
    fs::create_dir_all(&roots).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, roots.join("link")).unwrap();

    let mut codes = Vec::new();
    for secret in [false, true] {
        if secret {
            fs::write(elsewhere.join("secret"), b"x").unwrap();
        }
        let id = fx.job_id();
        let spec = fx.spec(
            &roots.join("link"),
            ConflictPolicy::StopIfNotEmpty,
            false,
            false,
            ids.k_id,
            ids.repo_id,
        );
        let options = RestoreOptions {
            roots: vec![roots.clone()],
            ..options()
        };
        let msgs = run(
            &hub,
            &id,
            spec,
            MemPlan::new(&planned),
            StoreFetcher::new(&fx),
            options,
        )
        .await;
        let f = finished_of(&msgs, &id).unwrap();
        assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
        codes.push(f.error_code.clone());
    }
    assert_eq!(codes[0], codes[1], "the outside folder's content leaks");
    assert_ne!(codes[0], "restore_target_not_empty");
}

#[tokio::test]
async fn restore_runner_stop_if_not_empty() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let dest = fx.root.join("out");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("keep"), b"mine").unwrap();

    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a/x"],
        &dest,
        ConflictPolicy::StopIfNotEmpty,
        false,
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32);
    assert_eq!(f.error_code, "restore_target_not_empty");
    let names: Vec<_> = fs::read_dir(&dest).unwrap().flatten().collect();
    assert_eq!(names.len(), 1);

    let empty = fx.root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a/x"],
        &empty,
        ConflictPolicy::StopIfNotEmpty,
        false,
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    same_file(&fx, &empty, "a/x");
}

#[tokio::test]
async fn restore_runner_refuses_root_and_in_place_stop() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let target = fx.root.join("target");
    for (dest, conflict, in_place, code) in [
        (
            "/",
            ConflictPolicy::OverwriteChanged,
            false,
            "restore_failed",
        ),
        ("//", ConflictPolicy::SkipExisting, false, "restore_failed"),
        (
            "/./",
            ConflictPolicy::OverwriteChanged,
            false,
            "restore_failed",
        ),
        (
            "/",
            ConflictPolicy::StopIfNotEmpty,
            true,
            "restore_target_not_empty",
        ),
    ] {
        let id = fx.job_id();
        let spec = fx.spec(
            Path::new(dest),
            conflict,
            in_place,
            false,
            ids.k_id,
            ids.repo_id,
        );
        let options = RestoreOptions {
            root: target.clone(),
            allow_in_place: true,
            ..options()
        };
        let msgs = run(
            &hub,
            &id,
            spec,
            MemPlan::new(&planned),
            StoreFetcher::new(&fx),
            options,
        )
        .await;
        let f = finished_of(&msgs, &id).unwrap();
        assert_eq!(f.outcome, JobOutcome::Failed as i32, "{dest} {msgs:?}");
        assert_eq!(f.error_code, code, "{dest}");
    }
    assert!(!target.exists());
}

#[tokio::test]
async fn restore_runner_refuses_outside_roots() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let roots = fx.root.join("r");
    let options = || RestoreOptions {
        roots: vec![roots.clone()],
        ..options()
    };

    let outside = fx.root.join("out");
    let id = fx.job_id();
    let spec = fx.spec(
        &outside,
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let msgs = run(
        &hub,
        &id,
        spec,
        MemPlan::new(&planned),
        StoreFetcher::new(&fx),
        options(),
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.error_code, "restore_outside_roots");
    assert!(!outside.exists());

    let inside = roots.join("out");
    let id = fx.job_id();
    let spec = fx.spec(
        &inside,
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let msgs = run(
        &hub,
        &id,
        spec,
        MemPlan::new(&planned),
        StoreFetcher::new(&fx),
        options(),
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    same_file(&fx, &inside, "a/x");
}

#[tokio::test]
async fn restore_runner_symlink_inside_root_does_not_escape() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let roots = fx.root.join("r");
    let elsewhere = fx.root.join("elsewhere");
    fs::create_dir_all(&roots).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, roots.join("link")).unwrap();

    let id = fx.job_id();
    let spec = fx.spec(
        &roots.join("link/x"),
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let options = RestoreOptions {
        roots: vec![roots],
        ..options()
    };
    let msgs = run(
        &hub,
        &id,
        spec,
        MemPlan::new(&planned),
        StoreFetcher::new(&fx),
        options,
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[tokio::test]
async fn restore_runner_in_place_disabled_by_default() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let target = fx.root.join("target");
    fs::create_dir_all(&target).unwrap();

    let id = fx.job_id();
    let spec = fx.spec(
        Path::new("/"),
        ConflictPolicy::OverwriteChanged,
        true,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let options = RestoreOptions {
        root: target.clone(),
        ..options()
    };
    assert!(!options.allow_in_place);
    let msgs = run(
        &hub,
        &id,
        spec,
        MemPlan::new(&planned),
        StoreFetcher::new(&fx),
        options,
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.error_code, "restore_in_place_disabled");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
}

#[tokio::test]
async fn restore_runner_skip_and_overwrite_counts() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let dest = fx.root.join("out");

    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a/x", "b/z"],
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
    )
    .await;
    assert_eq!(
        finished_of(&msgs, &id)
            .unwrap()
            .restore
            .clone()
            .unwrap()
            .created,
        2
    );

    let changed = fx.restored(&dest, "a/x");
    fs::write(&changed, b"edited by hand").unwrap();

    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a/x", "b/z"],
        &dest,
        ConflictPolicy::SkipExisting,
        false,
    )
    .await;
    let stats = finished_of(&msgs, &id).unwrap().restore.clone().unwrap();
    assert_eq!((stats.created, stats.overwritten, stats.skipped), (0, 0, 2));
    assert_eq!(fs::read(&changed).unwrap(), b"edited by hand");

    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a/x", "b/z", "a/y"],
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    let stats = f.restore.clone().unwrap();
    assert_eq!((stats.created, stats.overwritten, stats.skipped), (1, 1, 1));
    assert_eq!(stats.files, 2);
    same_file(&fx, &dest, "a/x");
}

#[tokio::test]
async fn restore_runner_in_place_keeps_ancestors() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let target = fx.root.join("target");
    let src_dir = fx.restored(&target, "");
    let a_dir = fx.restored(&target, "a");
    fs::create_dir_all(&a_dir).unwrap();
    fs::write(a_dir.join("extra"), b"not in the snapshot").unwrap();
    fs::set_permissions(&a_dir, fs::Permissions::from_mode(0o751)).unwrap();
    fs::set_permissions(&src_dir, fs::Permissions::from_mode(0o711)).unwrap();

    let planned = fx.plan(&["a/x"], 4, None).await;
    let id = fx.job_id();
    let spec = fx.spec(
        Path::new("/"),
        ConflictPolicy::OverwriteChanged,
        true,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let options = RestoreOptions {
        root: target.clone(),
        allow_in_place: true,
        ..options()
    };
    let msgs = run(
        &hub,
        &id,
        spec,
        MemPlan::new(&planned),
        StoreFetcher::new(&fx),
        options,
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    assert_eq!(f.restore.clone().unwrap().created, 1);
    same_file(&fx, &target, "a/x");
    assert_eq!(
        fs::metadata(&a_dir).unwrap().permissions().mode() & 0o7777,
        0o751
    );
    assert_eq!(
        fs::metadata(&src_dir).unwrap().permissions().mode() & 0o7777,
        0o711
    );
    assert_eq!(
        fs::read(a_dir.join("extra")).unwrap(),
        b"not in the snapshot"
    );
    assert!(!fx.restored(&target, "a/y").exists());
}

#[tokio::test]
async fn restore_runner_owner_warnings() {
    if is_root() {
        return;
    }
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["b/z"], 4, None).await;
    let foreign = rustix::process::getuid().as_raw() + 4242;
    let plan = || {
        MemPlan::new(&planned).map_nodes(move |n| {
            n.uid = foreign;
        })
    };

    for (owners, name) in [(false, "keeps-owner"), (true, "changes-owner")] {
        let dest = fx.root.join(name);
        let id = fx.job_id();
        let spec = fx.spec(
            &dest,
            ConflictPolicy::OverwriteChanged,
            false,
            owners,
            ids.k_id,
            ids.repo_id,
        );
        let msgs = run(&hub, &id, spec, plan(), StoreFetcher::new(&fx), options()).await;
        let f = finished_of(&msgs, &id).unwrap();
        let warned: Vec<_> = events(&msgs)
            .into_iter()
            .filter(|e| e.kind == "owner_not_applied")
            .collect();
        if owners {
            assert_eq!(f.outcome, JobOutcome::Warnings as i32, "{msgs:?}");
            assert!(f.warnings_total > 0);
            assert!(warned.iter().any(|e| e.path.ends_with("z")), "{warned:?}");
        } else {
            assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
            assert!(warned.is_empty());
        }
        assert!(fx.restored(&dest, "b/z").exists());
    }
}

#[tokio::test]
async fn restore_runner_corrupt_chunk_is_warning() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let planned = fx.plan(&["a", "b"], 4, None).await;
    let target = planned
        .batches
        .iter()
        .flat_map(|b| &b.items)
        .find_map(|item| match &item.kind {
            Some(Kind::Entry(e)) => {
                let node = Node::decode(e.node.as_slice()).unwrap();
                (node.name == b"z").then(|| e.chunks[0].clone())
            }
            _ => None,
        })
        .unwrap();
    let pack = planned
        .packs
        .iter()
        .find(|p| p.pack == target.pack)
        .unwrap();
    let path = fx.agent.repo_root.join(pack_object_key(&pack.pack_id));
    let mut bytes = fs::read(&path).unwrap();
    bytes[target.offset as usize + 20] ^= 0xff;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&path, bytes).unwrap();

    let dest = fx.root.join("out");
    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a", "b"],
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Warnings as i32, "{msgs:?}");
    assert_eq!(f.errors_total, 1);
    let bad = events(&msgs)
        .into_iter()
        .find(|e| e.kind == "restore_integrity")
        .expect("restore_integrity event");
    assert!(bad.path.ends_with("z"), "{}", bad.path);
    assert!(!fx.restored(&dest, "b/z").exists());
    for rel in ["a/x", "a/y", "b/h1", "b/h2"] {
        same_file(&fx, &dest, rel);
    }
    assert!(leftovers(&dest).is_empty());
}

#[tokio::test]
async fn restore_runner_unwritable_and_missing_data_codes() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;

    if !is_root() {
        let dest = fx.root.join("readonly");
        fs::create_dir_all(&dest).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o500)).unwrap();
        let id = fx.job_id();
        let spec = fx.spec(
            &dest,
            ConflictPolicy::OverwriteChanged,
            false,
            false,
            ids.k_id,
            ids.repo_id,
        );
        let msgs = run(
            &hub,
            &id,
            spec,
            MemPlan::new(&planned),
            StoreFetcher::new(&fx),
            options(),
        )
        .await;
        let f = finished_of(&msgs, &id).unwrap();
        assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
        assert_eq!(f.error_code, "restore_target_unwritable");
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let dest = fx.root.join("missing");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let mut fetch = StoreFetcher::new(&fx);
    fetch.missing = true;
    let msgs = run(&hub, &id, spec, MemPlan::new(&planned), fetch, options()).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.error_code, "restore_data_missing");
    assert!(leftovers(&dest).is_empty());
}

#[tokio::test]
async fn restore_runner_cancel() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a", "b", "c"], 4, None).await;
    let dest = fx.root.join("out");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let mut fetch = StoreFetcher::new(&fx);
    fetch.hook = Some(Box::new({
        let hub = Arc::clone(&hub);
        let id = id.clone();
        move |n| {
            if n == 1 {
                hub.handle(ServerBody::JobCancel(JobCancel { job_id: id.clone() }));
            }
        }
    }));
    let msgs = run(&hub, &id, spec, MemPlan::new(&planned), fetch, options()).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Cancelled as i32, "{msgs:?}");
    assert_eq!(f.error_code, "cancelled");
    assert!(leftovers(&dest).is_empty());
    assert!(!fx.restored(&dest, "c/deep/w").exists());
}

#[tokio::test]
async fn restore_runner_plan_restart_idempotent() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a", "b", "c"], 3, None).await;
    assert!(planned.batches.len() > 3);

    for (n, policy) in [
        ConflictPolicy::OverwriteChanged,
        ConflictPolicy::SkipExisting,
        ConflictPolicy::StopIfNotEmpty,
    ]
    .into_iter()
    .enumerate()
    {
        let dest = fx.root.join(format!("out-{n}"));
        let id = fx.job_id();
        let spec = fx.spec(&dest, policy, false, false, ids.k_id, ids.repo_id);
        let plan = MemPlan::new(&planned).breaking(&[2]);
        let msgs = run(&hub, &id, spec, plan, StoreFetcher::new(&fx), options()).await;
        let f = finished_of(&msgs, &id).unwrap();
        assert_eq!(
            f.outcome,
            JobOutcome::Succeeded as i32,
            "{policy:?} {msgs:?}"
        );
        assert!(events(&msgs).iter().any(|e| e.kind == "plan_restarted"));
        for rel in ["a/x", "a/y", "b/z", "b/h1", "b/h2", "c/deep/w"] {
            same_file(&fx, &dest, rel);
        }
        assert_eq!(
            fs::metadata(fx.restored(&dest, "b/h1")).unwrap().ino(),
            fs::metadata(fx.restored(&dest, "b/h2")).unwrap().ino()
        );
        assert!(leftovers(&dest).is_empty());
    }
}

#[tokio::test]
async fn restore_runner_unwritable_entry_is_warning() {
    if is_root() {
        return;
    }
    let fx = Fx::new().await;
    let hub = fx.hub();
    let dest = fx.root.join("out");
    let locked = fx.restored(&dest, "a");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();

    let (id, msgs) = restore(
        &fx,
        &hub,
        &["a", "b"],
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
    )
    .await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Warnings as i32, "{msgs:?}");
    let failed: Vec<_> = events(&msgs)
        .into_iter()
        .filter(|e| e.kind == "restore_entry_failed")
        .map(|e| e.path.clone())
        .collect();
    assert!(failed.iter().any(|p| p.ends_with("a/x")), "{failed:?}");
    assert_eq!(f.errors_total, failed.len() as u64);
    for rel in ["b/z", "b/h1", "b/h2"] {
        same_file(&fx, &dest, rel);
    }
}

#[tokio::test]
async fn restore_runner_restart_keeps_warnings_of_earlier_pass() {
    if is_root() {
        return;
    }
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["b"], 1, None).await;
    let after_z = planned
        .batches
        .iter()
        .position(|b| {
            b.items.iter().any(|i| match &i.kind {
                Some(Kind::Entry(e)) => Node::decode(e.node.as_slice()).unwrap().name == b"z",
                _ => false,
            })
        })
        .unwrap()
        + 1;
    let foreign = rustix::process::getuid().as_raw() + 4242;
    let plan = MemPlan::new(&planned)
        .map_nodes(move |n| n.uid = foreign)
        .breaking(&[after_z]);
    let dest = fx.root.join("out");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::SkipExisting,
        false,
        true,
        ids.k_id,
        ids.repo_id,
    );
    let msgs = run(&hub, &id, spec, plan, StoreFetcher::new(&fx), options()).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert!(events(&msgs).iter().any(|e| e.kind == "plan_restarted"));
    assert_eq!(f.outcome, JobOutcome::Warnings as i32, "{msgs:?}");
    assert!(f.warnings_total > 0);
}

#[tokio::test]
async fn restore_runner_plan_without_end_marker_fails() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let mut planned = fx.plan(&["a", "b"], 2, None).await;
    assert!(planned.batches.len() > 2);
    planned.batches.truncate(2);
    let dest = fx.root.join("out");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let msgs = run(
        &hub,
        &id,
        spec,
        MemPlan::new(&planned),
        StoreFetcher::new(&fx),
        options(),
    )
    .await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.error_code, "restore_failed");
    assert!(
        events(&msgs)
            .iter()
            .any(|e| e.kind == "plan_restarted" && e.message.contains("ended early")),
        "{msgs:?}"
    );
}

#[tokio::test]
async fn restore_runner_plan_restart_gives_up() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a"], 2, None).await;
    let dest = fx.root.join("out");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let plan = MemPlan::new(&planned).breaking(&[1, 0, 0, 0]);
    let msgs = run(&hub, &id, spec, plan, StoreFetcher::new(&fx), options()).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.error_code, "restore_failed");
    let restarts = events(&msgs)
        .iter()
        .filter(|e| e.kind == "plan_restarted")
        .count();
    assert_eq!(restarts, 3);
}

struct Served {
    requests: AtomicUsize,
    ranged: AtomicUsize,
    failures: Mutex<VecDeque<u16>>,
    delay: Duration,
}

/// A bucket that answers range requests from the repository folder. `t=old`
/// links are refused, and `failures` are answered first, one per request.
async fn bucket(repo_root: PathBuf, served: Arc<Served>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let served = Arc::clone(&served);
            let repo_root = repo_root.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                served.requests.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(served.delay).await;
                let target = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let range = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_string)
                    })
                    .and_then(|r| {
                        let (a, b) = r.trim().split_once('-')?;
                        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                    });
                let scripted = served.failures.lock().unwrap().pop_front();
                let status = if let Some(code) = scripted {
                    code
                } else if query.contains("t=old") {
                    403
                } else if range.is_none() {
                    400
                } else {
                    206
                };
                let body = match (status, range) {
                    (206, Some((a, b))) => {
                        served.ranged.fetch_add(1, Ordering::Relaxed);
                        let file = fs::read(repo_root.join(path.trim_start_matches('/'))).unwrap();
                        file[a..=b].to_vec()
                    }
                    _ => Vec::new(),
                };
                let reason = match status {
                    206 => "Partial Content",
                    403 => "Forbidden",
                    _ => "Error",
                };
                let reply = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    base
}

fn make_served(delay: Duration, failures: &[u16]) -> Arc<Served> {
    Arc::new(Served {
        requests: AtomicUsize::new(0),
        ranged: AtomicUsize::new(0),
        failures: Mutex::new(failures.iter().copied().collect()),
        delay,
    })
}

struct Linked {
    http: HttpFetcher,
    refreshed: Arc<AtomicUsize>,
    base: String,
    token: &'static str,
    packs: Vec<(u32, BlobId, [u8; 32])>,
}

impl Linked {
    fn new(
        base: &str,
        planned: &Planned,
        token: &'static str,
        refreshed: Arc<AtomicUsize>,
    ) -> Self {
        Linked {
            http: HttpFetcher::new(&Default::default())
                .unwrap()
                .with_pause(PAUSE),
            refreshed,
            base: base.to_string(),
            token,
            packs: planned
                .packs
                .iter()
                .map(|p| (p.pack, p.pack_id, p.key))
                .collect(),
        }
    }
}

impl PackFetcher for Linked {
    async fn fetch(
        &self,
        access: &PackAccess,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FetchError> {
        self.http.fetch(access, offset, len).await
    }

    async fn refresh(&self, packs: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        self.refreshed.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .packs
            .iter()
            .filter(|(pack, _, _)| packs.contains(pack))
            .map(|(pack, id, key)| PackAccess {
                pack: *pack,
                pack_id: id.to_vec(),
                key: Zeroizing::new(*key),
                url: Some(Zeroizing::new(format!(
                    "{}/{}?t={}",
                    self.base,
                    pack_object_key(id),
                    self.token
                ))),
                expires_at_ms: i64::MAX / 2,
            })
            .collect())
    }
}

async fn linked_restore(
    fx: &Fx,
    served: &Arc<Served>,
    tokens: (&'static str, &'static str),
    rels: &[&str],
) -> (String, Vec<AgentMessage>, Arc<AtomicUsize>, PathBuf) {
    let hub = fx.hub();
    let ids = ids(fx).await;
    let base = bucket(fx.agent.repo_root.clone(), Arc::clone(served)).await;
    let planned = fx.plan(rels, 4, Some((&base, tokens.0))).await;
    let refreshed = Arc::new(AtomicUsize::new(0));
    let fetch = Linked::new(&base, &planned, tokens.1, Arc::clone(&refreshed));
    let dest = fx.root.join("out");
    let id = fx.job_id();
    let spec = fx.spec(
        &dest,
        ConflictPolicy::OverwriteChanged,
        false,
        false,
        ids.k_id,
        ids.repo_id,
    );
    let msgs = run(&hub, &id, spec, MemPlan::new(&planned), fetch, options()).await;
    (id, msgs, refreshed, dest)
}

#[tokio::test]
async fn restore_runner_presigned_http_range() {
    let fx = Fx::new().await;
    let bucket = make_served(Duration::from_millis(40), &[]);
    let (id, msgs, refreshed, dest) =
        linked_restore(&fx, &bucket, ("new", "new"), &["a/x", "c/deep/w"]).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    assert!(bucket.ranged.load(Ordering::Relaxed) > 0);
    assert_eq!(
        bucket.ranged.load(Ordering::Relaxed),
        bucket.requests.load(Ordering::Relaxed)
    );
    assert_eq!(refreshed.load(Ordering::Relaxed), 0);
    same_file(&fx, &dest, "a/x");
    same_file(&fx, &dest, "c/deep/w");
    let downloaded = msgs
        .iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobProgress(p)) => Some(p.bytes_downloaded),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    assert!(downloaded > 0, "{msgs:?}");
}

#[tokio::test]
async fn restore_runner_presigned_refresh_on_403() {
    let fx = Fx::new().await;
    let bucket = make_served(Duration::ZERO, &[]);
    let (id, msgs, refreshed, dest) = linked_restore(&fx, &bucket, ("old", "new"), &["b/z"]).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    assert_eq!(refreshed.load(Ordering::Relaxed), 1);
    same_file(&fx, &dest, "b/z");

    let fx = Fx::new().await;
    let bucket = make_served(Duration::ZERO, &[]);
    let (id, msgs, refreshed, _) = linked_restore(&fx, &bucket, ("old", "old"), &["b/z"]).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.error_code, "restore_access_denied");
    assert_eq!(refreshed.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn restore_runner_presigned_retries_5xx() {
    let fx = Fx::new().await;
    let bucket = make_served(Duration::ZERO, &[503, 503]);
    let (id, msgs, _, dest) = linked_restore(&fx, &bucket, ("new", "new"), &["b/z"]).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{msgs:?}");
    same_file(&fx, &dest, "b/z");

    let fx = Fx::new().await;
    let bucket = make_served(Duration::ZERO, &[503, 503, 503]);
    let (id, msgs, _, _) = linked_restore(&fx, &bucket, ("new", "new"), &["b/z"]).await;
    let f = finished_of(&msgs, &id).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32, "{msgs:?}");
    assert_eq!(f.error_code, "restore_failed");
}

#[tokio::test]
async fn restore_refuses_a_target_it_cannot_run() {
    let fx = Fx::new().await;
    let hub = fx.hub();
    let ids = ids(&fx).await;
    let planned = fx.plan(&["a/x"], 4, None).await;
    let mut n = 0;
    for target in [
        Target::Guest(GuestTarget {
            node: "pve1".into(),
            vmid: 9001,
            in_place: false,
            ..Default::default()
        }),
        Target::Database(DatabaseTarget {
            database: "app".into(),
            in_place: false,
        }),
    ] {
        for (by_destination, in_place) in [(true, false), (true, true), (false, false)] {
            n += 1;
            let dest = fx.root.join(format!("target{n}"));
            fs::create_dir(&dest).unwrap();
            let id = fx.job_id();
            let spec = RestoreSpec {
                destination: if by_destination {
                    dest.to_string_lossy().into_owned()
                } else {
                    String::new()
                },
                target: Some(target.clone()),
                ..fx.spec(
                    &dest,
                    ConflictPolicy::SkipExisting,
                    in_place,
                    false,
                    ids.k_id,
                    ids.repo_id,
                )
            };
            let options = RestoreOptions {
                root: dest.clone(),
                allow_in_place: true,
                ..options()
            };
            let msgs = run(
                &hub,
                &id,
                spec,
                MemPlan::new(&planned),
                StoreFetcher::new(&fx),
                options,
            )
            .await;
            let f = finished_of(&msgs, &id).unwrap();
            let case = format!("{target:?} destination={by_destination} in_place={in_place}");
            assert_eq!(f.outcome, JobOutcome::Failed as i32, "{case} {msgs:?}");
            // A guest target goes to the container restore, which this hub is not set up for.
            let expected = match target {
                Target::Guest(_) => "not set up for Proxmox VE containers",
                Target::Database(_) => "unsupported restore target",
            };
            assert!(
                f.error_message.contains(expected),
                "{case} {}",
                f.error_message
            );
            assert_eq!(fs::read_dir(&dest).unwrap().count(), 0, "{case}");
        }
    }
}
