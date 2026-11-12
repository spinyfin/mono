//! Host-local observations terminate at durable normalized journal records.
pub mod capture;
pub mod hooks;
pub mod transcripts;

use anyhow::Result;
use boss_host_execution::worker::ExecutionRecord;
use boss_protocol_spoke::v1::{Command, CommandResult, ResultCode, Run, Stamp};
use boss_spoke_outbound::SharedJournal;
use boss_spoke_worker_api::Gateway;

fn rejected(command: &Command, reason: String) -> CommandResult {
    CommandResult {
        command_id: command.command_id.clone(),
        code: ResultCode::Rejected as i32,
        reason,
        value: None,
    }
}

/// Apply a hub `WorkerReply` or `ProposalDecision` to the run-scoped gateway.
/// A reply that does not belong to the run, or does not match its request, is
/// refused with the reason rather than failing the daemon.
pub fn apply_gateway_command(gateway: &Gateway, command: &Command) -> CommandResult {
    match gateway.apply(command) {
        Ok(()) => CommandResult {
            command_id: command.command_id.clone(),
            code: ResultCode::Ok as i32,
            reason: String::new(),
            value: None,
        },
        Err(error) => rejected(command, error.to_string()),
    }
}

/// Commit the assigned context snapshot before a Spawn runs. `Some` carries the
/// rejection to report; the worker is not started.
pub fn provision_rejection(gateway: &Gateway, command: &Command, stamp: &Stamp) -> Option<CommandResult> {
    gateway
        .provision(command, stamp)
        .err()
        .map(|error| rejected(command, error.to_string()))
}

pub fn run_identity(record: &ExecutionRecord) -> Run {
    Run {
        execution_id: record.execution_id.clone(),
        run_id: record.run_id.clone(),
        spawn_token: record.spawn_token.clone(),
        assignment_generation: record.assignment_generation,
    }
}

pub fn registered(journal: &SharedJournal) -> Result<Vec<ExecutionRecord>> {
    let rows = journal
        .lock()
        .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
        .list_executions()?;
    let mut records = Vec::new();
    for bytes in rows {
        match serde_json::from_slice(&bytes) {
            Ok(record) => records.push(record),
            Err(error) => tracing::warn!(%error, "skipping undecodable execution record"),
        }
    }
    Ok(records)
}

