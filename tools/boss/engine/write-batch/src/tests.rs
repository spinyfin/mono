use super::*;

fn connection() -> Arc<Mutex<Connection>> {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE writes (id INTEGER PRIMARY KEY, host TEXT)")
        .unwrap();
    Arc::new(Mutex::new(c))
}

fn job(bytes: usize, urgent: bool) -> Job {
    Job {
        bytes,
        urgent,
        queued: Instant::now(),
        write: Box::new(|_| Ok(Box::new(|_| {}))),
    }
}

#[test]
fn bounds_fifo_and_round_robin_include_a_quiet_host() {
    let mut q = Queue::default();
    for _ in 0..MAX_HOST_QUEUED {
        q.push("busy".into(), job(1, false));
    }
    assert!(q.full("busy", 1));
    assert!(!q.full("quiet", 1));
    q.push("quiet".into(), job(2, true));
    let batch = q.take();
    assert_eq!(batch.len(), MAX_BATCH_RECORDS);
    assert!(batch.iter().take(2).any(|j| j.bytes == 2));
    assert_eq!(q.count, MAX_HOST_QUEUED + 1 - MAX_BATCH_RECORDS);
    let mut q = Queue::default();
    q.push("a".into(), job(MAX_BATCH_BYTES, false));
    q.push("a".into(), job(1, true));
    q.push("b".into(), job(1, false));
    assert_eq!(q.take().len(), 1);
    let batch = q.take();
    assert!(batch[0].urgent, "ready control result gets the first turn");
    assert!(!batch[1].urgent, "other hosts still receive a turn");
}

#[test]
fn urgent_work_flushes_without_waiting_and_deadline_is_oldest_not_last_arrival() {
    let mut q = Queue::default();
    let first = job(1, false);
    let deadline = first.queued + MAX_DELAY;
    q.push("a".into(), first);
    assert!(!q.ready());
    q.push("b".into(), job(1, false));
    assert_eq!(q.deadline(), Some(deadline));
    q.push("a".into(), job(1, true));
    assert!(q.ready());
}

#[test]
fn batch_sized_control_flood_cannot_starve_another_host() {
    let mut q = Queue::default();
    for _ in 0..4 {
        q.push("control".into(), job(MAX_BATCH_BYTES, true));
    }
    q.push("quiet".into(), job(1, false));
    assert!(q.take()[0].urgent);
    assert!(!q.take()[0].urgent, "priority must yield to ordinary rotation");
}

#[test]
fn no_receipt_before_commit_and_shutdown_drains_admitted_work() {
    let c = connection();
    let writer = Writer::new(c.clone()).unwrap();
    let (entered, running) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let receipt = writer
        .submit("a".into(), 1, true, move |tx| {
            tx.execute("INSERT INTO writes VALUES (1, 'a')", [])?;
            entered.send(()).unwrap();
            gate.recv().unwrap();
            Ok(7)
        })
        .unwrap();
    running.recv().unwrap();
    assert!(matches!(receipt.0.try_recv(), Err(mpsc::TryRecvError::Empty)));
    drop(writer);
    release.send(()).unwrap();
    assert_eq!(receipt.wait().unwrap(), 7);
    assert_eq!(
        c.lock()
            .unwrap()
            .query_row("SELECT count(*) FROM writes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn failed_job_rolls_back_its_partial_writes_without_poisoning_other_hosts() {
    let c = connection();
    let writer = Writer::new(c.clone()).unwrap();
    let failed: Receipt<()> = writer
        .submit("bad".into(), 1, false, |tx| {
            tx.execute("INSERT INTO writes VALUES (1, 'bad')", [])?;
            anyhow::bail!("invalid record")
        })
        .unwrap();
    let good = writer
        .submit("good".into(), 1, false, |tx| {
            tx.execute("INSERT INTO writes VALUES (2, 'good')", [])?;
            Ok(())
        })
        .unwrap();
    assert!(failed.wait().is_err());
    good.wait().unwrap();
    assert_eq!(
        c.lock()
            .unwrap()
            .query_row("SELECT id FROM writes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn commit_failure_never_publishes_success_and_reopen_sees_no_partial_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let c = Connection::open(&path).unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
        CREATE TABLE receipt(id INTEGER PRIMARY KEY);
        CREATE TABLE frame(id INTEGER REFERENCES receipt(id) DEFERRABLE INITIALLY DEFERRED);",
    )
    .unwrap();
    let writer = Writer::new(Arc::new(Mutex::new(c))).unwrap();
    let receipt = writer
        .submit("host".into(), 1, false, |tx| {
            tx.execute("INSERT INTO receipt VALUES (1)", [])?;
            tx.execute("INSERT INTO frame VALUES (2)", [])?;
            Ok(1)
        })
        .unwrap();
    assert!(receipt.wait().unwrap_err().to_string().contains("commit failed"));
    drop(writer);
    let c = Connection::open(path).unwrap();
    assert_eq!(
        c.query_row("SELECT count(*) FROM receipt", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM frame", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn sparse_writes_flush_and_host_order_is_preserved() {
    let c = connection();
    let writer = Writer::new(c.clone()).unwrap();
    let mut receipts = Vec::new();
    for id in 1..=70 {
        receipts.push(
            writer
                .submit("host".into(), 1, false, move |tx| {
                    let previous: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) FROM writes", [], |r| r.get(0))?;
                    anyhow::ensure!(previous == id - 1, "host order changed");
                    tx.execute("INSERT INTO writes VALUES (?1, 'host')", [id])?;
                    Ok(id)
                })
                .unwrap(),
        );
    }
    for (index, receipt) in receipts.into_iter().enumerate() {
        assert_eq!(receipt.wait().unwrap(), index as i64 + 1);
    }
    let receipt = writer.submit("quiet".into(), 1, false, |_| Ok(())).unwrap();
    receipt.0.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    // An oversized record is not rejected: it commits alone as a batch of one.
    let before = writer.committed_batches();
    writer
        .submit("oversized".into(), MAX_BATCH_BYTES + 1, false, |_| Ok(7))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(writer.committed_batches(), before + 1);
}

#[test]
fn process_exit_before_and_after_commit_keeps_receipt_and_bytes_atomic() {
    for (phase, expected_exit, expected_rows) in [("before", 23, 0), ("after", 24, 1)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash.db");
        let status = std::process::Command::new(std::env::var("CRASH_FIXTURE").unwrap())
            .arg(&path)
            .arg(phase)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(expected_exit));
        let c = Connection::open(path).unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM receipt", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            expected_rows
        );
        assert_eq!(
            c.query_row("SELECT count(*) FROM frames", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            expected_rows
        );
        if expected_rows == 1 {
            assert_eq!(
                c.query_row("SELECT bytes FROM frames", [], |r| r.get::<_, Vec<u8>>(0))
                    .unwrap(),
                [1, 2, 3]
            );
        }
    }
}
