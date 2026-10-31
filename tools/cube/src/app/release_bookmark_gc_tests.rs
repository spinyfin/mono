use super::*;
use std::sync::{Arc, Barrier, mpsc};

fn burst(scheduler: &Scheduler, spawned: &mpsc::Sender<File>) {
    std::thread::scope(|scope| {
        let start = Arc::new(Barrier::new(33));
        for _ in 0..32 {
            let start = Arc::clone(&start);
            scope.spawn(move || {
                start.wait();
                scheduler
                    .trigger(|lock| {
                        spawned.send(lock).unwrap();
                        Ok(())
                    })
                    .unwrap();
            });
        }
        start.wait();
    });
}

#[test]
fn concurrent_triggers_share_one_worker_and_one_pending_rerun() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = Scheduler::new(Some(&dir.path().join("state.db")), "mono").unwrap();
    let (spawned, children) = mpsc::channel();
    // The child has not even started yet; its inherited lock already excludes
    // other spawns. All requests before its first pass share that pass.
    burst(&scheduler, &spawned);
    let running = children.try_recv().unwrap();
    assert!(children.try_recv().is_err());
    let (entered, sweeps) = mpsc::channel();
    let (resume, blocked) = mpsc::channel();
    std::thread::scope(|scope| {
        let scheduler = &scheduler;
        let worker = scope.spawn(move || {
            scheduler
                .drain(running, || {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                })
                .unwrap();
        });
        sweeps.recv().unwrap();
        burst(scheduler, &spawned);
        assert!(children.try_recv().is_err());
        resume.send(()).unwrap();
        sweeps.recv().unwrap();
        resume.send(()).unwrap();
        worker.join().unwrap();
        assert!(sweeps.try_recv().is_err(), "only one rerun for the burst");
    });
    assert!(!scheduler.dirty.exists());
    // Requests after the final unlock must start a new worker.
    scheduler
        .trigger(|lock| {
            spawned.send(lock).unwrap();
            Ok(())
        })
        .unwrap();
    assert!(children.try_recv().is_ok());
}

#[test]
fn final_unlock_racing_a_trigger_never_loses_work() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = Scheduler::new(Some(&dir.path().join("state.db")), "mono").unwrap();
    for _ in 0..32 {
        let (spawned, children) = mpsc::channel();
        scheduler
            .trigger(|lock| {
                spawned.send(lock).unwrap();
                Ok(())
            })
            .unwrap();
        let running = children.recv().unwrap();
        let race = Barrier::new(2);
        let mut passes = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                race.wait();
                scheduler
                    .trigger(|lock| {
                        spawned.send(lock).unwrap();
                        Ok(())
                    })
                    .unwrap();
            });
            scheduler
                .drain(running, || {
                    passes += 1;
                    if passes == 1 {
                        race.wait();
                    }
                })
                .unwrap();
        });
        if let Ok(next) = children.try_recv() {
            scheduler.drain(next, || passes += 1).unwrap();
        }
        assert_eq!(passes, 2);
        assert!(!scheduler.dirty.exists());
    }
}

#[test]
fn failed_spawn_and_dead_worker_do_not_strand_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = Scheduler::new(Some(&dir.path().join("state.db")), "mono").unwrap();
    assert!(
        scheduler
            .trigger(|_| Err(CubeError::InvalidArgument("spawn failed".into())))
            .is_err()
    );
    let mut child = None;
    scheduler
        .trigger(|lock| {
            child = Some(lock);
            Ok(())
        })
        .unwrap();
    drop(child.take()); // A killed child releases its inherited descriptor.
    scheduler
        .trigger(|lock| {
            child = Some(lock);
            Ok(())
        })
        .unwrap();
    let mut passes = 0;
    scheduler.drain(child.unwrap(), || passes += 1).unwrap();
    assert_eq!(passes, 1);
}

#[test]
fn different_repos_can_sweep_independently() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let first = Scheduler::new(Some(&database), "first").unwrap();
    let second = Scheduler::new(Some(&database), "second").unwrap();
    let mut held = Vec::new();
    for scheduler in [&first, &second] {
        scheduler
            .trigger(|lock| {
                held.push(lock);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(held.len(), 2);
}

#[test]
fn detached_process_inherits_lock_and_audits_sweep_failure() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let source = dir.path().join("missing-source");
    Store::open_at(&database)
        .unwrap()
        .upsert_repo(&crate::metadata::RepoRecord {
            repo: "mono".into(),
            origin: "https://example.invalid/mono".into(),
            main_branch: "main".into(),
            workspace_root: dir.path().join("workspaces"),
            workspace_prefix: "mono-agent-".into(),
            source: Some(source.clone()),
            clone_command: None,
        })
        .unwrap();
    let scheduler = Scheduler::new(Some(&database), "mono").unwrap();
    let log = dir.path().join("worker.log");
    let mut child = None;
    scheduler
        .trigger(|lock| {
            let mut command = std::process::Command::new(std::env::var_os("CUBE_TEST_BIN").unwrap());
            command
                .arg("release-bookmark-gc")
                .arg("mono")
                .arg("--database")
                .arg(&database);
            command.env("CUBE_CONFIG_DIR", dir.path());
            child = Some(
                super::super::periodic_cleanup::spawn_command_with_input(&mut command, &log, lock.into()).unwrap(),
            );
            Ok(())
        })
        .unwrap();
    assert!(
        child.unwrap().wait().unwrap().success(),
        "{}",
        std::fs::read_to_string(&log).unwrap()
    );
    assert!(!scheduler.dirty.exists(), "child drained the requested pass");
    assert!(RepoLock::try_acquire(&scheduler.running).unwrap().is_some());
    let events: Vec<serde_json::Value> = std::fs::read_dir(crate::paths::audit_dir_in(dir.path()))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    let failure = events
        .iter()
        .find(|event| event["event"] == "workspace.release_bookmark_gc_failed")
        .unwrap();
    assert_eq!(failure["repo"], "mono");
    assert!(
        failure["error"].as_str().unwrap().contains(source.to_str().unwrap()),
        "{failure}"
    );
}