/// Also runs offline and immediately after daemon restart. One run's malformed
/// source never prevents other runs' pending lifecycle evidence from draining.
pub async fn observe(journal: SharedJournal, stamp: impl Fn() -> Result<Stamp>) -> Result<()> {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut source_errors = std::collections::HashMap::new();
    loop {
        interval.tick().await;
        let records = match registered(&journal) {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "execution listing failed; skipping observe tick");
                continue;
            }
        };
        source_errors.retain(|run, _| records.iter().any(|record| &record.run_id == run && !record.reclaimed));
        for record in records {
            if record.observation.is_none() || record.reclaimed {
                continue;
            }
            // Once durably drained, sources may be pruned during reclaim; a
            // later poll would otherwise replay a surviving transcript from 0.
            match journal.lock().map(|j| j.source(&run_identity(&record), "drained")) {
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => {}
                _ => continue,
            }
            if let Err(error) = hooks::drain(&journal, &record, &stamp()?).await {
                tracing::warn!(run = record.run_id, %error, "pending hooks retained");
            }
            match transcripts::poll(&journal, &record, &stamp()?).await {
                Err(error) if !boss_spoke_journal::Fault::is_full(&error) => {
                    let message = error.to_string();
                    if source_errors.insert(record.run_id.clone(), message.clone()).as_ref() != Some(&message) {
                        tracing::warn!(run = record.run_id, %error, "transcript source retained");
                    }
                }
                Ok(()) => {
                    source_errors.remove(&record.run_id);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{hooks, run_identity, transcripts};
    use boss_host_execution::{
        process::process_snapshot,
        worker::{ExecutionRecord, ObservationConfig, Phase},
    };
    use boss_protocol_spoke::v1::{Record, Stamp, record};
    use boss_spoke_journal::{Admission, Fault, Journal, Limits};
    use boss_spoke_outbound::SharedJournal;
    use prost::Message;
    use std::{
        os::unix::fs::PermissionsExt,
        path::Path,
        sync::{Arc, Mutex},
    };
    use tokio::io::AsyncWriteExt;

    fn stamp() -> anyhow::Result<Stamp> {
        Ok(Stamp {
            boot_id: "boot".into(),
            wall_unix_ms: 1,
            awake_ns: 1,
            elapsed_ns: 1,
        })
    }
    fn limits() -> Limits {
        Limits {
            spool_bytes: 1024 * 1024,
            lifecycle_reserve: 64 * 1024,
        }
    }
    fn journal(root: &Path, limits: Limits) -> SharedJournal {
        let path = root.join("state");
        Arc::new(Mutex::new(
            Journal::open(&path, "host", limits).unwrap_or_else(|error| panic!("open {}: {error}", path.display())),
        ))
    }
    /// `Journal::open` takes an exclusive daemon lock. A spawned observer may
    /// still hold a clone after abort until the runtime next polls it.
    async fn reopen(root: &Path, limits: Limits, mut journal: SharedJournal) -> SharedJournal {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let inner = loop {
            match Arc::try_unwrap(journal) {
                Ok(mutex) => break mutex.into_inner().expect("journal mutex"),
                Err(shared) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "journal still shared after observer shutdown"
                    );
                    journal = shared;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        };
        drop(inner);
        self::journal(root, limits)
    }
    fn execution(root: &Path) -> ExecutionRecord {
        let pending = root.join("pending");
        std::fs::create_dir(&pending).unwrap();
        std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pid = std::process::id();
        ExecutionRecord::builder()
            .execution_id("execution")
            .run_id("run")
            .spawn_token("token")
            .assignment_generation(1)
            .lease_id("lease")
            .workspace_id("workspace")
            .session("session")
            .phase(Phase::Alive)
            .boot_id("boot")
            .pid(pid as u64)
            .driver_id("claude")
            .observation(ObservationConfig {
                workspace: root.into(),
                pending,
                file: None,
                pid_start: Some(process_snapshot(pid as i32).unwrap().start),
            })
            .build()
    }
    fn hook(path: Option<&Path>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
        "_boss_run_id": "run", "_boss_producer_id": "0123456789abcdef0123456789abcdef", "_boss_producer_sequence": 1,
        "hook_event_name": "Stop", "session_id": "session", "stop_hook_active": false,
        "transcript_path": path,
    }))
    .unwrap()
    }
    fn receipt_through(journal: &SharedJournal) {
        let mut journal = journal.lock().unwrap();
        let (_, id) = journal.identity().unwrap();
        let through = journal.record_bounds().unwrap().1;
        journal
            .receipt(&boss_protocol_spoke::v1::Cursor {
                journal_id: id,
                through,
            })
            .unwrap();
    }
    async fn poll_available(journal: &SharedJournal, execution: &ExecutionRecord) {
        for _ in 0..16 {
            match transcripts::poll(journal, execution, &stamp().unwrap()).await {
                Ok(()) => receipt_through(journal),
                Err(error) if Fault::is_full(&error) => receipt_through(journal),
                Err(error) => panic!("{error}"),
            }
        }
    }
    fn records(journal: &SharedJournal) -> Vec<Record> {
        let journal = journal.lock().unwrap();
        let mut after = 0;
        let mut records = Vec::new();
        while let Some(record) = journal.next_record(after).unwrap() {
            after = record.sequence;
            records.push(Record::decode(record.bytes.as_slice()).unwrap());
        }
        records
    }
    async fn shim(root: &Path, socket: &Path, payload: &[u8]) {
        let binary = std::env::current_dir()
            .unwrap()
            .join(std::env::var("SHIM_BIN").unwrap());
        let mut child = tokio::process::Command::new(binary)
            .env("BOSS_EVENTS_SOCKET", socket)
            .env(boss_host_execution::worker::EVENT_SPOOL_ENV, root.join("pending"))
            .env("BOSS_RUN_ID", "run")
            .env("BOSS_EVENT_RETRY_DELAYS_MS", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(payload).await.unwrap();
        let result = child.wait_with_output().await.unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }

    #[tokio::test]
    async fn subagent_tool_events_retain_origin_in_journal() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let journal = journal(root.path(), limits());
        let mut payload: serde_json::Value = serde_json::from_slice(&hook(None)).unwrap();
        payload["agent_id"] = "child".into();
        payload["hook_event_name"] = "PostToolUse".into();
        payload["tool_name"] = "Write".into();
        hooks::ingest(
            &journal,
            &execution,
            &serde_json::to_vec(&payload).unwrap(),
            &stamp().unwrap(),
        )
        .await
        .unwrap();
        let captured = records(&journal);
        let Some(record::Body::Event(event)) = &captured[0].body else {
            panic!("missing event");
        };
        assert!(event.from_subagent);
        assert!(
            matches!(&event.event, Some(boss_protocol_spoke::v1::worker_event::Event::PostToolUse(tool)) if tool.name == "Write")
        );
    }

    #[tokio::test]
    async fn truncated_hooks_journal_a_gap_with_the_event() {
        for (name, extra) in [
            ("Stop", serde_json::json!({"stop_hook_active": true})),
            ("UserPromptSubmit", serde_json::json!({"prompt": "bounded prefix"})),
        ] {
            let root = tempfile::tempdir().unwrap();
            let execution = execution(root.path());
            let journal = journal(root.path(), limits());
            let mut payload: serde_json::Value = serde_json::from_slice(&hook(None)).unwrap();
            payload["hook_event_name"] = name.into();
            payload["_boss_payload_truncated"] = true.into();
            for (key, value) in extra.as_object().unwrap() {
                payload[key] = value.clone();
            }
            hooks::ingest(
                &journal,
                &execution,
                &serde_json::to_vec(&payload).unwrap(),
                &stamp().unwrap(),
            )
            .await
            .unwrap();
            let captured = records(&journal);
            assert_eq!(captured.len(), 2, "{name}");
            assert!(
                matches!(&captured[0].body, Some(record::Body::Gap(g)) if g.resource.starts_with("hook:")),
                "{name}"
            );
            let Some(record::Body::Event(event)) = &captured[1].body else {
                panic!("missing event");
            };
            match &event.event {
                Some(boss_protocol_spoke::v1::worker_event::Event::Stop(stop)) => assert!(stop.stop_hook_active),
                Some(boss_protocol_spoke::v1::worker_event::Event::UserPrompt(prompt)) => {
                    assert_eq!(prompt, "bounded prefix")
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// A Terminal codex run whose rollout lives under a sessions directory.
    fn terminal_file_run(root: &std::path::Path) -> (ExecutionRecord, std::path::PathBuf) {
        let mut execution = execution(root);
        execution.driver_id = "codex".into();
        let directory = root.join("sessions");
        std::fs::create_dir(&directory).unwrap();
        execution.observation.as_mut().unwrap().file = Some((
            boss_engine_driver::AgentJsonlFileIngress {
                directory: directory.clone(),
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path: root.into(),
            },
            Vec::new(),
        ));
        let meta = serde_json::json!({"type":"session_meta", "payload":{"id":"session", "cwd":root}});
        std::fs::write(directory.join("rollout-example-session.jsonl"), format!("{meta}\n")).unwrap();
        execution.phase = Phase::Terminal;
        (execution, directory)
    }

    #[tokio::test]
    async fn transient_discovery_failures_leave_terminal_run_undrained_for_retry() {
        use boss_engine_driver::discovery::fault::{Point, inject_once};
        for point in [Point::Listing, Point::CandidateOpen, Point::Prepare] {
            for errno in [libc::EMFILE, libc::EINTR] {
                let root = tempfile::tempdir().unwrap();
                let (execution, directory) = terminal_file_run(root.path());
                let journal = journal(root.path(), limits());
                inject_once(point, &directory, errno);
                let failed = transcripts::poll(&journal, &execution, &stamp().unwrap()).await;
                assert!(
                    failed.is_err(),
                    "{point:?}/{errno}: transient failure must surface for retry"
                );
                let run = run_identity(&execution);
                assert!(
                    journal.lock().unwrap().source(&run, "drained").unwrap().is_none(),
                    "{point:?}/{errno}"
                );
                assert!(
                    records(&journal).is_empty(),
                    "{point:?}/{errno}: no gap for a transient failure"
                );
                transcripts::poll(&journal, &execution, &stamp().unwrap())
                    .await
                    .unwrap();
                assert!(
                    records(&journal)
                        .iter()
                        .any(|r| matches!(&r.body, Some(record::Body::Transcript(t)) if !t.data.is_empty())),
                    "{point:?}/{errno}: next poll must journal the data"
                );
            }
        }
    }

    #[tokio::test]
    async fn transient_cwd_resolution_failure_does_not_drain_terminal_run() {
        use boss_engine_driver::discovery::fault::{Point, inject_once};
        let root = tempfile::tempdir().unwrap();
        let (execution, _directory) = terminal_file_run(root.path());
        let journal = journal(root.path(), limits());
        inject_once(Point::Cwd, root.path(), libc::EMFILE);
        assert!(
            transcripts::poll(&journal, &execution, &stamp().unwrap())
                .await
                .is_err()
        );
        let run = run_identity(&execution);
        assert!(journal.lock().unwrap().source(&run, "drained").unwrap().is_none());
        assert!(
            !records(&journal)
                .iter()
                .any(|r| matches!(&r.body, Some(record::Body::Gap(_))))
        );
        transcripts::poll(&journal, &execution, &stamp().unwrap())
            .await
            .unwrap();
        assert!(
            records(&journal)
                .iter()
                .any(|r| matches!(&r.body, Some(record::Body::Transcript(t)) if !t.data.is_empty()))
        );
    }

    fn stamp_at(awake_ns: u64) -> Stamp {
        Stamp {
            awake_ns,
            ..stamp().unwrap()
        }
    }

    #[tokio::test]
    async fn persistent_unclassified_errno_drains_terminal_run_with_a_gap_after_awake_budget() {
        use boss_engine_driver::discovery::fault::{Point, inject_once};
        let root = tempfile::tempdir().unwrap();
        let (execution, directory) = terminal_file_run(root.path());
        let journal = journal(root.path(), limits());
        let run = run_identity(&execution);
        // A long burst of polls inside the budget never gives up.
        for i in 0..50 {
            inject_once(Point::Listing, &directory, libc::EIO);
            let _ = transcripts::poll(&journal, &execution, &stamp_at(1 + i)).await;
        }
        assert!(journal.lock().unwrap().source(&run, "drained").unwrap().is_none());
        assert!(records(&journal).is_empty());
        inject_once(Point::Listing, &directory, libc::EIO);
        let _ = transcripts::poll(&journal, &execution, &stamp_at(10 * 60 * 1_000_000_000)).await;
        assert!(journal.lock().unwrap().source(&run, "drained").unwrap().is_some());
        assert!(records(&journal).iter().any(
            |r| matches!(&r.body, Some(record::Body::Gap(g)) if g.reason.contains("Input/output error") || g.reason.contains("os error"))
        ));
    }

    /// With no receipts the journal fills; polling must neither drain nor gap.
    /// Once receipts flow, the whole transcript ships and the run drains.
    async fn assert_backpressure_ships_everything(
        execution: &ExecutionRecord,
        journal: &SharedJournal,
        expected_len: usize,
    ) {
        let run = run_identity(execution);
        for _ in 0..30 {
            let _ = transcripts::poll(journal, execution, &stamp().unwrap()).await;
        }
        assert!(journal.lock().unwrap().source(&run, "drained").unwrap().is_none());
        assert!(
            !records(journal)
                .iter()
                .any(|r| matches!(&r.body, Some(record::Body::Gap(_))))
        );
        for _ in 0..200 {
            if journal.lock().unwrap().source(&run, "drained").unwrap().is_some() {
                break;
            }
            let _ = transcripts::poll(journal, execution, &stamp().unwrap()).await;
            receipt_through(journal);
        }
        assert!(journal.lock().unwrap().source(&run, "drained").unwrap().is_some());
        let shipped: usize = records(journal)
            .iter()
            .filter_map(|r| match &r.body {
                Some(record::Body::Transcript(t)) => Some(t.data.len()),
                _ => None,
            })
            .sum();
        assert_eq!(shipped, expected_len, "the whole transcript must be journaled");
    }

    fn large_transcript(mut body: String) -> String {
        let line = format!("{}\n", serde_json::json!({"type":"noise", "payload": "x".repeat(900)}));
        for _ in 0..700 {
            body.push_str(&line);
        }
        body
    }

    #[tokio::test]
    async fn spool_backpressure_never_drains_or_gaps_a_terminal_codex_file_run() {
        let root = tempfile::tempdir().unwrap();
        let (execution, directory) = terminal_file_run(root.path());
        let rollout = directory.join("rollout-example-session.jsonl");
        let body = large_transcript(std::fs::read_to_string(&rollout).unwrap());
        std::fs::write(&rollout, &body).unwrap();
        let journal = journal(root.path(), limits());
        assert_backpressure_ships_everything(&execution, &journal, body.len()).await;
    }

    #[tokio::test]
    async fn spool_backpressure_never_drains_or_gaps_a_terminal_claude_hook_run() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        let transcript = root.path().join("claude-transcript.jsonl");
        let body = large_transcript(String::new());
        std::fs::write(&transcript, &body).unwrap();
        let journal = journal(root.path(), limits());
        hooks::ingest(&journal, &execution, &hook(Some(&transcript)), &stamp().unwrap())
            .await
            .unwrap();
        execution.phase = Phase::Terminal;
        assert_backpressure_ships_everything(&execution, &journal, body.len()).await;
    }

    #[tokio::test]
    async fn backpressure_between_transient_source_errors_ends_the_failure_streak() {
        use boss_engine_driver::discovery::fault::{Point, inject_once};
        let root = tempfile::tempdir().unwrap();
        let (execution, directory) = terminal_file_run(root.path());
        let rollout = directory.join("rollout-example-session.jsonl");
        let body = large_transcript(std::fs::read_to_string(&rollout).unwrap());
        std::fs::write(&rollout, &body).unwrap();
        let journal = journal(root.path(), limits());
        let run = run_identity(&execution);
        let minute = 60 * 1_000_000_000u64;
        // Saturate the bulk window without receipts so every later source read
        // succeeds but is refused by backpressure.
        let mut full = false;
        for _ in 0..200 {
            if let Err(error) = transcripts::poll(&journal, &execution, &stamp_at(0)).await {
                assert!(Fault::is_full(&error), "{error}");
                full = true;
                break;
            }
        }
        assert!(full, "the spool must fill");
        let marker = |journal: &SharedJournal| {
            journal
                .lock()
                .unwrap()
                .source(&run, "transient_since")
                .unwrap()
                .map(|b| String::from_utf8(b).unwrap())
        };
        inject_once(Point::Listing, &directory, libc::EIO);
        assert!(transcripts::poll(&journal, &execution, &stamp_at(1)).await.is_err());
        assert!(marker(&journal).is_some_and(|m| m != "none"), "streak must start");
        // Successful source reads blocked by a full spool, for over the budget.
        for i in 1..=8u64 {
            let error = transcripts::poll(&journal, &execution, &stamp_at(i * minute))
                .await
                .unwrap_err();
            assert!(Fault::is_full(&error), "{error}");
            assert_eq!(
                marker(&journal).as_deref(),
                Some("none"),
                "backpressure ends the streak"
            );
        }
        inject_once(Point::Listing, &directory, libc::EIO);
        assert!(
            transcripts::poll(&journal, &execution, &stamp_at(9 * minute))
                .await
                .is_err()
        );
        assert!(journal.lock().unwrap().source(&run, "drained").unwrap().is_none());
        assert!(
            !records(&journal)
                .iter()
                .any(|r| matches!(&r.body, Some(record::Body::Gap(_))))
        );
        for _ in 0..200 {
            if journal.lock().unwrap().source(&run, "drained").unwrap().is_some() {
                break;
            }
            let _ = transcripts::poll(&journal, &execution, &stamp_at(10 * minute)).await;
            receipt_through(&journal);
        }
        let shipped: usize = records(&journal)
            .iter()
            .filter_map(|r| match &r.body {
                Some(record::Body::Transcript(t)) => Some(t.data.len()),
                _ => None,
            })
            .sum();
        assert_eq!(shipped, body.len());
        assert!(
            journal.lock().unwrap().source(&run, "drained").unwrap().is_some(),
            "the run must drain once receipts resume"
        );
        assert!(
            !records(&journal)
                .iter()
                .any(|r| matches!(&r.body, Some(record::Body::Gap(_)))),
            "no gap may be written during recovery"
        );
    }

    #[tokio::test]
    async fn transient_streak_restarts_when_the_boot_changes() {
        use boss_engine_driver::discovery::fault::{Point, inject_once};
        let minute = 60 * 1_000_000_000u64;
        // (old-boot counter, new-boot counter): smaller and larger.
        for (old, new) in [(100 * minute, 10 * minute), (10 * minute, 100 * minute)] {
            let root = tempfile::tempdir().unwrap();
            let (execution, directory) = terminal_file_run(root.path());
            let journal = journal(root.path(), limits());
            let run = run_identity(&execution);
            inject_once(Point::Listing, &directory, libc::EIO);
            let _ = transcripts::poll(&journal, &execution, &stamp_at(old)).await;
            let mut next = stamp_at(new);
            next.boot_id = "boot-2".into();
            inject_once(Point::Listing, &directory, libc::EIO);
            let _ = transcripts::poll(&journal, &execution, &next).await;
            assert!(
                journal.lock().unwrap().source(&run, "drained").unwrap().is_none(),
                "{old}->{new}: a streak from another boot must not drain"
            );
            // Same new boot, past the budget since the restarted streak: drains.
            next.awake_ns = new + 6 * minute;
            inject_once(Point::Listing, &directory, libc::EIO);
            let _ = transcripts::poll(&journal, &execution, &next).await;
            assert!(journal.lock().unwrap().source(&run, "drained").unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn permanent_discovery_failure_drains_terminal_run_with_a_gap() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let (execution, directory) = terminal_file_run(root.path());
        let locked = directory.join("locked");
        std::fs::create_dir(&locked).unwrap();
        let journal = journal(root.path(), limits());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = transcripts::poll(&journal, &execution, &stamp().unwrap()).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert!(
            journal
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "drained")
                .unwrap()
                .is_some()
        );
        assert!(
            records(&journal)
                .iter()
                .any(|r| matches!(&r.body, Some(record::Body::Gap(_))))
        );
    }

    #[tokio::test]
    async fn terminal_ambiguous_and_unsafe_sources_drain_only_after_pending_hooks() {
        for ambiguous in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut execution = execution(root.path());
            let journal = journal(root.path(), limits());
            if ambiguous {
                execution.driver_id = "codex".into();
                let directory = root.path().join("sessions");
                std::fs::create_dir(&directory).unwrap();
                execution.observation.as_mut().unwrap().file = Some((
                    boss_engine_driver::AgentJsonlFileIngress {
                        directory: directory.clone(),
                        filename_prefix: "rollout-".into(),
                        filename_suffix: ".jsonl".into(),
                        workspace_path: root.path().into(),
                    },
                    Vec::new(),
                ));
                for id in ["one", "two"] {
                    let meta = serde_json::json!({"type":"session_meta", "payload":{"id":id,"cwd":root.path()}});
                    std::fs::write(directory.join(format!("rollout-{id}.jsonl")), format!("{meta}\n")).unwrap();
                }
            } else {
                let target = root.path().join("actual.jsonl");
                let link = root.path().join("link.jsonl");
                std::fs::write(&target, b"{}").unwrap();
                std::os::unix::fs::symlink(&target, &link).unwrap();
                hooks::ingest(&journal, &execution, &hook(Some(&link)), &stamp().unwrap())
                    .await
                    .unwrap();
            }
            execution.phase = Phase::Terminal;
            let pending = execution.observation.as_ref().unwrap().pending.join("unprocessed.json");
            std::fs::write(&pending, b"{}").unwrap();
            assert!(
                transcripts::poll(&journal, &execution, &stamp().unwrap())
                    .await
                    .is_err()
            );
            assert!(
                journal
                    .lock()
                    .unwrap()
                    .source(&run_identity(&execution), "drained")
                    .unwrap()
                    .is_none()
            );
            std::fs::remove_file(pending).unwrap();
            transcripts::poll(&journal, &execution, &stamp().unwrap())
                .await
                .unwrap();
            assert!(matches!(&records(&journal).last().unwrap().body,
                Some(record::Body::Gap(gap)) if gap.resource == "transcript"));
            assert!(
                journal
                    .lock()
                    .unwrap()
                    .source(&run_identity(&execution), "drained")
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[tokio::test]
    async fn terminal_missing_and_invalid_sources_record_gap_and_drain() {
        for path in [None, Some(Path::new("relative.jsonl"))] {
            let root = tempfile::tempdir().unwrap();
            let mut execution = execution(root.path());
            let journal = journal(root.path(), limits());
            if path.is_some() {
                hooks::ingest(&journal, &execution, &hook(path), &stamp().unwrap())
                    .await
                    .unwrap();
                assert!(
                    transcripts::poll(&journal, &execution, &stamp().unwrap())
                        .await
                        .is_err()
                );
            }
            execution.phase = Phase::Terminal;
            transcripts::poll(&journal, &execution, &stamp().unwrap())
                .await
                .unwrap();
            let count = records(&journal).len();
            transcripts::poll(&journal, &execution, &stamp().unwrap())
                .await
                .unwrap();
            assert_eq!(records(&journal).len(), count);
            assert!(matches!(&records(&journal).last().unwrap().body,
                Some(record::Body::Gap(gap)) if gap.resource == "transcript"));
            assert!(
                journal
                    .lock()
                    .unwrap()
                    .source(&run_identity(&execution), "drained")
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[tokio::test]
    async fn restart_drains_real_shim_without_a_next_hook_and_deduplicates_replayed_pending() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let original = journal(root.path(), limits());
        original
            .lock()
            .unwrap()
            .save_execution("run", "token", &serde_json::to_vec(&execution).unwrap())
            .unwrap();
        shim(root.path(), &root.path().join("offline.sock"), &hook(None)).await;
        let pending = std::fs::read_dir(root.path().join("pending"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .unwrap();
        let bytes = std::fs::read(&pending).unwrap();
        // Simulate a crash between the durable write and final rename.
        let temporary = pending.with_extension("tmp");
        std::fs::rename(&pending, &temporary).unwrap();
        let reopened = reopen(root.path(), limits(), original).await;
        let observer = tokio::spawn(crate::observe(reopened.clone(), stamp));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while records(&reopened).is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        observer.abort();
        let _ = observer.await;
        assert!(!pending.exists());
        assert!(!temporary.exists());
        hooks::ingest(&reopened, &execution, &bytes, &stamp().unwrap())
            .await
            .unwrap();
        assert_eq!(records(&reopened).len(), 1);
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        changed["stop_hook_active"] = true.into();
        assert!(
            hooks::ingest(
                &reopened,
                &execution,
                &serde_json::to_vec(&changed).unwrap(),
                &stamp().unwrap()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn live_socket_authenticates_peer_and_only_receipts_committed_records() {
        let root = tempfile::Builder::new().prefix("ingress-").tempdir().unwrap();
        let execution = execution(root.path());
        let journal = journal(root.path(), limits());
        journal
            .lock()
            .unwrap()
            .save_execution("run", "token", &serde_json::to_vec(&execution).unwrap())
            .unwrap();
        let path = root.path().join("hook.sock");
        let listener = hooks::bind(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let server = tokio::spawn(hooks::serve(listener, journal.clone(), stamp));
        shim(root.path(), &path, &hook(None)).await;
        assert_eq!(records(&journal).len(), 1);
        assert!(!hooks::has_pending(&root.path().join("pending")).unwrap());
        server.abort();
        let _ = server.await;
        let pid = std::process::id() as i32;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        assert!(hooks::authenticate(std::slice::from_ref(&execution), pid, uid + 1, "boot").is_err());
        assert!(hooks::authenticate(std::slice::from_ref(&execution), pid, uid, "old-boot").is_err());
        let mut recycled = execution.clone();
        recycled.observation.as_mut().unwrap().pid_start = Some(0);
        assert!(hooks::authenticate(&[recycled], pid, uid, "boot").is_err());
        let mut forged: serde_json::Value = serde_json::from_slice(&hook(None)).unwrap();
        forged["_boss_run_id"] = "sibling".into();
        assert!(
            hooks::ingest(
                &journal,
                &execution,
                &serde_json::to_vec(&forged).unwrap(),
                &stamp().unwrap()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn transcript_offsets_survive_restart_and_rotation_changes_generation() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        let path = root.path().join("transcript.jsonl");
        std::fs::write(&path, b"first\n").unwrap();
        let original = journal(root.path(), limits());
        hooks::ingest(&original, &execution, &hook(Some(&path)), &stamp().unwrap())
            .await
            .unwrap();
        transcripts::poll(&original, &execution, &stamp().unwrap())
            .await
            .unwrap();
        let reopened = reopen(root.path(), limits(), original).await;
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"second\n")
            .unwrap();
        transcripts::poll(&reopened, &execution, &stamp().unwrap())
            .await
            .unwrap();
        transcripts::poll(&reopened, &execution, &stamp().unwrap())
            .await
            .unwrap();
        std::fs::rename(&path, root.path().join("old.jsonl")).unwrap();
        std::fs::write(&path, b"new generation\n").unwrap();
        transcripts::poll(&reopened, &execution, &stamp().unwrap())
            .await
            .unwrap();
        let captured = records(&reopened);
        let chunks: Vec<_> = captured
            .iter()
            .filter_map(|r| match &r.body {
                Some(record::Body::Transcript(t)) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!((chunks[0].offset, chunks[1].offset, chunks[2].offset), (0, 6, 0));
        assert_eq!(chunks[0].generation, chunks[1].generation);
        assert_ne!(chunks[0].generation, chunks[2].generation);
        assert!(captured.iter().any(|r| matches!(r.body, Some(record::Body::Gap(_)))));
        // Same inode, truncate and regrow beyond the old cursor also changes generation.
        std::fs::write(&path, b"replacement in the same inode\n").unwrap();
        transcripts::poll(&reopened, &execution, &stamp().unwrap())
            .await
            .unwrap();
        assert_ne!(records(&reopened).last().unwrap().body, captured.last().unwrap().body);
        let path = root.path().join("next.jsonl");
        std::fs::write(&path, b"").unwrap();
        let mut next: serde_json::Value = serde_json::from_slice(&hook(Some(&path))).unwrap();
        next["_boss_producer_sequence"] = 2.into();
        hooks::ingest(
            &reopened,
            &execution,
            &serde_json::to_vec(&next).unwrap(),
            &stamp().unwrap(),
        )
        .await
        .unwrap();
        transcripts::poll(&reopened, &execution, &stamp().unwrap())
            .await
            .unwrap();
        let switched = records(&reopened);
        assert!(
            matches!(&switched[switched.len() - 2].body, Some(record::Body::Gap(g)) if g.reason.contains("path changed"))
        );
        assert!(
            matches!(&switched.last().unwrap().body, Some(record::Body::Transcript(t)) if t.offset == 0 && t.eof && t.data.is_empty() && t.generation != chunks[2].generation)
        );
        execution.phase = Phase::Terminal;
        std::fs::remove_file(&path).unwrap();
        transcripts::poll(&reopened, &execution, &stamp().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            records(&reopened).last().unwrap().body,
            Some(record::Body::Gap(_))
        ));
        assert!(
            reopened
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "drained")
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn pressure_preserves_source_cursor_and_leaves_lifecycle_reserve() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let path = root.path().join("transcript.jsonl");
        std::fs::write(&path, vec![b'x'; 64 * 1024]).unwrap();
        let journal = journal(
            root.path(),
            Limits {
                spool_bytes: 96 * 1024,
                lifecycle_reserve: 64 * 1024,
            },
        );
        hooks::ingest(&journal, &execution, &hook(Some(&path)), &stamp().unwrap())
            .await
            .unwrap();
        let error = transcripts::poll(&journal, &execution, &stamp().unwrap())
            .await
            .unwrap_err();
        assert!(Fault::is_full(&error), "{error}");
        assert!(
            journal
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "transcript")
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 * 1024);
        let mut payload: serde_json::Value = serde_json::from_slice(&hook(None)).unwrap();
        payload["_boss_producer_id"] = "fedcba9876543210fedcba9876543210".into();
        hooks::ingest(
            &journal,
            &execution,
            &serde_json::to_vec(&payload).unwrap(),
            &stamp().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(records(&journal).len(), 2);
        let Some(record::Body::Event(event)) = &records(&journal)[0].body else {
            panic!("missing Stop");
        };
        let Some(boss_protocol_spoke::v1::worker_event::Event::Stop(stop)) = &event.event else {
            panic!("missing Stop");
        };
        assert_eq!(stop.transcript_through, 64 * 1024);
        assert!(
            !stop.transcript_generation.is_empty(),
            "Stop watermark survives bulk pressure"
        );
        // A failed source transaction must neither consume sequence nor checkpoint.
        assert_eq!(journal.lock().unwrap().record_bounds().unwrap().1, 2);
        let record = records(&journal)[0].clone();
        while journal
            .lock()
            .unwrap()
            .append(record.clone(), Admission::Lifecycle)
            .is_ok()
        {}
        assert!(
            hooks::ingest(&journal, &execution, &hook(Some(&path)), &stamp().unwrap())
                .await
                .is_ok(),
            "duplicates need no space"
        );
    }

    #[tokio::test]
    async fn file_feed_reuses_discovery_and_normalizer_without_duplicate_hook_events() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        execution.driver_id = "codex".into();
        let directory = root.path().join("sessions");
        std::fs::create_dir(&directory).unwrap();
        execution.observation.as_mut().unwrap().file = Some((
            boss_engine_driver::AgentJsonlFileIngress {
                directory: directory.clone(),
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path: root.path().into(),
            },
            Vec::new(),
        ));
        let path = directory.join("rollout-example-session.jsonl");
        let meta = serde_json::json!({"type":"session_meta", "payload":{"id":"session", "cwd":root.path()}});
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        let journal = journal(root.path(), limits());
        hooks::ingest(&journal, &execution, &hook(None), &stamp().unwrap())
            .await
            .unwrap();
        assert!(
            records(&journal).is_empty(),
            "file-ingress hooks must not duplicate progress"
        );
        transcripts::poll(&journal, &execution, &stamp().unwrap())
            .await
            .unwrap();
        let count = records(&journal).len();
        assert!(count >= 2, "raw transcript and normalized session start");
        transcripts::poll(&journal, &execution, &stamp().unwrap())
            .await
            .unwrap();
        assert_eq!(records(&journal).len(), count);
        let cursor = journal
            .lock()
            .unwrap()
            .source(&run_identity(&execution), "transcript")
            .unwrap()
            .unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&cursor).unwrap()["progress"]["normalizer"].is_object());
        // Keep a large unterminated entry across a daemon restart, then flush
        // the final line only when the supervisor has observed process exit.
        use std::io::Write;
        let final_entry = serde_json::json!({"type":"event_msg", "payload":{
            "type":"task_complete", "last_agent_message":"é".repeat(90 * 1024)
        }});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(final_entry.to_string().as_bytes())
            .unwrap();
        for _ in 0..3 {
            transcripts::poll(&journal, &execution, &stamp().unwrap())
                .await
                .unwrap();
        }
        let reopened = reopen(root.path(), limits(), journal).await;
        execution.phase = Phase::Terminal;
        transcripts::poll(&reopened, &execution, &stamp().unwrap())
            .await
            .unwrap();
        let stops: Vec<_> = records(&reopened)
            .into_iter()
            .filter_map(|record| match record.body {
                Some(record::Body::Event(event)) => match event.event {
                    Some(boss_protocol_spoke::v1::worker_event::Event::Stop(stop)) => Some(stop),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].transcript_through, std::fs::metadata(&path).unwrap().len());
        assert!(
            reopened
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "drained")
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn invalid_oldest_pending_does_not_wedge_drain_or_transcript() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        let producer = "0123456789abcdef0123456789abcdef";
        let pending = root.path().join("pending");
        let empty = pending.join(format!("00000000000000000001-{producer}.tmp"));
        std::fs::File::create(&empty).unwrap();
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut wrong: serde_json::Value = serde_json::from_slice(&hook(None)).unwrap();
        wrong["_boss_run_id"] = "other".into();
        wrong["_boss_producer_sequence"] = 2.into();
        let wrong_path = pending.join(format!("00000000000000000002-{producer}.json"));
        std::fs::write(&wrong_path, serde_json::to_vec(&wrong).unwrap()).unwrap();
        std::fs::set_permissions(&wrong_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let path = root.path().join("transcript.jsonl");
        std::fs::write(&path, b"later\n").unwrap();
        let mut stop: serde_json::Value = serde_json::from_slice(&hook(Some(&path))).unwrap();
        stop["_boss_producer_sequence"] = 3.into();
        let stop_path = pending.join(format!("00000000000000000003-{producer}.json"));
        std::fs::write(&stop_path, serde_json::to_vec(&stop).unwrap()).unwrap();
        std::fs::set_permissions(&stop_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        execution.phase = Phase::Terminal;
        let original = journal(root.path(), limits());
        original
            .lock()
            .unwrap()
            .save_execution("run", "token", &serde_json::to_vec(&execution).unwrap())
            .unwrap();
        let observer = tokio::spawn(crate::observe(original.clone(), stamp));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while original
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "drained")
                .unwrap()
                .is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        observer.abort();
        let _ = observer.await;
        assert!(!empty.exists());
        assert!(!wrong_path.exists());
        assert!(!stop_path.exists());
        let captured = records(&original);
        assert!(
            captured
                .iter()
                .filter(|record| matches!(&record.body, Some(record::Body::Gap(gap)) if gap.resource.starts_with("pending:")))
                .count()
                >= 2
        );
        assert!(
            captured
                .iter()
                .any(|record| matches!(&record.body, Some(record::Body::Event(_))))
        );
        assert!(
            captured
                .iter()
                .any(|record| matches!(&record.body, Some(record::Body::Transcript(t)) if t.data == b"later\n"))
        );
        let reopened = reopen(root.path(), limits(), original).await;
        let observer = tokio::spawn(crate::observe(reopened.clone(), stamp));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        observer.abort();
        let _ = observer.await;
        assert!(
            reopened
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "drained")
                .unwrap()
                .is_some()
        );
        assert_eq!(records(&reopened).len(), captured.len());
    }

    #[tokio::test]
    async fn symlinked_oldest_pending_is_quarantined_and_later_stop_drains() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        let producer = "0123456789abcdef0123456789abcdef";
        let pending = root.path().join("pending");
        let target = root.path().join("target.json");
        std::fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, pending.join(format!("00000000000000000001-{producer}.json"))).unwrap();
        let path = root.path().join("transcript.jsonl");
        std::fs::write(&path, b"later\n").unwrap();
        let mut stop: serde_json::Value = serde_json::from_slice(&hook(Some(&path))).unwrap();
        stop["_boss_producer_sequence"] = 2.into();
        let stop_path = pending.join(format!("00000000000000000002-{producer}.json"));
        std::fs::write(&stop_path, serde_json::to_vec(&stop).unwrap()).unwrap();
        std::fs::set_permissions(&stop_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        execution.phase = Phase::Terminal;
        let journal = journal(root.path(), limits());
        hooks::drain(&journal, &execution, &stamp().unwrap()).await.unwrap();
        assert!(!stop_path.exists());
        assert!(
            records(&journal)
                .iter()
                .any(|r| matches!(&r.body, Some(record::Body::Event(_))))
        );
    }

    #[tokio::test]
    async fn drained_run_is_not_replayed_after_sources_are_pruned() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        execution.driver_id = "codex".into();
        execution.phase = Phase::Terminal;
        let directory = root.path().join("sessions");
        std::fs::create_dir(&directory).unwrap();
        execution.observation.as_mut().unwrap().file = Some((
            boss_engine_driver::AgentJsonlFileIngress {
                directory: directory.clone(),
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path: root.path().into(),
            },
            Vec::new(),
        ));
        let path = directory.join("rollout-example-session.jsonl");
        let meta = serde_json::json!({"type":"session_meta", "payload":{"id":"session", "cwd":root.path()}});
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        let journal = journal(root.path(), limits());
        journal
            .lock()
            .unwrap()
            .save_execution("run", "token", &serde_json::to_vec(&execution).unwrap())
            .unwrap();
        let observer = tokio::spawn(crate::observe(journal.clone(), stamp));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while journal
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "drained")
                .unwrap()
                .is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        observer.abort();
        let _ = observer.await;
        let before = records(&journal).len();
        journal
            .lock()
            .unwrap()
            .prune_sources(&run_identity(&execution))
            .unwrap();
        let observer = tokio::spawn(crate::observe(journal.clone(), stamp));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        observer.abort();
        let _ = observer.await;
        assert_eq!(records(&journal).len(), before);
    }

    #[tokio::test]
    async fn oversized_unterminated_entry_does_not_stall_cursor() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        execution.driver_id = "codex".into();
        let directory = root.path().join("sessions");
        std::fs::create_dir(&directory).unwrap();
        execution.observation.as_mut().unwrap().file = Some((
            boss_engine_driver::AgentJsonlFileIngress {
                directory: directory.clone(),
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path: root.path().into(),
            },
            Vec::new(),
        ));
        let path = directory.join("rollout-example-session.jsonl");
        let meta = serde_json::json!({"type":"session_meta", "payload":{"id":"session", "cwd":root.path()}});
        let mut body = meta.to_string();
        body.push('\n');
        body.push_str(&"x".repeat(6 * 64 * 1024));
        std::fs::write(&path, &body).unwrap();
        let journal = journal(
            root.path(),
            Limits {
                spool_bytes: 8 * 1024 * 1024,
                lifecycle_reserve: 64 * 1024,
            },
        );
        hooks::ingest(&journal, &execution, &hook(None), &stamp().unwrap())
            .await
            .unwrap();
        for _ in 0..2 {
            transcripts::poll(&journal, &execution, &stamp().unwrap())
                .await
                .unwrap();
        }
        receipt_through(&journal);
        drop(journal);
        let reopened = self::journal(
            root.path(),
            Limits {
                spool_bytes: 8 * 1024 * 1024,
                lifecycle_reserve: 64 * 1024,
            },
        );
        poll_available(&reopened, &execution).await;
        use std::io::Write;
        let complete = serde_json::json!({"type":"event_msg", "payload":{
            "type":"task_complete", "last_agent_message":"done"
        }});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("\n{complete}\n").as_bytes())
            .unwrap();
        execution.phase = Phase::Terminal;
        poll_available(&reopened, &execution).await;
        let captured = records(&reopened);
        assert!(captured.iter().any(|record| matches!(
            &record.body,
            Some(record::Body::Gap(gap)) if gap.reason.contains("256 KiB")
        )));
        assert!(captured.iter().any(|record| match &record.body {
            Some(record::Body::Event(event)) =>
                matches!(event.event, Some(boss_protocol_spoke::v1::worker_event::Event::Stop(_))),
            _ => false,
        }));
        assert!(
            reopened
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "drained")
                .unwrap()
                .is_some()
        );
        assert!(
            captured
                .iter()
                .filter_map(|record| match &record.body {
                    Some(record::Body::Transcript(t)) => Some(t.offset),
                    _ => None,
                })
                .max()
                .unwrap()
                + 64 * 1024
                >= std::fs::metadata(&path).unwrap().len()
        );
    }

    fn gateway(journal: &SharedJournal) -> boss_spoke_worker_api::Gateway {
        boss_spoke_worker_api::Gateway {
            journal: journal.clone(),
            state_dir: std::env::temp_dir(),
            probe: Arc::new(|_| Box::pin(async { Err(anyhow::anyhow!("no transcript")) })),
        }
    }

    fn save(journal: &SharedJournal, execution: &ExecutionRecord) {
        journal
            .lock()
            .unwrap()
            .save_execution(
                &execution.run_id,
                &execution.spawn_token,
                &serde_json::to_vec(execution).unwrap(),
            )
            .unwrap();
    }

    fn reply(execution: &ExecutionRecord, event: &boss_protocol::FrontendEvent) -> boss_protocol_spoke::v1::Command {
        boss_protocol_spoke::v1::Command {
            sequence: 1,
            command_id: "reply".into(),
            run: Some(run_identity(execution)),
            not_after_unix_ms: 99999,
            body: Some(boss_protocol_spoke::v1::command::Body::WorkerReply(
                boss_protocol_spoke::v1::WorkerReply {
                    request_id: "r1".into(),
                    code: boss_protocol_spoke::v1::ResultCode::Ok as i32,
                    result_json: serde_json::to_vec(event).unwrap(),
                    revision: "hub-1".into(),
                    observed_unix_ms: 4567,
                    reason: String::new(),
                },
            )),
        }
    }

    #[test]
    fn command_loop_routes_worker_replies_to_the_gateway_and_rejects_mismatches() {
        use boss_protocol::{FrontendEvent, PrBodyView};
        use boss_protocol_spoke::v1::{ResultCode, WorkerMethod};
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let journal = journal(root.path(), limits());
        save(&journal, &execution);
        let gateway = gateway(&journal);
        let run = run_identity(&execution);
        journal
            .lock()
            .unwrap()
            .commit_source(
                &run,
                "worker-request:r1",
                None,
                &serde_json::to_vec(&(WorkerMethod::PrBody as i32)).unwrap(),
                vec![],
            )
            .unwrap();
        let body = FrontendEvent::PrBodyResult {
            body: PrBodyView {
                pr_url: None,
                title: Some("Gateway".into()),
                body: Some("Evidence".into()),
            },
        };
        // A reply of the wrong shape for its request never reaches the cache.
        let wrong = FrontendEvent::PrStatusResult {
            status: boss_protocol::PrStatusView::builder().refreshed(false).build(),
        };
        let result = crate::apply_gateway_command(&gateway, &reply(&execution, &wrong));
        assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
        // A reply addressed to another assignment is refused.
        let mut foreign = reply(&execution, &body);
        foreign.run.as_mut().unwrap().spawn_token = "foreign".into();
        let result = crate::apply_gateway_command(&gateway, &foreign);
        assert_eq!(result.code, ResultCode::Rejected as i32);
        assert!(
            journal
                .lock()
                .unwrap()
                .source(&run, &format!("worker-cache:{}", WorkerMethod::PrBody as i32))
                .unwrap()
                .is_none()
        );
        let result = crate::apply_gateway_command(&gateway, &reply(&execution, &body));
        assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
        assert_eq!(result.command_id, "reply");
        assert!(
            journal
                .lock()
                .unwrap()
                .source(&run, &format!("worker-cache:{}", WorkerMethod::PrBody as i32))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn spawn_is_rejected_when_the_context_snapshot_cannot_be_provisioned() {
        use boss_protocol_spoke::v1::{ResultCode, Spawn, command};
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let journal = journal(root.path(), limits());
        save(&journal, &execution);
        let gateway = gateway(&journal);
        let spawn = |revision: &str| boss_protocol_spoke::v1::Command {
            sequence: 1,
            command_id: "spawn".into(),
            run: Some(run_identity(&execution)),
            not_after_unix_ms: 99999,
            body: Some(command::Body::Spawn(Spawn {
                run_context_json: b"{\"not\":\"a bundle\"}".to_vec(),
                context_revision: revision.into(),
                ..Default::default()
            })),
        };
        let rejection = crate::provision_rejection(&gateway, &spawn(""), &stamp().unwrap()).unwrap();
        assert_eq!(rejection.code, ResultCode::Rejected as i32);
        assert_eq!(rejection.command_id, "spawn");
        let rejection = crate::provision_rejection(&gateway, &spawn("rev-1"), &stamp().unwrap()).unwrap();
        assert_eq!(rejection.code, ResultCode::Rejected as i32);
    }

    #[tokio::test]
    async fn completion_watermark_follows_the_source_not_the_ingestion_cursor() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let path = root.path().join("transcript.jsonl");
        std::fs::write(&path, vec![b'x'; 64 * 1024]).unwrap();
        let journal = journal(root.path(), limits());
        assert!(
            transcripts::current_watermark(&journal, &execution).await.is_err(),
            "no resolved source is unavailable evidence, not offset zero"
        );
        hooks::ingest(&journal, &execution, &hook(Some(&path)), &stamp().unwrap())
            .await
            .unwrap();
        // Nothing has been ingested yet: the cursor lags the file entirely.
        assert!(
            journal
                .lock()
                .unwrap()
                .source(&run_identity(&execution), "transcript")
                .unwrap()
                .is_none()
        );
        let (generation, through) = transcripts::current_watermark(&journal, &execution).await.unwrap();
        assert_eq!(through, 64 * 1024);
        assert!(!generation.is_empty());
        std::fs::write(&path, vec![b'x'; 96 * 1024]).unwrap();
        assert_eq!(
            transcripts::current_watermark(&journal, &execution).await.unwrap().1,
            96 * 1024
        );
    }
    #[tokio::test]
    async fn stop_capture_marker_survives_ingestion_capacity_faults() {
        // Sweep admission boundaries to inject failures before and after each
        // source commit, then retry the actual ingestion after freeing space.
        for file_ingress in [false, true] {
            let mut failures = 0;
            for available in (100..2500).step_by(100) {
                let root = tempfile::tempdir().unwrap();
                let mut execution = execution(root.path());
                if file_ingress {
                    execution.driver_id = "codex".into();
                    let directory = root.path().join("sessions");
                    std::fs::create_dir(&directory).unwrap();
                    execution.observation.as_mut().unwrap().file = Some((
                        boss_engine_driver::AgentJsonlFileIngress {
                            directory: directory.clone(),
                            filename_prefix: "rollout-".into(),
                            filename_suffix: ".jsonl".into(),
                            workspace_path: root.path().into(),
                        },
                        Vec::new(),
                    ));
                    let meta = serde_json::json!({"type":"session_meta","payload":{"id":"session","cwd":root.path()}});
                    let stop = serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}});
                    std::fs::write(
                        directory.join("rollout-example-session.jsonl"),
                        format!("{meta}\n{stop}\n"),
                    )
                    .unwrap();
                }
                let test_limits = Limits {
                    spool_bytes: 12_000,
                    lifecycle_reserve: 3_000,
                };
                let journal = journal(root.path(), test_limits);
                let run = run_identity(&execution);
                let filler = vec![0; 4_000];
                {
                    let mut journal = journal.lock().unwrap();
                    journal.commit_source(&run, "filler-a", None, &filler, vec![]).unwrap();
                    let used = journal.bytes().unwrap() as usize;
                    let tail = vec![0; test_limits.spool_bytes as usize - used - available - "filler-b".len() - 128];
                    journal.commit_source(&run, "filler-b", None, &tail, vec![]).unwrap();
                }
                let result = if file_ingress {
                    transcripts::poll(&journal, &execution, &stamp().unwrap()).await
                } else {
                    hooks::ingest(&journal, &execution, &hook(None), &stamp().unwrap()).await
                };
                if let Err(error) = result {
                    assert!(Fault::is_full(&error), "{error:#}");
                    failures += 1;
                }
                let has_stop = records(&journal).iter().any(|record| {
                    matches!(&record.body,
                    Some(record::Body::Event(event)) if matches!(event.event,
                        Some(boss_protocol_spoke::v1::worker_event::Event::Stop(_))))
                });
                if has_stop {
                    assert!(
                        !boss_spoke_worker_api::captures_concluded(&journal, &execution).unwrap(),
                        "accepted Stop must already have a durable capture marker"
                    );
                }
                journal
                    .lock()
                    .unwrap()
                    .delete_sources(&run, &["filler-a".into(), "filler-b".into()])
                    .unwrap();
                if file_ingress {
                    transcripts::poll(&journal, &execution, &stamp().unwrap())
                        .await
                        .unwrap();
                } else {
                    hooks::ingest(&journal, &execution, &hook(None), &stamp().unwrap())
                        .await
                        .unwrap();
                }
                assert!(!boss_spoke_worker_api::captures_concluded(&journal, &execution).unwrap());
            }
            assert!(failures > 0);
        }
    }
}
