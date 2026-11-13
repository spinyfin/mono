//! Which requests may be sent a second time after the connection dropped.
//!
//! A connect-phase failure means nothing was sent, so *every* request is
//! safe to retry then. A failed write is the same only when zero bytes
//! reached the socket; a partial write or a failed flush may have delivered
//! the request, and is treated like a drop after send. The hard case is a
//! drop **after** (or possibly after) the request was written: the engine may have applied it before dying, and sending it
//! again could apply it twice. [`replay_safety`] decides, per request, whether
//! that second send is safe. The classification is an exhaustive `match`
//! with no catch-all in the protocol's request contract, so a new
//! `FrontendRequest` variant does not compile until someone deliberately
//! classifies it there; a variant a reviewer cannot show to be harmless to
//! repeat belongs under [`ReplaySafety::Never`] (“outcome unknown, check
//! state”).
//!
//! ## The classes
//!
//! The per-request classification lives in
//! [`boss_protocol::request_contract`], next to the engine's admission lanes,
//! so both answers are given once per request. This module turns its
//! [`ReplayContract`] into a window:
//!
//! * **Always** — a second send is harmless: reads, declarative setters,
//!   documented idempotent writes, and writes keyed by an engine-side
//!   idempotency key (`SubmitProposal`, `SubmitAttachment`).
//! * **Within(window)** — protected by the engine's duplicate-create guard,
//!   which refuses a same-named task/chore/investigation in the same product
//!   created in the last [`DUPLICATE_GUARD_WINDOW`]. A replay inside the
//!   window is refused instead of creating a twin, and the client turns that
//!   refusal into the existing item (see `BossClient::send_request`). The
//!   guard only looks back that far, so a replay after a long outage could
//!   miss an original that *was* created; the client therefore only replays
//!   while the original send is still inside the window (minus a safety
//!   margin), re-checks that at the moment of each resend, caps the reconnect
//!   wait to the remaining window, and otherwise reports “outcome unknown”.
//!   Applies to `CreateTask`, `CreateChore` and `CreateInvestigation`, and
//!   only when `force_duplicate` is false — with it set the guard is off.
//! * **Never** — everything else. A drop after send surfaces as
//!   [`OutcomeUnknown`](crate::OutcomeUnknown) and is never retried.

use std::time::Duration;

use boss_protocol::{FrontendRequest, ReplayContract, request_contract};

/// Mirror of the engine's `DUPLICATE_GUARD_WINDOW_SECS`
/// (`engine/core/src/work.rs`). Duplicated rather than imported so the CLI
/// does not depend on the engine crate; the engine constant is the source of
/// truth and this one is deliberately conservative to stay valid if it grows.
pub const DUPLICATE_GUARD_WINDOW: Duration = Duration::from_secs(60);
/// Replay a guarded create only while at least this fraction (1/N) of the
/// guard window remains, so clock skew and the reconnect itself cannot push
/// the replay past the guard's horizon (10s of the default 60s).
const DUPLICATE_GUARD_MARGIN_DIVISOR: u32 = 6;

/// Whether a request that was already sent may be sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySafety {
    /// Safe to resend at any time.
    Always,
    /// Safe to resend only if no more than this long has passed since the
    /// original send (a bounded engine-side duplicate guard).
    Within(Duration),
    /// Never resend; report the outcome as unknown.
    Never,
}

impl ReplaySafety {
    /// May the request be resent when `since_first_send` has elapsed since it
    /// was first written to the engine?
    pub fn allows_replay_after(self, since_first_send: Duration) -> bool {
        match self {
            Self::Always => true,
            Self::Within(window) => since_first_send < window,
            Self::Never => false,
        }
    }
}

/// The wire name of a request (its serde `type` tag, e.g. `submit_proposal`).
pub fn request_name(request: &FrontendRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("type").and_then(|tag| tag.as_str().map(str::to_owned)))
        .unwrap_or_else(|| "<unknown request>".to_owned())
}

