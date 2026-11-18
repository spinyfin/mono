use super::*;
use crate::{
    CreateChoreInput, CreateInvestigationInput, CreateProjectInput, CreateTaskInput, ProposalKind, WorkItemPatch,
};

fn replay_admission(request: &FrontendRequest) -> (ReplayContract, AdmissionLane) {
    let contract = request_contract(request);
    (contract.replay, contract.admission)
}

fn worker_progression(request: &FrontendRequest) -> (WorkerAccess, ProgressionAccess) {
    let contract = request_contract(request);
    (contract.worker, contract.progression)
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
    let classified: std::collections::BTreeSet<_> = live_variants().map(|name| name.to_string()).collect();
    assert_eq!(classified, audited);
    for request in audited_live_requests() {
        assert_eq!(
            replay_admission(&request),
            (ReplayContract::Always, AdmissionLane::Live),
            "{request:?}"
        );
    }
    // Non-audited reads and writes must not land on Live. Covering bulk,
    // ordered, and payload-dependent arms catches a stray Live assignment
    // outside the macro list.
    for request in [
        FrontendRequest::TrunkStatus,
        FrontendRequest::GitHubAuthStatus,
        FrontendRequest::ListCoordinatorGuidance { product_id: None },
        FrontendRequest::ListProducts,
        FrontendRequest::GetSettings,
        FrontendRequest::GetPrStatus {
            run_id: "run".into(),
            refresh: false,
        },
        FrontendRequest::GetPrStatus {
            run_id: "run".into(),
            refresh: true,
        },
        FrontendRequest::GetCoordinatorHandoff { redeliver: false },
        FrontendRequest::SetDispatchPaused {
            paused: true,
            reason: None,
            scope: None,
        },
        FrontendRequest::Shutdown { token: "t".into() },
        task(false),
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
            replay_admission(&request),
            (ReplayContract::Always, AdmissionLane::Bulk),
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
            replay_admission(&request),
            (ReplayContract::Always, AdmissionLane::Ordered),
            "{request:?}"
        );
    }
}

#[test]
fn handoff_redelivery_is_not_replayable() {
    let handoff = |redeliver| FrontendRequest::GetCoordinatorHandoff { redeliver };
    assert_eq!(
        replay_admission(&handoff(false)),
        (ReplayContract::Always, AdmissionLane::Bulk)
    );
    assert_eq!(
        replay_admission(&handoff(true)),
        (ReplayContract::Never, AdmissionLane::Bulk)
    );
    for redeliver in [false, true] {
        assert_eq!(
            worker_progression(&handoff(redeliver)),
            (WorkerAccess::Coordinator, ProgressionAccess::Deny),
            "redeliver={redeliver}"
        );
    }
}

#[test]
fn pr_status_refresh_leaves_the_bulk_lane_but_stays_replayable() {
    let status = |refresh| FrontendRequest::GetPrStatus {
        run_id: "run".into(),
        refresh,
    };
    assert_eq!(
        replay_admission(&status(false)),
        (ReplayContract::Always, AdmissionLane::Bulk)
    );
    assert_eq!(
        replay_admission(&status(true)),
        (ReplayContract::Always, AdmissionLane::Ordered)
    );
    for refresh in [false, true] {
        assert_eq!(
            worker_progression(&status(refresh)),
            (WorkerAccess::Allow, ProgressionAccess::Allow),
            "refresh={refresh}"
        );
    }
}

#[test]
fn guarded_creates_depend_on_force_duplicate() {
    for build in [task, chore, investigation] {
        assert_eq!(
            replay_admission(&build(false)),
            (ReplayContract::DuplicateGuarded, AdmissionLane::Ordered)
        );
        assert_eq!(
            replay_admission(&build(true)),
            (ReplayContract::Never, AdmissionLane::Ordered)
        );
        for force in [false, true] {
            assert_eq!(
                worker_progression(&build(force)),
                (
                    WorkerAccess::Redirect(WorkerRedirect::FollowupTask),
                    ProgressionAccess::Deny
                ),
            );
        }
    }
}

#[test]
fn unguarded_writes_are_never_replayed_and_stay_ordered() {
    let project = FrontendRequest::CreateProject {
        input: CreateProjectInput::builder().product_id("prod_1").name("p").build(),
    };
    for request in [project, FrontendRequest::Shutdown { token: "t".into() }] {
        assert_eq!(
            replay_admission(&request),
            (ReplayContract::Never, AdmissionLane::Ordered),
            "{request:?}"
        );
    }
}

#[test]
fn non_read_lane_requests_that_are_not_writes_keep_their_lane() {
    // Read-shaped requests that run on the ordered lane or are bulk but not
    // replayable: classified by what they do, not by their name.
    assert_eq!(
        replay_admission(&FrontendRequest::ListCriticalPriorityRequests),
        (ReplayContract::Always, AdmissionLane::Ordered)
    );
    assert_eq!(
        replay_admission(&FrontendRequest::EvaluateDispatchAdmission {
            work_item_id: "p".into()
        }),
        (ReplayContract::Never, AdmissionLane::Bulk)
    );
}

#[test]
fn worker_allow_does_not_imply_progression_allow() {
    assert_eq!(
        worker_progression(&FrontendRequest::ListProducts),
        (WorkerAccess::Allow, ProgressionAccess::Allow)
    );
    assert_eq!(
        worker_progression(&FrontendRequest::GetEngineVersion),
        (WorkerAccess::Allow, ProgressionAccess::Allow)
    );
    assert_eq!(
        worker_progression(&FrontendRequest::GetEngineHealth),
        (WorkerAccess::Runtime, ProgressionAccess::Deny)
    );
    assert_eq!(
        worker_progression(&FrontendRequest::CreateAutomationTask {
            automation_id: "auto_1".into(),
            name: "Fix".into(),
            description: None,
            target_files: vec![],
            target_symbols: vec![],
        }),
        (WorkerAccess::Allow, ProgressionAccess::Deny)
    );
}

#[test]
fn update_work_item_redirect_follows_the_patch() {
    let request = |patch: WorkItemPatch| FrontendRequest::UpdateWorkItem {
        id: "task_1".into(),
        patch: Box::new(patch),
    };
    assert_eq!(
        request_contract(&request(WorkItemPatch::default())).worker,
        WorkerAccess::Redirect(WorkerRedirect::General)
    );
    assert_eq!(
        request_contract(&request(WorkItemPatch {
            pr_url: Some("https://example/pr/1".into()),
            ..WorkItemPatch::default()
        }))
        .worker,
        WorkerAccess::Redirect(WorkerRedirect::PrCreated)
    );
    assert_eq!(
        request_contract(&request(WorkItemPatch {
            effort_level: Some("large".into()),
            ..WorkItemPatch::default()
        }))
        .worker,
        WorkerAccess::Redirect(WorkerRedirect::EffortEscalation)
    );
    assert_eq!(
        worker_progression(&request(WorkItemPatch::default())),
        (WorkerAccess::Redirect(WorkerRedirect::General), ProgressionAccess::Deny)
    );
}
