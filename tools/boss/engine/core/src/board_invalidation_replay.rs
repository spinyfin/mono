//! Replay of the board-invalidation traffic a fleet of workers produces, to
//! count how many product-wide assemblies the runtime/structural split saves
//! (hub-and-spoke design §2.4).
//!
//! This is a model, not a measurement of a live engine, and the report says
//! so: the push schedule is the happy-path lifecycle of one execution, with
//! every push carrying the class the real emitter gives it today, and the
//! consumer is the macOS app's coalescer (150 ms quiet window, never more
//! than 1 s after a burst's first push; `ChatViewModel+InvalidationRefetch`).
//! What it counts is exact for that schedule: pushes by class, bursts, and
//! what each burst costs the hub before and after the split. The unit costs
//! (one full assembly, one patch of k cards) are measured separately by the
//! scale fixture in `work/tests/work_tree_scale_tests.rs`.
//!
//! Run and print the table with
//! `bazel test //tools/boss/engine/core:engine_lib_test --test_sharding_strategy=disabled
//! --test_filter=board_invalidation_replay --test_output=all --test_arg=--nocapture`.

use std::collections::BTreeSet;

use boss_protocol::InvalidationClass::{self, Runtime, Structural};

/// Claim-to-merge time of one execution on the happy path, in ms.
const LIFECYCLE_MS: u64 = 45 * 60 * 1000;
/// The merge poller's sweep period. Poll-state pushes only leave at a sweep.
const SWEEP_MS: u64 = 60 * 1000;
/// Time between two PRs' probes inside one sweep (a GitHub round trip each).
const PROBE_SPACING_MS: u64 = 400;
/// The app's coalescing policy.
const WINDOW_MS: u64 = 150;
const MAX_WAIT_MS: u64 = 1000;

/// One execution's pushes, as `(offset from claim in permille of the
/// lifecycle, class, reason)`. Classes are the emitters' own: the review-pass
/// and run-done sites stay structural because their writers move the card
/// between lanes (status transitions, revision creation) in the same flow.
const HAPPY_PATH: &[(u64, InvalidationClass, &str)] = &[
    (0, Runtime, "dispatch_claimed"),
    (1, Structural, "execution_started_auto_advance"),
    (550, Structural, "execution_run_completed"),
    (550, Structural, "worker_pr_completed"),
    (551, Structural, "worker_run_done_delivered"),
    (600, Runtime, "pr_poll_state_updated"),
    (650, Structural, "pr_review_pass_completed"),
    (651, Structural, "review_verdict_applied"),
    (700, Runtime, "pr_poll_state_updated"),
    (750, Runtime, "pr_poll_state_updated"),
    (800, Runtime, "pr_poll_state_updated"),
    (850, Runtime, "trunk_queue_state_updated"),
    (900, Runtime, "pr_poll_state_updated"),
    (920, Runtime, "trunk_queue_renumbered"),
    (950, Structural, "pr_merged"),
    (951, Structural, "worker_pr_merged"),
];

#[derive(Clone, Copy)]
struct Push {
    at_ms: u64,
    class: InvalidationClass,
    card: usize,
    /// Published by a merge-poller sweep, which probes PRs one after another.
    probed: bool,
}

/// What the app sends after one coalesced burst, and so what the hub does.
struct Burst {
    structural: bool,
    cards: BTreeSet<usize>,
}

/// Counts for one fleet size.
#[derive(Debug, PartialEq, Eq, bon::Builder)]
struct Replay {
    workers: usize,
    structural_pushes: usize,
    runtime_pushes: usize,
    /// Coalesced bursts: one full assembly each without the split.
    bursts: usize,
    /// Bursts holding a structural push: still a full assembly with it.
    full_assemblies: usize,
    /// Runtime-only bursts: a patch of the cards they named.
    patches: usize,
    patched_cards: usize,
}

/// Every push `workers` workers publish in one steady-state lifecycle period,
/// workers staggered evenly across it.
fn schedule(workers: usize, class_of: impl Fn(InvalidationClass) -> InvalidationClass) -> Vec<Push> {
    let mut pushes = Vec::new();
    for card in 0..workers {
        let start = card as u64 * LIFECYCLE_MS / workers as u64;
        for &(permille, class, reason) in HAPPY_PATH {
            let at_ms = (start + permille * LIFECYCLE_MS / 1000) % LIFECYCLE_MS;
            let probed = reason == "pr_poll_state_updated";
            pushes.push(Push {
                // Poll-state pushes leave at the next sweep tick.
                at_ms: if probed {
                    at_ms.div_ceil(SWEEP_MS) * SWEEP_MS
                } else {
                    at_ms
                },
                class: class_of(class),
                card,
                probed,
            });
        }
    }
    pushes.sort_by_key(|push| (push.at_ms, push.card));
    // Within one sweep tick the probes are sequential, not simultaneous.
    let mut rank = 0;
    let mut tick = None;
    for push in pushes.iter_mut().filter(|push| push.probed) {
        rank = if tick == Some(push.at_ms) { rank + 1 } else { 0 };
        tick = Some(push.at_ms);
        push.at_ms += rank * PROBE_SPACING_MS;
    }
    pushes.sort_by_key(|push| (push.at_ms, push.card));
    pushes
}

