//! A batched probe is sent in fixed-size chunks, each with its own deadline,
//! so one query GitHub rejects or never answers costs only its own PRs.
//!
//! Every test drives `CommandMergeProbe` through its `gh` seam against a
//! fake GitHub that answers aliased queries. Time is tokio's virtual clock:
//! "GitHub is slow" is a runner that sleeps.

use super::*;
use crate::probe::{FULL_PROBE_CHUNK_SIZE, PR_PROBE_FIELDS, PR_SHALLOW_FIELDS, ProbeDepth, SHALLOW_PROBE_CHUNK_SIZE};
use crate::probe_budget::PassProbe;
use std::os::unix::process::ExitStatusExt;
use std::sync::Mutex as StdMutex;
use tokio::time::Instant;

const PR_BASE: &str = "https://github.com/foo/bar/pull";

fn urls(count: usize) -> Vec<String> {
    (1..=count).map(|n| format!("{PR_BASE}/{n}")).collect()
}

fn output(success: bool, stdout: serde_json::Value, stderr: &str) -> std::io::Result<std::process::Output> {
    Ok(std::process::Output {
        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
        stdout: stdout.to_string().into_bytes(),
        stderr: stderr.as_bytes().to_vec(),
    })
}

fn probe_with(runner: crate::probe::TestProbeRunner) -> CommandMergeProbe {
    let mut probe = CommandMergeProbe::new();
    probe.test_runner = Some(runner);
    probe
}

/// The `(pr alias, PR number)` pairs an aliased query asks for, in order.
fn queried(query: &str) -> Vec<(String, u64)> {
    query
        .match_indices("pullRequest(number: ")
        .map(|(at, matched)| {
            let number: String = query[at + matched.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let alias_end = query[..at].rfind(':').unwrap();
            let alias_start = query[..alias_end].rfind(' ').unwrap() + 1;
            (query[alias_start..alias_end].to_owned(), number.parse().unwrap())
        })
        .collect()
}

/// The `query=` argument of a `gh api graphql` call, if it is one.
fn graphql_query(args: &[String]) -> Option<&String> {
    (args.get(1).map(String::as_str) == Some("graphql")).then(|| args.last().unwrap())
}

/// A GitHub that answers every queried PR with `node(number)`.
fn answer(query: &str, node: impl Fn(u64) -> serde_json::Value) -> serde_json::Value {
    let nodes: serde_json::Map<String, serde_json::Value> = queried(query)
        .into_iter()
        .map(|(alias, number)| (alias, node(number)))
        .collect();
    serde_json::json!({"data": {"repo0": nodes}})
}

/// An open PR with a populated CI rollup, which needs no REST status call.
fn open_node(number: u64) -> serde_json::Value {
    serde_json::json!({
        "state": "OPEN",
        "headRefOid": format!("head-{number}"),
        "updatedAt": "2026-11-21T00:00:00Z",
        "commits": {"nodes": [{"commit": {
            "oid": format!("head-{number}"),
            "statusCheckRollup": {"contexts": {"nodes": [
                {"__typename": "StatusContext", "context": "ci", "state": "SUCCESS"}
            ]}},
        }}]},
    })
}

fn merged_node(number: u64) -> serde_json::Value {
    serde_json::json!({
        "state": "MERGED",
        "mergedAt": "2026-11-21T01:00:00Z",
        "baseRefName": "main",
        "headRefOid": format!("head-{number}"),
        "mergeCommit": {"oid": format!("merge-{number}")},
        "updatedAt": "2026-11-21T01:00:00Z",
        "repository": {"defaultBranchRef": {"name": "main"}},
    })
}

type Queries = Arc<StdMutex<Vec<String>>>;

/// A runner that records every GraphQL query and answers it with `respond`,
/// after the delay `respond` names.
fn fake_github(
    queries: &Queries,
    respond: impl Fn(&str) -> (Duration, std::io::Result<std::process::Output>) + Send + Sync + 'static,
) -> crate::probe::TestProbeRunner {
    let queries = queries.clone();
    Arc::new(move |args| {
        let Some(query) = graphql_query(&args) else {
            panic!("only GraphQL calls are expected, got {args:?}");
        };
        queries.lock().unwrap().push(query.clone());
        let (delay, result) = respond(query);
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            result
        })
    })
}