/// Classify `request`; see [`boss_protocol::request_contract`] for the full table.
pub fn replay_safety(request: &FrontendRequest) -> ReplaySafety {
    replay_safety_with(request, DUPLICATE_GUARD_WINDOW)
}

/// [`replay_safety`] for an engine whose duplicate guard looks back
/// `guard_window` (injectable so tests can shrink it).
pub fn replay_safety_with(request: &FrontendRequest, guard_window: Duration) -> ReplaySafety {
    match request_contract(request).replay {
        ReplayContract::Always => ReplaySafety::Always,
        ReplayContract::DuplicateGuarded => {
            ReplaySafety::Within(guard_window - guard_window / DUPLICATE_GUARD_MARGIN_DIVISOR)
        }
        ReplayContract::Never => ReplaySafety::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{CreateChoreInput, CreateProjectInput, CreateTaskInput, ProposalKind};

    fn task(force: bool) -> FrontendRequest {
        FrontendRequest::CreateTask {
            input: CreateTaskInput::builder()
                .product_id("prod_1")
                .project_id("proj_1")
                .name("a task")
                .force_duplicate(force)
                .build(),
        }
    }

    #[test]
    fn handoff_redelivery_is_not_automatically_replayed() {
        assert_eq!(
            replay_safety(&FrontendRequest::GetCoordinatorHandoff { redeliver: false }),
            ReplaySafety::Always
        );
        assert_eq!(
            replay_safety(&FrontendRequest::GetCoordinatorHandoff { redeliver: true }),
            ReplaySafety::Never
        );
    }

    #[test]
    fn reads_and_setters_are_always_replayable() {
        assert_eq!(replay_safety(&FrontendRequest::GetEngineVersion), ReplaySafety::Always);
        assert_eq!(replay_safety(&FrontendRequest::ListProducts), ReplaySafety::Always);
        assert_eq!(
            replay_safety(&FrontendRequest::SetDispatchPaused {
                paused: true,
                reason: None,
                scope: None,
            }),
            ReplaySafety::Always
        );
    }

    #[test]
    fn proposals_are_keyed_by_execution_and_replayable() {
        let request = FrontendRequest::SubmitProposal {
            run_id: "exec_1".into(),
            kind: ProposalKind::RunDone,
            payload: serde_json::json!({}),
            idempotency_key: None,
        };
        assert_eq!(replay_safety(&request), ReplaySafety::Always);
    }

    #[test]
    fn guarded_creates_are_replayable_only_inside_the_guard_window() {
        let safety = replay_safety(&task(false));
        assert!(matches!(safety, ReplaySafety::Within(window) if window < DUPLICATE_GUARD_WINDOW));
        assert!(safety.allows_replay_after(Duration::from_secs(5)));
        assert!(!safety.allows_replay_after(Duration::from_secs(55)));
        assert!(!safety.allows_replay_after(Duration::from_secs(300)));
    }

    #[test]
    fn force_duplicate_turns_the_guard_off() {
        assert_eq!(replay_safety(&task(true)), ReplaySafety::Never);
        let chore = FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id("prod_1")
                .name("c")
                .force_duplicate(true)
                .build(),
        };
        assert_eq!(replay_safety(&chore), ReplaySafety::Never);
    }

    #[test]
    fn unguarded_writes_and_unknown_verbs_are_never_replayed() {
        let project = FrontendRequest::CreateProject {
            input: CreateProjectInput::builder().product_id("prod_1").name("p").build(),
        };
        assert_eq!(replay_safety(&project), ReplaySafety::Never);
        assert_eq!(
            replay_safety(&FrontendRequest::Shutdown { token: "t".into() }),
            ReplaySafety::Never
        );
        assert!(!ReplaySafety::Never.allows_replay_after(Duration::ZERO));
    }

    #[test]
    fn request_name_is_the_wire_tag() {
        assert_eq!(request_name(&FrontendRequest::ListProducts), "list_products");
    }
}
