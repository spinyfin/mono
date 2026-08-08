//! Integration tests for the worker-facing remediation control verbs —
//! `boss engine ci …` (`classify` / `mark-failed` / `mark-retriggered` /
//! `mark-noop` / `mark-succeeded-via-rebase`) and `boss engine conflicts …`
//! (`mark-failed` / `list` / `show` / `retry` / `abandon`).
//!
//! Split out of the parent `control_verbs` integration test, which had grown
//! past the repo's 3000-line file-size check; this half is one coherent
//! surface (the remediation ledgers) and carries its own fixtures
//! (`FakeCiProbe`, `seed_and_claim_via_rebase`, `seed_two_conflict_resolutions`).
//! Behaviourally a pure move — the shared helpers (`spawn_engine`,
//! `TestEngine`) still come from the parent via `use super::*`.

use super::*;

/// End-to-end smoke for the worker-facing `boss engine conflicts
/// mark-failed` surface (chore #9 of the merge-conflict design's
/// Phase 3): seed a `conflict_resolutions` row, send the RPC, and
/// assert that the engine flips the row to `failed` with the supplied
/// reason. Also covers the "unknown attempt id" arm and the
/// "already-terminal row" idempotency arm.
#[tokio::test]
async fn mark_conflict_resolution_failed_flips_attempt_status() -> Result<()> {
    let engine = spawn_engine().await?;

    // Seed a product → in_review chore → conflict_resolutions row by
    // talking to the engine's own WorkDb. We avoid the RPC surface
    // for the seed because there's no public protocol-level
    // `insert_conflict_resolution`; that's an engine-internal flow.
    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C")
            .autostart(false)
            .build(),
    )?;
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some("https://github.com/foo/bar/pull/42".to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    work_db.mark_chore_blocked_merge_conflict(&chore.id, "https://github.com/foo/bar/pull/42")?;
    let attempt = work_db
        .insert_conflict_resolution(boss_engine::work::ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/42".to_owned(),
            pr_number: 42,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("abc123".to_owned()),
            head_sha_before: Some("def456".to_owned()),
        })?
        .expect("insert should succeed on a fresh row");

    // Drive the engine's WorkDb through a fresh connection of the
    // engine binary by talking to its frontend socket — release the
    // direct handle so its lock doesn't clash with the engine's.
    drop(work_db);

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: attempt.id.clone(),
            reason: "product_decision_required".to_owned(),
        })
        .await?;
    let flipped = match response {
        FrontendEvent::ConflictResolutionMarkedFailed { attempt } => attempt,
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(flipped.id, attempt.id);
    assert_eq!(flipped.status, "failed");
    assert_eq!(flipped.failure_reason.as_deref(), Some("product_decision_required"),);
    assert!(flipped.finished_at.is_some(), "finished_at must be stamped");

    // Idempotency: a second call on a now-terminal row surfaces a
    // structured error rather than silently no-op'ing.
    let response = client
        .send_request(&FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: attempt.id.clone(),
            reason: "ignored".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("already terminal") || message.contains("unknown"),
                "expected terminal/unknown message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }

    // Unknown attempt id: same error surface, distinguishable by the
    // bogus id in the message body.
    let response = client
        .send_request(&FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: "crz_does_not_exist".to_owned(),
            reason: "nope".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("crz_does_not_exist"),
                "expected message to name the bogus id, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

/// `MarkCiRemediationNoop` is the *validated* terminal signal, but its
/// pre-probe guards are deterministic without `gh`: a forged id errors,
/// a terminal row is rejected, an already-succeeded row echoes a HONORED
/// receipt, and a merge-queue rebounce is rejected outright (its failure
/// lives on a synthetic merge commit, not the PR head, so head-branch CI
/// can't validate it). These exercise the full request → dispatch →
/// handler → response wire without reaching the live-CI probe.
#[tokio::test]
async fn mark_ci_remediation_noop_pre_probe_guards() -> Result<()> {
    let engine = spawn_engine().await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C")
            .autostart(false)
            .build(),
    )?;
    let pr = "https://github.com/foo/bar/pull/77";
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr.to_owned()),
            ..WorkItemPatch::default()
        },
    )?;

    let seed = |head_sha: &str, failure_kind: &str, before: Option<String>| {
        work_db
            .insert_ci_remediation(boss_engine::work::CiRemediationInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: pr.to_owned(),
                pr_number: 77,
                head_branch: "feature".to_owned(),
                head_sha_at_trigger: head_sha.to_owned(),
                attempt_kind: "fix".to_owned(),
                consumes_budget: 1,
                failed_checks: "[]".to_owned(),
                failure_kind: failure_kind.to_owned(),
                before_commit_sha: before,
            })
            .map(|opt| opt.expect("insert should succeed on a fresh row"))
    };

    // (a) already-succeeded → echoed as a HONORED receipt.
    let succeeded = seed("sha-a", "pr_branch_ci", None)?;
    work_db
        .mark_ci_remediation_succeeded(&succeeded.id, Some("sha-a-green"))?
        .expect("flip to succeeded");
    // (b) merge-queue rebounce, still pending → rejected outright.
    let rebounce = seed("sha-b", "merge_queue_rebounce", Some("synthetic-merge-sha".to_owned()))?;
    // (c) terminal failed → rejected as already terminal.
    let failed = seed("sha-c", "pr_branch_ci", None)?;
    work_db
        .mark_ci_remediation_failed(&failed.id, "unfixable")?
        .expect("flip to failed");
    // (e) Trunk queue eviction, still pending → rejected outright, exactly
    // like the merge-queue rebounce case in (b).
    let trunk_eviction = seed("sha-e", "trunk_queue_eviction", None)?;

    drop(work_db);

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // (a) already-succeeded → honored echo carrying the recorded SHA.
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationNoop {
            attempt_id: succeeded.id.clone(),
            observed_sha: Some("sha-a-green".to_owned()),
            reason: Some("already-green".to_owned()),
        })
        .await?;
    match response {
        FrontendEvent::CiRemediationNoopValidated {
            attempt, validated_sha, ..
        } => {
            assert_eq!(attempt.id, succeeded.id);
            assert_eq!(attempt.status, "succeeded");
            assert_eq!(validated_sha.as_deref(), Some("sha-a-green"));
        }
        other => return Err(anyhow!("expected NoopValidated echo, got: {other:?}")),
    }

    // (b) rebounce → rejected, and the engine never probes head-branch CI.
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationNoop {
            attempt_id: rebounce.id.clone(),
            observed_sha: None,
            reason: None,
        })
        .await?;
    match response {
        FrontendEvent::CiRemediationNoopRejected { attempt_id, status, .. } => {
            assert_eq!(attempt_id, rebounce.id);
            assert!(
                status.contains("synthetic/ephemeral commit"),
                "rebounce rejection should explain why: {status}"
            );
        }
        other => return Err(anyhow!("expected NoopRejected for rebounce, got: {other:?}")),
    }

    // (c) terminal failed → WorkError naming "already terminal".
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationNoop {
            attempt_id: failed.id.clone(),
            observed_sha: None,
            reason: None,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("already terminal"), "got: {message}");
        }
        other => return Err(anyhow!("expected WorkError for terminal attempt, got: {other:?}")),
    }

    // (d) unknown id → WorkError naming the bogus id.
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationNoop {
            attempt_id: "cir_does_not_exist".to_owned(),
            observed_sha: None,
            reason: None,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("cir_does_not_exist"), "got: {message}");
        }
        other => return Err(anyhow!("expected WorkError for unknown id, got: {other:?}")),
    }

    // The guard changed from an equality check on `"merge_queue_rebounce"`
    // to `is_queue_side_failure_kind`, so pin the Trunk-eviction sibling (e)
    // separately: a future narrowing of that predicate must not silently
    // reopen the bypass for this kind.
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationNoop {
            attempt_id: trunk_eviction.id.clone(),
            observed_sha: None,
            reason: None,
        })
        .await?;
    match response {
        FrontendEvent::CiRemediationNoopRejected { attempt_id, status, .. } => {
            assert_eq!(attempt_id, trunk_eviction.id);
            assert!(
                status.contains("synthetic/ephemeral commit"),
                "trunk eviction rejection should explain why: {status}"
            );
        }
        other => return Err(anyhow!("expected NoopRejected for trunk eviction, got: {other:?}")),
    }

    Ok(())
}