fn pr_counts(queries: &Queries) -> Vec<usize> {
    queries.lock().unwrap().iter().map(|q| queried(q).len()).collect()
}

/// With 125 PRs tracked — the size at which one query for all of them was
/// rejected — no single query carries more than the chunk size, and every PR
/// is still answered.
#[tokio::test]
async fn no_query_exceeds_the_chunk_size() {
    let queries = Queries::default();
    let probe = probe_with(fake_github(&queries, |query| {
        (Duration::ZERO, output(true, answer(query, open_node), ""))
    }));

    let results = probe.probe_batch(&urls(125)).await;

    assert_eq!(results.len(), 125);
    assert!(results.values().all(Result::is_ok), "{results:?}");
    let counts = pr_counts(&queries);
    assert_eq!(counts, vec![25, 25, 25, 25, 25], "one query per full chunk");
    assert!(counts.iter().all(|count| *count <= FULL_PROBE_CHUNK_SIZE));
    assert!(
        queries
            .lock()
            .unwrap()
            .iter()
            .all(|query| query.contains("statusCheckRollup")),
        "the full tier asks for the full selection",
    );
}

/// `CommandMergeProbe::probe_chunked` stamps each chunk with the instant that
/// chunk started, not the batch start. A later slow chunk therefore cannot
/// make an early URL look as if it was observed when the batch finished —
/// and `PassProbe::record_unrecorded` must not be the only writer of those
/// timestamps.
#[tokio::test(start_paused = true)]
async fn probe_chunked_stamps_each_chunk_with_its_own_start() {
    let queries = Queries::default();
    let probe = probe_with(fake_github(&queries, |query| {
        let delay = if queried(query).iter().any(|(_, number)| *number == 1) {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        };
        (delay, output(true, answer(query, open_node), ""))
    }));
    let state = PassState::default();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let all = urls(FULL_PROBE_CHUNK_SIZE + 1);

    let _ = bounded.probe_batch(&all).await;

    let observation = state.probe_order.lock().unwrap().observation.clone();
    let first = observation.per_url[&all[0]];
    let second = observation.per_url[&all[FULL_PROBE_CHUNK_SIZE]];
    assert_eq!(
        first.elapsed(),
        Duration::from_secs(5),
        "chunk 1 started when the batch started"
    );
    assert_eq!(
        second.elapsed(),
        Duration::ZERO,
        "chunk 2 started after chunk 1 finished, not at the batch start"
    );
    assert!(
        second > first,
        "URLs in later chunks must carry a later start than URLs in chunk 1"
    );
    assert_eq!(observation.oldest_start, Some(first));
}

/// A chunk whose query never returns spends its own deadline and nothing
/// more: the chunks after it are still sent and observed, and the trace
/// reports the chunk that failed.
#[tokio::test(start_paused = true)]
async fn a_chunk_that_never_returns_does_not_starve_the_others() {
    let queries = Queries::default();
    let probe = probe_with(fake_github(&queries, |query| {
        // PR 7 is in the first chunk, and that chunk's query hangs.
        let delay = if query.contains("number: 7)") {
            Duration::from_secs(1000)
        } else {
            Duration::from_secs(3)
        };
        (delay, output(true, answer(query, open_node), ""))
    }));
    let state = PassState::default();
    state.trace.begin();
    let bounded = PassProbe {
        inner: &probe,
        state: &state,
        pass_end: None,
    };
    let all = urls(125);

    let started = Instant::now();
    let results = bounded.probe_batch(&all).await;

    for url in &all[..25] {
        let err = results[url].as_ref().unwrap_err();
        assert!(err.contains("deadline"), "{err}");
    }
    for url in &all[25..] {
        assert!(results[url].is_ok(), "{url}: {:?}", results[url]);
    }
    // The hung chunk's allowance, then four healthy chunks.
    assert_eq!(started.elapsed(), Duration::from_secs(30 + 4 * 3));
    let work = &state.trace.timings().probe_work["probe_chunks_full"];
    assert_eq!((work.attempted, work.completed, work.deferred), (5, 4, 1), "{work:?}");
}

