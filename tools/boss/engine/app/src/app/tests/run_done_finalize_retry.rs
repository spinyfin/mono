use super::*;

/// The submission path itself — no test-side helper call — must carry an
/// accepted declaration whose terminalizing write fails through to a
/// terminal state once the failure clears, without any further worker action.
#[tokio::test]
async fn accepted_run_done_with_a_failing_finalize_write_finalizes_once_the_failure_clears() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    let peer_pid = std::process::id() as libc::pid_t;
    server_state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_finalize BEFORE UPDATE OF status ON work_executions
             WHEN NEW.status IN ('failed', 'completed')
             BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
        )
        .unwrap();

    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "blocked", "summary": "Needs a decision before it can continue"}),
            ),
        )
        .await,
    );
    // The ack reports the durable submission; the finalize is pending, and the
    // declaration stamp is the record of that.
    assert_eq!(proposal.state, ProposalState::Applied);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_live(),
        "the failed finalize must leave the execution live, not claim it finished"
    );
    assert_eq!(
        server_state.work_db.execution_run_done_outcome(&execution_id).unwrap(),
        Some(boss_protocol::RunDoneOutcome::Blocked),
        "the stamp survives as the pending-finalize record"
    );

    server_state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_finalize;")
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while server_state
        .work_db
        .get_execution(&execution_id)
        .unwrap()
        .status
        .is_live()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the background retry never finalized the execution"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Failed
    );
    assert_eq!(
        task_status(&server_state, &work_item_id),
        boss_protocol::TaskStatus::Blocked
    );
}