/// `MarkCiRemediationSucceededViaRebase` shares the exact verify-before-honor
/// gate (PR spinyfin/mono#2023) as `MarkCiRemediationNoop`:
/// the engine no longer takes the worker's "rebase fixed it" claim on
/// say-so — it re-probes live CI for the PR's current head SHA and only
/// honors a verified-green claim. The pre-probe guards are deterministic
/// without `gh`: a forged id errors, a terminal-but-not-`succeeded` row is
/// rejected outright (the live probe is never reached), and an
/// already-`succeeded` row echoes a HONORED receipt without double-refunding
/// the budget counter. These exercise the full request → dispatch → handler
/// → response wire; the green/pending/red live-CI classification itself is
/// the shared `classify_noop_validation` decision function, already covered
/// by its unit tests in `ci_watch_tests.rs`.
#[tokio::test]
async fn mark_ci_remediation_succeeded_via_rebase_pre_probe_guards() -> Result<()> {
    let engine = spawn_engine().await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C")
            .autostart(false)
            .build(),
    )?;
    let pr = "https://github.com/foo/bar/pull/88";
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr.to_owned()),
            ..WorkItemPatch::default()
        },
    )?;

    let seed = |head_sha: &str| {
        work_db
            .insert_ci_remediation(boss_engine::work::CiRemediationInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: pr.to_owned(),
                pr_number: 88,
                head_branch: "feature".to_owned(),
                head_sha_at_trigger: head_sha.to_owned(),
                attempt_kind: "fix".to_owned(),
                consumes_budget: 1,
                failed_checks: "[]".to_owned(),
                failure_kind: "pr_branch_ci".to_owned(),
                before_commit_sha: None,
            })
            .map(|opt| opt.expect("insert should succeed on a fresh row"))
    };

    // (a) already-succeeded → echoed as a HONORED receipt, no double refund.
    let succeeded = seed("sha-a")?;
    work_db
        .mark_ci_remediation_succeeded_via_rebase(&succeeded.id, None)?
        .expect("flip to succeeded_via_rebase");
    // (b) terminal failed → rejected as already terminal; the live-CI probe
    //     is never reached for a row that isn't the worker's live attempt.
    let failed = seed("sha-b")?;
    work_db
        .mark_ci_remediation_failed(&failed.id, "unfixable")?
        .expect("flip to failed");

    drop(work_db);

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // (a) already-succeeded → honored echo; budget_refunded=false since it
    //     was already refunded by the original, verified call.
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
            attempt_id: succeeded.id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::CiRemediationSucceededViaRebase {
            attempt,
            budget_refunded,
        } => {
            assert_eq!(attempt.id, succeeded.id);
            assert_eq!(attempt.status, "succeeded");
            assert!(
                !budget_refunded,
                "idempotent echo must not re-refund the budget counter"
            );
        }
        other => return Err(anyhow!("expected SucceededViaRebase echo, got: {other:?}")),
    }

    // (b) terminal failed → WorkError naming "already terminal".
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
            attempt_id: failed.id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("already terminal"), "got: {message}");
        }
        other => return Err(anyhow!("expected WorkError for terminal attempt, got: {other:?}")),
    }

    // (c) unknown id → WorkError naming the bogus id.
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
            attempt_id: "cir_does_not_exist".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("cir_does_not_exist"), "got: {message}");
        }
        other => return Err(anyhow!("expected WorkError for unknown id, got: {other:?}")),
    }

    Ok(())
}

