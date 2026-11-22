use super::*;

/// Every v1 kind must be submittable — a kind the engine accepts on the wire
/// but cannot store would be a dead verb the CLI still advertises.
#[tokio::test]
async fn every_kind_can_be_submitted() {
    let fx = WorkerFixture::new();
    for &kind in ProposalKind::ALL {
        let payload = match kind {
            ProposalKind::Attention => json!({"title": "T", "body_markdown": "B"}),
            ProposalKind::EffortEscalation => json!({"requested_level": "large", "reason": "R"}),
            ProposalKind::Blocked => json!({"reason": "R"}),
            ProposalKind::DeferredScope => json!({"summary": "S", "reason": "R"}),
            ProposalKind::FollowupTask => {
                json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"})
            }
            ProposalKind::AutomationOutcome => json!({"outcome": "skip", "reason": "clean"}),
            ProposalKind::PrCreated => json!({"pr_url": "https://github.com/o/r/pull/1"}),
            ProposalKind::ReviewGuide => json!({"body_markdown": "# Guide"}),
            ProposalKind::ReviewOutcomes => json!({"outcomes":[]}),
            ProposalKind::ReviewReport => {
                json!({
                    "batch_id": "rvb_missing",
                    "target_sha": "head_missing",
                    "report": {
                        "batch_id": "rvb_missing",
                        "pr_url": "https://github.com/o/r/pull/1",
                        "target_sha": "head_missing",
                        "phase": "pre_merge",
                        "summary": "Clean.",
                        "coverage": {"files_inspected": [], "files_omitted": [], "limitations": []},
                        "findings": [],
                    },
                })
            }
            ProposalKind::ReviewVerdict => json!({
                "batch_id": "rvb_missing",
                "verdict": {
                    "batch_id": "rvb_missing",
                    "pr_url": "https://github.com/o/r/pull/1",
                    "target_sha": "head_missing",
                    "phase": "pre_merge",
                    "summary": "Clean.",
                    "revision_warranted": false,
                    "findings": [],
                    "contradictions": [],
                },
            }),
            ProposalKind::RunDone => json!({"outcome": "delivered", "summary": "S"}),
            ProposalKind::Wait => json!({"reason": "bazel test", "duration_secs": 60}),
            ProposalKind::Revise => json!({"brief": "Fix it"}),
            ProposalKind::Precursor => json!({"name": "Fix it on main", "brief": "Stand-alone."}),
            ProposalKind::Depend | ProposalKind::Undepend => json!({"on": "task_x"}),
            ProposalKind::Redispatch | ProposalKind::Unblock => json!({}),
            ProposalKind::ClearSignal => json!({"reason": "merge_conflict"}),
            ProposalKind::ArchiveRevision => json!({"revision": "task_x", "reason": "dead"}),
        };
        let response = submit(&fx, kind, payload).await;
        if kind.is_progression_verb() {
            // The progression verbs are valid payloads the engine judges: a
            // plain worker run is not a progression run, so each is refused and
            // stores nothing, whether or not the worker tier is enforced.
            let error = rejected(response);
            assert_eq!(error.code, ProposalErrorCode::NotAccepted, "{kind}");
            assert!(
                error.message.contains("only a live progression run"),
                "{kind}: {}",
                error.message
            );
            continue;
        }
        if kind == ProposalKind::ReviewOutcomes {
            let error = rejected(response);
            assert_eq!(error.code, ProposalErrorCode::NotAccepted);
            assert!(
                error.message.contains("no assigned review finding ids"),
                "{}",
                error.message
            );
            continue;
        }
        if kind == ProposalKind::ReviewGuide {
            // The fixture is a plain chore run with no review-guide attempt,
            // and a guide is accepted atomically or refused without a row:
            // the kind reaches the engine and is judged, nothing is stored.
            let error = rejected(response);
            assert_eq!(error.code, ProposalErrorCode::NotAccepted);
            assert!(
                error.message.contains("no running review-guide attempt"),
                "{}",
                error.message
            );
            continue;
        }
        let (proposal, _) = submitted(response);
        assert_eq!(proposal.kind, kind);
    }
}
