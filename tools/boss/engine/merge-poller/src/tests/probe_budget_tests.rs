use super::*;
use crate::probe_budget::PassProbe;
use std::os::unix::process::ExitStatusExt;
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

fn urls() -> Vec<String> {
    (1..=4)
        .map(|n| format!("https://github.com/foo/bar/pull/{n}"))
        .collect()
}

fn output(success: bool, body: serde_json::Value) -> std::io::Result<std::process::Output> {
    Ok(std::process::Output {
        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
        stdout: body.to_string().into_bytes(),
        stderr: Vec::new(),
    })
}

fn probe_with(runner: crate::probe::TestProbeRunner) -> CommandMergeProbe {
    let mut probe = CommandMergeProbe::new();
    probe.test_runner = Some(runner);
    probe
}

#[tokio::test(start_paused = true)]
async fn initial_request_and_late_refresh_use_remaining_pass_time() {
    let probe = probe_with(Arc::new(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(100)).await;
            output(true, serde_json::json!({}))
        })
    }));
    let state = PassState::default();
    let end = Instant::now() + Duration::from_secs(100);
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: Some(end),
    };
    let started = Instant::now();
    let results = bounded.probe_batch(&urls()).await;
    assert_eq!(started.elapsed(), Duration::from_secs(25));
    assert!(results.values().all(|r| r.as_ref().unwrap_err().contains("deadline")));
    let mut snapshot = crate::pass::ProbeSnapshot::new(results);
    tokio::time::sleep(Duration::from_secs(67)).await;
    let late = Instant::now();
    assert!(snapshot.ensure_fresh(&bounded, &state, urls).await);
    assert_eq!(late.elapsed(), Duration::from_secs(2));
    assert_eq!(snapshot.results.len(), 4);
}

#[tokio::test(start_paused = true)]
async fn failed_batches_terminate_and_next_pass_starts_with_deferred_urls() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let probe = probe_with(Arc::new(move |args| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(args);
            tokio::time::sleep(Duration::from_secs(6)).await;
            output(false, serde_json::Value::Null)
        })
    }));
    let state = PassState::default();
    state.trace.begin();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let started = Instant::now();
    let results = bounded.probe_batch(&urls()).await;
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert_eq!(results.len(), 4);
    assert!(results.values().all(Result::is_err));
    let work = &state.trace.timings().probe_work["probe_fallback"];
    assert!(work.elapsed > Duration::ZERO);
    assert_eq!(work.deferred, 4);
    assert!(work.attempted > 0);
    calls.lock().unwrap().clear();
    bounded.probe_batch(&urls()).await;
    let calls = calls.lock().unwrap();
    // The first fallback half of the next invocation contains the untouched
    // right half, rather than repeatedly spending the budget on the left.
    assert!(
        calls[1]
            .iter()
            .any(|arg| arg.contains("number: 3") || arg.contains("number:3"))
    );
}

#[tokio::test(start_paused = true)]
async fn combined_status_deadline_preserves_completed_siblings_and_trace() {
    let probe = probe_with(Arc::new(|args| {
        Box::pin(async move {
            if args.get(1).is_some_and(|arg| arg == "graphql") {
                return output(
                    true,
                    serde_json::json!({"data": {"repo0": {
                        "pr0": {"state":"MERGED", "mergedAt":"2026-01-01", "headRefOid":""},
                        "pr1": {"state":"OPEN", "headRefOid":"head", "commits":{"nodes":[{"commit":{"oid":"head"}}]}}
                    }}}),
                );
            }
            tokio::time::sleep(Duration::from_secs(100)).await;
            output(true, serde_json::json!({}))
        })
    }));
    let state = PassState::default();
    state.trace.begin();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let results = bounded.probe_batch(&urls()[..2]).await;
    assert!(results[&urls()[0]].is_ok());
    assert!(results[&urls()[1]].is_err());
    let timing = state.trace.timings();
    let combined = &timing.probe_work["probe_combined_status"];
    assert_eq!(combined.attempted, 1);
    assert_eq!(combined.completed, 0);
    assert_eq!(combined.deferred, 1);
    assert_eq!(combined.elapsed, Duration::from_secs(30));
}

