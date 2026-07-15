// Behaviour tests for the host-registry RPC surface: the `AddHost`,
// `GetHost`, `ListHosts`, `SetHostEnabled`, `RemoveHost`, `AddHostTag`,
// and `RemoveHostTag` verbs dispatched into `app::hosts`.
//
// Every assertion here goes request-in / response-out: a `FrontendRequest`
// is handed to its handler and the resulting `FrontendEvent` (or the
// `HostSnapshot` a follow-up `GetHost` / `ListHosts` reports) is what gets
// checked. Nothing asserts on which `WorkDb` methods the handler called or
// in what order — those are the implementation of the verb, not the
// contract it owes the app, and pinning them would make every future
// handler refactor a test change. The `WorkDb` layer under these handlers
// has its own direct coverage in `host_registry.rs`.
//
// The add path eagerly contacts the remote host to install the wrapper, so
// these drive `handle_add_host_with` and supply a `HostProvisioner` double
// in place of the real ssh control-master + scp seam. That mirrors how the
// sweep and coordinator suites fake their external seams (`NoopCube`,
// `AlwaysSucceedsCube` in `test_support.rs`) — the point is to make the
// registration policy assertable without an actual machine on the far end.

use super::*;
use crate::app::hosts::{HostProvisioner, ProvisionOutcome};
use crate::protocol::HostSnapshot;

// ── Provisioner doubles ──────────────────────────────────────────────────────

/// Reports `outcome` whatever host it is handed, without touching the
/// network. One double covers every branch of the real seam: `Ok` for a
/// clean push, `Failed` for any of its failure modes (control master
/// refused, scp failed, `cube` not on PATH), `Skipped` for "no
/// control-socket dir, don't even try".
struct FixedProvisioner(ProvisionOutcome);

#[async_trait::async_trait]
impl HostProvisioner for FixedProvisioner {
    async fn provision(&self, _host_id: &str, _ssh_target: &str) -> ProvisionOutcome {
        self.0.clone()
    }
}

/// Shorthand for the common `FixedProvisioner(ProvisionOutcome::Ok)`.
fn provision_ok() -> FixedProvisioner {
    FixedProvisioner(ProvisionOutcome::Ok)
}

/// A [`FixedProvisioner`] reporting failure with `detail` as the
/// operator-facing reason.
fn provision_fails(detail: &str) -> FixedProvisioner {
    FixedProvisioner(ProvisionOutcome::Failed(detail.to_owned()))
}

/// Fails the test if the add path tries to contact a remote at all. Used
/// where registration must be rejected before any host is touched.
struct ProvisionMustNotBeCalled;

#[async_trait::async_trait]
impl HostProvisioner for ProvisionMustNotBeCalled {
    async fn provision(&self, host_id: &str, _ssh_target: &str) -> ProvisionOutcome {
        panic!("add path must not contact host {host_id} for a request it rejects");
    }
}

// ── Request plumbing ─────────────────────────────────────────────────────────

/// Build a per-request `Dispatch` the way `handle_frontend_connection`
/// does for a real socket frame.
fn host_dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// The single response a handler enqueued on `sink`. Closes the sink
/// first so a handler that replied with nothing surfaces as this panic
/// rather than hanging the whole shard until Bazel's timeout — `next`
/// only yields `None` once a closed sink runs dry, and blocks forever
/// otherwise. Same reason `t06`'s `drain_topic_events` closes before
/// draining.
async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

/// Drive one host verb through its handler and return the response it
/// sent. Mirrors `app.rs`'s dispatch table for the host verbs; `AddHost`
/// goes through [`add_host`] instead so its provisioner can be supplied.
async fn call(state: &Arc<ServerState>, req: FrontendRequest) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = host_dispatch(state, &sink);
    match req {
        r @ FrontendRequest::ListHosts => hosts::handle_list_hosts(ctx, r).await,
        r @ FrontendRequest::GetHost { .. } => hosts::handle_get_host(ctx, r).await,
        r @ FrontendRequest::SetHostEnabled { .. } => hosts::handle_set_host_enabled(ctx, r).await,
        r @ FrontendRequest::RemoveHost { .. } => hosts::handle_remove_host(ctx, r).await,
        r @ FrontendRequest::AddHostTag { .. } => hosts::handle_add_host_tag(ctx, r).await,
        r @ FrontendRequest::RemoveHostTag { .. } => hosts::handle_remove_host_tag(ctx, r).await,
        other => panic!("not a host verb: {other:?}"),
    }
    sole_response(&sink).await
}

