//! The differential property of incremental image backups: a run that follows
//! `lode`'s rule gives the same tree as a full read of the same disk, however
//! the bitmap, the disk, the transport and the other machines behave.

use lodestore_format::image::{FullReason, ReadPlan};
use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, RngAlgorithm, TestRng, TestRunner};

use crate::image_model::{
    C, Cfg, Env, Failure, MAX_DISK, RunSpec, Tally, Transport, World, WriteOp, ZeroHint,
    initial_disk, other_machine, reference_full_read, restore_bytes, run_backup, same_bytes,
};

#[derive(Debug, Clone, Copy)]
enum Resize {
    Grow(u64),
    Shrink(u64),
    Wobble(u64),
}

#[derive(Debug, Clone)]
enum Step {
    Write(WriteOp),
    Resize(Resize),
    Backup(RunSpec),
    OtherMachine,
    StaleState(usize),
}

#[derive(Debug, Clone)]
struct Case {
    granule: u64,
    segment_chunks: u64,
    transport: Transport,
    fills: Vec<Option<u8>>,
    tail: u64,
    steps: Vec<Step>,
}

fn write_op() -> impl Strategy<Value = WriteOp> {
    let len = prop_oneof![8 => 1u64..=(256 << 10), 1 => 1u64..=(6 << 20)];
    let fill = prop_oneof![3 => any::<u8>().prop_map(Some), 1 => Just(None)];
    (any::<u64>(), len, fill).prop_map(|(offset, len, fill)| WriteOp { offset, len, fill })
}

fn failure() -> impl Strategy<Value = Failure> {
    prop_oneof![
        24 => Just(Failure::None),
        1 => (1u64..=3).prop_map(Failure::CancelAfterReads),
        1 => (0u64..=3).prop_map(Failure::SourceFailsAfterReads),
        1 => Just(Failure::ParentFails),
        1 => Just(Failure::Rejected),
    ]
}

fn run_spec() -> impl Strategy<Value = RunSpec> {
    (
        failure(),
        proptest::option::weighted(0.4, write_op()),
        proptest::bool::weighted(0.96),
        proptest::bool::weighted(0.08),
        proptest::bool::weighted(0.07),
        proptest::bool::weighted(0.07),
    )
        .prop_map(
            |(failure, window, export_bitmap, late_next, skip_rotation, time_back)| RunSpec {
                failure,
                window,
                export_bitmap,
                late_next,
                skip_rotation,
                time_back,
            },
        )
}

fn resize() -> impl Strategy<Value = Resize> {
    let delta = (1u64..=16_384).prop_map(|units| units * 512);
    prop_oneof![
        1 => delta.clone().prop_map(Resize::Grow),
        1 => delta.clone().prop_map(Resize::Shrink),
        2 => delta.prop_map(Resize::Wobble),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        5 => write_op().prop_map(Step::Write),
        9 => run_spec().prop_map(Step::Backup),
        1 => resize().prop_map(Step::Resize),
        1 => Just(Step::OtherMachine),
        1 => (1usize..=3).prop_map(Step::StaleState),
    ]
}

fn case_strategy() -> impl Strategy<Value = Case> {
    let granule = prop_oneof![
        2 => proptest::sample::select(vec![512u64, 4 << 10, 64 << 10, 1 << 20]),
        3 => Just(C),
        2 => proptest::sample::select(vec![8u64 << 20, 64 << 20]),
    ];
    let transport = prop_oneof![
        2 => Just(Transport::Direct(ZeroHint::Always)),
        1 => Just(Transport::Direct(ZeroHint::Never)),
        1 => Just(Transport::Direct(ZeroHint::Alternate)),
        4 => Just(Transport::Nbd),
    ];
    (
        granule,
        1u64..=3,
        transport,
        proptest::collection::vec(
            prop_oneof![3 => any::<u8>().prop_map(Some), 1 => Just(None)],
            1..=5,
        ),
        (0u64..=4096).prop_map(|units| units * 512),
        run_spec(),
        proptest::collection::vec(step(), 3..=9),
        run_spec(),
    )
        .prop_map(
            |(granule, segment_chunks, transport, fills, tail, first, mut steps, last)| {
                steps.insert(0, Step::Backup(first));
                steps.push(Step::Backup(last));
                Case {
                    granule,
                    segment_chunks,
                    transport,
                    fills,
                    tail,
                    steps,
                }
            },
        )
}