/// Within a chunk the bisect fallback still applies: a query GitHub rejects
/// is retried in halves, and the other chunks are untouched.
#[tokio::test]
async fn a_rejected_chunk_is_bisected_within_the_chunk() {
    let queries = Queries::default();
    // GitHub rejects any query for more than 10 PRs.
    let probe = probe_with(fake_github(&queries, |query| {
        if queried(query).len() > 10 {
            (Duration::ZERO, output(false, serde_json::Value::Null, "HTTP 504"))
        } else {
            (Duration::ZERO, output(true, answer(query, open_node), ""))
        }
    }));

    let results = probe.probe_batch(&urls(50)).await;

    assert!(results.values().all(Result::is_ok), "{results:?}");
    let counts = pr_counts(&queries);
    // Two chunks of 25: each rejected, then halves 12 + 13 rejected, then 6 + 6 and 6 + 7.
    assert!(counts.iter().all(|count| *count <= FULL_PROBE_CHUNK_SIZE), "{counts:?}");
    assert_eq!(
        counts[..2],
        [25, 12],
        "the first chunk is retried in halves before the second starts"
    );
    assert!(counts.iter().filter(|count| **count == 25).count() == 2, "{counts:?}");
}

/// The measured incident, as a fixture: GitHub gives up on a query for more
/// than 30 PRs after 15 s, and answers a smaller one in about two seconds.
/// One query for all 125 PRs (and its halves) spends the whole 30 s allowance
/// and observes nothing; in chunks, every PR is observed.
///
/// Run with `--test_output=all` for the before and after figures.
#[tokio::test(start_paused = true)]
async fn chunking_observes_what_one_query_for_everything_did_not() {
    const NODES_PER_FULL_PR: usize = 51;
    fn github(queries: &Queries) -> crate::probe::TestProbeRunner {
        fake_github(queries, |query| {
            let prs = queried(query).len();
            if prs > 30 {
                (
                    Duration::from_secs(15),
                    output(false, serde_json::Value::Null, "HTTP 504"),
                )
            } else {
                (
                    Duration::from_millis(1500 + 20 * prs as u64),
                    output(true, answer(query, open_node), ""),
                )
            }
        })
    }
    let all = urls(125);

    // Before: one query for every tracked PR, halved on failure, all of it
    // sharing the one 30 s allowance.
    let before_queries = Queries::default();
    let before_probe = probe_with(github(&before_queries));
    let started = Instant::now();
    let before = before_probe
        .probe_batch_with_deadline(&all, Instant::now() + Duration::from_secs(30), ProbeDepth::Full)
        .await;
    let before_elapsed = started.elapsed();

    // After: the same PRs, probed in chunks.
    let after_queries = Queries::default();
    let after_probe = probe_with(github(&after_queries));
    let started = Instant::now();
    let after = after_probe.probe_batch(&all).await;
    let after_elapsed = started.elapsed();

    let report =
        |label: &str, queries: &Queries, elapsed: Duration, results: &HashMap<_, Result<PrLifecycleProbe, String>>| {
            let counts = pr_counts(queries);
            println!(
                "{label}: queries={} requested_nodes={} largest_query_prs={} observed={}/{} elapsed={}s",
                counts.len(),
                counts.iter().sum::<usize>() * NODES_PER_FULL_PR,
                counts.iter().max().unwrap(),
                results.values().filter(|r| r.is_ok()).count(),
                results.len(),
                elapsed.as_secs_f32(),
            );
        };
    report(
        "before (one query, 30s shared)",
        &before_queries,
        before_elapsed,
        &before,
    );
    report("after  (chunks of 25)         ", &after_queries, after_elapsed, &after);

    assert!(
        before.values().all(Result::is_err),
        "the fixture must reproduce the incident: every PR unobserved",
    );
    assert!(after.values().all(Result::is_ok), "every PR observed in chunks");
    assert_eq!(pr_counts(&after_queries), vec![25; 5]);
    assert!(after_elapsed < Duration::from_secs(30), "{after_elapsed:?}");
}

