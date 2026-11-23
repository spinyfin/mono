use super::*;

#[test]
fn every_legacy_slot_round_trips_and_classifies() {
    for (start, end, pool, prefix) in [
        (1, 16, WorkerSlotPool::Local(DispatchPool::Main), "worker-"),
        (17, 24, WorkerSlotPool::Local(DispatchPool::Automation), "auto-worker-"),
        (25, 40, WorkerSlotPool::Local(DispatchPool::Review), "review-"),
        (41, 44, WorkerSlotPool::Local(DispatchPool::Progression), "progression-"),
        (200, 255, WorkerSlotPool::Remote, "worker-"),
    ] {
        for slot in start..=end {
            let identity = WorkerSlotIdentity::from_legacy_slot(slot).unwrap();
            assert_eq!(identity.pool, pool);
            assert_eq!(identity.index.get(), u32::from(slot - start + 1));
            assert_eq!(identity.to_legacy_slot(), Some(slot));
            let ordinal = if pool == WorkerSlotPool::Remote {
                u32::from(slot)
            } else {
                identity.index.get()
            };
            let id = format!("{prefix}{ordinal}");
            assert_eq!(identity.to_legacy_worker_id(), Some(id.clone()));
            assert_eq!(WorkerSlotIdentity::from_legacy_worker_id(&id), Some(identity));
        }
    }
}

#[test]
fn invalid_and_overlapping_legacy_values_are_rejected() {
    for slot in std::iter::once(0).chain(45..200) {
        assert_eq!(WorkerSlotIdentity::from_legacy_slot(slot), None);
    }
    for id in [
        "",
        "worker-0",
        "worker-17",
        "worker-25",
        "worker-32",
        "worker-64",
        "worker-199",
        "worker-256",
        "auto-worker-9",
        "review-17",
        "progression-5",
        "progression-160",
        "worker-01",
        "worker-+1",
        "worker--1",
        "worker-",
        "worker-a",
        "worker-4294967296",
        "remote-1",
        "review-0",
        "auto-worker-0",
        "progression-0",
    ] {
        assert_eq!(WorkerSlotIdentity::from_legacy_worker_id(id), None, "{id}");
    }
}

#[test]
fn canonical_indexes_are_positive_and_independent_of_legacy_capacity() {
    let pools = [
        WorkerSlotPool::Local(DispatchPool::Main),
        WorkerSlotPool::Local(DispatchPool::Automation),
        WorkerSlotPool::Local(DispatchPool::Review),
        WorkerSlotPool::Local(DispatchPool::Progression),
        WorkerSlotPool::Remote,
    ];
    for pool in pools {
        assert_eq!(WorkerSlotIdentity::new(pool, 0), None);
        for index in [32, 64, 256, 1024, u32::MAX] {
            let identity = WorkerSlotIdentity::new(pool, index).unwrap();
            let json = serde_json::to_string(&identity).unwrap();
            assert_eq!(serde_json::from_str::<WorkerSlotIdentity>(&json).unwrap(), identity);
            for other in pools {
                assert_eq!(
                    identity == WorkerSlotIdentity::new(other, index).unwrap(),
                    pool == other
                );
            }
            if pool != WorkerSlotPool::Remote || index > 56 {
                assert_eq!(identity.to_legacy_slot(), None);
                assert_eq!(identity.to_legacy_worker_id(), None);
            }
        }
    }
    assert!(serde_json::from_str::<WorkerSlotIdentity>(r#"{"pool":{"local":"main"},"index":0}"#).is_err());
}
