use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

type Loads = SingleFlight<&'static str, usize, usize>;

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "workers did not reach the fence");
        thread::yield_now();
    }
}

fn waiters(loads: &Loads, pending: bool, count: usize) {
    wait_for(|| {
        let state = loads.state.lock().unwrap();
        state.slots.get("board").is_some_and(|slot| {
            let flight = if pending {
                slot.next.as_ref()
            } else {
                Some(&slot.current)
            };
            flight.is_some_and(|flight| Arc::strong_count(flight) > count)
        })
    });
}

fn burst(error: bool) {
    let loads = Loads::default();
    let scans = AtomicUsize::new(0);
    let (started, running) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            loads
                .run(
                    "board",
                    || Ok(0),
                    || {
                        scans.fetch_add(1, Ordering::SeqCst);
                        started.send(()).unwrap();
                        gate.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
                        if error { Err("broken projection".into()) } else { Ok(42) }
                    },
                )
                .unwrap()
        });
        running.recv().unwrap();
        let followers: Vec<_> = (0..31)
            .map(|_| {
                scope.spawn(|| {
                    loads
                        .run(
                            "board",
                            || Ok(0),
                            || {
                                scans.fetch_add(1, Ordering::SeqCst);
                                Ok(99)
                            },
                        )
                        .unwrap()
                })
            })
            .collect();
        waiters(&loads, false, 32);
        release.send(()).unwrap();
        let result = first.join().unwrap();
        assert_eq!(
            result.value,
            if error { Err("broken projection".into()) } else { Ok(42) }
        );
        for follower in followers {
            assert!(Arc::ptr_eq(&result, &follower.join().unwrap()));
        }
        assert_eq!(scans.load(Ordering::SeqCst), 1);
    });
    assert!(loads.state.lock().unwrap().slots.is_empty());
    assert_eq!(
        loads.run("board", || Ok(0), || Ok(100)).unwrap().value,
        Ok(100),
        "idle results are not cached"
    );
}

#[test]
fn concurrent_requests_share_exactly_one_scan() {
    burst(false);
}

#[test]
fn every_waiter_receives_the_shared_error() {
    burst(true);
}

#[test]
fn invalidated_waiters_share_one_follow_up_started_after_all_prior_invalidations() {
    let loads = Loads::default();
    let epoch = AtomicUsize::new(0);
    let scans = AtomicUsize::new(0);
    let (started, running) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            loads
                .run(
                    "board",
                    || Ok(epoch.load(Ordering::SeqCst)),
                    || {
                        scans.fetch_add(1, Ordering::SeqCst);
                        started.send(()).unwrap();
                        gate.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(0)
                    },
                )
                .unwrap()
        });
        running.recv().unwrap();
        let mut followers = Vec::new();
        for invalidation in 1..=20 {
            epoch.store(invalidation, Ordering::SeqCst);
            followers.push(scope.spawn(|| {
                loads
                    .run(
                        "board",
                        || Ok(epoch.load(Ordering::SeqCst)),
                        || {
                            scans.fetch_add(1, Ordering::SeqCst);
                            Ok(epoch.load(Ordering::SeqCst))
                        },
                    )
                    .unwrap()
            }));
            waiters(&loads, true, invalidation);
        }
        assert_eq!(
            scans.load(Ordering::SeqCst),
            1,
            "follow-up cannot overlap the first scan"
        );
        release.send(()).unwrap();
        let old = first.join().unwrap();
        assert_eq!(old.value, Ok(0));
        let results: Vec<_> = followers.into_iter().map(|f| f.join().unwrap()).collect();
        for result in &results {
            assert_eq!(result.value, Ok(20));
            assert!(result.sequence > old.sequence);
            assert!(Arc::ptr_eq(result, &results[0]));
        }
        assert_eq!(scans.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn different_keys_finish_while_one_key_is_blocked() {
    let loads = Loads::default();
    let (started, running) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            loads
                .run(
                    "board",
                    || Ok(0),
                    || {
                        started.send(()).unwrap();
                        gate.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(1)
                    },
                )
                .unwrap()
        });
        running.recv().unwrap();
        assert_eq!(loads.run("another board", || Ok(0), || Ok(2)).unwrap().value, Ok(2));
        release.send(()).unwrap();
        first.join().unwrap();
    });
}

#[test]
fn losing_the_owners_response_does_not_cancel_other_waiters() {
    let loads = Loads::default();
    let (started, running) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    let (reply, disconnected) = mpsc::channel();
    drop(disconnected);
    thread::scope(|scope| {
        scope.spawn(|| {
            let result = loads.run(
                "board",
                || Ok(0),
                || {
                    started.send(()).unwrap();
                    gate.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(42)
                },
            );
            assert!(reply.send(result).is_err());
        });
        running.recv().unwrap();
        let follower = scope.spawn(|| loads.run("board", || Ok(0), || panic!("duplicate scan")).unwrap());
        waiters(&loads, false, 2);
        release.send(()).unwrap();
        assert_eq!(follower.join().unwrap().value, Ok(42));
    });
}

#[test]
fn panicking_loader_releases_waiters_and_allows_retry() {
    let loads = Loads::default();
    assert_eq!(
        loads
            .run("board", || Ok(0), || panic!("projection panic"))
            .unwrap()
            .value,
        Err("shared load panicked".into())
    );
    assert_eq!(loads.run("board", || Ok(0), || Ok(2)).unwrap().value, Ok(2));
}