/// Drive `AddHost` with `provisioner` standing in for the remote seam.
async fn add_host(
    state: &Arc<ServerState>,
    provisioner: &dyn HostProvisioner,
    id: &str,
    ssh_target: &str,
    pool_size: i64,
    tags: &[&str],
) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = host_dispatch(state, &sink);
    hosts::handle_add_host_with(
        ctx,
        FrontendRequest::AddHost {
            id: id.to_owned(),
            ssh_target: ssh_target.to_owned(),
            pool_size,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        },
        provisioner,
    )
    .await;
    sole_response(&sink).await
}

// ── Response accessors ───────────────────────────────────────────────────────

/// The `HostSnapshot` from a `HostResult` reply — the variant `AddHost`
/// and `GetHost` owe their caller (protocol `wire.rs`). Kept distinct from
/// [`expect_host_updated`] because the app dispatches on the variant:
/// `HostResult` means "here is the host you asked for", `HostUpdated`
/// means "refresh this row in place". A handler that swapped one for the
/// other would still hand back an identical snapshot, so only the variant
/// itself pins that contract.
fn expect_host_result(event: FrontendEvent) -> HostSnapshot {
    match event {
        FrontendEvent::HostResult { host } => host,
        other => panic!("expected HostResult, got {other:?}"),
    }
}

/// The `HostSnapshot` from a `HostUpdated` reply — the variant
/// `SetHostEnabled` / `AddHostTag` / `RemoveHostTag` owe their caller.
/// See [`expect_host_result`] for why the two are not interchangeable.
fn expect_host_updated(event: FrontendEvent) -> HostSnapshot {
    match event {
        FrontendEvent::HostUpdated { host } => host,
        other => panic!("expected HostUpdated, got {other:?}"),
    }
}

/// The message from an `Error` reply, panicking if the verb reported
/// success instead — which is the failure mode these tests exist to catch.
fn expect_error(event: FrontendEvent) -> String {
    match event {
        FrontendEvent::Error { message } => message,
        other => panic!("expected an Error response, got {other:?}"),
    }
}

/// Snapshots from a `ListHosts` reply, minus the built-in `local` host
/// that `WorkDb::open` seeds into every fresh DB.
fn expect_remote_hosts(event: FrontendEvent) -> Vec<HostSnapshot> {
    match event {
        FrontendEvent::HostsList { hosts } => hosts.into_iter().filter(|h| h.id != "local").collect(),
        other => panic!("expected HostsList, got {other:?}"),
    }
}

/// `(capability, source)` pairs on a snapshot, for asserting tag state.
fn caps(host: &HostSnapshot) -> Vec<(String, String)> {
    host.capabilities
        .iter()
        .map(|c| (c.capability.clone(), c.source.clone()))
        .collect()
}

/// Plant an `auto`-sourced capability row on `host_id`. No RPC creates
/// one (the engine discovers them on its own heartbeat), so a test that
/// needs a non-user capability to contrast against has to seed it.
fn seed_auto_capability(state: &Arc<ServerState>, host_id: &str, capability: &str) {
    crate::test_support::insert_host_capability(&state.work_db, host_id, capability, "auto");
}

// ── Add / get / list round-trip ──────────────────────────────────────────────

