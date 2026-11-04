//! Backups and garbage collection steps in every order the protocol allows:
//! whatever the order, every snapshot in the bucket restores afterwards.

mod gc_support;

use std::path::{Path, PathBuf};

use gc_support::*;
use lodestore_format::crypto::BLOB_TYPE_DATA;
use lodestore_format::pack::pack_object_key;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::Writer;

async fn locks_left(fx: &Fx) -> Vec<String> {
    let mut left = fx.listing("locks/").await;
    left.retain(|key, _| !key.starts_with("locks/attest-"));
    left.into_keys().collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Survey,
    RepackPublish,
    VerifyCompact,
    SweepShards,
    SweepPacks,
    Open(usize),
    Commit(usize),
}

const GC: [Step; 5] = [
    Step::Survey,
    Step::RepackPublish,
    Step::VerifyCompact,
    Step::SweepShards,
    Step::SweepPacks,
];

const FORGOTTEN: Files<'static> = &[("shared", 1, 100 << 10), ("dead", 2, 200 << 10)];
const KEPT: Files<'static> = &[("shared", 1, 100 << 10), ("kept", 3, 200 << 10)];
/// The first brings back a blob that only the forgotten snapshot's pack holds.
const BACKUPS: [Files<'static>; 2] = [
    &[("dead", 2, 200 << 10), ("x-only", 4, 50 << 10)],
    &[("shared", 1, 100 << 10), ("y-only", 5, 50 << 10)],
];

/// Where a backup opens and commits: the number of GC steps already done.
type Placement = (usize, usize);

/// A backup opened before the manifest is published must commit before the
/// verification, which is what the time limit of an agent job guarantees.
fn placements() -> Vec<Placement> {
    let published = 2;
    let verified = 2;
    let mut out = Vec::new();
    for open in 0..=GC.len() {
        for commit in open..=GC.len() {
            if open >= published || commit <= verified {
                out.push((open, commit));
            }
        }
    }
    out
}

fn order(backups: &[(usize, Placement)]) -> Vec<Step> {
    let mut out = Vec::new();
    for gap in 0..=GC.len() {
        for (i, (open, commit)) in backups {
            if *open == gap {
                out.push(Step::Open(*i));
            }
            if *commit == gap {
                out.push(Step::Commit(*i));
            }
        }
        if let Some(step) = GC.get(gap) {
            out.push(*step);
        }
    }
    out
}

async fn base() -> (Fx, [PathBuf; 2]) {
    let fx = Fx::new().await;
    let forgotten = fx.backup("forgotten", FORGOTTEN).await;
    fx.backup("kept", KEPT).await;
    fx.forget(&forgotten.snapshot_id).await;
    let srcs = [fx.make_src("x", BACKUPS[0]), fx.make_src("y", BACKUPS[1])];
    (fx, srcs)
}

struct Run<'a> {
    fx: &'a Fx,
    srcs: &'a [PathBuf; 2],
    cycle: Cycle<'a>,
    writers: [Option<Writer<&'a LocalFsStore>>; 2],
}

impl<'a> Run<'a> {
    fn new(fx: &'a Fx, srcs: &'a [PathBuf; 2]) -> Self {
        Run {
            fx,
            srcs,
            cycle: Cycle::new(fx),
            writers: [None, None],
        }
    }

    async fn step(&mut self, step: Step) {
        match step {
            Step::Open(i) => self.writers[i] = Some(self.fx.open_writer().await),
            Step::Commit(i) => {
                let writer = self.writers[i].as_mut().expect("opened first");
                self.fx.commit(writer, self.srcs[i].clone()).await;
            }
            gc => gc_step(&mut self.cycle, gc).await,
        }
    }
}

async fn gc_step(cycle: &mut Cycle<'_>, step: Step) {
    match step {
        Step::Survey => cycle.survey(50).await,
        Step::RepackPublish => cycle.repack_publish().await,
        Step::VerifyCompact => {
            cycle.verify_compact().await;
        }
        Step::SweepShards => assert!(cycle.sweep_shards().await.failed.is_empty()),
        Step::SweepPacks => {
            assert!(cycle.sweep_packs(usize::MAX).await.failed.is_empty());
            assert!(cycle.finish().await.failed.is_empty());
        }
        Step::Open(_) | Step::Commit(_) => unreachable!("not a collection step"),
    }
}

async fn run_order(base: &Fx, srcs: &[PathBuf; 2], order: &[Step], backups: usize) {
    let fx = base.fork().await;
    let mut run = Run::new(&fx, srcs);
    for step in order {
        run.step(*step).await;
    }
    drop(run);
    assert_eq!(fx.sources.borrow().len(), 1 + backups, "{order:?}");
    fx.assert_all_restore().await;
    assert!(locks_left(&fx).await.is_empty(), "{order:?}");
}