/// Fake [`MergeProbe`] returning a fixed CI status on a fixed head SHA for
/// every PR url — drives the green/pending/red branches of the
/// `MarkCiRemediationSucceededViaRebase` / `MarkCiRemediationNoop`
/// validation gates deterministically, without shelling out to `gh`. The
/// engine reads `ServerState::merge_probe` (rather than constructing a
/// `CommandMergeProbe` itself) specifically so tests can inject this.
struct FakeCiProbe {
    ci: OpenPrCiStatus,
    head_sha: String,
}

#[async_trait]
impl MergeProbe for FakeCiProbe {
    async fn probe(&self, pr_url: &str) -> anyhow::Result<PrLifecycleProbe> {
        Ok(PrLifecycleProbe::builder()
            .url(pr_url.to_owned())
            .state(PrLifecycleState::Open(OpenPrStatus {
                mergeability: OpenPrMergeability::Clean,
                ci: self.ci.clone(),
            }))
            .head_ref_oid(self.head_sha.clone())
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .build())
    }
}

/// Shared setup for the `mark_ci_remediation_succeeded_via_rebase_*_head`
/// tests below: spawn an engine wired with a [`FakeCiProbe`] reporting `ci`
/// on `head_sha` for every PR, seed one `pending`/`fix`-kind ci_remediation
/// attempt, and claim it via `MarkCiRemediationSucceededViaRebase`. Each
/// case gets its own engine (rather than three sequential spawns sharing
/// one test) to keep resource usage in line with every other test in this
/// suite when the full binary runs with many tests in flight concurrently.
///
/// Returns the `TestEngine` too (not just the `WorkDb` handle open against
/// its backing file) — the engine owns the `TempDir` that backs
/// `state.db`, and dropping it deletes that directory, so the caller must
/// keep it alive for as long as it still wants to touch the DB.
async fn seed_and_claim_via_rebase(
    ci: OpenPrCiStatus,
    head_sha: &str,
) -> Result<(FrontendEvent, TestEngine, WorkDb, String)> {
    let engine = TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        merge_probe: Some(Arc::new(FakeCiProbe {
            ci,
            head_sha: head_sha.to_owned(),
        })),
        ..Default::default()
    })
    .await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C")
            .autostart(false)
            .build(),
    )?;
    let pr = "https://github.com/foo/bar/pull/99";
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr.to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    let attempt = work_db
        .insert_ci_remediation(boss_engine::work::CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr.to_owned(),
            pr_number: 99,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "sha-at-trigger".to_owned(),
            attempt_kind: "fix".to_owned(),
            consumes_budget: 1,
            failed_checks: "[]".to_owned(),
            failure_kind: "pr_branch_ci".to_owned(),
            before_commit_sha: None,
        })?
        .expect("insert should succeed on a fresh row");

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
            attempt_id: attempt.id.clone(),
        })
        .await?;
    Ok((response, engine, work_db, attempt.id))
}

