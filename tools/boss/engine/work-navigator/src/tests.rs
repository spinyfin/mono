use super::*;

const STATUSES: [TaskStatus; 6] = [
    TaskStatus::Todo,
    TaskStatus::Active,
    TaskStatus::Blocked,
    TaskStatus::InReview,
    TaskStatus::Done,
    TaskStatus::Archived,
];
const EXECUTIONS: [ExecutionStatus; 13] = [
    ExecutionStatus::Queued,
    ExecutionStatus::Ready,
    ExecutionStatus::WaitingDependency,
    ExecutionStatus::Claimed,
    ExecutionStatus::Running,
    ExecutionStatus::WaitingHuman,
    ExecutionStatus::WaitingReview,
    ExecutionStatus::WaitingMerge,
    ExecutionStatus::Completed,
    ExecutionStatus::Failed,
    ExecutionStatus::Abandoned,
    ExecutionStatus::Cancelled,
    ExecutionStatus::Orphaned,
];
const RUNS: [RunState; 5] = [
    RunState::Running,
    RunState::WaitingInput,
    RunState::Suspended,
    RunState::Exited,
    RunState::Unavailable,
];
const KINDS: [ActivityKind; 5] = [
    ActivityKind::Implementing,
    ActivityKind::Revising,
    ActivityKind::FixingCi,
    ActivityKind::ResolvingConflict,
    ActivityKind::Reviewing,
];
const HUMAN: [HumanAction; 5] = [
    HumanAction::Question,
    HumanAction::Input,
    HumanAction::Intervention,
    HumanAction::Review,
    HumanAction::Merge,
];
const ATTENTION_STATES: [AttentionState; 6] = [
    AttentionState::Open,
    AttentionState::PartlyAnswered,
    AttentionState::Answered,
    AttentionState::Actioned,
    AttentionState::Dismissed,
    AttentionState::Cleared,
];

fn member(id: &str) -> Member {
    Member {
        id: id.into(),
        status: TaskStatus::Todo,
        activities: vec![],
        attentions: vec![],
        wait: None,
    }
}
fn family() -> Family {
    Family {
        root: member("root"),
        revisions: vec![],
        commands: vec![],
        pr: None,
        ordering: Ordering {
            priority: 1,
            project_ordinal: None,
            created_at: 100,
        },
    }
}
fn activity(id: &str, kind: ActivityKind) -> Activity {
    Activity {
        reference: ActivityRef {
            task_id: id.into(),
            execution_id: format!("exec-{id}"),
            run_id: Some(format!("run-{id}")),
        },
        kind,
        runtime: Runtime {
            execution: ExecutionStatus::Running,
            run: RunState::Running,
            current: true,
        },
        started_at: 200,
        created_at: 200,
    }
}
fn attention(kind: HumanAction) -> Attention {
    Attention {
        id: "attention".into(),
        kind,
        state: AttentionState::Open,
        actionable_since: 150,
    }
}
fn pr() -> PullRequest {
    PullRequest {
        binding: "repo/pull/1".into(),
        evidence: None,
        health: PrHealth {
            ci: Ci::Success,
            review: Review::Approved,
            ai_review: AiReview::AllClear,
            conflict: Conflict::None,
        },
        merge_flow: MergeFlow::None,
    }
}
fn merged() -> PullRequest {
    let mut p = pr();
    p.evidence = Some(MergeEvidence {
        binding: p.binding.clone(),
        merged_at: Some(300),
        observed_at: 400,
    });
    p
}
fn projection(f: &Family) -> Projection {
    classify(f).expect("included root has one placement")
}
fn section(f: &Family) -> Section {
    projection(f).placement.section
}
fn reason(f: &Family) -> Reason {
    projection(f).placement.reason
}

