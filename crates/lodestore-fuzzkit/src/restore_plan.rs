//! The restore executor of the agent, fed a plan from the fuzzer. Each call
//! restores into its own temporary folder and checks that nothing appeared
//! beside it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use lodestore_agent::Paths;
use lodestore_agent::jobs::JobHub;
use lodestore_agent::jobs::fetch::{FetchError, PackAccess, PackFetcher};
use lodestore_agent::jobs::restore::{PlanError, PlanSource, RestoreOptions, execute_with};
use lodestore_format::pb::{Node, NodeType};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::plan_item::Kind;
use lodestore_proto::agent::v1::{
    AgentMessage, ConflictPolicy, JobFinished, RestorePlanBatch, RestoreSpec,
};
use prost::Message;
use tokio::runtime::{Builder, Runtime};

const MAX_BATCHES: usize = 32;
const MAX_FETCH: u64 = 1 << 20;
const FINISH_WITHIN: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);

struct MemPlan {
    batches: Vec<RestorePlanBatch>,
    pos: usize,
}

impl PlanSource for MemPlan {
    async fn next(&mut self) -> Result<Option<RestorePlanBatch>, PlanError> {
        let batch = self.batches.get(self.pos).cloned();
        self.pos += 1;
        Ok(batch)
    }

    async fn reopen(&mut self) -> Result<(), PlanError> {
        self.pos = 0;
        Ok(())
    }
}

struct Fetch {
    zeros: bool,
}

impl PackFetcher for Fetch {
    async fn fetch(&self, _: &PackAccess, _: u64, len: u64) -> Result<Vec<u8>, FetchError> {
        if !self.zeros {
            return Err(FetchError::NotFound);
        }
        if len > MAX_FETCH {
            return Err(FetchError::Failed("the harness serves small ranges".into()));
        }
        Ok(vec![0; len as usize])
    }

    async fn refresh(&self, _: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        Err(FetchError::Failed("no refresh in the harness".into()))
    }
}

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime")
    })
}

fn batches(mut rest: &[u8]) -> Vec<RestorePlanBatch> {
    let mut out = Vec::new();
    while out.len() < MAX_BATCHES
        && let Some((len, tail)) = rest.split_first_chunk::<2>()
    {
        let (body, tail) = tail.split_at(usize::from(u16::from_le_bytes(*len)).min(tail.len()));
        rest = tail;
        if let Ok(batch) = RestorePlanBatch::decode(body) {
            out.push(batch);
        }
    }
    if out.is_empty() {
        out.push(RestorePlanBatch::default());
    }
    if let Some(batch) = out.last_mut() {
        batch.last = true;
    }
    for item in out.iter_mut().flat_map(|b| b.items.iter_mut()) {
        let raw = match item.kind.as_mut() {
            Some(Kind::Enter(e)) => &mut e.node,
            Some(Kind::Entry(e)) => &mut e.node,
            _ => continue,
        };
        if let Ok(mut node) = Node::decode(raw.as_slice())
            && (node.r#type == NodeType::Chardev as i32 || node.r#type == NodeType::Blockdev as i32)
        {
            node.rdev = 0;
            *raw = node.encode_to_vec();
        }
    }
    out
}

struct Wipe(PathBuf);

fn wipe(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
                wipe(&path);
                let _ = fs::remove_dir(&path);
            }
            _ => {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

impl Drop for Wipe {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        wipe(&self.0);
    }
}

fn finished_of(messages: &[AgentMessage], id: &str) -> Option<JobFinished> {
    messages.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobFinished(f)) if f.job_id == id => Some(f.clone()),
        _ => None,
    })
}

async fn wait_finished(hub: &JobHub, id: &str) -> JobFinished {
    let deadline = tokio::time::Instant::now() + FINISH_WITHIN;
    loop {
        if let Some(done) = finished_of(&hub.outbound(), id) {
            return done;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the restore did not finish within {FINISH_WITHIN:?}"
        );
        let _ = tokio::time::timeout(POLL, hub.changed()).await;
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .expect("the harness folder is readable")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

pub fn restore_plan(data: &[u8]) {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let plan = batches(rest);
    let base = tempfile::tempdir().expect("a temporary folder");
    let base_path = base
        .path()
        .canonicalize()
        .expect("the temporary folder resolves");
    let _guard = Wipe(base_path.clone());
    let (root, outside, state) = (
        base_path.join("root"),
        base_path.join("outside"),
        base_path.join("state"),
    );
    for dir in [&root, &outside, &state] {
        fs::create_dir(dir).expect("a harness folder");
    }
    let paths = Paths {
        config_dir: state.join("etc"),
        state_dir: state.join("var"),
    };
    let hub = JobHub::open_with_tick(&paths, "agent-fuzz", POLL).expect("a job hub");
    let options = RestoreOptions {
        plan_pauses: Vec::new(),
        roots: vec![root.clone()],
        ..Default::default()
    };
    let conflict = match flags & 3 {
        0 => ConflictPolicy::Unspecified,
        1 => ConflictPolicy::OverwriteChanged,
        2 => ConflictPolicy::SkipExisting,
        _ => ConflictPolicy::StopIfNotEmpty,
    };
    let spec = RestoreSpec {
        snapshot_id: "snapshot".into(),
        source_repo_id: vec![5; 16],
        k_id: vec![6; 32],
        destination: root.join("dest").to_string_lossy().into_owned(),
        in_place: false,
        conflict: conflict as i32,
        restore_owners: flags & 4 != 0,
    };
    let zeros = flags & 8 != 0;
    for round in 0..=usize::from(flags & 16 != 0) {
        let id = format!("fuzz-{round}");
        let (spec, options) = (spec.clone(), options.clone());
        let mut source = MemPlan {
            batches: plan.clone(),
            pos: 0,
        };
        hub.start_restore_with(id.clone(), spec, move |ctx, spec| {
            let finished = Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| {
                    rt.block_on(execute_with(
                        &ctx,
                        &spec,
                        &mut source,
                        &Fetch { zeros },
                        &options,
                    ))
                })
                .unwrap_or_default();
            ctx.finish(finished);
        });
        let done = runtime().block_on(wait_finished(&hub, &id));
        assert!(
            !done.error_message.contains("task panicked"),
            "the restore task panicked: {}",
            done.error_message
        );
        assert_eq!(
            names(&base_path),
            ["outside", "root", "state"],
            "the restore created something beside its folder"
        );
        assert!(
            names(&outside).is_empty(),
            "the restore wrote into a sibling folder"
        );
    }
}
