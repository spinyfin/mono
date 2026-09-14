use super::*;

#[test]
fn burst_extends_first_worker_and_retains_contention_after_peers_finish() {
    let load = DiscoveryLoad::default();
    let first = load.begin();
    assert_eq!(first.timeout(), Duration::from_secs(120));
    let peers: Vec<_> = (0..5).map(|_| load.begin()).collect();
    assert_eq!(first.timeout(), Duration::from_secs(165));
    assert!(peers.iter().all(|peer| peer.timeout() == first.timeout()));
    drop(peers);
    assert_eq!(first.timeout(), Duration::from_secs(165));
    drop(first);
    assert_eq!(load.begin().timeout(), Duration::from_secs(120));
}

#[test]
fn repeated_arrivals_cannot_extend_a_dead_spawn_forever() {
    let load = DiscoveryLoad::default();
    let first = load.begin();
    let peers: Vec<_> = (0..100).map(|_| load.begin()).collect();
    assert_eq!(first.timeout(), Duration::from_secs(240));
    drop(peers);
    for _ in 0..100 {
        assert_eq!(load.begin().timeout(), Duration::from_secs(129));
    }
    assert_eq!(first.timeout(), Duration::from_secs(240));
}

#[test]
fn resume_paces_backlog_until_driver_ready_but_never_new_arrivals() {
    let now = Instant::now();
    let mut admission = ResumeAdmission::default();
    admission.observe_ready(["one".into(), "two".into()].into_iter());
    assert!(!admission.holds("two", true, now));
    admission.resume(now);
    admission.observe_ready(["one".into(), "two".into()].into_iter());
    assert!(!admission.holds("one", false, now));
    assert!(admission.holds("two", true, now));
    assert!(!admission.holds("two", false, now));
    admission.observe_ready(["two".into(), "new".into()].into_iter());
    assert!(!admission.holds("new", true, now));
    assert!(admission.holds("two", true, now));
    admission.observe_ready(std::iter::empty());
    assert!(!admission.holds("two", true, now));
    admission.resume(now);
    admission.observe_ready(["new".into()].into_iter());
    assert!(admission.holds("new", true, now));
}

#[test]
fn no_signal_fallback_reopens_one_admission_and_restarts_only_on_actual_admission() {
    let now = Instant::now();
    let mut admission = ResumeAdmission::default();
    admission.resume(now);
    admission.observe_ready(["one".into(), "two".into(), "three".into()].into_iter());
    admission.admitted("one", now);
    let before = now + RESUME_STARTUP_INTERVAL - Duration::from_millis(1);
    assert!(admission.holds("two", true, before));
    let due = now + RESUME_STARTUP_INTERVAL;
    assert!(!admission.holds("two", true, due));
    // Merely considering an ineligible row must not consume the allowance.
    assert!(!admission.holds("three", true, due));
    admission.admitted("two", due);
    assert!(admission.holds("three", true, due));
    assert!(!admission.holds("three", false, due));
    assert!(!admission.holds("three", true, due + RESUME_STARTUP_INTERVAL));
}