/// A claim verified green on the current head is honored: the row flips to
/// `succeeded`, budget is refunded, and `head_sha_after` is stamped with
/// the SHA the gate actually verified (PR spinyfin/mono#2023). This is the
/// seam the PR review flagged as missing:
/// `MergeProbe` already has test doubles (`completion.rs`'s
/// `with_merge_probe` fixtures); the gap was that the handler constructed
/// its own `CommandMergeProbe` instead of reading one off `ServerState`
/// where a test could substitute it — fixed by `ServerState::merge_probe` /
/// `TestEngineOptions::merge_probe`.
#[tokio::test]
async fn mark_ci_remediation_succeeded_via_rebase_honors_verified_green_head() -> Result<()> {
    let (response, _engine, work_db, attempt_id) =
        seed_and_claim_via_rebase(OpenPrCiStatus::Clean, "sha-green").await?;
    match response {
        FrontendEvent::CiRemediationSucceededViaRebase {
            attempt,
            budget_refunded,
        } => {
            assert_eq!(attempt.status, "succeeded");
            assert!(budget_refunded, "fix-kind attempt with consumes_budget=1 must refund");
            assert_eq!(attempt.head_sha_after.as_deref(), Some("sha-green"));
        }
        other => return Err(anyhow!("expected honored SucceededViaRebase, got: {other:?}")),
    }
    let row = work_db.get_ci_remediation(&attempt_id)?.expect("row still exists");
    assert_eq!(
        row.status, "succeeded",
        "DB row must actually be succeeded, not just the receipt"
    );
    Ok(())
}

/// A claim made while checks are still pending/in-flight on the current
/// head is rejected: the row stays actionable (not `succeeded`, not
/// `failed`) and no `CiRemediationSucceeded` event is published.
#[tokio::test]
async fn mark_ci_remediation_succeeded_via_rebase_rejects_pending_head() -> Result<()> {
    let (response, _engine, work_db, attempt_id) =
        seed_and_claim_via_rebase(OpenPrCiStatus::InFlight, "sha-pending").await?;
    match response {
        FrontendEvent::CiRemediationSucceededViaRebaseRejected { status, live_sha, .. } => {
            assert!(
                status.contains("pending") || status.contains("in-flight"),
                "got: {status}"
            );
            assert_eq!(live_sha.as_deref(), Some("sha-pending"));
        }
        other => {
            return Err(anyhow!(
                "expected SucceededViaRebaseRejected for pending CI, got: {other:?}"
            ));
        }
    }
    let row = work_db.get_ci_remediation(&attempt_id)?.expect("row still exists");
    assert_eq!(
        row.status, "pending",
        "a rejected claim must not flip the row to succeeded or failed"
    );
    Ok(())
}

