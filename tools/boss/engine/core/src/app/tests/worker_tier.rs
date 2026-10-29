// Worker RPC tier: peer classification, the pre-dispatch verb gate, and the
// outbound row sanitizer.
//
// The verb *table* is the `boss-engine-worker-policy` crate's business and is
// tested there. What is unique to this layer — and therefore what is tested
// here — is the wiring: that a worker-descended socket peer is recognised as
// one and resolved to its run, that the flag really does gate enforcement,
// that a coordinator shell is untouched, and that the gate and the sanitizer
// are actually installed in the connection path rather than merely written.
//
// The last point is why several tests below drive a real `UnixStream` pair
// through `handle_frontend_connection` instead of calling the pure functions:
// a unit test of `sanitize_event_for_worker` passes whether or not anything
// calls it.
//
// Classification is exercised with the test process's own pid registered as a
// worker — `lookup_with_ancestor_walk` treats a pid as its own ancestor, so
// registering `std::process::id()` makes this process look exactly like a
// worker session, the same trick `t03`, `trust_authorization` and `proposals`
// use.

use super::*;
use boss_protocol::WorkerTierDenialReason;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn critical_confirmation_authenticates_socket_peer_even_with_worker_gate_disabled() {
    let (state, _dir) = test_server_state();
    state.feature_flags.set("worker_rpc_tier", false).unwrap();
    let product = crate::test_support::create_test_product(&state.work_db);
    let chore = crate::test_support::create_test_chore(&state.work_db, product.id, "Priority confirmation");
    let request = serde_json::json!({"request_id": "request-critical", "payload": {
        "type": "request_critical_priority", "ids": [chore.id], "ask": "Please make this critical."
    }})
    .to_string();
    let refused = round_trip(state.clone(), Some(self_pid()), &request).await;
    assert_eq!(refused["payload"]["type"], "work_error");
    state.set_boss_pid(self_pid());
    state.set_app_pid(i32::MAX);
    let register =
        serde_json::json!({"request_id": "forge-app", "payload": {"type": "register_app_session"}}).to_string();
    let refused = round_trip(state.clone(), Some(self_pid()), &register).await;
    assert_eq!(refused["payload"]["type"], "error");
    assert_eq!(state.current_app_pid(), Some(i32::MAX));
    state.worker_registry.register(self_pid(), "worker");
    let refused = round_trip(state.clone(), Some(self_pid()), &request).await;
    assert_eq!(refused["payload"]["type"], "work_error");
    state.worker_registry.unregister(self_pid());
    let pending = round_trip(state.clone(), Some(self_pid()), &request).await;
    assert_eq!(pending["payload"]["type"], "critical_priority_requested", "{pending}");
    let confirm = serde_json::json!({"request_id": "confirm-critical", "payload": {
        "type": "resolve_critical_priority_request", "id": pending["payload"]["id"], "confirm": true
    }})
    .to_string();
    let refused = round_trip(state.clone(), Some(self_pid()), &confirm).await;
    assert_eq!(refused["payload"]["type"], "work_error");
    state.clear_boss_pid();
    state.worker_registry.register(self_pid(), "worker");
    let refused = round_trip(state.clone(), Some(self_pid()), &register).await;
    assert_eq!(refused["payload"]["type"], "error");
    assert_eq!(state.current_app_pid(), Some(i32::MAX));
    state.worker_registry.unregister(self_pid());
    state.set_app_pid(self_pid());
    state.worker_registry.register(self_pid(), "worker");
    let refused = round_trip(state.clone(), Some(self_pid()), &confirm).await;
    assert_eq!(refused["payload"]["type"], "work_error");
    state.worker_registry.unregister(self_pid());
    let accepted = round_trip(state.clone(), Some(self_pid()), &confirm).await;
    assert_eq!(accepted["payload"]["type"], "critical_priority_resolved", "{accepted}");
    let WorkItem::Chore(updated) = state.work_db.get_work_item(&chore.id).unwrap() else {
        panic!("chore")
    };
    assert_eq!(updated.priority, "critical");
    let replay = round_trip(state.clone(), Some(self_pid()), &confirm).await;
    assert_eq!(replay["payload"]["type"], "work_error");
}

fn self_pid() -> libc::pid_t {
    std::process::id() as libc::pid_t
}

