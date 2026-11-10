mod common;
#[path = "../../lodestore-format/tests/tree_support/mod.rs"]
mod tree_support;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Api, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions, Paths, connect, tls};
use lodestore_format::crypto::{BLOB_TYPE_DATA, Entropy};
use lodestore_format::index::hex32;
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb;
use lodestore_format::reader::open_chunk;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobStore, LocalFsStore, RepoLocation, StaticCredentials};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::plan_item::Kind;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, ConflictPolicy, GetTrustAnchorRequest, Hello, JobAccepted, JobFinished,
    JobOutcome, JobPhase, JobPhaseKind, JobProgress, PackAccess, ReadPackRangeRequest,
    RefreshPackAccessRequest, RestorePlanRequest, RestoreSpec, RestoreStats, ServerMessage,
    Versions,
};
use lodestore_server::catalog::ingest_snapshot;
use lodestore_server::db::now_ms;
use lodestore_server::grpc::dispatch::janitor_once;
use lodestore_server::jobs::restore::{RestoreRequest, enqueue_restore};
use lodestore_server::repos::db::{AccessKey, NewRepository, RepoLocation as DbLocation};
use lodestore_testkit::minio::MinioServer;
use prost::Message;
use sqlx::{AnyPool, Row};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Code, Streaming};

const PASSWORD: &[u8] = b"test-password";
const CAP: &str = "restore_v1";
const BIG_LEN: usize = 2_500_000;

struct Host {
    id: String,
    paths: Paths,
}

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> Host {
    let token = api.join_token(repo, serde_json::json!([])).await;
    let paths = ts.agent_paths(hostname);
    let enrolled = lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    Host {
        id: enrolled.agent_id,
        paths,
    }
}

async fn channel(ts: &TestServer, paths: &Paths) -> Channel {
    let address = ts.grpc_address();
    let bootstrap = connect::channel(
        &address,
        "127.0.0.1",
        tls::bootstrap_config().unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("bootstrap channel");
    let ca_pem = EnrollmentClient::new(bootstrap)
        .get_trust_anchor(GetTrustAnchorRequest {})
        .await
        .expect("trust anchor")
        .into_inner()
        .ca_pem;
    let cert = std::fs::read_to_string(paths.cert()).unwrap();
    let key = std::fs::read_to_string(paths.key()).unwrap();
    connect::channel(
        &address,
        "127.0.0.1",
        tls::pinned_config(&ca_pem, Some((cert.as_str(), key.as_str()))).unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("agent channel")
}

struct Fake {
    tx: mpsc::Sender<AgentMessage>,
    rx: Streaming<ServerMessage>,
}

impl Fake {
    async fn connect(ts: &TestServer, host: &Host, capabilities: &[&str]) -> Fake {
        let (tx, rx) = mpsc::channel(64);
        tx.send(AgentMessage {
            body: Some(AgentBody::Hello(Hello {
                versions: Some(Versions {
                    min: PROTOCOL_VERSION,
                    max: PROTOCOL_VERSION,
                }),
                agent_version: "test".to_string(),
                jobs: Vec::new(),
                capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            })),
        })
        .await
        .unwrap();
        let inbound = AgentClient::new(channel(ts, &host.paths).await)
            .session(ReceiverStream::new(rx))
            .await
            .expect("session")
            .into_inner();
        Fake { tx, rx: inbound }
    }

    async fn send(&self, body: AgentBody) {
        self.tx
            .send(AgentMessage { body: Some(body) })
            .await
            .unwrap();
    }

    async fn recv<T>(&mut self, pick: impl Fn(ServerBody) -> Option<T>) -> T {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(15), self.rx.message())
                .await
                .expect("timed out waiting for a server message")
                .expect("stream error")
                .expect("the server closed the stream");
            if let Some(found) = message.body.and_then(&pick) {
                return found;
            }
        }
    }

    async fn drain(&mut self, window: Duration) -> Vec<ServerBody> {
        let deadline = tokio::time::Instant::now() + window;
        let mut out = Vec::new();
        while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, self.rx.message()).await
        {
            out.extend(message.body);
        }
        out
    }

    async fn assigned(&mut self) -> (String, RestoreSpec) {
        self.recv(|body| match body {
            ServerBody::JobAssign(a) => a.restore.map(|r| (a.job_id, r)),
            _ => None,
        })
        .await
    }

    async fn accept(&self, job_id: &str) {
        self.send(AgentBody::JobAccepted(JobAccepted {
            job_id: job_id.to_string(),
            started_at_ms: now_ms(),
        }))
        .await;
    }
}

