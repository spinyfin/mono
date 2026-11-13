//! Real WorkDb receipt transactions, with identical SQL in both modes.
use super::*;
use std::sync::{Arc, Barrier};
use std::time::Instant;

#[test]
fn durable_ingress_scale_measurement() {
    let samples: usize = std::env::var("BOSS_BENCH_SAMPLES")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(1);
    for workers in [8, 32, 64] {
        for sample in 0..samples {
            for batched in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let db = WorkDb::open(dir.path().join("work.db")).unwrap();
                let admissions: Vec<_> = (0..workers)
                    .map(|host| {
                        let host = format!("host-{host}");
                        let admission = db.open_spoke_journal(&host, "journal", 0, 1).unwrap();
                        (host, admission)
                    })
                    .collect();
                let barrier = Arc::new(Barrier::new(workers + 1));
                let mut threads = Vec::new();
                for (host, admission) in admissions {
                    let db = db.clone();
                    let barrier = barrier.clone();
                    threads.push(std::thread::spawn(move || {
                        let frame = vec![42; 1024];
                        let mut latency = Vec::new();
                        barrier.wait();
                        for sequence in 1..=32 {
                            let started = Instant::now();
                            let record = commit(&host, &admission, sequence, &frame, &[], None);
                            let received = if batched {
                                db.commit_spoke_record(record).unwrap()
                            } else {
                                let mut conn = db.connect().unwrap();
                                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
                                let received = commit_in_tx(&tx, record, DEFAULT_QUOTA).unwrap();
                                tx.commit().unwrap();
                                received
                            };
                            assert_eq!(received, sequence);
                            latency.push(started.elapsed().as_secs_f64() * 1000.0);
                        }
                        assert_eq!(db.spoke_journal(&host).unwrap().unwrap().received, 32);
                        latency
                    }));
                }
                let start = Instant::now();
                barrier.wait();
                let mut latency: Vec<_> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
                let elapsed = start.elapsed().as_secs_f64();
                latency.sort_by(f64::total_cmp);
                let records = latency.len();
                let commits = if batched {
                    db.ingress_writer().unwrap().committed_batches()
                } else {
                    records as u64
                };
                println!(
                    "durable_ingress workers={workers} sample={sample} batched={batched} records={records} commits={commits} records_per_second={:.1} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3}",
                    records as f64 / elapsed,
                    latency[records / 2],
                    latency[records * 95 / 100],
                    latency[records * 99 / 100]
                );
                assert!(commits <= records as u64);
            }
        }
    }
}
