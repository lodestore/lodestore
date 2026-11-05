#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::time::Duration;

use adv_support::{attacker_keys, plant_blobs};
use common::{TestAgent, local_agent, snapshots};
use lodestore_agent::jobs::JobHub;
use lodestore_format::attest::ensure_upgraded;
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{JobAssign, JobOutcome, JobSpec};

const WAIT: Duration = Duration::from_secs(120);

async fn run_job(hub: &std::sync::Arc<JobHub>, id: &str, spec: JobSpec) {
    hub.handle(ServerBody::JobAssign(JobAssign {
        job_id: id.into(),
        spec: Some(spec),
        restore: None,
    }));
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        for m in hub.outbound() {
            if let Some(AgentBody::JobFinished(f)) = m.body {
                assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{f:?}");
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the job did not finish"
        );
        tokio::select! {
            () = hub.changed() => {}
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

async fn restores_equal(
    agent: &TestAgent,
    snapshot_id: &BlobId,
    src: &std::path::Path,
    want: &[u8],
) -> bool {
    let store = LocalFsStore::new(agent.repo_root.clone());
    let repo = Repository::open(&store, agent.password.as_bytes())
        .await
        .unwrap();
    let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let target = tempfile::tempdir().unwrap();
    let report = reader
        .restore(
            snapshot_id,
            target.path(),
            &RestoreOptions::default(),
            &|_| {},
        )
        .await;
    let Ok(report) = report else { return false };
    if !report.errors.is_empty() {
        return false;
    }
    let canonical = src.canonicalize().unwrap();
    let restored = target
        .path()
        .join(canonical.strip_prefix("/").unwrap())
        .join("file");
    std::fs::read(restored).is_ok_and(|got| got == want)
}

#[tokio::test]
async fn distrusted_shard_is_not_used_for_dedup() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let store = LocalFsStore::new(agent.repo_root.clone());
    let repo = Repository::open(&store, agent.password.as_bytes())
        .await
        .unwrap();

    let content = b"the content the lying shard claims to hold".to_vec();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("file"), &content).unwrap();

    let liar = plant_blobs(
        &store,
        &attacker_keys(&repo),
        &[(blob_id(repo.read_keys().k_id(), &content), BLOB_TYPE_DATA)],
    )
    .await;
    ensure_upgraded(&store, repo.read_keys(), &mut Entropy::os())
        .await
        .unwrap();

    let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
    let spec = JobSpec {
        policy_name: "p".into(),
        paths: vec![src.to_string_lossy().into_owned()],
        ..Default::default()
    };

    run_job(&hub, "job-1", spec.clone()).await;
    let first = snapshots(&agent).await[0].id;
    assert!(
        !restores_equal(&agent, &first, &src, &content).await,
        "control: without the list the hint is trusted and the snapshot points into the junk pack"
    );

    let spec = JobSpec {
        distrusted_shards: vec![liar.to_vec()],
        ..spec
    };
    run_job(&hub, "job-2", spec).await;
    let on_disk = std::fs::read_to_string(agent.paths.distrusted_shards()).unwrap();
    assert_eq!(on_disk.trim(), lodestore_format::index::hex32(&liar));
    let second: Vec<_> = snapshots(&agent)
        .await
        .into_iter()
        .filter(|e| e.id != first)
        .collect();
    assert_eq!(second.len(), 1);
    assert!(
        restores_equal(&agent, &second[0].id, &src, &content).await,
        "with the list the backup writes its own copy and restores byte for byte"
    );
}