#[tokio::test]
async fn added_host_round_trips_through_get_and_list() {
    // The core registration contract: what AddHost was given is what
    // GetHost and ListHosts report back afterwards.
    let (state, _dir) = test_server_state();

    let added = expect_host_result(
        add_host(
            &state,
            &provision_ok(),
            "zakalwe",
            "user@zakalwe",
            4,
            &["os=macos", "gpu=none"],
        )
        .await,
    );
    assert_eq!(added.id, "zakalwe");
    assert_eq!(added.ssh_target.as_deref(), Some("user@zakalwe"));
    assert_eq!(added.pool_size, 4);
    assert!(added.enabled, "a successfully provisioned host must be enabled");
    assert_eq!(
        caps(&added),
        vec![
            ("gpu=none".to_owned(), "user".to_owned()),
            ("os=macos".to_owned(), "user".to_owned()),
        ],
        "tags passed to AddHost must come back as user-sourced capabilities",
    );

    let fetched = expect_host_result(
        call(
            &state,
            FrontendRequest::GetHost {
                id: "zakalwe".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(fetched, added, "GetHost must report exactly what AddHost returned");

    let listed = expect_remote_hosts(call(&state, FrontendRequest::ListHosts).await);
    assert_eq!(listed, vec![added], "ListHosts must include the newly added host");
}

#[tokio::test]
async fn list_hosts_reports_every_added_host_with_its_own_capabilities() {
    // Regression guard for the per-host capability join in ListHosts:
    // each snapshot must carry its own tags, not the first host's or a
    // merged set.
    let (state, _dir) = test_server_state();
    add_host(&state, &provision_ok(), "zakalwe", "user@zakalwe", 2, &["os=macos"]).await;
    add_host(&state, &provision_ok(), "sleeper", "user@sleeper", 1, &["os=linux"]).await;

    let listed = expect_remote_hosts(call(&state, FrontendRequest::ListHosts).await);
    let by_id = |id: &str| -> HostSnapshot { listed.iter().find(|h| h.id == id).expect("host listed").clone() };

    assert_eq!(listed.len(), 2);
    assert_eq!(
        caps(&by_id("zakalwe")),
        vec![("os=macos".to_owned(), "user".to_owned())]
    );
    assert_eq!(
        caps(&by_id("sleeper")),
        vec![("os=linux".to_owned(), "user".to_owned())]
    );
    assert_eq!(by_id("sleeper").pool_size, 1);
}

// ── Auto-disable on provisioning failure ─────────────────────────────────────

#[tokio::test]
async fn add_host_disables_a_host_whose_remote_provisioning_fails() {
    // The highest-value behaviour in the module. A host the engine could
    // not provision must never be left enabled: an enabled-but-broken
    // host gets offered dispatch slots and fails every one of them (the
    // anaplian incident). The response the app renders has to show that
    // state immediately, not on some later refresh.
    let (state, _dir) = test_server_state();

    let added = expect_host_result(
        add_host(
            &state,
            &provision_fails("cube not invocable via non-interactive ssh: exit 127"),
            "anaplian",
            "user@anaplian",
            2,
            &[],
        )
        .await,
    );
    assert!(
        !added.enabled,
        "a host whose provisioning failed must be reported disabled, got {added:?}",
    );
    assert_eq!(
        added.last_error_text.as_deref(),
        Some("cube not invocable via non-interactive ssh: exit 127"),
        "the provisioning failure reason must reach the app, not just the log",
    );

    // The disable is persisted, not just decorated onto the add response.
    let fetched = expect_host_result(
        call(
            &state,
            FrontendRequest::GetHost {
                id: "anaplian".to_owned(),
            },
        )
        .await,
    );
    assert!(!fetched.enabled, "the host must still read as disabled on re-fetch");
    assert_eq!(fetched.last_error_text, added.last_error_text);
}

#[tokio::test]
async fn add_host_keeps_a_failed_hosts_row_and_tags() {
    // Disabling is not the same as rejecting: the row survives so the
    // operator can see it, fix the remote, and re-enable it — rather than
    // having to re-register from scratch.
    let (state, _dir) = test_server_state();
    add_host(
        &state,
        &provision_fails("wrapper push failed (connection_lost): broken pipe"),
        "zakalwe",
        "user@zakalwe",
        3,
        &["os=macos"],
    )
    .await;

    let listed = expect_remote_hosts(call(&state, FrontendRequest::ListHosts).await);
    assert_eq!(listed.len(), 1, "a failed host must still be registered");
    assert_eq!(listed[0].pool_size, 3);
    assert_eq!(caps(&listed[0]), vec![("os=macos".to_owned(), "user".to_owned())]);

    let re_enabled = expect_host_updated(
        call(
            &state,
            FrontendRequest::SetHostEnabled {
                id: "zakalwe".to_owned(),
                enabled: true,
            },
        )
        .await,
    );
    assert!(
        re_enabled.enabled,
        "the operator must be able to re-enable a fixed host"
    );
}

#[tokio::test]
async fn add_host_leaves_the_host_enabled_when_provisioning_is_skipped() {
    // "Not attempted" must not be conflated with "attempted and failed" —
    // skipping the push leaves the host as registered, with no invented
    // error text.
    let (state, _dir) = test_server_state();

    let added = expect_host_result(
        add_host(
            &state,
            &FixedProvisioner(ProvisionOutcome::Skipped),
            "zakalwe",
            "user@zakalwe",
            2,
            &[],
        )
        .await,
    );
    assert!(added.enabled, "a host that was never contacted must stay enabled");
    assert_eq!(added.last_error_text, None, "skipping must not fabricate an error");
}

#[tokio::test]
async fn add_host_rejects_a_duplicate_id_without_contacting_the_host() {
    // Registration is rejected on the DB insert, so a duplicate must
    // never reach the remote seam — and must not disturb the existing
    // host's state.
    let (state, _dir) = test_server_state();
    add_host(&state, &provision_ok(), "zakalwe", "user@zakalwe", 2, &[]).await;

    let message = expect_error(
        add_host(
            &state,
            &ProvisionMustNotBeCalled,
            "zakalwe",
            "user@other",
            8,
            &["os=linux"],
        )
        .await,
    );
    assert!(message.contains("already exists"), "got: {message}");

    let existing = expect_host_result(
        call(
            &state,
            FrontendRequest::GetHost {
                id: "zakalwe".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(existing.ssh_target.as_deref(), Some("user@zakalwe"));
    assert_eq!(
        existing.pool_size, 2,
        "the rejected add must not have overwritten fields"
    );
    assert!(caps(&existing).is_empty(), "the rejected add must not have added tags");
}

// ── Not-found errors ─────────────────────────────────────────────────────────

#[tokio::test]
async fn get_host_reports_not_found_for_an_unknown_id() {
    let (state, _dir) = test_server_state();
    let message = expect_error(call(&state, FrontendRequest::GetHost { id: "ghost".to_owned() }).await);
    assert!(message.contains("not found"), "got: {message}");
    assert!(message.contains("ghost"), "the error must name the host: {message}");
}

#[tokio::test]
async fn set_host_enabled_reports_not_found_for_an_unknown_id() {
    let (state, _dir) = test_server_state();
    let message = expect_error(
        call(
            &state,
            FrontendRequest::SetHostEnabled {
                id: "ghost".to_owned(),
                enabled: false,
            },
        )
        .await,
    );
    assert!(message.contains("not found"), "got: {message}");
}

#[tokio::test]
async fn remove_host_reports_not_found_for_an_unknown_id() {
    let (state, _dir) = test_server_state();
    let message = expect_error(call(&state, FrontendRequest::RemoveHost { id: "ghost".to_owned() }).await);
    assert!(message.contains("not found"), "got: {message}");
}

#[tokio::test]
async fn add_host_tag_reports_the_host_as_not_found_for_an_unknown_host() {
    let (state, _dir) = test_server_state();
    let message = expect_error(
        call(
            &state,
            FrontendRequest::AddHostTag {
                host_id: "ghost".to_owned(),
                tag: "os=macos".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(message, "host 'ghost' not found");
}

#[tokio::test]
async fn remove_host_tag_blames_the_tag_not_the_host_for_an_unknown_host() {
    // Pins a real inconsistency rather than papering over it. AddHostTag
    // checks host existence and says so; RemoveHostTag does not, so for a
    // host that does not exist it reports the *tag* as missing — telling
    // the operator the host is fine and merely lacks the tag. The two tag
    // verbs therefore disagree about identical state, which an operator
    // (or the app, racing a concurrent RemoveHost) can hit.
    //
    // Asserting the exact string is the point: a loose `contains("not
    // found")` passes for either message, so it could not tell this apart
    // from the tag-genuinely-absent case below and would silently bless
    // the divergence as covered. If RemoveHostTag is later taught to check
    // the host first, this test fails and is the place to record the new
    // contract.
    let (state, _dir) = test_server_state();
    let message = expect_error(
        call(
            &state,
            FrontendRequest::RemoveHostTag {
                host_id: "ghost".to_owned(),
                tag: "os=macos".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(message, "capability 'os=macos' not found on host 'ghost'");
}

#[tokio::test]
async fn remove_host_tag_reports_not_found_for_a_tag_the_host_does_not_have() {
    let (state, _dir) = test_server_state();
    add_host(&state, &provision_ok(), "zakalwe", "user@zakalwe", 2, &[]).await;

    let message = expect_error(
        call(
            &state,
            FrontendRequest::RemoveHostTag {
                host_id: "zakalwe".to_owned(),
                tag: "os=macos".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(message, "capability 'os=macos' not found on host 'zakalwe'");
}

// ── Tag semantics ────────────────────────────────────────────────────────────

#[tokio::test]
async fn host_tags_round_trip_through_add_and_remove() {
    // AddHostTag then RemoveHostTag must both be visible on the snapshot
    // each verb replies with — the app renders that reply directly rather
    // than re-fetching.
    let (state, _dir) = test_server_state();
    add_host(&state, &provision_ok(), "zakalwe", "user@zakalwe", 2, &[]).await;

    let tagged = expect_host_updated(
        call(
            &state,
            FrontendRequest::AddHostTag {
                host_id: "zakalwe".to_owned(),
                tag: "os=macos".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(caps(&tagged), vec![("os=macos".to_owned(), "user".to_owned())]);

    let untagged = expect_host_updated(
        call(
            &state,
            FrontendRequest::RemoveHostTag {
                host_id: "zakalwe".to_owned(),
                tag: "os=macos".to_owned(),
            },
        )
        .await,
    );
    assert!(
        caps(&untagged).is_empty(),
        "the removed tag must be gone from the reply, got {:?}",
        caps(&untagged),
    );

    let fetched = expect_host_result(
        call(
            &state,
            FrontendRequest::GetHost {
                id: "zakalwe".to_owned(),
            },
        )
        .await,
    );
    assert!(caps(&fetched).is_empty(), "the removal must be persisted");
}

#[tokio::test]
async fn user_tags_are_distinguishable_from_auto_discovered_capabilities() {
    // Both kinds of capability share one list on the snapshot, so `source`
    // is the only thing telling the app which ones a human can remove.
    let (state, _dir) = test_server_state();
    add_host(&state, &provision_ok(), "zakalwe", "user@zakalwe", 2, &[]).await;
    seed_auto_capability(&state, "zakalwe", "arch=arm64");

    let tagged = expect_host_updated(
        call(
            &state,
            FrontendRequest::AddHostTag {
                host_id: "zakalwe".to_owned(),
                tag: "role=builder".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(
        caps(&tagged),
        vec![
            ("arch=arm64".to_owned(), "auto".to_owned()),
            ("role=builder".to_owned(), "user".to_owned()),
        ],
        "the snapshot must carry each capability's source",
    );

    // RemoveHostTag is a user-tag verb: an auto capability is engine-owned
    // and must be refused rather than silently dropped.
    let message = expect_error(
        call(
            &state,
            FrontendRequest::RemoveHostTag {
                host_id: "zakalwe".to_owned(),
                tag: "arch=arm64".to_owned(),
            },
        )
        .await,
    );
    assert!(message.contains("auto-discovered"), "got: {message}");

    let fetched = expect_host_result(
        call(
            &state,
            FrontendRequest::GetHost {
                id: "zakalwe".to_owned(),
            },
        )
        .await,
    );
    assert_eq!(
        caps(&fetched),
        vec![
            ("arch=arm64".to_owned(), "auto".to_owned()),
            ("role=builder".to_owned(), "user".to_owned()),
        ],
        "the refused removal must leave both capabilities intact",
    );
}

// ── Enable/disable and removal ───────────────────────────────────────────────

#[tokio::test]
async fn set_host_enabled_round_trips_disable_and_re_enable() {
    let (state, _dir) = test_server_state();
    add_host(&state, &provision_ok(), "zakalwe", "user@zakalwe", 2, &[]).await;

    let disabled = expect_host_updated(
        call(
            &state,
            FrontendRequest::SetHostEnabled {
                id: "zakalwe".to_owned(),
                enabled: false,
            },
        )
        .await,
    );
    assert!(!disabled.enabled);

    let listed = expect_remote_hosts(call(&state, FrontendRequest::ListHosts).await);
    assert!(
        !listed[0].enabled,
        "disabling must be visible on the list, not just the reply",
    );

    let enabled = expect_host_updated(
        call(
            &state,
            FrontendRequest::SetHostEnabled {
                id: "zakalwe".to_owned(),
                enabled: true,
            },
        )
        .await,
    );
    assert!(enabled.enabled);
}

#[tokio::test]
async fn removed_host_disappears_from_list_along_with_its_capabilities() {
    // Removal has to take the capability rows with it: leaving them behind
    // would let a later host registered under the same id silently inherit
    // the dead host's tags — and therefore its dispatch eligibility.
    let (state, _dir) = test_server_state();
    add_host(
        &state,
        &provision_ok(),
        "zakalwe",
        "user@zakalwe",
        2,
        &["os=macos", "role=builder"],
    )
    .await;

    let removed = call(
        &state,
        FrontendRequest::RemoveHost {
            id: "zakalwe".to_owned(),
        },
    )
    .await;
    match removed {
        FrontendEvent::HostRemoved { id } => assert_eq!(id, "zakalwe"),
        other => panic!("expected HostRemoved, got {other:?}"),
    }

    let listed = expect_remote_hosts(call(&state, FrontendRequest::ListHosts).await);
    assert!(listed.is_empty(), "a removed host must not be listed, got {listed:?}");

    let message = expect_error(
        call(
            &state,
            FrontendRequest::GetHost {
                id: "zakalwe".to_owned(),
            },
        )
        .await,
    );
    assert!(message.contains("not found"), "got: {message}");

    // Re-registering under the same id must start from a clean slate.
    let re_added = expect_host_result(add_host(&state, &provision_ok(), "zakalwe", "user@zakalwe", 2, &[]).await);
    assert!(
        caps(&re_added).is_empty(),
        "the removed host's capability rows must not outlive it, got {:?}",
        caps(&re_added),
    );
}