#[test]
fn every_task_status_has_one_default_placement_except_archived() {
    let expected = [
        Wait::Queued,
        Wait::ActivityUnavailable,
        Wait::WaitingDependency,
        Wait::Diagnostic("PR observation unavailable".into()),
        Wait::CompletedWithoutPr,
    ];
    for (status, wait) in STATUSES.into_iter().zip(expected) {
        let mut f = family();
        f.root.status = status;
        assert_eq!(section(&f), Section::Backlog);
        assert_eq!(reason(&f), Reason::Waiting(wait));
        assert!(projection(&f).actions.is_empty());
    }
    let mut f = family();
    f.root.status = TaskStatus::Archived;
    f.pr = Some(merged());
    f.root.activities.push(activity("root", ActivityKind::Implementing));
    assert_eq!(classify(&f), None);
}

#[test]
fn precedence_cross_product_covers_root_revision_and_runtime_states() {
    let mut cases = 0;
    for status in STATUSES {
        for execution in EXECUTIONS {
            for run in RUNS {
                for current in [false, true] {
                    for has_question in [false, true] {
                        for has_merge in [false, true] {
                            for in_revision in [false, true] {
                                let mut f = family();
                                f.root.status = status;
                                let mut a =
                                    activity(if in_revision { "revision" } else { "root" }, ActivityKind::Revising);
                                a.runtime = Runtime {
                                    execution,
                                    run,
                                    current,
                                };
                                if in_revision {
                                    let mut m = member("revision");
                                    m.activities.push(a);
                                    f.revisions.push(m);
                                } else {
                                    f.root.activities.push(a);
                                }
                                if has_question {
                                    f.root.attentions.push(attention(HumanAction::Question));
                                }
                                if has_merge {
                                    f.pr = Some(merged());
                                }
                                let expected = if status == TaskStatus::Archived {
                                    None
                                } else if has_merge {
                                    Some(Section::Landed)
                                } else if current && execution == ExecutionStatus::Running && run == RunState::Running {
                                    Some(Section::Working)
                                } else if has_question
                                    || (current
                                        && matches!(
                                            execution,
                                            ExecutionStatus::Running | ExecutionStatus::WaitingHuman
                                        )
                                        && !matches!(run, RunState::Exited | RunState::Suspended)
                                        && (execution == ExecutionStatus::WaitingHuman
                                            || run == RunState::WaitingInput))
                                {
                                    Some(Section::NeedsAttention)
                                } else {
                                    Some(Section::Backlog)
                                };
                                assert_eq!(
                                    classify(&f).map(|p| p.placement.section),
                                    expected,
                                    "{status:?} {execution:?} {run:?} current={current} question={has_question} merged={has_merge} revision={in_revision}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 6240);
}

#[test]
fn execution_status_reasons_do_not_confuse_eligibility_with_activity() {
    let waits = [
        Wait::Queued,
        Wait::Ready,
        Wait::WaitingDependency,
        Wait::Starting,
        Wait::ActivityUnavailable,
        Wait::ActivityUnavailable,
        Wait::WaitingReview,
        Wait::Merging,
        Wait::Queued,
        Wait::Failed,
        Wait::Interrupted,
        Wait::Interrupted,
        Wait::Interrupted,
    ];
    for (execution, wait) in EXECUTIONS.into_iter().zip(waits) {
        let mut f = family();
        let mut a = activity("root", ActivityKind::Implementing);
        a.runtime.execution = execution;
        a.runtime.run = RunState::Exited;
        f.root.activities.push(a);
        assert_eq!(reason(&f), Reason::Waiting(wait), "{execution:?}");
        assert!(projection(&f).activities.is_empty());
    }
}

#[test]
fn every_attention_state_and_kind_is_classified_without_historical_actions() {
    for state in ATTENTION_STATES {
        for kind in HUMAN {
            let mut f = family();
            f.pr = Some(pr());
            let mut a = attention(kind);
            a.state = state;
            f.root.attentions.push(a);
            let open = matches!(state, AttentionState::Open | AttentionState::PartlyAnswered);
            assert_eq!(
                section(&f),
                if open {
                    Section::NeedsAttention
                } else {
                    Section::Backlog
                }
            );
            assert_eq!(projection(&f).actions.len(), usize::from(open));
            if open {
                assert_eq!(reason(&f), Reason::Human(kind));
            }
        }
    }
}

#[test]
fn every_passive_pr_combination_needs_explicit_human_action_or_activity() {
    let mut cases = 0;
    for ci in [Ci::Pending, Ci::Unknown, Ci::Failed, Ci::Success] {
        for review in [
            Review::Required,
            Review::Approved,
            Review::ChangesRequested,
            Review::Unknown,
        ] {
            for ai_review in [
                AiReview::Reviewing,
                AiReview::Queued,
                AiReview::NotReviewed,
                AiReview::Findings,
                AiReview::AllClear,
                AiReview::CleanPending,
                AiReview::NotRequired,
            ] {
                for conflict in [
                    Conflict::None,
                    Conflict::Detected,
                    Conflict::Deferred {
                        blocker: "sibling".into(),
                    },
                ] {
                    for merge_flow in [
                        MergeFlow::None,
                        MergeFlow::Requested,
                        MergeFlow::WhenReady,
                        MergeFlow::Queued,
                        MergeFlow::QueueCi,
                    ] {
                        let mut f = family();
                        let mut p = pr();
                        p.health = PrHealth {
                            ci,
                            review,
                            ai_review,
                            conflict: conflict.clone(),
                        };
                        p.merge_flow = merge_flow;
                        f.pr = Some(p);
                        assert_eq!(section(&f), Section::Backlog);
                        assert!(projection(&f).actions.is_empty());
                        if merge_flow != MergeFlow::None {
                            assert_eq!(reason(&f), Reason::Waiting(Wait::Merging));
                        }
                        f.root.attentions.push(attention(HumanAction::Intervention));
                        assert_eq!(section(&f), Section::NeedsAttention);
                        f.root.activities.push(activity("root", ActivityKind::Reviewing));
                        assert_eq!(section(&f), Section::Working);
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 1680);
}

#[test]
fn every_authoritative_wait_preserves_its_label_and_payload() {
    let waits = [
        Wait::Failed,
        Wait::Interrupted,
        Wait::Queued,
        Wait::Ready,
        Wait::Starting,
        Wait::Waiting,
        Wait::ActivityUnavailable,
        Wait::WaitingReview,
        Wait::WaitingChecks,
        Wait::Merging,
        Wait::ApplyingReview,
        Wait::ReviewQueued,
        Wait::RepairQueued,
        Wait::WaitingDependency,
        Wait::FocusHeld,
        Wait::DispatchPaused,
        Wait::Capacity,
        Wait::Quota,
        Wait::Backoff,
        Wait::ChainHold,
        Wait::NotBefore(900),
        Wait::Parked,
        Wait::Deferred,
        Wait::DeferredConflict {
            blocker: "sibling".into(),
        },
        Wait::HumanWork,
        Wait::UnavailableParent,
        Wait::Diagnostic("new engine hold".into()),
        Wait::CompletedWithoutPr,
        Wait::MergeUnverified,
    ];
    for wait in waits {
        let mut f = family();
        f.root.wait = Some(wait.clone());
        assert_eq!(section(&f), Section::Backlog);
        assert_eq!(reason(&f), Reason::Waiting(wait));
        f.root.attentions.push(attention(HumanAction::Question));
        assert_eq!(section(&f), Section::NeedsAttention);
        f.root.activities.push(activity("root", ActivityKind::Implementing));
        assert_eq!(section(&f), Section::Working);
        f.pr = Some(merged());
        assert_eq!(section(&f), Section::Landed);
    }
}

#[test]
fn revisions_of_every_origin_fold_and_followups_stay_independent() {
    for kind in KINDS {
        let mut f = family();
        f.root.status = TaskStatus::InReview;
        let mut revision = member("nested-revision");
        revision.activities.push(activity(&revision.id, kind));
        revision.attentions.push(attention(HumanAction::Question));
        f.revisions.push(revision);
        let mut p = pr();
        p.health.ci = Ci::Failed;
        f.pr = Some(p);
        let result = projection(&f);
        assert_eq!(result.placement.reason, Reason::Active(kind));
        assert_eq!(result.actions.len(), 1);
        assert_eq!(result.activities[0].task_id, "nested-revision");
        let mut followup = family();
        followup.root.id = "accepted-followup".into();
        assert_eq!(section(&followup), Section::Backlog);
        f.pr = Some(merged());
        assert_eq!(section(&f), Section::Landed);
        assert_eq!(section(&followup), Section::Backlog);
        assert_eq!(projection(&f).actions.len(), 1);
    }
}

#[test]
fn completed_and_archived_children_cannot_complete_the_root() {
    for status in [TaskStatus::Done, TaskStatus::Archived] {
        let mut f = family();
        let mut revision = member("revision");
        revision.status = status;
        let mut a = activity("revision", ActivityKind::Revising);
        a.runtime.execution = ExecutionStatus::Completed;
        revision.activities.push(a);
        f.revisions.push(revision);
        assert_eq!(reason(&f), Reason::Waiting(Wait::Queued));
        f.pr = Some(pr());
        assert_eq!(section(&f), Section::Backlog);
        f.root.status = TaskStatus::Done;
        assert_eq!(reason(&f), Reason::Waiting(Wait::MergeUnverified));
        assert!(projection(&f).actions.is_empty());
    }
}

#[test]
fn merge_evidence_is_bound_to_current_pr_and_preserves_observation_fallback() {
    let mut f = family();
    let mut p = merged();
    p.binding = "repo/pull/2".into();
    f.root.status = TaskStatus::Done;
    f.pr = Some(p);
    assert_eq!(reason(&f), Reason::Waiting(Wait::MergeUnverified));
    let mut p = merged();
    p.evidence.as_mut().unwrap().merged_at = None;
    f.pr = Some(p);
    let projected = projection(&f);
    assert_eq!(
        projected.placement.sort_key,
        SortKey::Landed(Reverse(400), "root".into())
    );
    assert!(matches!(
        projected.placement.reason,
        Reason::Merged(MergeEvidence { merged_at: None, .. })
    ));
}

#[test]
fn current_activity_picker_orders_waits_before_running_and_ignores_history() {
    let mut f = family();
    let mut running = activity("running", ActivityKind::Revising);
    running.started_at = 10;
    let mut waiting = activity("waiting", ActivityKind::Revising);
    waiting.runtime.run = RunState::WaitingInput;
    let mut old = activity("old", ActivityKind::Revising);
    old.runtime.current = false;
    f.root.activities = vec![running.clone(), old, waiting.clone()];
    let result = projection(&f);
    assert_eq!(result.activities, vec![waiting.reference.clone(), running.reference]);
    assert_eq!(result.placement.section, Section::Working);
    assert_eq!(result.actions, vec![Action::OpenInput(waiting.reference)]);
    f.root.activities[0].runtime.run = RunState::WaitingInput;
    assert_eq!(section(&f), Section::NeedsAttention);
}

#[test]
fn deferred_conflict_and_focus_hold_do_not_manufacture_a_question() {
    let mut f = family();
    f.root.wait = Some(Wait::FocusHeld);
    let mut p = pr();
    p.health.conflict = Conflict::Deferred {
        blocker: "sibling".into(),
    };
    f.pr = Some(p);
    assert_eq!(reason(&f), Reason::Waiting(Wait::FocusHeld));
    assert!(projection(&f).actions.is_empty());
    f.root.wait = None;
    assert_eq!(
        reason(&f),
        Reason::Waiting(Wait::DeferredConflict {
            blocker: "sibling".into()
        })
    );
}

#[test]
fn sorting_uses_semantic_groups_and_stable_ties() {
    let mut f = family();
    let key = |f: &Family| projection(f).placement.sort_key;
    let original = key(&f);
    f.ordering.priority = 2;
    assert!(key(&f) < original);
    let priority = key(&f);
    f.root.id = "a".into();
    assert!(key(&f) < priority);
    f.root.wait = Some(Wait::Failed);
    let failed = key(&f);
    f.root.wait = Some(Wait::Starting);
    let starting = key(&f);
    f.root.wait = Some(Wait::Merging);
    let merging = key(&f);
    f.root.wait = Some(Wait::FocusHeld);
    let held = key(&f);
    f.root.wait = Some(Wait::Parked);
    let parked = key(&f);
    f.root.wait = Some(Wait::MergeUnverified);
    let completed = key(&f);
    assert!(failed < starting && starting < merging && merging < held && held < parked && parked < completed);
    f.root.wait = None;
    f.root.activities.push(activity("root", ActivityKind::Implementing));
    let active = key(&f);
    f.root.activities[0].started_at -= 1;
    assert!(key(&f) < active);
    f.root.activities.clear();
    f.root.attentions.push(attention(HumanAction::Merge));
    f.pr = Some(pr());
    let merge = key(&f);
    f.root.attentions[0].kind = HumanAction::Review;
    let review = key(&f);
    f.root.attentions[0].kind = HumanAction::Intervention;
    let intervention = key(&f);
    f.root.attentions[0].kind = HumanAction::Question;
    let question = key(&f);
    assert!(question < intervention && intervention < review && review < merge);
    let attention = key(&f);
    f.root.attentions[0].actionable_since -= 1;
    assert!(key(&f) < attention);
    f.pr = Some(merged());
    let landed = key(&f);
    f.pr.as_mut().unwrap().evidence.as_mut().unwrap().merged_at = Some(301);
    assert!(key(&f) < landed);
    assert!(active < question && question < failed && completed < landed);
}

#[test]
fn automatic_review_holds_suppress_stale_review_actions_but_keep_questions() {
    for ai_review in [
        AiReview::Reviewing,
        AiReview::Queued,
        AiReview::NotReviewed,
        AiReview::Findings,
    ] {
        let mut f = family();
        let mut p = pr();
        p.health.ai_review = ai_review;
        f.pr = Some(p);
        f.root.attentions = vec![attention(HumanAction::Review), attention(HumanAction::Merge)];
        assert_eq!(section(&f), Section::Backlog);
        assert!(projection(&f).actions.is_empty());
        f.root.attentions.push(attention(HumanAction::Question));
        assert_eq!(section(&f), Section::NeedsAttention);
        assert_eq!(projection(&f).actions.len(), 1);
    }
}

#[test]
fn revision_statuses_explain_pending_work_without_authorizing_review() {
    for (status, wait) in [
        (TaskStatus::Todo, Wait::Queued),
        (TaskStatus::Active, Wait::ActivityUnavailable),
        (TaskStatus::Blocked, Wait::WaitingDependency),
    ] {
        let mut f = family();
        f.pr = Some(pr());
        f.root.attentions = vec![attention(HumanAction::Review), attention(HumanAction::Merge)];
        let mut revision = member("revision");
        revision.status = status;
        f.revisions.push(revision);
        assert_eq!(section(&f), Section::Backlog);
        assert_eq!(reason(&f), Reason::Waiting(wait));
        assert!(projection(&f).actions.is_empty());
    }
}

#[test]
fn terminal_revision_waits_are_history_and_optional_commands_do_not_elevate() {
    let mut f = family();
    let mut child = member("child");
    child.status = TaskStatus::Done;
    child.wait = Some(Wait::Failed);
    f.revisions.push(child);
    f.root.status = TaskStatus::Done;
    f.pr = Some(pr());
    let guide = Command {
        task_id: "root".into(),
        kind: CommandKind::GenerateGuide,
    };
    f.commands = vec![guide.clone(), guide.clone()];
    let result = projection(&f);
    assert_eq!(result.placement.reason, Reason::Waiting(Wait::MergeUnverified));
    assert_eq!(result.commands, vec![guide]);
    assert!(result.actions.is_empty());
}

#[test]
fn detail_defaults_and_input_routes_follow_the_winning_reason() {
    let mut f = family();
    assert_eq!(projection(&f).placement.detail, Detail::Activity);
    f.pr = Some(pr());
    f.pr.as_mut().unwrap().merge_flow = MergeFlow::Queued;
    assert_eq!(projection(&f).placement.detail, Detail::Review);
    f.root.attentions.push(attention(HumanAction::Question));
    assert_eq!(projection(&f).placement.detail, Detail::Attention);
    f.root.attentions.clear();
    let mut a = activity("root", ActivityKind::Implementing);
    a.runtime.execution = ExecutionStatus::WaitingHuman;
    a.runtime.run = RunState::Unavailable;
    f.root.activities.push(a);
    assert_eq!(projection(&f).placement.detail, Detail::Activity);
    assert!(matches!(&projection(&f).actions[..], [Action::OpenInput(_)]));
    f.pr = Some(merged());
    assert_eq!(projection(&f).placement.detail, Detail::History);
}

#[test]
fn permutation_of_members_and_activity_does_not_change_projection() {
    let mut f = family();
    for id in ["one", "two", "three"] {
        let mut m = member(id);
        m.attentions.push(attention(HumanAction::Question));
        m.activities.push(activity(id, ActivityKind::Revising));
        f.revisions.push(m);
    }
    let original = projection(&f);
    f.revisions.reverse();
    assert_eq!(projection(&f), original);
    for revision in &mut f.revisions {
        revision.activities[0].runtime.run = RunState::Exited;
    }
    let original = projection(&f);
    f.revisions.reverse();
    assert_eq!(projection(&f), original);
}

#[test]
fn backlog_ordinal_creation_and_root_id_are_final_stable_tiebreakers() {
    let mut f = family();
    f.ordering.project_ordinal = Some(2);
    let key = |f: &Family| projection(f).placement.sort_key;
    let original = key(&f);
    f.ordering.project_ordinal = Some(1);
    assert!(key(&f) < original);
    let original = key(&f);
    f.ordering.created_at -= 1;
    assert!(key(&f) < original);
    let original = key(&f);
    f.root.id = "a".into();
    assert!(key(&f) < original);
}

#[test]
fn revision_in_review_can_request_current_head_human_review() {
    let mut f = family();
    f.pr = Some(pr());
    let mut revision = member("revision");
    revision.status = TaskStatus::InReview;
    f.revisions.push(revision);
    assert_eq!(reason(&f), Reason::Waiting(Wait::WaitingReview));
    f.revisions[0].attentions.push(attention(HumanAction::Review));
    assert_eq!(reason(&f), Reason::Human(HumanAction::Review));
    assert!(matches!(&projection(&f).actions[..], [Action::Attention { task_id, .. }] if task_id == "revision"));
}

#[test]
fn scheduling_labels_explain_why_a_queued_execution_is_held() {
    let mut f = family();
    let mut a = activity("root", ActivityKind::Implementing);
    a.runtime.execution = ExecutionStatus::Queued;
    f.root.activities.push(a);
    for wait in [
        Wait::FocusHeld,
        Wait::Parked,
        Wait::NotBefore(500),
        Wait::DeferredConflict {
            blocker: "sibling".into(),
        },
    ] {
        f.root.wait = Some(wait.clone());
        assert_eq!(reason(&f), Reason::Waiting(wait));
    }
}

#[test]
fn activity_picker_uses_creation_while_working_rows_use_first_current_start() {
    let mut f = family();
    let mut early_created = activity("early-created", ActivityKind::Revising);
    early_created.created_at = 10;
    early_created.started_at = 100;
    let mut early_started = activity("early-started", ActivityKind::Reviewing);
    early_started.created_at = 20;
    early_started.started_at = 50;
    f.root.activities = vec![early_started.clone(), early_created.clone()];
    let result = projection(&f);
    assert_eq!(
        result.activities,
        vec![early_created.reference, early_started.reference]
    );
    assert_eq!(
        result.placement.sort_key,
        SortKey::Working(Reverse(1), 50, "root".into())
    );
}

#[test]
fn review_queued_wait_requires_a_reservation_or_queued_review() {
    let mut f = family();
    let mut p = pr();
    p.health.ai_review = AiReview::NotReviewed;
    p.health.ci = Ci::Success;
    f.pr = Some(p);
    f.root.wait = None;
    assert_eq!(reason(&f), Reason::Waiting(Wait::WaitingReview));

    f.root.wait = Some(Wait::ReviewQueued);
    assert_eq!(reason(&f), Reason::Waiting(Wait::ReviewQueued));

    f.root.wait = None;
    f.pr.as_mut().unwrap().health.ai_review = AiReview::Queued;
    assert_eq!(reason(&f), Reason::Waiting(Wait::ReviewQueued));
}