/// The shallow tier asks for the lifecycle only, in larger chunks, and never
/// follows an open PR with a REST status call.
#[tokio::test]
async fn shallow_probes_ask_for_lifecycle_only_in_larger_chunks() {
    let queries = Queries::default();
    let probe = probe_with(fake_github(&queries, |query| {
        let merged = |number: u64| {
            if number.is_multiple_of(50) {
                merged_node(number)
            } else {
                serde_json::json!({
                    "state": "OPEN", "updatedAt": "2026-11-21T00:00:00Z",
                })
            }
        };
        (Duration::ZERO, output(true, answer(query, merged), ""))
    }));

    let results = probe.probe_shallow_batch(&urls(250)).await;

    assert_eq!(pr_counts(&queries), vec![100, 100, 50]);
    assert_eq!(SHALLOW_PROBE_CHUNK_SIZE, 100);
    for query in queries.lock().unwrap().iter() {
        assert!(query.contains(PR_SHALLOW_FIELDS));
        assert!(!query.contains("statusCheckRollup") && !query.contains("labels(") && !query.contains("reviews("));
    }
    assert!(!PR_PROBE_FIELDS.is_empty());
    assert_eq!(results.len(), 250);
    let all = urls(250);
    for number in 1..=250usize {
        match results[&all[number - 1]].as_ref().unwrap() {
            ShallowProbe::Open { updated_at, .. } => {
                assert!(!number.is_multiple_of(50));
                assert_eq!(updated_at.as_deref(), Some("2026-11-21T00:00:00Z"));
            }
            ShallowProbe::Terminal(probe) => {
                assert!(number.is_multiple_of(50));
                assert_eq!(probe.state, PrLifecycleState::Merged);
                assert_eq!(
                    probe.merge_commit_oid.as_deref(),
                    Some(format!("merge-{number}").as_str())
                );
                assert_eq!(
                    probe.base_branch_error, None,
                    "the base-branch check survives a shallow probe"
                );
            }
        }
    }
}

/// A shallow probe carries queue and auto-merge flags so a row that has gone
/// cheap can still be promoted on the next pass.
#[tokio::test]
async fn a_shallow_probe_notices_queue_entry_and_auto_merge() {
    let probe = probe_with(fake_github(&Queries::default(), |query| {
        (
            Duration::ZERO,
            output(
                true,
                answer(query, |number| {
                    serde_json::json!({
                        "state": "OPEN",
                        "updatedAt": "2026-11-21T00:00:00Z",
                        "mergeQueueEntry": if number == 1 {
                            serde_json::json!({"state": "QUEUED"})
                        } else {
                            serde_json::Value::Null
                        },
                        "autoMergeRequest": if number == 2 {
                            serde_json::json!({"enabledAt": "2026-11-21T00:00:00Z"})
                        } else {
                            serde_json::Value::Null
                        },
                    })
                }),
                "",
            ),
        )
    }));

    let all = urls(2);
    let results = probe.probe_shallow_batch(&all).await;
    match results[&all[0]].as_ref().unwrap() {
        ShallowProbe::Open {
            in_merge_queue,
            auto_merge_enabled,
            ..
        } => {
            assert!(*in_merge_queue);
            assert!(!*auto_merge_enabled);
        }
        other => panic!("expected an open queued probe, got {other:?}"),
    }
    match results[&all[1]].as_ref().unwrap() {
        ShallowProbe::Open {
            in_merge_queue,
            auto_merge_enabled,
            ..
        } => {
            assert!(!*in_merge_queue);
            assert!(*auto_merge_enabled);
        }
        other => panic!("expected an open auto-merge probe, got {other:?}"),
    }
}

