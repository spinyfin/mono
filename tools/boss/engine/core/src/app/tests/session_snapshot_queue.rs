use super::*;

fn auth_snapshot(sequence: usize) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push(FrontendEvent::GitHubAuthState {
        state: GitHubAuthStateDto::Error {
            message: format!("auth-{sequence}"),
        },
    })
}

fn row_delta(sequence: usize) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push(FrontendEvent::WorkItemDeleted {
        id: format!("item-{sequence}"),
    })
}

fn assert_payload(actual: FrontendEventEnvelope, expected: FrontendEventEnvelope) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn thousand_snapshots_keep_only_newest_payload_without_resync() {
    let mut q = SessionQueue::new();
    let mut coalesced = 0;
    let mut degraded = 0;
    for sequence in 0..1_000 {
        match q.enqueue(auth_snapshot(sequence)) {
            EnqueueOutcome::Enqueued => {}
            EnqueueOutcome::Coalesced => coalesced += 1,
            EnqueueOutcome::Degraded => degraded += 1,
            other => panic!("unexpected admission: {other:?}"),
        }
    }
    let markers = q
        .items
        .iter()
        .filter(|(_, env)| topic_of(env).as_deref() == Some(RESYNC_TOPIC))
        .count();
    println!(
        "1,000 auth pushes: depth={}, coalesced={coalesced}, degraded={degraded}, resync_markers={markers}",
        q.stats().depth
    );
    assert_eq!(q.stats().depth, 1);
    assert_eq!(coalesced, 999);
    assert_eq!(degraded, 0);
    assert_eq!(markers, 0);
    assert_payload(q.pop_front().unwrap(), auth_snapshot(999));
    assert!(q.pop_front().is_none());
}

#[test]
fn interleaved_deltas_keep_order_and_snapshot_keeps_position_and_age() {
    let mut q = SessionQueue::new();
    q.enqueue(row_delta(0));
    q.enqueue(auth_snapshot(0));
    let enqueued_at = Instant::now() - Duration::from_millis(STUCK_CLIENT_AGE_MS + 1);
    q.items[1].0 = enqueued_at;
    for sequence in 1..100 {
        assert_eq!(q.enqueue(row_delta(sequence)), EnqueueOutcome::Enqueued);
        assert_eq!(q.enqueue(auth_snapshot(sequence)), EnqueueOutcome::Coalesced);
    }
    assert_eq!(q.stats().depth, 101);
    assert_eq!(q.items[1].0, enqueued_at);
    assert!(q.last_write_progress.is_none());
    assert_payload(q.pop_front().unwrap(), row_delta(0));
    // Popping an earlier row shifts the snapshot's index.
    assert_eq!(q.enqueue(auth_snapshot(100)), EnqueueOutcome::Coalesced);
    assert_payload(q.pop_front().unwrap(), auth_snapshot(100));
    for sequence in 1..100 {
        assert_payload(q.pop_front().unwrap(), row_delta(sequence));
    }
    assert!(q.pop_front().is_none());
    assert_eq!(q.enqueue(auth_snapshot(101)), EnqueueOutcome::Enqueued);
    assert_payload(q.pop_front().unwrap(), auth_snapshot(101));
}

#[test]
fn distinct_row_delta_overflow_still_delivers_exactly_one_resync() {
    let mut q = SessionQueue::new();
    for sequence in 0..MAX_SESSION_QUEUE {
        assert_eq!(q.enqueue(row_delta(sequence)), EnqueueOutcome::Enqueued);
    }
    assert_eq!(q.enqueue(row_delta(256)), EnqueueOutcome::Degraded);
    assert_eq!(q.enqueue(row_delta(257)), EnqueueOutcome::Degraded);
    assert_eq!(q.stats().depth, MAX_SESSION_QUEUE);
    assert!(!q.slow);
    // First overflow evicts two rows for the marker and new row; the next
    // evicts one. Every remaining delta retains its relative order.
    for sequence in 3..MAX_SESSION_QUEUE {
        assert_payload(q.pop_front().unwrap(), row_delta(sequence));
    }
    assert!(matches!(
        q.pop_front().unwrap().payload,
        FrontendEvent::TopicEvent {
            event: TopicEventPayload::ResyncRequired,
            ..
        }
    ));
    assert_payload(q.pop_front().unwrap(), row_delta(256));
    assert_payload(q.pop_front().unwrap(), row_delta(257));
    assert!(q.pop_front().is_none());
}

#[test]
fn snapshot_indices_survive_pressure_eviction_and_readmission() {
    let mut q = SessionQueue::new();
    q.enqueue(response_envelope("protected"));
    q.enqueue(auth_snapshot(0));
    q.enqueue(topic_envelope("other", 1));
    for sequence in 3..MAX_SESSION_QUEUE {
        q.enqueue(row_delta(sequence));
    }
    // Replacing at capacity requires neither eviction nor resync.
    assert_eq!(q.enqueue(auth_snapshot(1)), EnqueueOutcome::Coalesced);
    assert!(!q.pending_topics.contains_key(RESYNC_TOPIC));
    // Pressure evicts the snapshot and topic behind the protected reply.
    assert_eq!(q.enqueue(row_delta(256)), EnqueueOutcome::Degraded);
    assert_eq!(q.enqueue(auth_snapshot(2)), EnqueueOutcome::Degraded);
    // Another eviction shifts the newly admitted snapshot's index.
    assert_eq!(q.enqueue(row_delta(257)), EnqueueOutcome::Degraded);
    assert_eq!(q.enqueue(auth_snapshot(3)), EnqueueOutcome::Coalesced);
    assert_eq!(q.pop_front().unwrap().request_id.as_deref(), Some("protected"));
    let mut snapshots = 0;
    while let Some(env) = q.pop_front() {
        if matches!(env.payload, FrontendEvent::GitHubAuthState { .. }) {
            assert_payload(env, auth_snapshot(3));
            snapshots += 1;
        }
    }
    assert_eq!(snapshots, 1);
    assert_eq!(q.enqueue(auth_snapshot(4)), EnqueueOutcome::Enqueued);
    assert_payload(q.pop_front().unwrap(), auth_snapshot(4));
}

