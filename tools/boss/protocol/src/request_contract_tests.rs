use super::*;
use crate::{CreateChoreInput, CreateInvestigationInput, CreateProjectInput, CreateTaskInput, ProposalKind};

fn contract(replay: ReplayContract, admission: AdmissionLane) -> RequestContract {
    RequestContract { replay, admission }
}

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

fn chore(force: bool) -> FrontendRequest {
    FrontendRequest::CreateChore {
        input: CreateChoreInput::builder()
            .product_id("prod_1")
            .name("a chore")
            .force_duplicate(force)
            .build(),
    }
}

fn investigation(force: bool) -> FrontendRequest {
    FrontendRequest::CreateInvestigation {
        input: CreateInvestigationInput::builder()
            .product_id("prod_1")
            .name("an investigation")
            .force_duplicate(force)
            .build(),
    }
}

/// The live lane's membership is an audit: each handler reads only local
/// process state, local SQL, or the local cube socket. Changing it requires
/// changing this list as well as the list in `request_contract`.
fn audited_live_requests() -> Vec<FrontendRequest> {
    use FrontendRequest as R;
    vec![
        R::GetEngineHealth,
        R::GetEnginePoolConfig,
        R::GetEngineVersion,
        R::ListHostedPaneStatuses,
        R::ListTmuxWorkerStatuses,
        R::ListWorkerLiveStates,
        R::MetricsListLive,
        R::MetricsShowLive { name: "n".into() },
        R::ProbeStatus { probe_id: "p".into() },
        R::WorkerPoolSummary,
        R::WorkspacePoolSummary,
    ]
}

#[test]
fn live_lane_is_exactly_the_audited_local_only_set() {
    let audited: std::collections::BTreeSet<_> = audited_live_requests()
        .iter()
        .map(|request| format!("{request:?}").split([' ', '{']).next().unwrap().to_owned())
        .collect();
    let classified: std::collections::BTreeSet<_> = LIVE_VARIANTS.iter().map(|name| name.to_string()).collect();
    assert_eq!(classified, audited);
    for request in audited_live_requests() {
        assert_eq!(
            request_contract(&request),
            contract(ReplayContract::Always, AdmissionLane::Live),
            "{request:?}"
        );
    }
    // Network-bound reads must stay out of the live lane.
    for request in [
        FrontendRequest::TrunkStatus,
        FrontendRequest::GitHubAuthStatus,
        FrontendRequest::ListCoordinatorGuidance { product_id: None },
    ] {
        assert_ne!(request_contract(&request).admission, AdmissionLane::Live, "{request:?}");
    }
}

#[test]
fn reads_are_replayable_and_use_the_bulk_lane() {
    for request in [
        FrontendRequest::ListProducts,
        FrontendRequest::TrunkStatus,
        FrontendRequest::GitHubAuthStatus,
        FrontendRequest::ListOperatorQuestions { id: "task".into() },
        FrontendRequest::ListProposals {
            run_id: "run".into(),
            kind: None,
            state: None,
        },
    ] {
        assert_eq!(
            request_contract(&request),
            contract(ReplayContract::Always, AdmissionLane::Bulk),
            "{request:?}"
        );
    }
}

#[test]
fn setters_and_keyed_writes_are_replayable_but_ordered() {
    let proposal = FrontendRequest::SubmitProposal {
        run_id: "exec_1".into(),
        kind: ProposalKind::RunDone,
        payload: serde_json::json!({}),
        idempotency_key: None,
    };
    let setter = FrontendRequest::SetDispatchPaused {
        paused: true,
        reason: None,
        scope: None,
    };
    for request in [proposal, setter] {
        assert_eq!(
            request_contract(&request),
            contract(ReplayContract::Always, AdmissionLane::Ordered),
            "{request:?}"
        );
    }
}

#[test]
fn handoff_redelivery_is_not_replayable() {
    assert_eq!(
        request_contract(&FrontendRequest::GetCoordinatorHandoff { redeliver: false }),
        contract(ReplayContract::Always, AdmissionLane::Bulk)
    );
    assert_eq!(
        request_contract(&FrontendRequest::GetCoordinatorHandoff { redeliver: true }),
        contract(ReplayContract::Never, AdmissionLane::Bulk)
    );
}

#[test]
fn pr_status_refresh_leaves_the_bulk_lane_but_stays_replayable() {
    let status = |refresh| FrontendRequest::GetPrStatus {
        run_id: "run".into(),
        refresh,
    };
    assert_eq!(
        request_contract(&status(false)),
        contract(ReplayContract::Always, AdmissionLane::Bulk)
    );
    assert_eq!(
        request_contract(&status(true)),
        contract(ReplayContract::Always, AdmissionLane::Ordered)
    );
}

#[test]
fn guarded_creates_depend_on_force_duplicate() {
    for build in [task, chore, investigation] {
        assert_eq!(
            request_contract(&build(false)),
            contract(ReplayContract::DuplicateGuarded, AdmissionLane::Ordered)
        );
        assert_eq!(
            request_contract(&build(true)),
            contract(ReplayContract::Never, AdmissionLane::Ordered)
        );
    }
}

#[test]
fn unguarded_writes_are_never_replayed_and_stay_ordered() {
    let project = FrontendRequest::CreateProject {
        input: CreateProjectInput::builder().product_id("prod_1").name("p").build(),
    };
    for request in [project, FrontendRequest::Shutdown { token: "t".into() }] {
        assert_eq!(
            request_contract(&request),
            contract(ReplayContract::Never, AdmissionLane::Ordered),
            "{request:?}"
        );
    }
}

#[test]
fn non_read_lane_requests_that_are_not_writes_keep_their_lane() {
    // Read-shaped requests that run on the ordered lane or are bulk but not
    // replayable: classified by what they do, not by their name.
    assert_eq!(
        request_contract(&FrontendRequest::ListCriticalPriorityRequests),
        contract(ReplayContract::Always, AdmissionLane::Ordered)
    );
    assert_eq!(
        request_contract(&FrontendRequest::EvaluateDispatchAdmission {
            work_item_id: "p".into()
        }),
        contract(ReplayContract::Never, AdmissionLane::Bulk)
    );
}
