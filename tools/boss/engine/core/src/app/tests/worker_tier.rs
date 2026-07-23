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
    let (server_state, _dir) = enforcing_state("exec_abc");
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
    server_state.worker_registry.register(self_pid(), "exec_abc".to_owned());
    server_state.feature_flags.set("worker_rpc_tier", false).unwrap();
    let parsed = round_trip(server_state, Some(self_pid()), &request).await;
    let runs = parsed["payload"]["runs"].as_array().unwrap();
    assert_eq!(
        runs[0]["transcript_path"].as_str(),
        run.transcript_path.as_deref(),
        "with worker_rpc_tier off, sanitization must be off too",
    );
}
