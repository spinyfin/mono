//! Explicit review admission and actionable focus diagnostics across CLI/RPC.
use super::*;
use boss_protocol::{DispatchFocus, DispatchFocusTargetKind};

fn focused_state(fanout: bool) -> (Arc<ServerState>, tempfile::TempDir) {
    let (dir, db) = boss_work_db::test_support::open_db();
    for driver in ["claude", "codex"] {
        boss_engine_runtime::test_support::insert_host_capability(&db, "local", &format!("driver={driver}"), "auto");
    }
    let cfg = Arc::new(RuntimeConfig::from_parts(
        boss_engine_runtime::config::WorkConfig::builder()
            .cwd(dir.path().to_path_buf())
            .db_path(dir.path().join("state.db"))
            .review_pool_size(2)
            .build(),
        None,
    ));
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(
        cfg,
        None,
        None,
        ServerStateOverrides {
            work_db: Some(db),
            pr_state_checker: Some(Arc::new(OpenPr)),
            branch_verifier: Some(Arc::new(crate::app::tests::SettableHeadRefVerifier::default())),
            cube_client: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsCube)),
            execution_runner: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsRunner)),
            ..Default::default()
        },
    )
    .unwrap();
    state.feature_flags.set("review_batch_fanout", fanout).unwrap();
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, "proj_elsewhere".into());
    state.work_db.save_dispatch_focus(&focus).unwrap();
    state.execution_coordinator.restore_dispatch_focus(focus);
    (state, dir)
}

async fn wait_running(state: &ServerState, id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if state.work_db.get_execution(id).unwrap().status == ExecutionStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("execution did not dispatch: {:?}", state.work_db.get_execution(id)));
}

#[tokio::test]
async fn explicit_review_start_dispatches_every_member_outside_focus() {
    for fanout in [false, true] {
        let (state, _dir) = focused_state(fanout);
        let root = seed(&state, "example/repo", 42);
        let sibling = seed(&state, "example/repo", 43);
        let held = state
            .work_db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&sibling)
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        triggered(request(&state, 42, None, Ok(metadata("head"))).await);
        let focus = state.work_db.load_dispatch_focus().unwrap();
        assert!(
            state
                .work_db
                .executions_outside_dispatch_focus(&state.work_db.list_executions(Some(&root)).unwrap(), &focus)
                .unwrap()
                .is_empty()
        );
        // A second explicit request must also release an existing batch.
        state
            .work_db
            .connect()
            .unwrap()
            .execute("UPDATE tasks SET explicitly_launched = 0 WHERE id = ?1", [&root])
            .unwrap();
        triggered(request(&state, 42, None, Ok(metadata("head"))).await);
        let executions = state.work_db.list_executions(Some(&root)).unwrap();
        assert_eq!(executions.len(), if fanout { 2 } else { 1 });
        state.execution_coordinator.drain_ready_queue_for_test().await;
        for execution in executions {
            wait_running(&state, &execution.id).await;
        }
        assert_eq!(
            state.work_db.get_execution(&held.id).unwrap().status,
            ExecutionStatus::Ready
        );
        assert_eq!(
            state
                .work_db
                .get_execution(&held.id)
                .unwrap()
                .dispatch_wait_reason
                .as_deref(),
            Some("focus_mode")
        );
    }
}

async fn cli(args: &[&str]) -> std::process::Output {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::process::Command::new(std::env::var("BOSSCTL_TEST_BINARY").unwrap())
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap()
    })
    .await
    .expect("CLI did not complete its request")
}

#[tokio::test]
async fn diagnosed_bypass_and_guide_refusal_commands_pass_cli_and_engine() {
    for diagnose_guide in [true, false] {
        let (state, dir) = focused_state(true);
        let root = seed(&state, "example/repo", 42);
        let (_, comparison) = boss_work_db::test_support::seed_review_guide_series_for_pr(
            &state.work_db,
            &root,
            "https://github.com/example/repo/pull/42",
            "base",
            "head",
        );
        let guide = state
            .work_db
            .create_pr_review_guide_execution(&comparison, "https://github.com/example/repo")
            .unwrap();
        let review = state
            .work_db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&root)
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("https://github.com/example/repo")
                    .build(),
            )
            .unwrap();
        let socket = dir.path().join("frontend.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(super::super::super::handle_frontend_connection(
                    stream,
                    server_state.clone(),
                    None,
                ));
            }
        });
        for id in [&comparison, &guide.id] {
            let output = cli(&["--socket-path", socket.to_str().unwrap(), "agents", "launch", id]).await;
            assert!(!output.status.success());
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(error.contains(&format!("bossctl agents launch {root}")), "{error}");
            assert!(
                error.contains("review guide, not a pre-merge review request"),
                "{error}"
            );
        }
        assert_eq!(
            state.work_db.get_execution(&guide.id).unwrap().status,
            ExecutionStatus::Ready
        );
        let execution = if diagnose_guide { &guide } else { &review };
        let output = cli(&[
            "dispatch",
            "diagnose",
            &execution.id,
            "--state-root",
            dir.path().to_str().unwrap(),
        ])
        .await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8(output.stdout).unwrap();
        let command = text
            .split('`')
            .find(|part| part.starts_with("bossctl agents launch "))
            .expect(&text);
        assert_eq!(command, format!("bossctl agents launch {root}"));

        let mut args = vec!["--socket-path", socket.to_str().unwrap()];
        args.extend(command.split_whitespace().skip(1));
        let output = cli(&args).await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        wait_running(&state, &review.id).await;
        server.abort();
        let _ = server.await;
    }
}