/// A `ServerState` with this process registered as the worker running
/// `run_id`, and `worker_rpc_tier` enabled.
fn enforcing_state(run_id: &str) -> (Arc<ServerState>, tempfile::TempDir) {
    let (server_state, dir) = test_server_state();
    server_state.worker_registry.register(self_pid(), run_id.to_owned());
    server_state
        .feature_flags
        .set("worker_rpc_tier", true)
        .expect("worker_rpc_tier must be a registered flag");
    (server_state, dir)
}

/// A representative denied verb: the exact call the design names as the gap
/// this task closes ("The only things stopping a worker from `boss task
/// update` today are prompt text").
fn denied_request() -> FrontendRequest {
    FrontendRequest::UpdateWorkItem {
        id: "task_1".to_owned(),
        patch: boss_protocol::WorkItemPatch::default(),
    }
}

/// A representative allowed verb.
fn allowed_request() -> FrontendRequest {
    FrontendRequest::ListProducts
}

#[tokio::test]
async fn progression_gate_is_unconditional_and_precedes_proposal_dispatch() {
    let (state, _dir) = test_server_state();
    state.feature_flags.set("worker_rpc_tier", false).unwrap();
    let product = crate::test_support::create_test_product(&state.work_db);
    let item = crate::test_support::create_test_chore_manual(&state.work_db, product.id, "Stuck");
    let execution = state
        .work_db
        .create_execution(
            boss_protocol::CreateExecutionInput::builder()
                .work_item_id(&item.id)
                .kind(boss_protocol::ExecutionKind::TaskImplementation)
                .status(boss_protocol::ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    // Register an ancestor, exercising the same walk used for ordinary workers.
    let parent = crate::worker_registry::parent_pid(self_pid()).unwrap().unwrap();
    state
        .worker_registry
        .register_progression(parent, &execution.id, &state.work_db)
        .unwrap();
    let peer = state.classify_peer(Some(self_pid()));
    assert_eq!(
        peer,
        PeerClass::Progression {
            run_id: execution.id.clone()
        }
    );
    assert!(state.authorize_rpc(RpcTier::Progression, Some(self_pid())));
    assert!(!state.authorize_rpc(RpcTier::Worker, Some(self_pid())));
    // Progression peers are agents: the critical-raise guard covers them and
    // their writes are attributed to the progression actor, not `human`.
    assert!(peer.is_agent());
    assert!(!peer.is_worker());
    assert_eq!(
        resolve_status_actor(&state, Some(self_pid())),
        boss_protocol::LAST_STATUS_ACTOR_PROGRESSION
    );
    assert!(work_items::resolve_write_actor(&state, Some(self_pid()), Some("critical"), Some("medium")).is_err());
    assert_eq!(
        work_items::resolve_write_actor(&state, Some(self_pid()), Some("high"), Some("medium")).unwrap(),
        boss_protocol::LAST_STATUS_ACTOR_PROGRESSION
    );
    let denial = state.progression_scope_denial(&peer, "test", "outside").unwrap();
    assert_eq!(denial.reason, WorkerTierDenialReason::ProgressionScope);
    assert!(!denial.message.contains("boss propose blocked"), "{}", denial.message);
    assert!(state.progression_scope_denial(&peer, "test", &item.id).is_none());
    assert!(state.progression_scope_denial(&peer, "test", "outside").is_some());
    assert!(
        state
            .progression_scope_denial(&PeerClass::Other, "test", &item.id)
            .is_some()
    );
    for &kind in boss_protocol::ProposalKind::ALL {
        let request = serde_json::json!({"request_id": "proposal", "payload": {
            "type": "submit_proposal", "run_id": execution.id, "kind": kind,
            "payload": {}
        }})
        .to_string();
        let response = round_trip(state.clone(), Some(self_pid()), &request).await;
        assert_eq!(response["payload"]["type"], "worker_tier_denied", "{kind}");
        assert_eq!(response["payload"]["denial"]["verb"], format!("SubmitProposal:{kind}"));
    }
    let request =
        r#"{"request_id":"write","payload":{"type":"update_work_item","id":"outside","patch":{"name":"No"}}}"#;
    let response = round_trip(state.clone(), Some(self_pid()), request).await;
    assert_eq!(response["payload"]["type"], "worker_tier_denied");
    let request = r#"{"request_id":"read","payload":{"type":"list_products"}}"#;
    let response = round_trip(state.clone(), Some(self_pid()), request).await;
    assert_eq!(response["payload"]["type"], "products_list");
    let run = state
        .work_db
        .create_run(
            boss_protocol::CreateRunInput::builder()
                .agent_id("progression_agent")
                .execution_id(&execution.id)
                .transcript_path("/tmp/progression-private.jsonl")
                .build(),
        )
        .unwrap();
    let request = serde_json::json!({"request_id": "runs", "payload": {
        "type": "list_runs", "execution_id": execution.id
    }})
    .to_string();
    let response = round_trip(state.clone(), Some(self_pid()), &request).await;
    assert_eq!(response["payload"]["runs"][0]["id"], run.id);
    assert!(response["payload"]["runs"][0]["transcript_path"].is_null());
    let request = r#"{"request_id":"outside","payload":{"type":"list_runs","execution_id":"exec_other"}}"#;
    let response = round_trip(state.clone(), Some(self_pid()), request).await;
    assert_eq!(response["payload"]["type"], "worker_tier_denied");
    state.worker_registry.unregister(parent);
    assert_eq!(state.classify_peer(Some(self_pid())), PeerClass::Other);
}

// ── Classification ───────────────────────────────────────────────────────────

#[test]
fn peer_descending_from_a_registered_worker_resolves_to_its_run() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register(self_pid(), "exec_abc".to_owned());

    let class = server_state.classify_peer(Some(self_pid()));
    assert!(class.is_worker());
    assert_eq!(
        class.worker_run_id(),
        Some("exec_abc"),
        "classification must resolve the peer to its specific run, not just to 'some worker'",
    );
}

#[test]
fn peer_not_descending_from_a_worker_is_unclassified() {
    let (server_state, _dir) = test_server_state();
    // No worker registered at all: the coordinator's shell, a plain
    // terminal, the app.
    assert!(!server_state.classify_peer(Some(self_pid())).is_worker());
    assert_eq!(server_state.classify_peer(Some(self_pid())).worker_run_id(), None);
}

#[test]
fn connection_without_a_local_peer_pid_is_not_a_worker() {
    // Remote (non-local) peers present no pid. v1 gives them no worker tier;
    // the proposal verbs refuse them explicitly with `no_local_peer`.
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register(self_pid(), "exec_abc".to_owned());
    assert!(!server_state.classify_peer(None).is_worker());
}

#[test]
fn worker_tier_authorization_is_not_permissive_without_trust_roots() {
    // `authorize_rpc` short-circuits to `true` for AppOrBoss/BossOnly when
    // neither trust root is registered (test mode / an engine started
    // without the macOS app). `Worker` must NOT inherit that: answering
    // "yes, a worker" there would confine the coordinator to worker tier on
    // exactly the setups where no app is running.
    let (server_state, _dir) = test_server_state();
    assert_eq!(server_state.current_app_pid(), None);
    assert_eq!(server_state.current_boss_pid(), None);
    assert!(server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid())));
    assert!(
        !server_state.authorize_rpc(RpcTier::Worker, Some(self_pid())),
        "RpcTier::Worker must answer from the worker registry, not from the trust-root shortcut",
    );

    server_state.worker_registry.register(self_pid(), "exec_abc".to_owned());
    assert!(server_state.authorize_rpc(RpcTier::Worker, Some(self_pid())));
}