#[tokio::test(start_paused = true)]
async fn successful_batches_preserve_terminal_tail_and_rotate_deferred_status_calls() {
    let rest_calls = Arc::new(Mutex::new(Vec::new()));
    let seen = rest_calls.clone();
    let probe = probe_with(Arc::new(move |args| {
        let seen = seen.clone();
        Box::pin(async move {
            if args.get(1).is_some_and(|arg| arg == "graphql") {
                // Aliases follow the requested order, which changes each pass.
                let query = args.last().unwrap();
                let mut numbers: Vec<_> = (1..=4).collect();
                numbers.sort_by_key(|n| query.find(&format!("number: {n}")).unwrap());
                let mut nodes = serde_json::Map::new();
                for (alias, number) in numbers.into_iter().enumerate() {
                    let node = match number {
                        3 => serde_json::json!({"state":"MERGED", "mergedAt":"2026-01-01", "headRefOid":"merged-head"}),
                        4 => {
                            serde_json::json!({"state":"OPEN", "headRefOid":"checked", "commits":{"nodes":[{"commit":{"oid":"checked", "statusCheckRollup":{"contexts":{"nodes":[{"__typename":"StatusContext", "context":"ci", "state":"SUCCESS"}]}}}}]}})
                        }
                        _ => {
                            serde_json::json!({"state":"OPEN", "headRefOid":format!("head-{number}"), "commits":{"nodes":[{"commit":{"oid":format!("head-{number}")}}]}})
                        }
                    };
                    nodes.insert(format!("pr{alias}"), node);
                }
                return output(true, serde_json::json!({"data":{"repo0":nodes}}));
            }
            seen.lock().unwrap().push(args[1].clone());
            tokio::time::sleep(Duration::from_secs(100)).await;
            output(true, serde_json::json!({}))
        })
    }));
    let state = PassState::default();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    for pass in 0..3 {
        let results = bounded.probe_batch(&urls()).await;
        assert!(matches!(
            results[&urls()[2]].as_ref().unwrap().state,
            PrLifecycleState::Merged
        ));
        assert!(results[&urls()[3]].is_ok(), "populated rollup survives deadline");
        assert!(results[&urls()[0]].is_err());
        assert!(results[&urls()[1]].is_err());
        let next = state.probe_order.lock().unwrap().order(&urls());
        assert_eq!(next[0], urls()[2], "known terminal stays first");
        assert_eq!(
            next[1],
            urls()[if pass % 2 == 0 { 1 } else { 0 }],
            "unstarted status call leads open work"
        );
    }
    assert_eq!(
        *rest_calls.lock().unwrap(),
        vec![
            "repos/foo/bar/commits/head-1/status",
            "repos/foo/bar/commits/head-2/status",
            "repos/foo/bar/commits/head-1/status",
        ]
    );
}

#[tokio::test]
async fn bisect_rate_limit_rejection_stops_all_remaining_halves() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let probe = probe_with(Arc::new(move |args| {
        let mut calls = seen.lock().unwrap();
        calls.push(args);
        let count = calls.len();
        Box::pin(async move {
            if count <= 2 {
                output(false, serde_json::Value::Null)
            } else {
                let mut rejection = output(false, serde_json::Value::Null)?;
                rejection.stderr = b"API rate limit exceeded".to_vec();
                Ok(rejection)
            }
        })
    }));
    let results = probe.probe_batch(&urls()).await;
    // A rejection records an empty quota in a process-wide gauge that stretches
    // every poll interval. Put it back, or a test sharing the process that
    // reads the interval (`pr_poll_schedule_observe_drain_and_reschedule`)
    // fails depending on which tests the shard happens to hold.
    crate::record_rate_limit_remaining(i64::MAX, "test_reset");
    assert_eq!(results.len(), 4);
    assert!(results.values().all(|r| r.as_ref().unwrap_err().contains("rate limit")));
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        3,
        "no singleton fallback or right-half calls after rejection"
    );
    assert!(calls.iter().all(|args| args[1] == "graphql"));
}