/// What the previous steps say about the next run.
#[derive(Default)]
struct Expect {
    next_full: bool,
    next_incremental: bool,
    poison: u8,
    previous_failed: bool,
    previous_window: bool,
    resized_since: bool,
    last_size: Option<u64>,
}

fn resized(w: &mut World, ex: &mut Expect, tally: &Tally, how: Resize) {
    let size = w.disk.len() as u64;
    let shrunk = |delta: u64| size.saturating_sub(delta).max(512);
    match how {
        Resize::Grow(delta) => {
            w.resize((size + delta).min(MAX_DISK));
            Tally::add(&tally.resizes, 1);
        }
        Resize::Shrink(delta) => {
            w.resize(shrunk(delta));
            Tally::add(&tally.resizes, 1);
        }
        Resize::Wobble(delta) => {
            w.resize(shrunk(delta));
            w.resize(size);
            Tally::add(&tally.resizes, 2);
        }
    }
    ex.next_full = true;
    ex.next_incremental = false;
    ex.resized_since = true;
}

async fn run_case(case: &Case, tally: &Tally) -> Result<(), TestCaseError> {
    let env = Env::new().await;
    let cfg = Cfg {
        segment_chunks: case.segment_chunks,
        transport: case.transport,
    };
    let mut w = World::new(initial_disk(&case.fills, case.tail), case.granule);
    let mut ex = Expect::default();
    let mut had_incremental = false;
    if matches!(case.transport, Transport::Nbd) {
        Tally::add(&tally.nbd_cases, 1);
    }
    for step in &case.steps {
        match step {
            Step::Write(op) => w.write(op),
            Step::Resize(how) => resized(&mut w, &mut ex, tally, *how),
            Step::OtherMachine => {
                other_machine(&env, &cfg, &mut w).await;
                ex.next_full = true;
                ex.next_incremental = false;
            }
            Step::StaleState(back) => {
                if w.stale_state(*back) {
                    ex.next_full = true;
                    ex.next_incremental = false;
                }
            }
            Step::Backup(spec) => {
                let out = run_backup(&env, &cfg, &mut w, spec).await;
                Tally::add(&tally.backups, 1);
                let size = out.frozen.len() as u64;
                let incremental = matches!(out.plan, ReadPlan::Incremental { .. });

                let unbound_increment = incremental && !out.record_bound;
                if matches!(spec.failure, Failure::None | Failure::Rejected) && !unbound_increment {
                    prop_assert!(out.error.is_none(), "the run failed: {:?}", out.error);
                }
                if ex.next_full {
                    prop_assert!(
                        !incremental,
                        "an incremental run after an event that forbids it"
                    );
                }
                if ex.next_incremental && spec.export_bitmap && ex.poison == 0 {
                    prop_assert!(
                        incremental,
                        "a clean chain was not continued incrementally: {:?}",
                        out.plan
                    );
                }
                if incremental {
                    prop_assert!(
                        out.outside_reads == 0,
                        "an incremental run read {} times outside the dirty ranges",
                        out.outside_reads
                    );
                    prop_assert!(
                        out.source_bytes <= out.dirty_bytes,
                        "read {} bytes, only {} are dirty",
                        out.source_bytes,
                        out.dirty_bytes
                    );
                    prop_assert!(
                        out.parent_reads <= out.partial_chunks,
                        "read {} parent chunks for {} partly dirty chunks",
                        out.parent_reads,
                        out.partial_chunks
                    );
                }
                if let ReadPlan::Full(reason) = out.plan {
                    match reason {
                        FullReason::ParentNotNewest => Tally::add(&tally.parent_not_newest, 1),
                        FullReason::GenMismatch => Tally::add(&tally.gen_mismatch, 1),
                        FullReason::BitmapMissing => Tally::add(&tally.bitmap_missing, 1),
                        _ => {}
                    }
                }

                if let (Some(snapshot), Some(root_tree)) = (out.snapshot, out.root_tree) {
                    let reference =
                        reference_full_read(&env, &out.frozen, out.time, case.segment_chunks).await;
                    prop_assert!(
                        root_tree == reference,
                        "the tree differs from a full read, plan {:?}",
                        out.plan
                    );
                    let restored = restore_bytes(&env, &snapshot).await;
                    if let Err(why) = same_bytes(&restored, &out.frozen) {
                        prop_assert!(false, "the restore differs from the disk: {why}");
                    }
                    if incremental {
                        had_incremental = true;
                        Tally::add(&tally.incrementals, 1);
                        Tally::add(&tally.merged_chunks, out.partial_chunks);
                        if ex.previous_window {
                            Tally::add(&tally.window_in_increment, 1);
                        }
                    } else if ex.previous_failed {
                        Tally::add(&tally.full_after_failure, 1);
                    }
                    if size.div_ceil(C) > case.segment_chunks {
                        Tally::add(&tally.multi_segment, 1);
                    }
                    if ex.resized_since && ex.last_size == Some(size) {
                        Tally::add(&tally.same_size_after_resize, 1);
                    }
                }
                if spec.late_next && !out.record_bound {
                    Tally::add(&tally.late_next_unbound, 1);
                }

                ex.previous_failed = out.snapshot.is_none() || out.rejected;
                ex.previous_window = out.window_applied && out.rotated;
                ex.resized_since = false;
                ex.last_size = Some(size);
                if spec.skip_rotation && out.snapshot.is_some() && !out.rejected {
                    ex.poison = 2;
                } else if out.rotated {
                    ex.poison = ex.poison.saturating_sub(1);
                }
                ex.next_full = !out.clean && !spec.time_back;
                ex.next_incremental = out.clean && ex.poison == 0;
            }
        }
    }
    if had_incremental {
        let group = if case.granule < C {
            &tally.below_chunk
        } else if case.granule == C {
            &tally.equal_chunk
        } else {
            &tally.above_chunk
        };
        Tally::add(group, 1);
    }
    Ok(())
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16)
}

