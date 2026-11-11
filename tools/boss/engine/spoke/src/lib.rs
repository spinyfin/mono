//! Host-local observations terminate at durable normalized journal records.
pub mod hooks;
pub mod transcripts;

use anyhow::Result;
use boss_host_execution::worker::ExecutionRecord;
use boss_protocol_spoke::v1::{Run, Stamp};
use boss_spoke_outbound::SharedJournal;

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
        Arc::new(Mutex::new(Journal::open(&root.join("state"), "host", limits).unwrap()))
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
        drop(original);
        let reopened = journal(root.path(), limits());
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
        drop(original);
        let reopened = journal(root.path(), limits());
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
        drop(journal);
        let reopened = self::journal(root.path(), limits());
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
        drop(original);
        let reopened = journal(root.path(), limits());
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
}