// ── The gate ─────────────────────────────────────────────────────────────────

#[test]
fn denied_verb_from_a_worker_is_refused_with_a_typed_error() {
    let (server_state, _dir) = enforcing_state("exec_abc");
    let class = server_state.classify_peer(Some(self_pid()));

    let denial = server_state
        .worker_tier_denial(&class, &denied_request())
        .expect("a mutating taxonomy verb must be denied at worker tier");
    assert_eq!(denial.verb, "UpdateWorkItem");
    assert_eq!(denial.reason, WorkerTierDenialReason::MutatingTaxonomy);
    assert!(
        denial.use_instead.is_some(),
        "the denial must name the verb to use instead",
    );
}

#[test]
fn allowed_verb_from_a_worker_passes_the_gate() {
    let (server_state, _dir) = enforcing_state("exec_abc");
    let class = server_state.classify_peer(Some(self_pid()));
    assert!(server_state.worker_tier_denial(&class, &allowed_request()).is_none());
}

#[test]
fn coordinator_shell_is_unaffected_by_the_gate() {
    // The acceptance criterion: a human/coordinator shell not descended from
    // a worker pid keeps its existing authority, flag on or off.
    let (server_state, _dir) = test_server_state();
    server_state.feature_flags.set("worker_rpc_tier", true).unwrap();
    // Note: no worker registered, so this pid classifies as Other.
    let class = server_state.classify_peer(Some(self_pid()));
    assert!(!class.is_worker());
    assert!(
        server_state.worker_tier_denial(&class, &denied_request()).is_none(),
        "a shell that is not worker-descended must keep its existing authority",
    );
}

