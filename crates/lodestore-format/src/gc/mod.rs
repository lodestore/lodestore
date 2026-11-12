//! Garbage collection: finding what the bucket no longer needs.
//!
//! [`survey`] only reads. The live set comes from the snapshots found by
//! listing `snapshots/`, never from a catalog, so a snapshot that retention
//! could not delete keeps its data.

use std::collections::HashSet;

use crate::crypto::BlobId;
use crate::error::Result;
use crate::index::{Index, PACKS_PREFIX, listed_sizes};
use crate::keys::ReadKeys;
use crate::pb;
use crate::store::BlobStore;

pub mod compact;
pub mod mark;
pub mod plan;
pub mod repack;
pub mod sweep;
pub mod verify;

use mark::{LiveSet, MarkOutcome, MarkPolicy, Quarantined, ShardSet, load_shards, mark_with};
use plan::{GcPlan, PlanOptions, plan};
use repack::RepackOutcome;

/// The packs a collection means to delete: those with nothing alive in them
/// and those the repack replaced.
pub fn candidates(plan: &GcPlan, repack: &RepackOutcome) -> Vec<BlobId> {
    let mut out: Vec<BlobId> = plan
        .delete
        .iter()
        .map(|p| p.pack_id)
        .chain(repack.replaced.iter().copied())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurveyProgress {
    Snapshots { done: u64, total: u64 },
}

pub struct Survey {
    pub shards: ShardSet,
    pub index: Index,
    pub live: LiveSet,
    pub snapshots: Vec<BlobId>,
    pub ignored: Vec<(String, String)>,
    pub quarantined: Vec<Quarantined>,
    pub plan: GcPlan,
}

/// Marks the live blobs of every snapshot in the bucket and plans which packs
/// can be deleted or repacked. Writes nothing.
pub async fn survey<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    opts: &PlanOptions,
    progress: &(dyn Fn(SurveyProgress) + Sync),
) -> Result<Survey> {
    survey_with(store, keys, config, opts, &MarkPolicy::default(), progress).await
}

/// [`survey`] with a policy for snapshots the walk cannot follow. While any
/// snapshot is quarantined the plan is empty: nothing is deleted or repacked.
pub async fn survey_with<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    opts: &PlanOptions,
    policy: &MarkPolicy,
    progress: &(dyn Fn(SurveyProgress) + Sync),
) -> Result<Survey> {
    let shards = load_shards(store, keys).await?;
    let mut index = shards.index(&HashSet::new())?;
    let MarkOutcome {
        live,
        snapshots,
        ignored,
        quarantined,
    } = mark_with(store, keys, config, &index, policy, &|done, total| {
        progress(SurveyProgress::Snapshots { done, total });
    })
    .await?;
    let listing = store.list(PACKS_PREFIX).await?;
    // A planted shard can name packs the bucket lacks; plan and repack must place a blob where verify does.
    index.retain_packs(&listed_sizes(&listing));
    let plan = if quarantined.is_empty() {
        if let Some((id, blob_type)) = live.iter().find(|(id, t)| index.get(id, *t).is_none())
            && let Some(gone) = shards.index(&HashSet::new())?.get(id, *blob_type)
        {
            return Err(plan::pack_missing(&gone.pack_id));
        }
        plan(&shards, &index, &live, &listing, opts)?
    } else {
        GcPlan::default()
    };
    Ok(Survey {
        shards,
        index,
        live,
        snapshots,
        ignored,
        quarantined,
        plan,
    })
}