/// The state the attention item is filed in: implementation complete, PR
/// bound, a review guide pending, and no reviewer ever admitted. Every command
/// the engine prints for it must start both reviewers when run.
#[tokio::test]
async fn never_admitted_review_recovery_command_starts_both_reviewers() {
    let (state, dir) = focused_state(true);
    let root = seed(&state, "example/repo", 42);
    let pr_url = "https://github.com/example/repo/pull/42";
    let (_, comparison) =
        boss_work_db::test_support::seed_review_guide_series_for_pr(&state.work_db, &root, pr_url, "base", "head");
    assert!(state.work_db.list_executions(Some(&root)).unwrap().is_empty());

    state.work_db.file_admission_deferred_attention(&root, pr_url).unwrap();
    let item = state
        .work_db
        .list_open_attention_items_of_kind(boss_work_db::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
        .unwrap()
        .into_iter()
        .next()
        .expect("deferred-admission attention filed");
    let advertised = |text: &str| {
        text.split('`')
            .find(|part| part.starts_with("bossctl "))
            .unwrap_or_else(|| panic!("no command in {text}"))
            .to_owned()
    };
    let command = advertised(&item.body_markdown);
    let refusal = state.work_db.validate_manual_launch_target(&comparison).unwrap_err();
    assert_eq!(advertised(&refusal.to_string()), command);
    assert_eq!(command, "bossctl review start --pr 42 --repo example/repo");

    // The CLI must accept the printed arguments (no usage error); the review
    // request itself is then served by the engine handler below, because the
    // CLI's handler fetches PR metadata with `gh`.
    let unreachable = dir.path().join("none.sock");
    let mut args = vec!["--socket-path", unreachable.to_str().unwrap()];
    args.extend(command.split_whitespace().skip(1));
    let output = cli(&args).await;
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        !error.contains("Usage:") && !error.contains("unexpected argument"),
        "{error}"
    );

    // `dispatch diagnose` on the held guide, and the guide `agents launch`
    // refusal, must advertise this same command.
    let guide = state
        .work_db
        .create_pr_review_guide_execution(&comparison, "https://github.com/example/repo")
        .unwrap();
    let output = cli(&[
        "dispatch",
        "diagnose",
        &guide.id,
        "--state-root",
        dir.path().to_str().unwrap(),
    ])
    .await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let diagnosed = String::from_utf8(output.stdout).unwrap();
    assert_eq!(advertised(&diagnosed), command, "{diagnosed}");
    let socket = dir.path().join("never-admitted.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(super::super::super::handle_frontend_connection(
                stream,
                server_state.clone(),
                None,
            ));
        }
    });
    let output = cli(&["--socket-path", socket.to_str().unwrap(), "agents", "launch", &guide.id]).await;
    assert!(!output.status.success());
    assert_eq!(advertised(&String::from_utf8(output.stderr).unwrap()), command);
    server.abort();
    let _ = server.await;

    let words: Vec<&str> = command.split_whitespace().collect();
    let number: i64 = words[words.iter().position(|w| *w == "--pr").unwrap() + 1]
        .parse()
        .unwrap();
    let repo = words[words.iter().position(|w| *w == "--repo").unwrap() + 1];
    triggered(request(&state, number, Some(repo), Ok(metadata("head"))).await);
    let executions: Vec<_> = state
        .work_db
        .list_executions(Some(&root))
        .unwrap()
        .into_iter()
        .filter(|execution| execution.kind == ExecutionKind::PrReview)
        .collect();
    assert_eq!(executions.len(), 2);
    state.execution_coordinator.drain_ready_queue_for_test().await;
    for execution in executions {
        wait_running(&state, &execution.id).await;
    }
}

/// A focus-held answer agent on an out-of-focus task's comment: the command
/// `dispatch diagnose` prints, and the execution-id refusal, must start it.
#[tokio::test]
async fn held_answer_agent_diagnosed_launch_command_starts_it() {
    let (state, dir) = focused_state(true);
    let (_, series) = boss_work_db::test_support::seed_published_guide(&state.work_db, 42);
    let comment = boss_work_db::test_support::add_guide_comment(&state.work_db, &series, "why?");
    let execution = state
        .work_db
        .create_answer_agent_execution(&comment.id, "https://github.com/example/repo")
        .unwrap();
    let focus = state.work_db.load_dispatch_focus().unwrap();
    assert_eq!(
        state
            .work_db
            .executions_outside_dispatch_focus(std::slice::from_ref(&execution), &focus)
            .unwrap(),
        vec![execution.id.clone()].into_iter().collect()
    );

    let socket = dir.path().join("frontend.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(super::super::super::handle_frontend_connection(
                stream,
                server_state.clone(),
                None,
            ));
        }
    });
    let expected = format!("bossctl agents launch {}", comment.id);
    let output = cli(&[
        "dispatch",
        "diagnose",
        &execution.id,
        "--state-root",
        dir.path().to_str().unwrap(),
    ])
    .await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    let command = text
        .split('`')
        .find(|part| part.starts_with("bossctl agents launch "))
        .expect(&text);
    assert_eq!(command, expected);

    let output = cli(&[
        "--socket-path",
        socket.to_str().unwrap(),
        "agents",
        "launch",
        &execution.id,
    ])
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains(&expected));

    let mut args = vec!["--socket-path", socket.to_str().unwrap()];
    args.extend(command.split_whitespace().skip(1));
    let output = cli(&args).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    wait_running(&state, &execution.id).await;
    server.abort();
    let _ = server.await;
}