#[test]
fn flag_off_leaves_workers_at_the_historical_user_tier() {
    // Rollback is a flag flip, not a redeploy.
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register(self_pid(), "exec_abc".to_owned());
    let class = server_state.classify_peer(Some(self_pid()));
    assert!(class.is_worker(), "classification happens regardless of the flag");
    assert!(
        server_state.worker_tier_denial(&class, &denied_request()).is_none(),
        "with worker_rpc_tier off, a worker must keep the unconditional RpcTier::User behaviour",
    );

    server_state.feature_flags.set("worker_rpc_tier", true).unwrap();
    assert!(
        server_state.worker_tier_denial(&class, &denied_request()).is_some(),
        "flipping the flag on must start enforcing without anything else changing",
    );
}

// ── End to end over a real connection ────────────────────────────────────────

/// Drive one request through a real `handle_frontend_connection` as `peer_pid`
/// and return the decoded response frame.
async fn round_trip(server_state: Arc<ServerState>, peer_pid: Option<libc::pid_t>, request: &str) -> serde_json::Value {
    let (engine_side, client_side) = tokio::net::UnixStream::pair().unwrap();
    let conn = tokio::spawn(handle_frontend_connection(engine_side, server_state, peer_pid));

    let (read_half, mut write_half) = client_side.into_split();
    let mut reader = BufReader::new(read_half);

    // Drain the Hello push the engine emits on connect.
    let mut hello = String::new();
    reader.read_line(&mut hello).await.unwrap();

    write_half.write_all(request.as_bytes()).await.unwrap();
    write_half.write_all(b"\n").await.unwrap();
    write_half.flush().await.unwrap();

    let mut response = String::new();
    reader.read_line(&mut response).await.unwrap();

    drop(write_half);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn).await;

    serde_json::from_str(&response).expect("response must be JSON")
}

#[tokio::test]
async fn denied_verb_never_reaches_its_handler() {
    let (server_state, _dir) = enforcing_state("exec_abc");
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let chore = crate::test_support::create_test_chore(db, product.id, "Cleanup");
    let chore_id = chore.id.clone();

    let request = serde_json::json!({
        "request_id": "req-1",
        "payload": {
            "type": "update_work_item",
            "id": chore_id,
            "patch": { "name": "Renamed by a worker" },
        },
    })
    .to_string();

    let parsed = round_trip(server_state.clone(), Some(self_pid()), &request).await;
    assert_eq!(parsed["request_id"], "req-1");
    assert_eq!(parsed["payload"]["type"], "worker_tier_denied");
    assert_eq!(parsed["payload"]["denial"]["verb"], "UpdateWorkItem");
    assert_eq!(
        server_state
            .metrics
            .counter_value("frontend_requests_total.updateworkitem"),
        Some(1)
    );
    assert_eq!(parsed["payload"]["denial"]["reason"], "mutating_taxonomy");
    assert!(
        parsed["payload"]["denial"]["use_instead"]
            .as_str()
            .expect("a mutating-taxonomy denial must carry a redirect")
            .starts_with("boss propose"),
    );

    // The gate runs before dispatch, so the mutation must not have happened.
    let after = server_state.work_db.get_work_item(&chore_id).unwrap();
    let WorkItem::Chore(after) = after else {
        panic!("fixture creates a chore");
    };
    assert_eq!(
        after.name, "Cleanup",
        "a denied verb must be refused before its handler runs, not after",
    );
}

#[tokio::test]
async fn allowed_verb_from_a_worker_still_executes() {
    let (server_state, _dir) = enforcing_state("exec_abc");
    crate::test_support::create_test_product(&server_state.work_db);

    let request = r#"{"request_id":"req-1","payload":{"type":"list_products"}}"#;
    let parsed = round_trip(server_state, Some(self_pid()), request).await;
    assert_eq!(parsed["payload"]["type"], "products_list");
    assert_eq!(
        parsed["payload"]["products"]
            .as_array()
            .expect("products must be a list")
            .len(),
        1,
    );
}