#[tokio::test(start_paused = true)]
async fn cancellation_records_fallback_and_attendance() {
    let probe = probe_with(Arc::new(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            output(false, serde_json::Value::Null)
        })
    }));
    let state = PassState::default();
    state.trace.begin();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    assert!(
        tokio::time::timeout(Duration::from_secs(7), bounded.probe_batch(&urls()))
            .await
            .is_err()
    );
    let timing = state.trace.timings();
    let fallback = &timing.probe_work["probe_fallback"];
    assert_eq!(fallback.elapsed, Duration::from_secs(2));
    assert_eq!(fallback.attempted, 2);
    assert_eq!(fallback.deferred, 4);
    assert_eq!(state.probe_order.lock().unwrap().order(&urls())[0], urls()[2]);
}

#[tokio::test(start_paused = true)]
async fn known_terminal_urls_precede_open_urls_even_on_refresh() {
    let state = PassState::default();
    let terminal = PrLifecycleProbe::builder()
        .url(urls()[3].clone())
        .state(PrLifecycleState::Merged)
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build();
    state
        .probe_order
        .lock()
        .unwrap()
        .observe(&HashMap::from([(urls()[3].clone(), Ok(terminal))]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let probe = probe_with(Arc::new(move |args| {
        seen.lock().unwrap().push(args);
        Box::pin(async { Err(std::io::Error::other("offline")) })
    }));
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let mut snapshot = crate::pass::ProbeSnapshot::new(bounded.probe_batch(&urls()).await);
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert!(
        snapshot
            .ensure_fresh(&bounded, &state, || vec![urls()[0].clone(), urls()[3].clone()])
            .await
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for call in calls.iter() {
        let query = call.last().unwrap();
        assert!(query.find("number: 4").unwrap() < query.find("number: 1").unwrap());
    }
}

#[test]
fn successful_empty_dequeue_timeline_is_skipped() {
    let url = urls()[0].clone();
    let order = vec![url.clone()];
    let parsed = HashMap::from([(url.clone(), ("foo".to_owned(), "bar".to_owned(), 1))]);
    let (_, aliases) = build_batch_query(&order, &parsed, "timelineItems(first: 10) { nodes { __typename } }");
    let body = serde_json::json!({"data":{"repo0":{"pr0":{"timelineItems":{"nodes":[]}}}}});
    let events: HashMap<_, _> = walk_batch_response(&body, &aliases)
        .into_iter()
        .map(|(url, node)| {
            let nodes = node.unwrap()["timelineItems"]["nodes"].as_array().unwrap();
            (url, parse_dequeue_event_nodes(nodes))
        })
        .collect();
    assert!(events.contains_key(&url));
    assert!(!has_dequeue_events(&events, &url));
}

#[tokio::test]
async fn rebounce_pass_confirms_and_retires_only_candidates_with_events() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "RebouncePass", Some("git@github.com:foo/bar.git"));
    let candidates: Vec<_> = urls()[..2]
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let task = chore_in_review_for_product(&db, &product.id, &format!("candidate-{i}"), url);
            db.record_github_merge_intent(
                crate::work::GithubMergeIntentInsertInput::builder()
                    .work_item_id(task.clone())
                    .pr_url(url.clone())
                    .head_sha("head")
                    .build(),
                "{}",
            )
            .unwrap();
            PendingMergeCheck {
                work_item_id: task,
                product_id: product.id.clone(),
                pr_url: url.clone(),
            }
        })
        .collect();
    let created_at = chrono::DateTime::from_timestamp(boss_engine_utils::epoch_time::now_epoch_secs() + 3600, 0)
        .unwrap()
        .to_rfc3339();
    let events = HashMap::from([
        (candidates[0].pr_url.clone(), Vec::new()),
        (
            candidates[1].pr_url.clone(),
            vec![MergeQueueDequeueEvent {
                queue_base_oid: None,
                reason: "manual_removal".to_owned(),
                created_at: Some(created_at),
                pr_head_oid: Some("head".to_owned()),
                before_commit_oid: None,
            }],
        ),
    ]);
    let publisher = RecordingPublisher::default();
    crate::merge_queue::TEST_DEQUEUE_EVENTS
        .scope(events, async {
            crate::merge_queue::TEST_REBOUNCE_CONFIRMATIONS
                .scope(Mutex::new(Vec::new()), async {
                    let mut outcome = SweepOutcome::default();
                    run_merge_queue_rebounce_pass(
                        &db,
                        &publisher,
                        None,
                        &candidates,
                        &[],
                        Some(&Default::default()),
                        &mut outcome,
                    )
                    .await;
                    crate::merge_queue::TEST_REBOUNCE_CONFIRMATIONS.with(|ids| {
                        assert_eq!(*ids.lock().unwrap(), vec![candidates[1].work_item_id.clone()]);
                    });
                })
                .await;
        })
        .await;
    for (i, candidate) in candidates.iter().enumerate() {
        assert_eq!(
            db.get_active_github_merge_intent(&candidate.work_item_id, &candidate.pr_url)
                .unwrap()
                .is_some(),
            i == 0
        );
    }
    let events = publisher.events.lock().await;
    assert!(
        events
            .iter()
            .any(|(_, id, reason)| id == &candidates[1].work_item_id && reason == "github_merge_intent_dequeued")
    );
    // Retiring the second intent also renumbers its product's other queue
    // members; those notifications do not mean their intents were retired.
    assert!(
        events
            .iter()
            .all(|(_, id, reason)| id != &candidates[0].work_item_id || reason != "github_merge_intent_dequeued")
    );
}