/// A burst whose timer has not fired yet.
struct Open {
    first_ms: u64,
    fires_at: u64,
    burst: Burst,
}

/// The app's coalescer: a push joins the pending burst if it arrives before
/// the burst's timer fires; the timer waits `WINDOW_MS` after the latest push
/// but never past `MAX_WAIT_MS` after the first.
fn coalesce(pushes: &[Push]) -> Vec<Burst> {
    let mut bursts = Vec::new();
    let mut open: Option<Open> = None;
    for push in pushes {
        if open.as_ref().is_some_and(|open| push.at_ms >= open.fires_at) {
            bursts.extend(open.take().map(|open| open.burst));
        }
        let open = open.get_or_insert_with(|| Open {
            first_ms: push.at_ms,
            fires_at: push.at_ms,
            burst: Burst {
                structural: false,
                cards: BTreeSet::new(),
            },
        });
        open.burst.structural |= push.class == Structural;
        open.burst.cards.insert(push.card);
        open.fires_at = push.at_ms + WINDOW_MS.min(MAX_WAIT_MS.saturating_sub(push.at_ms - open.first_ms));
    }
    bursts.extend(open.map(|open| open.burst));
    bursts
}

fn replay(workers: usize, class_of: impl Fn(InvalidationClass) -> InvalidationClass) -> Replay {
    let pushes = schedule(workers, class_of);
    let bursts = coalesce(&pushes);
    let full_assemblies = bursts.iter().filter(|burst| burst.structural).count();
    let patches: Vec<_> = bursts.iter().filter(|burst| !burst.structural).collect();
    Replay::builder()
        .workers(workers)
        .structural_pushes(pushes.iter().filter(|push| push.class == Structural).count())
        .runtime_pushes(pushes.iter().filter(|push| push.class == Runtime).count())
        .bursts(bursts.len())
        .full_assemblies(full_assemblies)
        .patches(patches.len())
        .patched_cards(patches.iter().map(|burst| burst.cards.len()).sum())
        .build()
}

#[test]
fn the_split_removes_full_assemblies_only_through_runtime_only_bursts() {
    println!(
        "| workers | pushes (structural / runtime) | bursts = full assemblies before | full assemblies after | patches after | cards per patch | full assemblies saved |"
    );
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for workers in [8, 32, 64] {
        let after = replay(workers, |class| class);
        // Control: the same schedule with every push structural is today's
        // behavior. Any saving must therefore come from the classes alone.
        let before = replay(workers, |_| Structural);

        let per_worker = HAPPY_PATH.len();
        assert_eq!(after.structural_pushes + after.runtime_pushes, workers * per_worker);
        assert_eq!(
            after.runtime_pushes,
            workers * HAPPY_PATH.iter().filter(|(_, class, _)| *class == Runtime).count()
        );
        assert_eq!(
            before.runtime_pushes, 0,
            "the control must classify every push structural"
        );
        assert_eq!(
            before.full_assemblies, before.bursts,
            "without the split every burst is a full assembly"
        );
        assert_eq!(after.bursts, before.bursts, "coalescing does not depend on class");
        assert_eq!(
            after.full_assemblies + after.patches,
            after.bursts,
            "every burst is exactly one full assembly or one patch"
        );
        assert!(
            after.full_assemblies < before.full_assemblies,
            "{workers} workers: the split must remove full assemblies ({after:?} vs {before:?})"
        );
        assert!(after.patches > 0 && after.patched_cards >= after.patches);

        let saved = before.full_assemblies - after.full_assemblies;
        println!(
            "| {workers} | {} / {} | {} | {} | {} | {:.2} | {saved} ({:.0}%) |",
            after.structural_pushes,
            after.runtime_pushes,
            before.full_assemblies,
            after.full_assemblies,
            after.patches,
            after.patched_cards as f64 / after.patches as f64,
            100.0 * saved as f64 / before.full_assemblies as f64,
        );
    }
}

#[test]
fn a_burst_that_mixes_classes_is_one_full_assembly_and_a_gap_starts_a_new_burst() {
    let push = |at_ms, class, card| Push {
        at_ms,
        class,
        card,
        probed: false,
    };
    let bursts = coalesce(&[
        push(0, Runtime, 1),
        push(100, Runtime, 2),
        push(200, Structural, 3), // inside the window of the previous push
        push(1000, Runtime, 4),   // 800 ms after the last: a new burst
    ]);
    assert_eq!(bursts.len(), 2);
    assert!(bursts[0].structural, "a structural push upgrades the whole burst");
    assert_eq!(bursts[0].cards, BTreeSet::from([1, 2, 3]));
    assert!(!bursts[1].structural);
    assert_eq!(bursts[1].cards, BTreeSet::from([4]));

    // A steady stream closer together than the window still fires at the cap.
    let stream: Vec<_> = (0..30).map(|i| push(i * 100, Runtime, i as usize)).collect();
    let bursts = coalesce(&stream);
    assert!(
        bursts.len() >= 3,
        "30 pushes over 3 s fire at least every second: {}",
        bursts.len()
    );
}