#[tokio::test]
async fn run_rows_reaching_a_worker_have_their_transcript_path_stripped() {
    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let chore = crate::test_support::create_test_chore(db, product.id, "Cleanup");
    let execution = crate::test_support::create_ready_chore_execution(db, chore.id);
    let run = db
        .create_run(
            boss_protocol::CreateRunInput::builder()
                .agent_id("agent_1")
                .execution_id(execution.id.clone())
                .transcript_path("/Users/someone/Library/Application Support/Boss/transcripts/exec_abc.jsonl")
                .build(),
        )
        .unwrap();
    assert!(
        run.transcript_path.is_some(),
        "fixture must actually store a transcript path, or this test proves nothing",
    );

    // Register the worker as running *this* execution, not an unrelated one
    // — the point of this test is what a worker sees of its own run, and
    // `run_scope_denies_a_worker_reading_another_executions_runs` below
    // covers the cross-execution case.
    server_state.worker_registry.register(self_pid(), execution.id.clone());
    server_state
        .feature_flags
        .set("worker_rpc_tier", true)
        .expect("worker_rpc_tier must be a registered flag");

    let request = serde_json::json!({
        "request_id": "req-1",
        "payload": { "type": "list_runs", "execution_id": execution.id },
    })
    .to_string();

    // As a worker: the path must be gone.
    let parsed = round_trip(server_state.clone(), Some(self_pid()), &request).await;
    assert_eq!(parsed["payload"]["type"], "runs_list");
    let runs = parsed["payload"]["runs"].as_array().expect("runs must be a list");
    assert_eq!(runs.len(), 1, "the row itself must still be returned, not suppressed");
    assert!(
        runs[0]["transcript_path"].is_null(),
        "a worker must not receive a transcript path, got {:?}",
        runs[0]["transcript_path"],
    );
    // The rest of the row survives — sanitizing must not blank the read.
    assert_eq!(runs[0]["id"], run.id);
    assert_eq!(runs[0]["execution_id"], execution.id);

    // As a non-worker (no peer pid resolvable to a worker): unchanged. This
    // is what makes the previous assertion meaningful rather than a property
    // of the row never having had a path.
    server_state.worker_registry.unregister(self_pid());
    let parsed = round_trip(server_state.clone(), Some(self_pid()), &request).await;
    let runs = parsed["payload"]["runs"].as_array().unwrap();
    assert_eq!(
        runs[0]["transcript_path"].as_str(),
        run.transcript_path.as_deref(),
        "a coordinator/app connection must keep seeing the transcript path",
    );

    // And with the flag off, a worker connection behaves exactly as it did
    // before this change — the flag is a kill switch for the whole tier, not
    // just for the verb gate.
    server_state.worker_registry.register(self_pid(), execution.id.clone());
    server_state.feature_flags.set("worker_rpc_tier", false).unwrap();
    let parsed = round_trip(server_state, Some(self_pid()), &request).await;
    let runs = parsed["payload"]["runs"].as_array().unwrap();
    assert_eq!(
        runs[0]["transcript_path"].as_str(),
        run.transcript_path.as_deref(),
        "with worker_rpc_tier off, sanitization must be off too",
    );
}