/// A PR GitHub no longer resolves is reported closed, as in the full probe.
#[tokio::test]
async fn a_shallow_probe_of_a_vanished_pr_is_closed_unmerged() {
    let probe = probe_with(fake_github(&Queries::default(), |query| {
        (
            Duration::ZERO,
            output(true, answer(query, |_| serde_json::Value::Null), ""),
        )
    }));

    let results = probe.probe_shallow_batch(&urls(1)).await;

    match results.values().next().unwrap().as_ref().unwrap() {
        ShallowProbe::Terminal(probe) => assert_eq!(probe.state, PrLifecycleState::ClosedUnmerged),
        other => panic!("expected a terminal probe, got {other:?}"),
    }
}

/// A rejection for rate limiting stops the call: the chunks after it are not
/// sent, rather than each being rejected in turn.
#[tokio::test]
async fn a_rate_limit_rejection_stops_the_remaining_chunks() {
    let queries = Queries::default();
    let probe = probe_with(fake_github(&queries, |_| {
        (
            Duration::ZERO,
            output(false, serde_json::Value::Null, "API rate limit exceeded"),
        )
    }));

    let results = probe.probe_batch(&urls(125)).await;
    // A rejection records an empty quota in a process-wide gauge that stretches
    // every poll interval; put it back so it cannot leak into other tests.
    crate::record_rate_limit_remaining(i64::MAX, "test_reset");

    assert_eq!(queries.lock().unwrap().len(), 1, "one rejected query, nothing after it");
    assert_eq!(results.len(), 125);
    assert!(results.values().all(|r| r.as_ref().unwrap_err().contains("rate limit")));
}