#[tokio::test]
async fn gc_interleave_all_orders_restore() {
    let (base, srcs) = base().await;
    assert_eq!(placements().len(), 15);
    for i in 0..2 {
        for placement in placements() {
            run_order(&base, &srcs, &order(&[(i, placement)]), 1).await;
        }
    }
}

#[tokio::test]
async fn gc_interleave_two_backups_restore() {
    let (base, srcs) = base().await;
    for x in placements() {
        for y in [(1, 2), (3, 5)] {
            run_order(&base, &srcs, &order(&[(0, x), (1, y)]), 2).await;
        }
    }
}

#[tokio::test]
async fn gc_interleave_post_publish_writer_reuploads() {
    let (base, srcs) = base().await;
    let fx = base.fork().await;
    let mut cycle = Cycle::new(&fx);
    cycle.survey(50).await;
    let holder = cycle
        .survey
        .as_ref()
        .unwrap()
        .plan
        .repack
        .iter()
        .find(|p| p.blob_type == BLOB_TYPE_DATA)
        .expect("the forgotten snapshot's data pack is sparse")
        .pack_id;
    cycle.repack_publish().await;

    let packs_before = fx.packs().await.len();
    let mut writer = fx.open_writer().await;
    fx.commit(&mut writer, srcs[0].clone()).await;
    assert!(fx.packs().await.len() > packs_before);

    cycle.verify_compact().await;
    assert!(!cycle.resurrected.contains(&holder));
    assert!(cycle.candidates.contains(&holder));
    cycle.sweep_shards().await;
    cycle.sweep_packs(usize::MAX).await;
    cycle.finish().await;
    assert!(!fx.packs().await.contains_key(&pack_object_key(&holder)));
    fx.assert_all_restore().await;
}

/// An abort takes the marker down while the collector still checks. A
/// backup that starts then reuses a blob only a candidate pack holds, and
/// commits after the collector wrote its compaction: the old shard's record
/// of that pack must still be found, now and by the next cycle.
#[tokio::test]
async fn gc_interleave_abort_while_verifying() {
    let (base, srcs) = base().await;
    let fx = base.fork().await;
    let mut cycle = Cycle::new(&fx);
    cycle.survey(50).await;
    cycle.repack_publish().await;
    assert!(cycle.finish().await.failed.is_empty());

    let mut late = fx.open_writer().await;
    cycle.verify_compact().await;
    let bytes_before = fx.pack_bytes().await;
    fx.commit(&mut late, srcs[0].clone()).await;
    assert!(
        fx.pack_bytes().await - bytes_before < 200 << 10,
        "the late backup must reuse the candidate's blob rather than upload it"
    );
    fx.assert_all_restore().await;

    let mut next = Cycle::new(&fx);
    for step in GC {
        gc_step(&mut next, step).await;
    }
    fx.assert_all_restore().await;
    assert!(locks_left(&fx).await.is_empty());
}

#[derive(Clone, Copy, Debug)]
enum Stop {
    AfterVerify,
    AfterShards,
    HalfwayThroughPacks,
}

/// Pack and shard counts and pack bytes: names differ between runs because
/// every pack gets a new random key.
async fn shape(fx: &Fx) -> (usize, u64, usize) {
    (
        fx.packs().await.len(),
        fx.pack_bytes().await,
        fx.record_shards().await,
    )
}

async fn with_first_backup(base: &Fx, src: &Path) -> Fx {
    let fx = base.fork().await;
    let mut writer = fx.open_writer().await;
    fx.commit(&mut writer, src.to_path_buf()).await;
    drop(writer);
    fx
}

#[tokio::test]
async fn gc_interleave_sweep_resume() {
    let (base, srcs) = base().await;

    let whole = with_first_backup(&base, &srcs[0]).await;
    let mut cycle = Cycle::new(&whole);
    for step in GC {
        gc_step(&mut cycle, step).await;
    }
    let expected = shape(&whole).await;

    for stop in [
        Stop::AfterVerify,
        Stop::AfterShards,
        Stop::HalfwayThroughPacks,
    ] {
        let fx = with_first_backup(&base, &srcs[0]).await;
        let mut cycle = Cycle::new(&fx);
        cycle.survey(50).await;
        cycle.repack_publish().await;
        cycle.verify_compact().await;
        match stop {
            Stop::AfterVerify => {}
            Stop::AfterShards => {
                cycle.sweep_shards().await;
            }
            Stop::HalfwayThroughPacks => {
                cycle.sweep_shards().await;
                let half = (cycle.candidates.len() / 2).max(1);
                let out = cycle.sweep_packs(half).await;
                assert_eq!(out.deleted as usize, half);
            }
        }
        drop(cycle);
        fx.assert_all_restore().await;

        let mut resumed = Cycle::resume(&fx).await;
        for step in &GC[2..] {
            gc_step(&mut resumed, *step).await;
        }
        fx.assert_all_restore().await;
        assert!(locks_left(&fx).await.is_empty(), "{stop:?}");
        assert_eq!(shape(&fx).await, expected, "{stop:?}");
    }
}