#[test]
/// An incremental run equals a full read of the same disk; the runs after an
/// event that breaks the chain are full and equal as well.
fn image_props_incremental_equals_full_read() {
    let tally = Tally::default();
    let cases = cases();
    let mut runner = TestRunner::new_with_rng(
        Config {
            failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
                "tests/proptest-regressions/image_props.txt",
            ))),
            ..Config::with_cases(cases)
        },
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    runner
        .run(&case_strategy(), |case| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_case(&case, &tally))
        })
        .unwrap_or_else(|e| panic!("{e}"));
    println!(
        "image_props counters: backups={} incrementals={} merged_chunks={} below={} equal={} above={} multi_segment={} nbd_cases={}",
        tally.backups.get(),
        tally.incrementals.get(),
        tally.merged_chunks.get(),
        tally.below_chunk.get(),
        tally.equal_chunk.get(),
        tally.above_chunk.get(),
        tally.multi_segment.get(),
        tally.nbd_cases.get(),
    );
    println!(
        "image_props events: full_after_failure={} resizes={} same_size_after_resize={} window_in_increment={} late_next_unbound={} parent_not_newest={} gen_mismatch={} bitmap_missing={}",
        tally.full_after_failure.get(),
        tally.resizes.get(),
        tally.same_size_after_resize.get(),
        tally.window_in_increment.get(),
        tally.late_next_unbound.get(),
        tally.parent_not_newest.get(),
        tally.gen_mismatch.get(),
        tally.bitmap_missing.get(),
    );
    if cases >= 16 {
        assert!(
            tally.incrementals.get() >= 24,
            "too few cases ran an incremental backup"
        );
        assert!(
            tally.merged_chunks.get() >= 8,
            "too few cases merged a partly dirty chunk"
        );
        assert!(
            tally.below_chunk.get() >= 2,
            "too few cases with a bitmap finer than a chunk"
        );
        assert!(
            tally.equal_chunk.get() >= 2,
            "too few cases with a bitmap as coarse as a chunk"
        );
        assert!(
            tally.above_chunk.get() >= 2,
            "too few cases with a bitmap coarser than a chunk"
        );
        assert!(
            tally.multi_segment.get() >= 4,
            "too few images of more than one segment"
        );
        assert!(tally.nbd_cases.get() >= 4, "too few cases through NBD");
        assert!(
            tally.full_after_failure.get() >= 4,
            "too few full runs after a failure"
        );
        assert!(tally.resizes.get() >= 4, "too few resizes");
        assert!(
            tally.same_size_after_resize.get() >= 1,
            "no resize came back to the same size"
        );
        assert!(
            tally.window_in_increment.get() >= 4,
            "too few writes in the window reached an increment"
        );
        assert!(
            tally.late_next_unbound.get() >= 2,
            "too few runs without a binding"
        );
        assert!(
            tally.parent_not_newest.get() >= 1,
            "no run saw parent_not_newest"
        );
        assert!(tally.gen_mismatch.get() >= 1, "no run saw gen_mismatch");
        assert!(tally.bitmap_missing.get() >= 1, "no run saw bitmap_missing");
    }
}