/// The incident's set, end to end through a real pass: 121 tracked PRs (nine
/// blocked on a merge conflict, eighteen parked, the rest in review; 98 of
/// them in the product being worked on, 23 in five others), against a GitHub
/// that gives up on any full query for more than 30 PRs after 15 s.
///
/// Run with `--test_output=all` for the before and after figures.
#[tokio::test(start_paused = true)]
async fn an_incident_shaped_pass_probes_eighty_prs_in_full_and_the_rest_cheaply() {
    const NODES_PER_FULL_PR: usize = 51;
    let (_dir, db) = open_db();
    let mono = create_test_product_with_repo(&db, "Mono", Some("git@github.com:foo/bar.git")).id;
    let others: Vec<String> = (0..5)
        .map(|n| create_test_product_with_repo(&db, &format!("Other {n}"), Some("git@github.com:foo/bar.git")).id)
        .collect();
    let mut blocked = std::collections::HashSet::new();
    for number in 1..=121u64 {
        let product = if number <= 98 {
            &mono
        } else {
            &others[(number % 5) as usize]
        };
        let id = chore_in_review_for_product(&db, product, &format!("Card {number}"), &format!("{PR_BASE}/{number}"));
        match number {
            1..=9 => {
                db.mark_chore_blocked_merge_conflict(&id, &format!("{PR_BASE}/{number}"))
                    .unwrap();
                blocked.insert(number);
            }
            10..=27 => {
                db.park_work_item_as_actor(&id, true, Some("not now"), "human").unwrap();
            }
            _ => {}
        }
    }
    let queries = Queries::default();
    let blocked_for_node = blocked.clone();
    let probe = probe_with(fake_github(&queries, move |query| {
        let full = query.contains("statusCheckRollup");
        let prs = queried(query).len();
        if full && prs > 30 {
            return (
                Duration::from_secs(15),
                output(false, serde_json::Value::Null, "HTTP 504"),
            );
        }
        let delay = if full {
            Duration::from_millis(1500 + 20 * prs as u64)
        } else {
            Duration::from_millis(400 + 5 * prs as u64)
        };
        let blocked = blocked_for_node.clone();
        let node = move |number: u64| {
            let mut node = open_node(number);
            if blocked.contains(&number) {
                node["mergeable"] = "CONFLICTING".into();
            }
            node
        };
        (delay, output(true, answer(query, node), ""))
    }));
    let state = PassState::default();
    state.set_active_products(Some([mono.clone()].into_iter().collect()));
    let all: Vec<String> = (1..=121).map(|n| format!("{PR_BASE}/{n}")).collect();

    // Before: the whole set in one query, halved on failure, all inside one
    // 30 s allowance.
    let before_queries = Queries::default();
    let before_probe = probe_with(fake_github(&before_queries, |query| {
        let prs = queried(query).len();
        if prs > 30 {
            (
                Duration::from_secs(15),
                output(false, serde_json::Value::Null, "HTTP 504"),
            )
        } else {
            (Duration::from_secs(2), output(true, answer(query, open_node), ""))
        }
    }));
    let started = Instant::now();
    let before = before_probe
        .probe_batch_with_deadline(&all, Instant::now() + Duration::from_secs(30), ProbeDepth::Full)
        .await;
    let before_elapsed = started.elapsed();
    let before_counts = pr_counts(&before_queries);

    let mut passes = Vec::new();
    for pass_number in 0..7 {
        queries.lock().unwrap().clear();
        let started = Instant::now();
        run_one_pass_tracked(
            &db,
            &probe,
            &RecordingPublisher::default(),
            SweepHandlers {
                cube_client: None,
                completion_handler: None,
                host: None,
                parked_ids: None,
            },
            &state,
            Duration::from_secs(180),
        )
        .await;
        let elapsed = started.elapsed();
        let mut full_nodes = 0;
        let (mut full_queries, mut shallow_queries) = (0, 0);
        for query in queries.lock().unwrap().iter() {
            if query.contains("statusCheckRollup") {
                full_queries += 1;
                full_nodes += queried(query).len() * NODES_PER_FULL_PR;
            } else {
                shallow_queries += 1;
            }
        }
        println!(
            "after  pass {pass_number}: full_queries={full_queries} shallow_queries={shallow_queries} \
             requested_nodes={full_nodes} elapsed={}s",
            elapsed.as_secs_f32()
        );
        passes.push((full_queries, shallow_queries, full_nodes, elapsed));
        tokio::time::advance(Duration::from_secs(60).saturating_sub(elapsed)).await;
    }
    println!(
        "before pass   : queries={} requested_nodes={} observed={}/{} elapsed={}s",
        before_counts.len(),
        before_counts.iter().sum::<usize>() * NODES_PER_FULL_PR,
        before.values().filter(|r| r.is_ok()).count(),
        before.len(),
        before_elapsed.as_secs_f32(),
    );

    assert!(
        before.values().all(Result::is_err),
        "the fixture must reproduce the incident"
    );
    // 9 blocked + 71 live in-review rows in the active product are probed in
    // full on every pass: four queries of at most 25 PRs.
    for (pass_number, (full_queries, _, full_nodes, elapsed)) in passes.iter().enumerate() {
        assert_eq!(*full_queries, 4, "pass {pass_number}");
        assert_eq!(*full_nodes, 80 * NODES_PER_FULL_PR, "pass {pass_number}");
        assert!(*elapsed < Duration::from_secs(30), "pass {pass_number}: {elapsed:?}");
    }
    // The other 41 PRs (18 parked, 23 in other products) are one shallow
    // query, on the first pass and then once the five minutes are up.
    let shallow_by_pass: Vec<usize> = passes.iter().map(|(_, shallow, ..)| *shallow).collect();
    assert_eq!(shallow_by_pass, vec![1, 0, 0, 0, 0, 0, 1]);
    let all_queries = queries.lock().unwrap();
    assert!(
        all_queries
            .iter()
            .all(|query| queried(query).len() <= SHALLOW_PROBE_CHUNK_SIZE)
    );
}