/// A worker classified for one execution must not be able to read another
/// execution's run rows via `ListRuns` or `GetRun` — the two verbs are
/// allowed by `worker_verb_decision` (a worker must see its own runs), but
/// [`ServerState::worker_row_scope_denial`] scopes them to the caller's own
/// execution at the connection gate, since the pure verb table has no way to
/// know which execution a `GetRun`/`ListRuns` target belongs to.
#[tokio::test]
async fn run_scope_denies_a_worker_reading_another_executions_runs() {
    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let mine_chore = crate::test_support::create_test_chore(db, product.id.clone(), "Mine");
    let mine_execution = crate::test_support::create_ready_chore_execution(db, mine_chore.id);
    let other_chore = crate::test_support::create_test_chore(db, product.id, "Someone else's");
    let other_execution = crate::test_support::create_ready_chore_execution(db, other_chore.id);
    let other_run = db
        .create_run(
            boss_protocol::CreateRunInput::builder()
                .agent_id("agent_1")
                .execution_id(other_execution.id.clone())
                .build(),
        )
        .unwrap();

    server_state
        .worker_registry
        .register(self_pid(), mine_execution.id.clone());
    server_state
        .feature_flags
        .set("worker_rpc_tier", true)
        .expect("worker_rpc_tier must be a registered flag");

    // ListRuns against another execution: denied.
    let list_request = serde_json::json!({
        "request_id": "req-1",
        "payload": { "type": "list_runs", "execution_id": other_execution.id },
    })
    .to_string();
    let parsed = round_trip(server_state.clone(), Some(self_pid()), &list_request).await;
    assert_eq!(parsed["payload"]["type"], "worker_tier_denied");
    assert_eq!(parsed["payload"]["denial"]["verb"], "ListRuns");
    assert_eq!(parsed["payload"]["denial"]["reason"], "runtime_isolation");

    // GetRun against another execution's run, by run id: denied.
    let get_request = serde_json::json!({
        "request_id": "req-2",
        "payload": { "type": "get_run", "id": other_run.id },
    })
    .to_string();
    let parsed = round_trip(server_state.clone(), Some(self_pid()), &get_request).await;
    assert_eq!(parsed["payload"]["type"], "worker_tier_denied");
    assert_eq!(parsed["payload"]["denial"]["verb"], "GetRun");
    assert_eq!(parsed["payload"]["denial"]["reason"], "runtime_isolation");

    // ListRuns against its own execution still works.
    let own_request = serde_json::json!({
        "request_id": "req-3",
        "payload": { "type": "list_runs", "execution_id": mine_execution.id },
    })
    .to_string();
    let parsed = round_trip(server_state, Some(self_pid()), &own_request).await;
    assert_eq!(parsed["payload"]["type"], "runs_list");
}

/// A progression session reading another execution's runs is refused with its
/// own tier's guidance, not the worker-only `boss propose blocked` recovery.
#[tokio::test]
async fn run_scope_refusal_for_progression_peers_is_typed_to_their_tier() {
    let (state, _dir) = test_server_state();
    state.feature_flags.set("worker_rpc_tier", false).unwrap();
    let db = &state.work_db;
    let product = crate::test_support::create_test_product(db);
    let mine_chore = crate::test_support::create_test_chore(db, product.id.clone(), "Mine");
    let mine_execution = crate::test_support::create_ready_chore_execution(db, mine_chore.id);
    let other_chore = crate::test_support::create_test_chore(db, product.id, "Someone else's");
    let other_execution = crate::test_support::create_ready_chore_execution(db, other_chore.id);
    let other_run = db
        .create_run(
            boss_protocol::CreateRunInput::builder()
                .agent_id("agent_1")
                .execution_id(other_execution.id.clone())
                .build(),
        )
        .unwrap();
    let parent = crate::worker_registry::parent_pid(self_pid()).unwrap().unwrap();
    state
        .worker_registry
        .register_progression(parent, &mine_execution.id, &state.work_db)
        .unwrap();

    for (verb, payload) in [
        (
            "ListRuns",
            serde_json::json!({"type": "list_runs", "execution_id": other_execution.id}),
        ),
        ("GetRun", serde_json::json!({"type": "get_run", "id": other_run.id})),
    ] {
        let request = serde_json::json!({"request_id": "scope", "payload": payload}).to_string();
        let parsed = round_trip(state.clone(), Some(self_pid()), &request).await;
        assert_eq!(parsed["payload"]["type"], "worker_tier_denied", "{verb}");
        let denial = &parsed["payload"]["denial"];
        assert_eq!(denial["verb"], verb);
        assert_eq!(denial["reason"], "progression_scope", "{verb}");
        assert!(denial["use_instead"].is_null(), "{verb}: {denial}");
        let message = denial["message"].as_str().unwrap();
        assert!(!message.contains("boss propose blocked"), "{message}");
        assert!(!message.contains("worker sessions"), "{message}");
        assert!(message.contains("progression sessions"), "{message}");
    }
}