/// A claim made once the head has gone red (required checks failing) is
/// rejected the same way as the pending case, so a worker that claims
/// early off a synthetic/stale green cannot manufacture a succeeded row
/// once the real CI result lands red.
#[tokio::test]
async fn mark_ci_remediation_succeeded_via_rebase_rejects_red_head() -> Result<()> {
    let (response, _engine, work_db, attempt_id) =
        seed_and_claim_via_rebase(OpenPrCiStatus::Failing { failures: vec![] }, "sha-red").await?;
    match response {
        FrontendEvent::CiRemediationSucceededViaRebaseRejected { status, live_sha, .. } => {
            assert!(status.contains("failing"), "got: {status}");
            assert_eq!(live_sha.as_deref(), Some("sha-red"));
        }
        other => {
            return Err(anyhow!(
                "expected SucceededViaRebaseRejected for red CI, got: {other:?}"
            ));
        }
    }
    let row = work_db.get_ci_remediation(&attempt_id)?.expect("row still exists");
    assert_eq!(
        row.status, "pending",
        "a red-head claim must not be honored as succeeded; the row stays actionable"
    );
    Ok(())
}

/// The `merge_queue_rebounce` guard on `MarkCiRemediationSucceededViaRebase`
/// mirrors the one on `MarkCiRemediationNoop`: for a rebounce attempt the
/// PR's head-branch CI rollup is green BY DEFINITION (the failure lives on
/// the synthetic merge commit, not the PR head — see the runner's rebounce
/// directive), so honoring a rebase claim off a green head-branch probe
/// would be exactly the bypass the rebase-claim postmortem (PR spinyfin/mono#2023) forbade. This must be
/// rejected BEFORE the live probe ever runs — proven here by wiring a probe
/// that would otherwise report green.
#[tokio::test]
async fn mark_ci_remediation_succeeded_via_rebase_rejects_merge_queue_rebounce() -> Result<()> {
    let engine = TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        merge_probe: Some(Arc::new(FakeCiProbe {
            ci: OpenPrCiStatus::Clean,
            head_sha: "sha-green-but-rebounce".to_owned(),
        })),
        ..Default::default()
    })
    .await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C")
            .autostart(false)
            .build(),
    )?;
    let pr = "https://github.com/foo/bar/pull/100";
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr.to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    let attempt = work_db
        .insert_ci_remediation(boss_engine::work::CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr.to_owned(),
            pr_number: 100,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "sha-at-trigger".to_owned(),
            attempt_kind: "fix".to_owned(),
            consumes_budget: 1,
            failed_checks: "[]".to_owned(),
            failure_kind: "merge_queue_rebounce".to_owned(),
            before_commit_sha: None,
        })?
        .expect("insert should succeed on a fresh row");

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
            attempt_id: attempt.id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::CiRemediationSucceededViaRebaseRejected { status, live_sha, .. } => {
            assert!(status.contains("synthetic/ephemeral commit"), "got: {status}");
            assert!(
                live_sha.is_none(),
                "rebounce guard rejects before the live probe ever runs"
            );
        }
        other => return Err(anyhow!("expected rebounce rejection, got: {other:?}")),
    }
    let row = work_db.get_ci_remediation(&attempt.id)?.expect("row still exists");
    assert_eq!(
        row.status, "pending",
        "rebounce claim must not be honored even though the fake probe reports green"
    );

    Ok(())
}

/// Sibling of [`mark_ci_remediation_succeeded_via_rebase_rejects_merge_queue_rebounce`]
/// for the other `is_queue_side_failure_kind` member: a Trunk queue eviction's
/// PR head-branch CI is also green by construction (Trunk only builds a
/// construction branch for a PR GitHub already reports mergeable), so the
/// same guard must reject a rebase claim before the live probe ever runs.
/// The guard condition changed from an equality check on
/// `"merge_queue_rebounce"` to `is_queue_side_failure_kind`; this pins the
/// Trunk-eviction case separately so a future narrowing of that predicate
/// cannot silently reopen the bypass for it.
#[tokio::test]
async fn mark_ci_remediation_succeeded_via_rebase_rejects_trunk_queue_eviction() -> Result<()> {
    let engine = TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        merge_probe: Some(Arc::new(FakeCiProbe {
            ci: OpenPrCiStatus::Clean,
            head_sha: "sha-green-but-trunk-eviction".to_owned(),
        })),
        ..Default::default()
    })
    .await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C")
            .autostart(false)
            .build(),
    )?;
    let pr = "https://github.com/foo/bar/pull/101";
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr.to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    let attempt = work_db
        .insert_ci_remediation(boss_engine::work::CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr.to_owned(),
            pr_number: 101,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "trunk:entry_101@2026-07-22T00:00:00.000Z".to_owned(),
            attempt_kind: "fix".to_owned(),
            consumes_budget: 1,
            failed_checks: "[]".to_owned(),
            failure_kind: "trunk_queue_eviction".to_owned(),
            before_commit_sha: None,
        })?
        .expect("insert should succeed on a fresh row");

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
            attempt_id: attempt.id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::CiRemediationSucceededViaRebaseRejected { status, live_sha, .. } => {
            assert!(status.contains("synthetic/ephemeral commit"), "got: {status}");
            assert!(
                live_sha.is_none(),
                "trunk eviction guard rejects before the live probe ever runs"
            );
        }
        other => return Err(anyhow!("expected trunk eviction rejection, got: {other:?}")),
    }
    let row = work_db.get_ci_remediation(&attempt.id)?.expect("row still exists");
    assert_eq!(
        row.status, "pending",
        "trunk eviction claim must not be honored even though the fake probe reports green"
    );

    Ok(())
}

