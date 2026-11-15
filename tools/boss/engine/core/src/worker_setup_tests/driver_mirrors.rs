//! Driver-local mirrors of the engine's per-kind permission posture.
//!
//! The driver crate cannot depend on the engine, so it keeps its own copy of
//! `WorkerKind` and of `forced_permission_mode`. These tests are where the two
//! meet: if either side changes without the other, a restricted kind could be
//! silently downgraded to an allow-by-default posture.

use super::super::*;

/// Exhaustive on the engine's enum: a new engine kind fails to compile here
/// until the driver mirror decides how to treat it.
fn driver_kind(kind: &WorkerKind) -> crate::driver::WorkerKind {
    match kind {
        WorkerKind::Standard => crate::driver::WorkerKind::Standard,
        WorkerKind::Reviewer => crate::driver::WorkerKind::Reviewer,
        WorkerKind::Triage => crate::driver::WorkerKind::Triage,
        WorkerKind::AnswerAgent => crate::driver::WorkerKind::AnswerAgent,
        WorkerKind::ReviewGuide => crate::driver::WorkerKind::ReviewGuide,
        WorkerKind::Progression => crate::driver::WorkerKind::Progression,
    }
}

const ALL_KINDS: [WorkerKind; 6] = [
    WorkerKind::Standard,
    WorkerKind::Reviewer,
    WorkerKind::Triage,
    WorkerKind::AnswerAgent,
    WorkerKind::ReviewGuide,
    WorkerKind::Progression,
];

#[test]
fn agy_driver_mirrors_forced_permission_mode_for_every_kind() {
    use crate::driver::agy::permissions::{
        VerdictPosture, permission_mode_for_worker_kind, verdict_posture_for_worker_kind,
    };
    for kind in ALL_KINDS {
        let mirrored = driver_kind(&kind);
        assert_eq!(
            permission_mode_for_worker_kind(mirrored),
            kind.forced_permission_mode(),
            "{kind:?}: the agy driver's mirror of forced_permission_mode drifted"
        );
        // The deny-by-default kinds are exactly the ones answered with an
        // allowlist, so a restricted kind cannot render an allow-by-default
        // posture.
        assert_eq!(
            verdict_posture_for_worker_kind(mirrored) == VerdictPosture::GuardedAllowlist,
            kind.forced_permission_mode().is_some(),
            "{kind:?}"
        );
    }
}