#[tokio::test(start_paused = true)]
async fn singleton_fallback_cancels_slow_individual_probe() {
    let probe = probe_with(Arc::new(|args| {
        Box::pin(async move {
            if args.first().is_some_and(|arg| arg == "pr") {
                tokio::time::sleep(Duration::from_secs(100)).await;
            }
            output(false, serde_json::Value::Null)
        })
    }));
    let state = PassState::default();
    state.trace.begin();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let started = Instant::now();
    let results = bounded.probe_batch(&urls()[..1]).await;
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert!(results[&urls()[0]].as_ref().unwrap_err().contains("deadline"));
    assert_eq!(state.trace.timings().probe_work["probe_fallback"].attempted, 1);
}

#[tokio::test(start_paused = true)]
async fn successful_bisected_half_survives_a_slow_sibling() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let probe = probe_with(Arc::new(move |_| {
        let call = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(async move {
            match call {
                0 => output(false, serde_json::Value::Null),
                1 => output(true, serde_json::json!({"data":{"repo0":{"pr0":null,"pr1":null}}})),
                _ => {
                    tokio::time::sleep(Duration::from_secs(100)).await;
                    output(false, serde_json::Value::Null)
                }
            }
        })
    }));
    let state = PassState::default();
    state.trace.begin();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let results = bounded.probe_batch(&urls()).await;
    assert!(results[&urls()[0]].is_ok());
    assert!(results[&urls()[1]].is_ok());
    assert!(results[&urls()[2]].is_err());
    assert!(results[&urls()[3]].is_err());
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
    let timing = state.trace.timings();
    let fallback = &timing.probe_work["probe_fallback"];
    assert_eq!(fallback.attempted, 4);
    assert_eq!(fallback.completed, 2);
    assert_eq!(fallback.deferred, 2);
}

#[tokio::test(start_paused = true)]
async fn scoped_fallback_keeps_attendance_order_ahead_of_slow_combined_status() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let probe = probe_with(Arc::new(move |args| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(args.clone());
            if args.get(1).is_some_and(|arg| arg == "graphql") {
                return output(
                    false,
                    serde_json::json!({
                        "data":{"repo0":{"pr0":null,"pr1":{"state":"OPEN","headRefOid":"head","commits":{"nodes":[{"commit":{"oid":"head"}}]}}}},
                        "errors":[{"path":["repo0","pr0"]}]
                    }),
                );
            }
            if args.first().is_some_and(|arg| arg == "pr") {
                return output(false, serde_json::Value::Null);
            }
            tokio::time::sleep(Duration::from_secs(100)).await;
            output(false, serde_json::Value::Null)
        })
    }));
    let state = PassState::default();
    state.trace.begin();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let results = bounded.probe_batch(&urls()[..2]).await;
    assert_eq!(results.len(), 2);
    let timing = state.trace.timings();
    assert_eq!(timing.probe_work["probe_fallback"].elapsed, Duration::ZERO);
    assert_eq!(
        timing.probe_work["probe_combined_status"].elapsed,
        Duration::from_secs(30)
    );
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls[1][0], "pr",
        "the earliest affected URL is attempted before later serial status calls"
    );
    assert_eq!(calls[1][2], urls()[0]);
}