/// The third member of the `is_queue_side_failure_kind` guard family, and the
/// one that was missing it.
///
/// `mark-retriggered` is TERMINAL and has no resubmit path: honoring it for a
/// queue-side failure flips the attempt to `retriggered`, writes no
/// `boss:awaiting_resubmit` sentinel, and leaves the Trunk merge intent parked
/// at `failed` forever. The verb's own premise — "the merge-poller observes
/// the re-run's outcome and clears the signal when CI goes green" — is
/// structurally false here, because a queue-side failure's PR head CI is green
/// already. A real PR sat out of the queue for 50 h 23 m this way, its card
/// still rendering "Merging / Testing", until a human resubmitted by hand.
///
/// The refusal must be LOUD (a `WorkError`, which the CLI surfaces as a
/// non-zero exit), never a silent no-op, and must name the verb that is
/// actually correct. It also must not touch the row.
#[tokio::test]
async fn mark_ci_remediation_retriggered_rejects_queue_side_failures() -> Result<()> {
    let engine = TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        ..Default::default()
    })
    .await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // Both members of the predicate, so narrowing it for either one reopens
    // the strand.
    for (idx, failure_kind, trigger) in [
        (
            0_i64,
            "trunk_queue_eviction",
            "trunk:entry_900@2026-07-27T23:07:13.000Z",
        ),
        (1, "merge_queue_rebounce", "synthetic-merge-sha-900"),
    ] {
        let chore = work_db.create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(format!("C{idx}"))
                .autostart(false)
                .build(),
        )?;
        let pr_number = 900 + idx;
        let pr = format!("https://github.com/foo/bar/pull/{pr_number}");
        work_db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr.clone()),
                ..WorkItemPatch::default()
            },
        )?;
        let attempt = work_db
            .insert_ci_remediation(boss_engine::work::CiRemediationInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: pr.clone(),
                pr_number,
                head_branch: "feature".to_owned(),
                head_sha_at_trigger: trigger.to_owned(),
                attempt_kind: "fix".to_owned(),
                consumes_budget: 1,
                failed_checks: "[]".to_owned(),
                failure_kind: failure_kind.to_owned(),
                before_commit_sha: None,
            })?
            .expect("insert should succeed on a fresh row");

        let response = client
            .send_request(&FrontendRequest::MarkCiRemediationRetriggered {
                attempt_id: attempt.id.clone(),
                new_id: "rerun-1".to_owned(),
            })
            .await?;
        match response {
            FrontendEvent::WorkError { message } => {
                assert!(message.contains(failure_kind), "must name the kind; got: {message}");
                assert!(
                    message.contains("mark-failed"),
                    "must name the verb that IS accepted; got: {message}",
                );
            }
            other => {
                return Err(anyhow!(
                    "expected a loud refusal for {failure_kind}, got: {other:?} — a queue-side \
                     attempt accepted here strands the PR out of its merge queue",
                ));
            }
        }

        let row = work_db.get_ci_remediation(&attempt.id)?.expect("row still exists");
        assert_eq!(
            row.status, "pending",
            "a rejected {failure_kind} verb must leave the attempt actionable, not terminal",
        );
    }

    // Control: an ordinary `pr_branch_ci` attempt is still accepted, so the
    // guard is scoped to queue-side kinds rather than disabling the verb.
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C-ordinary")
            .autostart(false)
            .build(),
    )?;
    let pr = "https://github.com/foo/bar/pull/902";
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr.to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    let ordinary = work_db
        .insert_ci_remediation(boss_engine::work::CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr.to_owned(),
            pr_number: 902,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "plain-head-sha".to_owned(),
            attempt_kind: "retrigger".to_owned(),
            consumes_budget: 0,
            failed_checks: "[]".to_owned(),
            failure_kind: "pr_branch_ci".to_owned(),
            before_commit_sha: None,
        })?
        .expect("insert should succeed on a fresh row");
    let response = client
        .send_request(&FrontendRequest::MarkCiRemediationRetriggered {
            attempt_id: ordinary.id.clone(),
            new_id: "rerun-2".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::CiRemediationRetriggered { attempt, .. } => {
            assert_eq!(attempt.status, "retriggered");
        }
        other => {
            return Err(anyhow!(
                "a pr_branch_ci retrigger must still be honored, got: {other:?}"
            ));
        }
    }

    Ok(())
}

