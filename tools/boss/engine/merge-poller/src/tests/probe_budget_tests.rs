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
    assert!(snapshot.ensure_fresh(&bounded, urls).await);
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
            .ensure_fresh(&bounded, || vec![urls()[0].clone(), urls()[3].clone()])
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