#[test]
fn worker_tier_gate_handoff_write_is_audited() {
    let _audit = crate::audit::lock_audit_globals_for_tests();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pid = runtime.block_on(async {
        let (server_state, _dir) = enforcing_state("exec_handoff_worker");
        server_state
            .work_db
            .record_coordinator_tmux_spawn_intent("boss-coordinator", "token", "opus", None)
            .unwrap();
        let credential = server_state
            .work_db
            .coordinator_handoff_authorization()
            .unwrap()
            .unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                // `read` is a shell builtin. Piped stdin without a writer close
                // keeps this process as `/bin/sh` with the original argv, so the
                // credential stays visible for redaction. `sleep` is an external
                // binary: a missing PATH entry or a shell exec of it would exit
                // or rewrite argv before the audit ran.
                "read x; :",
                "boss",
                "handoff",
                "write",
                "--authorization",
                credential.as_str(),
                "-",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let started = std::time::Instant::now();
        loop {
            if boss_engine_utils::process_args::command(pid as u32).is_some() {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "handoff argv fixture exited before its command line was readable"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "handoff argv fixture command line never became readable"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        server_state.worker_registry.unregister(self_pid());
        server_state
            .worker_registry
            .register(pid, "exec_handoff_worker".to_owned());
        let request = serde_json::json!({
            "request_id": "handoff-write",
            "payload": {
                "type": "set_coordinator_handoff",
                "body": "should never land",
                "authorization": credential,
            },
        })
        .to_string();
        let parsed = round_trip(server_state.clone(), Some(pid), &request).await;
        child.kill().ok();
        let _ = child.wait();
        assert_eq!(parsed["payload"]["type"], "worker_tier_denied");
        assert_eq!(parsed["payload"]["denial"]["verb"], "SetCoordinatorHandoff");
        assert_eq!(parsed["payload"]["denial"]["reason"], "coordinator_only");
        match server_state.work_db.coordinator_handoff_state() {
            crate::coordinator_handoff::HandoffState::Missing => {}
            other => panic!("a denied worker write must not persist: {other:?}"),
        }
        (pid, credential)
    });
    let (pid, credential) = pid;
    let log = std::fs::read_to_string(crate::audit::default_audit_log_path().unwrap()).unwrap();
    let events: Vec<serde_json::Value> = log.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let denial = events
        .iter()
        .rev()
        .find(|event| event["event"] == "coordinator_handoff_rejected" && event["reason"] == "worker-tier denial")
        .unwrap_or_else(|| panic!("missing worker-tier denial audit in {log}"));
    assert_eq!(denial["pid"], pid as u32);
    assert_eq!(denial["run_id"], "exec_handoff_worker");
    assert_eq!(denial["caller"]["parent_chain"][0], pid as u32);
    let argv = denial["caller"]["argv"].as_str().unwrap();
    assert!(
        !argv.contains(&credential),
        "the real credential must be redacted from argv: {argv}"
    );
    assert!(
        argv.contains("--authorization [redacted]"),
        "the presented credential is redacted in place: {argv}"
    );
}

#[tokio::test]
async fn critical_update_handler_refuses_worker_and_accepts_human_peer() {
    let (state, _dir) = test_server_state();
    state.feature_flags.set("worker_rpc_tier", false).unwrap();
    let product = crate::test_support::create_test_product(&state.work_db);
    let chore = crate::test_support::create_test_chore(&state.work_db, product.id, "Priority");
    let request = serde_json::json!({
        "request_id": "critical-update",
        "payload": {
            "type": "update_work_item", "id": chore.id,
            "patch": { "priority": "critical" }
        }
    })
    .to_string();
    state.worker_registry.register(self_pid(), "exec_worker".to_owned());
    let refused = round_trip(state.clone(), Some(self_pid()), &request).await;
    assert_eq!(refused["payload"]["type"], "work_error");
    assert!(refused.to_string().contains("`worker`"), "{refused}");
    let WorkItem::Chore(unchanged) = state.work_db.get_work_item(&chore.id).unwrap() else {
        panic!("expected chore");
    };
    assert_eq!(unchanged.priority, "medium");

    state.worker_registry.unregister(self_pid());
    let accepted = round_trip(state.clone(), Some(self_pid()), &request).await;
    assert_eq!(accepted["payload"]["type"], "work_item_updated", "{accepted}");
    let WorkItem::Chore(updated) = state.work_db.get_work_item(&chore.id).unwrap() else {
        panic!("expected chore");
    };
    assert_eq!(updated.priority, "critical");
    assert_eq!(updated.critical_set_by.as_deref(), Some("human"));
}