#[test]
fn allowlisted_kinds_are_independent_and_correlated_replies_are_never_coalesced() {
    let snapshots = [
        auth_snapshot(0).payload,
        FrontendEvent::WorkerLiveStatesList {
            states: vec![],
            doing_sections: Default::default(),
        },
        FrontendEvent::EngineHealthResult {
            report: boss_protocol::EngineHealthReport::builder()
                .anthropic_api_key_present(true)
                .dispatch_paused(false)
                .automation_paused(false)
                .issues(vec![])
                .build(),
        },
        FrontendEvent::EnginePoolConfig {
            worker_slots: 4,
            automation_slots: 2,
            review_slots: 1,
            coordinator_model: "test-model".into(),
        },
        FrontendEvent::FeatureFlagsList { flags: vec![] },
        FrontendEvent::DriverQuotaUsageResult {
            snapshot: boss_protocol::DriverQuotaSnapshot::default(),
        },
    ];
    let mut q = SessionQueue::new();
    for payload in &snapshots {
        assert_eq!(
            q.enqueue(FrontendEventEnvelope::push(payload.clone())),
            EnqueueOutcome::Enqueued
        );
        for request_id in ["reply-a", "reply-b"] {
            assert_eq!(
                q.enqueue(FrontendEventEnvelope::response(request_id, payload.clone())),
                EnqueueOutcome::Enqueued
            );
        }
    }
    for payload in &snapshots {
        assert_eq!(
            q.enqueue(FrontendEventEnvelope::push(payload.clone())),
            EnqueueOutcome::Coalesced
        );
    }
    assert_eq!(q.stats().depth, snapshots.len() * 3);
    for payload in snapshots {
        assert_payload(q.pop_front().unwrap(), FrontendEventEnvelope::push(payload.clone()));
        for request_id in ["reply-a", "reply-b"] {
            assert_payload(
                q.pop_front().unwrap(),
                FrontendEventEnvelope::response(request_id, payload.clone()),
            );
        }
    }
    assert!(q.pop_front().is_none());
}

#[test]
fn row_events_and_control_requests_retain_each_envelope() {
    let task = boss_protocol::Task::builder()
        .id("item")
        .product_id("product")
        .created_at("0")
        .updated_at("0")
        .description("test")
        .name("test")
        .kind(boss_protocol::TaskKind::Task)
        .status(boss_protocol::TaskStatus::Todo)
        .build();
    let item = boss_protocol::WorkItem::Task(task);
    let attention = boss_protocol::WorkAttentionItem::builder()
        .id("attention")
        .body_markdown("test")
        .created_at("0")
        .kind("failure")
        .status("open")
        .title("test")
        .build();
    let attempt = |kind: &str, status: &str| {
        boss_protocol::EngineAttemptListEntry::builder()
            .id("attempt")
            .product_id("product")
            .created_at("0")
            .extra(Default::default())
            .kind(kind)
            .pr_url("https://example.com/pr")
            .status(status)
            .work_item_id("item")
            .build()
    };
    let events = [
        FrontendEvent::WorkItemCreated { item: item.clone() },
        FrontendEvent::WorkItemUpdated { item: item.clone() },
        FrontendEvent::WorkItemDeleted { id: "item".into() },
        FrontendEvent::WorkItemsCreated { items: vec![item] },
        FrontendEvent::ProjectTasksReordered {
            project_id: "project".into(),
            task_ids: vec!["item".into()],
        },
        FrontendEvent::AttentionItemCreated {
            item: attention.clone(),
        },
        FrontendEvent::AttentionItemUpdated { item: attention },
        FrontendEvent::ConflictResolutionStarted {
            attempt: attempt("conflict", "running"),
            product_id: "product".into(),
            work_item_id: "item".into(),
            attempt_id: "attempt".into(),
            pr_url: "https://example.com/pr".into(),
        },
        FrontendEvent::CiRemediationSucceeded {
            attempt: attempt("ci", "succeeded"),
            product_id: "product".into(),
            work_item_id: "item".into(),
            attempt_id: "attempt".into(),
            pr_url: "https://example.com/pr".into(),
        },
        FrontendEvent::AutomationDeleted {
            automation_id: "automation".into(),
        },
        FrontendEvent::CommentResult {
            comment: boss_protocol::WorkComment::builder()
                .id("comment")
                .artifact_id("item")
                .anchor(boss_protocol::CommentAnchor::default())
                .artifact_kind("work_item")
                .author("user")
                .body("test")
                .created_at("0")
                .doc_version("v1")
                .updated_at("0")
                .build(),
        },
        engine_request_envelope("control").payload,
    ];
    for event in events {
        let mut q = SessionQueue::new();
        for _ in 0..3 {
            assert_eq!(
                q.enqueue(FrontendEventEnvelope::push(event.clone())),
                EnqueueOutcome::Enqueued
            );
        }
        for _ in 0..3 {
            assert_payload(q.pop_front().unwrap(), FrontendEventEnvelope::push(event.clone()));
        }
        assert!(q.pop_front().is_none());
    }
}