/// Phase 5 #13 happy paths for the read-only `list` and `show` verbs:
/// seed two attempts under one product, query the freshest-first list,
/// then fetch one by id.
#[tokio::test]
async fn engine_conflicts_list_and_show_round_trip() -> Result<()> {
    let engine = spawn_engine().await?;
    let (product, _chore, a, b) = seed_two_conflict_resolutions(&engine).await?;

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // List with no filters: both attempts come back, freshest first.
    let response = client
        .send_request(&FrontendRequest::ListConflictResolutions {
            product_id: None,
            status: vec![],
            work_item_id: None,
            limit: None,
        })
        .await?;
    let attempts = match response {
        FrontendEvent::ConflictResolutionsList { attempts } => attempts,
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(attempts.len(), 2, "expected both seeded attempts");
    assert_eq!(attempts[0].id, b.id, "freshest attempt should sort first");
    assert_eq!(attempts[1].id, a.id);

    // Product-scoped query returns the same rows.
    let response = client
        .send_request(&FrontendRequest::ListConflictResolutions {
            product_id: Some(product.id.clone()),
            status: vec![],
            work_item_id: None,
            limit: None,
        })
        .await?;
    match response {
        FrontendEvent::ConflictResolutionsList { attempts } => {
            assert_eq!(attempts.len(), 2);
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }

    // Status filter limits the result set.
    let response = client
        .send_request(&FrontendRequest::ListConflictResolutions {
            product_id: None,
            status: vec!["pending".to_owned()],
            work_item_id: None,
            limit: None,
        })
        .await?;
    match response {
        FrontendEvent::ConflictResolutionsList { attempts } => {
            assert_eq!(attempts.len(), 2);
            assert!(attempts.iter().all(|a| a.status == "pending"));
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    let response = client
        .send_request(&FrontendRequest::ListConflictResolutions {
            product_id: None,
            status: vec!["succeeded".to_owned()],
            work_item_id: None,
            limit: None,
        })
        .await?;
    match response {
        FrontendEvent::ConflictResolutionsList { attempts } => {
            assert!(attempts.is_empty());
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }

    // Limit caps the response.
    let response = client
        .send_request(&FrontendRequest::ListConflictResolutions {
            product_id: None,
            status: vec![],
            work_item_id: None,
            limit: Some(1),
        })
        .await?;
    match response {
        FrontendEvent::ConflictResolutionsList { attempts } => {
            assert_eq!(attempts.len(), 1);
            assert_eq!(attempts[0].id, b.id);
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }

    // `show` round-trips by id.
    let response = client
        .send_request(&FrontendRequest::GetConflictResolution {
            attempt_id: a.id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::ConflictResolution { attempt } => {
            assert_eq!(attempt.id, a.id);
            assert_eq!(attempt.pr_url, a.pr_url);
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }

    // `show` on unknown id surfaces a structured error.
    let response = client
        .send_request(&FrontendRequest::GetConflictResolution {
            attempt_id: "crz_does_not_exist".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("crz_does_not_exist"),
                "expected message to name the missing id, got: {message}",
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

/// Phase 5 #13 `retry`: only `failed` and `abandoned` rows can be
/// reset; non-terminal rows are rejected.
#[tokio::test]
async fn engine_conflicts_retry_resets_terminal_rows() -> Result<()> {
    let engine = spawn_engine().await?;
    let (_product, _chore, _a, b) = seed_two_conflict_resolutions(&engine).await?;

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // A `pending` row cannot be retried.
    let response = client
        .send_request(&FrontendRequest::RetryConflictResolution {
            attempt_id: b.id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("terminal-failure"),
                "expected non-terminal rejection, got: {message}",
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }

    // Flip `b` to `failed` so retry can reset it.
    client
        .send_request(&FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: b.id.clone(),
            reason: "architectural_mismatch".to_owned(),
        })
        .await?;

    let response = client
        .send_request(&FrontendRequest::RetryConflictResolution {
            attempt_id: b.id.clone(),
        })
        .await?;
    let reset = match response {
        FrontendEvent::ConflictResolutionRetried { attempt } => attempt,
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(reset.id, b.id);
    assert_eq!(reset.status, "pending");
    assert!(reset.failure_reason.is_none(), "failure_reason cleared");
    assert!(reset.started_at.is_none(), "started_at cleared");
    assert!(reset.finished_at.is_none(), "finished_at cleared");

    // A second retry of a now-pending row is rejected.
    let response = client
        .send_request(&FrontendRequest::RetryConflictResolution {
            attempt_id: b.id.clone(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { .. } => {}
        other => return Err(anyhow!("expected WorkError on re-retry, got: {other:?}")),
    }
    Ok(())
}

/// Phase 5 #13 `abandon`: flip non-terminal rows to `abandoned`; the
/// already-terminal arm rejects.
#[tokio::test]
async fn engine_conflicts_abandon_flips_attempt_status() -> Result<()> {
    let engine = spawn_engine().await?;
    let (_product, _chore, a, _b) = seed_two_conflict_resolutions(&engine).await?;

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::AbandonConflictResolution {
            attempt_id: a.id.clone(),
            reason: "pr_closed".to_owned(),
        })
        .await?;
    let flipped = match response {
        FrontendEvent::ConflictResolutionMarkedAbandoned { attempt } => attempt,
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(flipped.id, a.id);
    assert_eq!(flipped.status, "abandoned");
    assert_eq!(flipped.failure_reason.as_deref(), Some("pr_closed"));
    assert!(flipped.finished_at.is_some());

    // Idempotency: terminal rows are rejected.
    let response = client
        .send_request(&FrontendRequest::AbandonConflictResolution {
            attempt_id: a.id.clone(),
            reason: "ignored".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("already terminal") || message.contains("unknown"),
                "expected terminal/unknown message, got: {message}",
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

/// Current wall-clock time as whole seconds since the Unix epoch, mirroring
/// `boss_engine_utils::epoch_time::now_epoch_secs` (not a dependency of this
/// test binary) — used only to poll for a `created_at` second-boundary tick
/// in [`seed_two_conflict_resolutions`].
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Helper: seed a product + chore + two `pending` `conflict_resolutions`
/// rows. Returns the second one as the freshest (different
/// `base_sha_at_trigger` so the UNIQUE key allows both inserts).
async fn seed_two_conflict_resolutions(
    engine: &TestEngine,
) -> Result<(
    boss_protocol::Product,
    boss_protocol::Task,
    boss_protocol::ConflictResolution,
    boss_protocol::ConflictResolution,
)> {
    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("P")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("C")
            .autostart(false)
            .build(),
    )?;
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some("https://github.com/foo/bar/pull/77".to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    work_db.mark_chore_blocked_merge_conflict(&chore.id, "https://github.com/foo/bar/pull/77")?;
    let a = work_db
        .insert_conflict_resolution(boss_engine::work::ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/77".to_owned(),
            pr_number: 77,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("aaa".to_owned()),
            head_sha_before: Some("ddd".to_owned()),
        })?
        .expect("first insert seeds the row");
    // `created_at` has second resolution (`now_string()` stamps whole
    // epoch seconds), so the second row only sorts after the first once
    // the wall clock ticks into a new second. Poll for that instead of a
    // flat worst-case sleep — usually much less than a full second, and
    // never more than one.
    let a_created_at: i64 = a.created_at.parse().expect("created_at is epoch seconds");
    while now_epoch_secs() <= a_created_at {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let b = work_db
        .insert_conflict_resolution(boss_engine::work::ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/77".to_owned(),
            pr_number: 77,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("bbb".to_owned()),
            head_sha_before: Some("eee".to_owned()),
        })?
        .expect("second insert seeds the row (different base_sha)");
    drop(work_db);
    Ok((product, chore, a, b))
}