async fn text(pool: &AnyPool, sql: &'static str, id: &str) -> Option<String> {
    sqlx::query(sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .and_then(|row| row.try_get::<Option<String>, _>(0).unwrap())
}

async fn state_of(pool: &AnyPool, id: &str) -> String {
    text(pool, "SELECT state FROM jobs WHERE id = $1", id)
        .await
        .unwrap_or_else(|| "<missing>".to_string())
}

async fn wait_state(pool: &AnyPool, id: &str, want: &str) {
    let reached = wait_until(Duration::from_secs(10), || async {
        state_of(pool, id).await == want
    })
    .await;
    assert!(
        reached,
        "job {id} is '{}', expected '{want}'",
        state_of(pool, id).await
    );
}

struct Fixture {
    root: PathBuf,
    snapshot: String,
}

impl Fixture {
    fn item(&self, rel: &str) -> Vec<u8> {
        self.root
            .join(rel)
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    }
}

fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn write_files(root: &Path) {
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a/one.txt"), "alpha").unwrap();
    std::fs::write(root.join("a/two.txt"), "bravo bravo").unwrap();
    std::fs::write(root.join("b"), "bee").unwrap();
    std::fs::write(root.join("big.bin"), noise(BIG_LEN)).unwrap();
    std::os::unix::fs::symlink("b", root.join("link")).unwrap();
}

async fn snapshot_of<S: BlobStore>(
    store: &S,
    ts: &TestServer,
    repo_id: &str,
    name: &str,
) -> Fixture {
    let root = ts.dir.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    write_files(&root);
    let root = root.canonicalize().unwrap();
    let repo = Repository::open(store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(store, keys, repo.config()).await.unwrap();
    let outcome = writer
        .backup(
            std::slice::from_ref(&root),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .expect("backup");
    let snapshot = hex32(&outcome.snapshot_id);
    ingest_snapshot(ts.state(), repo_id, &snapshot, None)
        .await
        .expect("ingest");
    Fixture { root, snapshot }
}

async fn local_fixture(ts: &TestServer, repo_id: &str) -> (Fixture, LocalFsStore) {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let fixture = snapshot_of(&store, ts, repo_id, "data").await;
    (fixture, store)
}

async fn queue(ts: &TestServer, fx: &Fixture, host: &Host, items: Vec<Vec<u8>>) -> String {
    enqueue_restore(
        ts.state(),
        RestoreRequest {
            snapshot_id: fx.snapshot.clone(),
            items,
            target_host_id: host.id.clone(),
            destination: "/srv/out".to_string(),
            in_place: false,
            conflict: ConflictPolicy::OverwriteChanged,
            restore_owners: false,
            accept_unverified: true,
        },
        "admin",
    )
    .await
    .expect("queue a restore")
}

#[derive(Default)]
struct Plan {
    totals: Option<(u64, u64)>,
    packs: HashMap<u32, PackAccess>,
    items: Vec<Kind>,
    batches: usize,
}

async fn read_plan(channel: Channel, job_id: &str) -> Plan {
    let mut stream = AgentClient::new(channel)
        .restore_plan(RestorePlanRequest {
            job_id: job_id.to_string(),
        })
        .await
        .expect("restore plan")
        .into_inner();
    let mut plan = Plan::default();
    let mut ended = false;
    while let Some(batch) = stream.message().await.expect("plan stream") {
        assert!(!ended, "nothing follows the batch marked last");
        ended = batch.last;
        if plan.batches == 0 {
            plan.totals = batch.totals.map(|t| (t.files, t.bytes));
        } else {
            assert!(batch.totals.is_none(), "only the first batch has totals");
        }
        plan.batches += 1;
        assert!(batch.encoded_len() <= 1 << 20, "a batch stays within 1 MiB");
        for pack in batch.packs {
            plan.packs.insert(pack.pack, pack);
        }
        plan.items
            .extend(batch.items.into_iter().filter_map(|i| i.kind));
    }
    assert!(ended, "the final batch is marked last");
    plan
}

fn node_of(bytes: &[u8]) -> pb::Node {
    pb::Node::decode(bytes).unwrap()
}

struct Source {
    repo_id: [u8; 16],
    k_id: [u8; 32],
}

fn open_with(
    source: &Source,
    access: &PackAccess,
    frame: &[u8],
    chunk: &lodestore_proto::agent::v1::ChunkLoc,
) -> Vec<u8> {
    let key: [u8; 32] = access.key.as_slice().try_into().unwrap();
    let id: [u8; 32] = chunk.id.as_slice().try_into().unwrap();
    open_chunk(
        &zeroize::Zeroizing::new(key),
        &source.repo_id,
        BLOB_TYPE_DATA,
        &id,
        frame,
        chunk.ulen,
        &source.k_id,
    )
    .expect("the frame opens with the issued key")
}

async fn file_contents(
    plan: &Plan,
    source: &Source,
    store: &LocalFsStore,
) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut open: Option<(String, Vec<u8>)> = None;
    for item in &plan.items {
        let (chunks, more) = match item {
            Kind::Entry(entry) => {
                let node = node_of(&entry.node);
                assert!(node.content.is_empty(), "plan nodes carry no content");
                open = Some((String::from_utf8(node.name).unwrap(), Vec::new()));
                (&entry.chunks, entry.more)
            }
            Kind::Chunks(chunks) => (&chunks.chunks, chunks.more),
            _ => continue,
        };
        let (name, data) = open.as_mut().unwrap();
        for chunk in chunks {
            let access = &plan.packs[&chunk.pack];
            let pack_id: [u8; 32] = access.pack_id.as_slice().try_into().unwrap();
            let start = u64::from(chunk.offset);
            let frame = store
                .get_range(
                    &pack_object_key(&pack_id),
                    start..start + u64::from(chunk.enc_len),
                )
                .await
                .unwrap();
            data.extend(open_with(source, access, &frame, chunk));
        }
        if !more {
            let (name, data) = (std::mem::take(name), std::mem::take(data));
            out.insert(name, data);
            open = None;
        }
    }
    out
}

async fn source_of(store: &LocalFsStore) -> Source {
    let repo = Repository::open(store, PASSWORD).await.unwrap();
    Source {
        repo_id: *repo.read_keys().repo_id(),
        k_id: *repo.read_keys().k_id(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_streams_selected_files() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, store) = local_fixture(&ts, &repo).await;
        let source = source_of(&store).await;

        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let job = queue(&ts, &fx, &host, vec![fx.item("a"), fx.item("b")]).await;
        let (assigned, spec) = agent.assigned().await;
        assert_eq!(assigned, job, "[{}]", backend.name());
        assert_eq!(spec.k_id, source.k_id);
        assert_eq!(spec.source_repo_id, source.repo_id);
        assert_eq!(spec.snapshot_id, fx.snapshot);
        assert_eq!(spec.destination, "/srv/out");

        let plan = read_plan(channel(&ts, &host.paths).await, &job).await;
        assert_eq!(plan.totals, Some((3, 19)), "[{}]", backend.name());
        assert!(plan.packs.values().all(|p| p.url.is_empty()));

        let files = file_contents(&plan, &source, &store).await;
        assert_eq!(
            files,
            BTreeMap::from([
                ("one.txt".to_string(), b"alpha".to_vec()),
                ("two.txt".to_string(), b"bravo bravo".to_vec()),
                ("b".to_string(), b"bee".to_vec()),
            ]),
            "[{}]",
            backend.name()
        );
        let enters = plan
            .items
            .iter()
            .filter(|i| matches!(i, Kind::Enter(_)))
            .count();
        let leaves = plan
            .items
            .iter()
            .filter(|i| matches!(i, Kind::Leave(_)))
            .count();
        assert_eq!(enters, leaves);
        assert!(
            plan.items
                .iter()
                .any(|i| matches!(i, Kind::Enter(e) if !e.ancestor))
        );
        assert!(
            plan.items
                .iter()
                .any(|i| matches!(i, Kind::Enter(e) if e.ancestor))
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_refuses_wide_directory_by_byte_limit() {
    let backend = backends().remove(0);
    let ts = TestServer::start(&backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let host = enroll(&ts, &api, &repo, "host-a").await;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
    let pieces = tree_support::wide_piece_count();
    let wide = hex32(&tree_support::wide_snapshot(&mut writer, pieces).await);
    ingest_snapshot(ts.state(), &repo, &wide, None)
        .await
        .expect("ingest");
    let fx = Fixture {
        root: PathBuf::from("/"),
        snapshot: wide,
    };

    let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
    let job = queue(&ts, &fx, &host, vec![fx.item("wide")]).await;
    let (assigned, _) = agent.assigned().await;
    assert_eq!(assigned, job);

    let mut client = AgentClient::new(channel(&ts, &host.paths).await);
    let request = RestorePlanRequest { job_id: job };
    let status = match client.restore_plan(request).await {
        Err(status) => status,
        Ok(response) => response
            .into_inner()
            .message()
            .await
            .expect_err("the plan of the wide directory must fail"),
    };
    assert_eq!(status.code(), Code::ResourceExhausted, "{status}");
    assert!(
        status.message().contains("bytes in one directory"),
        "{status}"
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_rejects_other_agents_and_idle_jobs() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let other = enroll(&ts, &api, &repo, "host-b").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;
        let request = || RestorePlanRequest {
            job_id: job.clone(),
        };

        let owner = channel(&ts, &host.paths).await;
        let status = AgentClient::new(owner.clone())
            .restore_plan(request())
            .await
            .expect_err("a queued job has no plan yet");
        assert_eq!(status.code(), Code::PermissionDenied);

        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        agent.assigned().await;
        let status = AgentClient::new(channel(&ts, &other.paths).await)
            .restore_plan(request())
            .await
            .expect_err("another agent must not read the plan");
        assert_eq!(
            status.code(),
            Code::PermissionDenied,
            "[{}]",
            backend.name()
        );
        assert!(
            AgentClient::new(owner)
                .restore_plan(request())
                .await
                .is_ok(),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

async fn issued_plan(
    ts: &TestServer,
    host: &Host,
    fx: &Fixture,
    items: Vec<Vec<u8>>,
    agent: &mut Fake,
) -> (String, Plan) {
    let job = queue(ts, fx, host, items).await;
    agent.assigned().await;
    let plan = read_plan(channel(ts, &host.paths).await, &job).await;
    (job, plan)
}

async fn range(
    channel: Channel,
    job: &str,
    pack: u32,
    offset: u64,
    length: u32,
) -> Result<Vec<Vec<u8>>, tonic::Status> {
    let mut stream = AgentClient::new(channel)
        .read_pack_range(ReadPackRangeRequest {
            job_id: job.to_string(),
            pack,
            offset,
            length,
        })
        .await?
        .into_inner();
    let mut parts = Vec::new();
    while let Some(part) = stream.message().await? {
        parts.push(part.data);
    }
    Ok(parts)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_read_pack_range_authorized_only() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let other = enroll(&ts, &api, &repo, "host-b").await;
        let (fx, store) = local_fixture(&ts, &repo).await;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let (job, plan) = issued_plan(&ts, &host, &fx, vec![fx.item("b")], &mut agent).await;
        let access = &plan.packs[&0];
        let pack_id: [u8; 32] = access.pack_id.as_slice().try_into().unwrap();
        let whole = store.get(&pack_object_key(&pack_id)).await.unwrap();
        let tag = backend.name();

        let parts = range(channel(&ts, &host.paths).await, &job, 0, 8, 64)
            .await
            .unwrap();
        assert_eq!(parts.concat(), whole[8..72], "[{tag}]");

        let unknown = range(channel(&ts, &host.paths).await, &job, 9999, 0, 8).await;
        assert_eq!(
            unknown.unwrap_err().code(),
            Code::PermissionDenied,
            "[{tag}]"
        );

        let foreign = range(channel(&ts, &other.paths).await, &job, 0, 0, 8).await;
        assert_eq!(
            foreign.unwrap_err().code(),
            Code::PermissionDenied,
            "[{tag}]"
        );

        let past = range(
            channel(&ts, &host.paths).await,
            &job,
            0,
            whole.len() as u64 - 4,
            16,
        )
        .await;
        assert_eq!(past.unwrap_err().code(), Code::OutOfRange, "[{tag}]");

        let empty = range(channel(&ts, &host.paths).await, &job, 0, 0, 0).await;
        assert_eq!(empty.unwrap_err().code(), Code::InvalidArgument, "[{tag}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_read_pack_range_chunks_1mib() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, store) = local_fixture(&ts, &repo).await;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let (job, plan) = issued_plan(&ts, &host, &fx, vec![fx.item("big.bin")], &mut agent).await;

        let (pack, access) = plan
            .packs
            .iter()
            .max_by_key(|(_, a)| a.pack)
            .map(|(n, a)| (*n, a.clone()))
            .unwrap();
        let pack_id: [u8; 32] = access.pack_id.as_slice().try_into().unwrap();
        let whole = store.get(&pack_object_key(&pack_id)).await.unwrap();
        assert!(
            whole.len() > 1 << 20,
            "the pack must exceed one message part"
        );

        let length = u32::try_from(whole.len()).unwrap();
        let parts = range(channel(&ts, &host.paths).await, &job, pack, 0, length)
            .await
            .unwrap();
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= 1 << 20));
        assert_eq!(parts.concat(), whole.to_vec(), "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_refresh_only_issued() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let (job, plan) = issued_plan(&ts, &host, &fx, vec![fx.item("b")], &mut agent).await;
        let mut client = AgentClient::new(channel(&ts, &host.paths).await);
        let tag = backend.name();

        let refreshed = client
            .refresh_pack_access(RefreshPackAccessRequest {
                job_id: job.clone(),
                packs: vec![0],
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(refreshed.packs.len(), 1, "[{tag}]");
        assert_eq!(refreshed.packs[0].pack_id, plan.packs[&0].pack_id);
        assert_eq!(refreshed.packs[0].key, plan.packs[&0].key);

        let denied = client
            .refresh_pack_access(RefreshPackAccessRequest {
                job_id: job.clone(),
                packs: vec![0, 77],
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied, "[{tag}]");

        let again = read_plan(channel(&ts, &host.paths).await, &job).await;
        assert_eq!(again.packs.len(), plan.packs.len());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_presigned_range_minio() {
    let minio = MinioServer::start().await;
    let bucket = "repo";
    minio.create_bucket(bucket).await;
    let root = minio.root_credentials();
    let agent_key = minio
        .create_user_with_policy(
            bucket,
            &lodestore_format::store::policy::agent_minio_policy_json(bucket, ""),
        )
        .await;
    let options = BTreeMap::from([
        ("bucket".to_string(), bucket.to_string()),
        ("endpoint".to_string(), minio.endpoint().to_string()),
        ("path_style".to_string(), "true".to_string()),
    ]);
    let location = RepoLocation {
        backend: "s3".to_string(),
        options: options.clone(),
    };
    let open = |key: &(String, String)| {
        location
            .open(Some(StaticCredentials {
                access_key_id: key.0.clone(),
                secret_access_key: key.1.clone(),
            }))
            .unwrap()
    };
    let server_store = open(&root);

    let mut entropy = Entropy::os();
    let pending = PendingInit::generate(
        &mut entropy,
        InitOptions {
            kdf: KdfCost {
                memory_kib: 64,
                iterations: 1,
                parallelism: 1,
            },
            ..InitOptions::default()
        },
    )
    .unwrap();
    let keys = pending.read_keys();
    let master = keys.master().clone();
    let format_repo_id = *keys.repo_id();
    let config_hash = lodestore_format::config::config_hash(pending.config_bytes());
    pending
        .commit(&server_store, PASSWORD, &mut entropy)
        .await
        .unwrap();

    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = lodestore_server::repos::db::create_repository(
            ts.state(),
            NewRepository {
                name: "cloud".to_string(),
                location: DbLocation {
                    backend: "s3".to_string(),
                    options: options.clone(),
                },
                experimental: false,
                format_repo_id,
                config_hash,
                master: master.clone(),
                server_slot_secret: None,
                server_creds: Some(AccessKey {
                    access_key_id: root.0.clone(),
                    secret_access_key: root.1.clone(),
                }),
                agent_creds: Some(AccessKey {
                    access_key_id: agent_key.0.clone(),
                    secret_access_key: agent_key.1.clone(),
                }),
            },
        )
        .await
        .unwrap();
        let api = ts.api().await;
        let host = enroll(&ts, &api, &repo, "host-s3").await;
        let fx = snapshot_of(
            &server_store,
            &ts,
            &repo,
            &format!("data-{}", backend.name()),
        )
        .await;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let job = queue(&ts, &fx, &host, vec![fx.item("big.bin")]).await;
        let (_, spec) = agent.assigned().await;
        let source = Source {
            repo_id: spec.source_repo_id.as_slice().try_into().unwrap(),
            k_id: spec.k_id.as_slice().try_into().unwrap(),
        };
        let plan = read_plan(channel(&ts, &host.paths).await, &job).await;
        assert!(plan.packs.values().all(|p| !p.url.is_empty()));

        let http = reqwest::Client::new();
        let mut data = Vec::new();
        for item in &plan.items {
            let Kind::Entry(entry) = item else { continue };
            for chunk in &entry.chunks {
                let access = &plan.packs[&chunk.pack];
                let end = u64::from(chunk.offset) + u64::from(chunk.enc_len) - 1;
                let response = http
                    .get(&access.url)
                    .header("Range", format!("bytes={}-{end}", chunk.offset))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 206, "[{}]", backend.name());
                let frame = response.bytes().await.unwrap();
                assert_eq!(frame.len(), chunk.enc_len as usize);
                data.extend(open_with(&source, access, &frame, chunk));
            }
        }
        assert_eq!(data, noise(BIG_LEN), "[{}]", backend.name());

        let pack_id: [u8; 32] = plan.packs[&0].pack_id.as_slice().try_into().unwrap();
        let denied = open(&agent_key)
            .get_range(&pack_object_key(&pack_id), 0..16)
            .await;
        assert!(
            denied.is_err(),
            "the write-only agent key must not read packs"
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        let refreshed = AgentClient::new(channel(&ts, &host.paths).await)
            .refresh_pack_access(RefreshPackAccessRequest {
                job_id: job.clone(),
                packs: vec![0],
            })
            .await
            .unwrap()
            .into_inner();
        assert!(!refreshed.packs[0].url.is_empty());
        assert!(refreshed.packs[0].expires_at_ms > plan.packs[&0].expires_at_ms);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_finished_stores_counters() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let snapshots = |pool: &AnyPool| {
            let pool = pool.clone();
            async move {
                sqlx::query("SELECT COUNT(*) FROM snapshots")
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                    .try_get::<i64, _>(0)
                    .unwrap()
            }
        };
        let before = snapshots(pool).await;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let (job, _) = issued_plan(&ts, &host, &fx, vec![fx.item("b")], &mut agent).await;
        assert!(ts.state().registry.issued_pack(&job, 0).is_some());
        agent.accept(&job).await;
        wait_state(pool, &job, "running").await;

        agent
            .send(AgentBody::JobFinished(JobFinished {
                job_id: job.clone(),
                outcome: JobOutcome::Succeeded as i32,
                restore: Some(RestoreStats {
                    created: 2,
                    overwritten: 1,
                    skipped: 3,
                    files: 3,
                    bytes: 10,
                }),
                finished_at_ms: now_ms(),
                ..Default::default()
            }))
            .await;
        wait_state(pool, &job, "succeeded").await;

        let stats = text(pool, "SELECT stats_json FROM jobs WHERE id = $1", &job)
            .await
            .unwrap();
        let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
        assert_eq!(
            stats,
            serde_json::json!({"created": 2, "overwritten": 1, "skipped": 3, "files": 3, "bytes": 10}),
            "[{}]",
            backend.name()
        );
        assert!(
            text(pool, "SELECT snapshot_id FROM jobs WHERE id = $1", &job)
                .await
                .is_none()
        );
        assert_eq!(snapshots(pool).await, before);
        assert!(ts.state().registry.issued_pack(&job, 0).is_none());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_revoke_and_late_finish_drop_issued_keys() {
    let backend = backends().remove(0);
    let ts = TestServer::start(&backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let (fx, _) = local_fixture(&ts, &repo).await;
    let registry = ts.state().registry.clone();

    let host = enroll(&ts, &api, &repo, "host-a").await;
    let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
    let (job, _) = issued_plan(&ts, &host, &fx, vec![fx.item("b")], &mut agent).await;
    assert!(registry.issued_pack(&job, 0).is_some());
    let revoked = api
        .post(
            &format!("/api/v1/hosts/{}/revoke", host.id),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(revoked.status(), 204);
    assert!(registry.issued_pack(&job, 0).is_none());

    let host = enroll(&ts, &api, &repo, "host-b").await;
    let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
    let (job, _) = issued_plan(&ts, &host, &fx, vec![fx.item("b")], &mut agent).await;
    sqlx::query("UPDATE jobs SET state = 'cancelled' WHERE id = $1")
        .bind(&job)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
    agent
        .send(AgentBody::JobFinished(JobFinished {
            job_id: job.clone(),
            outcome: JobOutcome::Cancelled as i32,
            ..Default::default()
        }))
        .await;
    let cleared = wait_until(Duration::from_secs(10), || async {
        registry.issued_pack(&job, 0).is_none()
    })
    .await;
    assert!(cleared);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_progress_bytes_downloaded() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;
        agent.assigned().await;
        agent.accept(&job).await;
        wait_state(pool, &job, "running").await;

        agent
            .send(AgentBody::JobProgress(JobProgress {
                job_id: job.clone(),
                bytes_downloaded: 42,
                ..Default::default()
            }))
            .await;
        let registry = ts.state().registry.clone();
        let seen = wait_until(Duration::from_secs(10), || async {
            registry
                .live
                .get(&job)
                .is_some_and(|p| p.bytes_downloaded == 42)
        })
        .await;
        assert!(seen, "[{}]", backend.name());
        let stored = wait_until(Duration::from_secs(15), || async {
            text(pool, "SELECT progress_json FROM jobs WHERE id = $1", &job)
                .await
                .is_some_and(|json| json.contains("\"bytes_downloaded\":42"))
        })
        .await;
        assert!(stored, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_phases_stored() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;
        agent.assigned().await;
        agent.accept(&job).await;
        wait_state(pool, &job, "running").await;

        for (kind, name) in [
            (JobPhaseKind::RestorePlan, "restore_plan"),
            (JobPhaseKind::RestoreFiles, "restore_files"),
            (JobPhaseKind::RestoreMetadata, "restore_metadata"),
        ] {
            agent
                .send(AgentBody::JobPhase(JobPhase {
                    job_id: job.clone(),
                    phase: kind as i32,
                    script: String::new(),
                }))
                .await;
            let stored = wait_until(Duration::from_secs(10), || async {
                text(pool, "SELECT phase FROM jobs WHERE id = $1", &job)
                    .await
                    .as_deref()
                    == Some(name)
            })
            .await;
            assert!(stored, "{name} [{}]", backend.name());
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_waits_for_running_backup() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, assigned_at, started_at) \
             VALUES ('backup-1', $1, 'manual', 'running', $2, $3, $3, $3)",
        )
        .bind(&host.id)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .execute(pool)
        .await
        .unwrap();
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(state_of(pool, &job).await, "queued", "[{}]", backend.name());

        sqlx::query("UPDATE jobs SET state = 'succeeded', finished_at = $1 WHERE id = 'backup-1'")
            .bind(now_ms())
            .execute(pool)
            .await
            .unwrap();
        let (assigned, _) = agent.assigned().await;
        assert_eq!(assigned, job);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_requires_capability() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let _agent = Fake::connect(&ts, &host, &[]).await;
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;

        wait_state(pool, &job, "failed").await;
        assert_eq!(
            text(pool, "SELECT error_code FROM jobs WHERE id = $1", &job)
                .await
                .as_deref(),
            Some("restore_agent_outdated"),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_cancel_pushes_job_cancel() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;
        agent.assigned().await;
        agent.accept(&job).await;
        wait_state(pool, &job, "running").await;

        sqlx::query("UPDATE jobs SET cancel_requested = 1 WHERE id = $1")
            .bind(&job)
            .execute(pool)
            .await
            .unwrap();
        let cancelled = agent
            .recv(|body| match body {
                ServerBody::JobCancel(c) => Some(c.job_id),
                _ => None,
            })
            .await;
        assert_eq!(cancelled, job, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_offline_target_expires() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;
        assert_eq!(state_of(pool, &job).await, "queued");

        janitor_once(ts.state(), now_ms() + 25 * 3600 * 1000)
            .await
            .unwrap();
        assert_eq!(
            state_of(pool, &job).await,
            "not_started",
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

async fn mark_unavailable(ts: &TestServer, repo: &str) {
    sqlx::query("UPDATE repositories SET open_error = 'differs from the configuration recorded' WHERE id = $1")
        .bind(repo)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_unavailable_source_fails_assignment() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let pool = &ts.state().db.pool;
        let job = queue(&ts, &fx, &host, vec![fx.item("b")]).await;
        assert_eq!(state_of(pool, &job).await, "queued");
        mark_unavailable(&ts, &repo).await;

        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        wait_state(pool, &job, "failed").await;
        let code: String = sqlx::query("SELECT error_code FROM jobs WHERE id = $1")
            .bind(&job)
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("error_code")
            .unwrap();
        assert_eq!(code, "repository_unavailable", "[{}]", backend.name());
        let sent = agent.drain(Duration::from_millis(500)).await;
        assert!(
            sent.iter()
                .all(|body| !matches!(body, ServerBody::JobAssign(_))),
            "[{}] the agent got keys of an unavailable source",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_unavailable_source_refuses_pack_access() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let (job, _) = issued_plan(&ts, &host, &fx, vec![fx.item("b")], &mut agent).await;
        mark_unavailable(&ts, &repo).await;
        let tag = backend.name();

        let read = range(channel(&ts, &host.paths).await, &job, 0, 8, 64).await;
        assert_eq!(
            read.unwrap_err().code(),
            Code::FailedPrecondition,
            "[{tag}]"
        );
        let refreshed = AgentClient::new(channel(&ts, &host.paths).await)
            .refresh_pack_access(RefreshPackAccessRequest {
                job_id: job.clone(),
                packs: vec![0],
            })
            .await
            .unwrap_err();
        assert_eq!(refreshed.code(), Code::FailedPrecondition, "[{tag}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_unavailable_source_refuses_plan() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let mut agent = Fake::connect(&ts, &host, &[CAP]).await;
        let (job, _) = issued_plan(&ts, &host, &fx, vec![fx.item("b")], &mut agent).await;
        sqlx::query("UPDATE repositories SET config_hash = $1 WHERE id = $2")
            .bind(vec![9u8; 32])
            .bind(&repo)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let status = AgentClient::new(channel(&ts, &host.paths).await)
            .restore_plan(RestorePlanRequest { job_id: job })
            .await
            .expect_err("the planner must not open an unavailable source");
        assert_eq!(
            status.code(),
            Code::FailedPrecondition,
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_plan_enqueue_validates() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let (fx, _) = local_fixture(&ts, &repo).await;
        let request = |tweak: &dyn Fn(&mut RestoreRequest)| {
            let mut req = RestoreRequest {
                snapshot_id: fx.snapshot.clone(),
                items: vec![fx.item("b")],
                target_host_id: host.id.clone(),
                destination: "/srv/out".to_string(),
                in_place: false,
                conflict: ConflictPolicy::SkipExisting,
                restore_owners: false,
                accept_unverified: true,
            };
            tweak(&mut req);
            req
        };
        let fails = |req: RestoreRequest| {
            let state = ts.state().clone();
            async move { enqueue_restore(&state, req, "admin").await.unwrap_err() }
        };
        use lodestore_server::error::ApiError;
        assert!(matches!(
            fails(request(&|r| r.snapshot_id = "0".repeat(64))).await,
            ApiError::NotFound(_)
        ));
        assert!(matches!(
            fails(request(&|r| r.target_host_id = "nobody".into())).await,
            ApiError::NotFound(_)
        ));
        for tweak in [
            (&|r: &mut RestoreRequest| r.destination = "relative".into())
                as &dyn Fn(&mut RestoreRequest),
            &|r| r.destination = "/a/../b".into(),
            &|r| r.in_place = true,
            &|r| r.items.clear(),
            &|r| r.items = vec![b"/a/../b".to_vec()],
            &|r| r.items = vec![b"relative".to_vec()],
            &|r| {
                r.items = (0..10_001)
                    .map(|i| format!("/x/{i}").into_bytes())
                    .collect()
            },
        ] {
            assert!(
                matches!(fails(request(tweak)).await, ApiError::Validation(_)),
                "[{}]",
                backend.name()
            );
        }
        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = $1")
            .bind(&host.id)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert!(matches!(
            fails(request(&|_| {})).await,
            ApiError::Validation(_)
        ));
        ts.stop().await;
    }
}
