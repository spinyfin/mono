use super::*;
#[path = "spoke_ingress_scale.rs"]
mod scale;
use std::path::PathBuf;

fn db() -> WorkDb {
    WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
}

fn spoke_db() -> WorkDb {
    let db = db();
    db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
    db
}

fn key(assignment: &SpokeAssignment) -> SpokeRunKey {
    SpokeRunKey {
        execution_id: assignment.execution_id.clone(),
        assignment_generation: assignment.assignment_generation,
        run_id: assignment.run_id.clone(),
        spawn_token: assignment.spawn_token.clone(),
    }
}

fn fault_of(error: &anyhow::Error) -> Option<HostOutboxFault> {
    error.downcast_ref::<HostOutboxFault>().copied()
}

fn commit<'a>(
    host_id: &'a str,
    admission: &'a SpokeJournalAdmission,
    sequence: u64,
    frame: &'a [u8],
    runs: &'a [SpokeRunKey],
    result: Option<SpokeIngressResult<'a>>,
) -> SpokeIngressCommit<'a> {
    SpokeIngressCommit::builder()
        .host_id(host_id)
        .generation(admission.generation)
        .journal_id(&admission.journal_id)
        .sequence(sequence)
        .frame(frame)
        .epoch(1)
        .features("execution=1")
        .observed_at_ms(1)
        .runs(runs)
        .maybe_result(result)
        .build()
}

#[test]
fn first_contact_admits_an_empty_journal_and_increments_generation() {
    let db = db();
    let first = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    assert_eq!(
        first,
        SpokeJournalAdmission {
            journal_id: "journal".into(),
            generation: 1,
            received: 0,
        }
    );
    let second = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    assert_eq!(second.generation, 2);
    assert_eq!(second.received, 0);
    assert_eq!(
        fault_of(&db.open_spoke_journal("host-a", "other", 0, 1).unwrap_err()),
        Some(HostOutboxFault::ReplayGap)
    );
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().generation, 2);
}

#[test]
fn a_spoke_that_forgot_records_or_compacted_past_receipt_is_a_replay_gap() {
    let db = db();
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    db.commit_spoke_record(commit("host-a", &admission, 1, b"one", &[], None))
        .unwrap();
    assert_eq!(
        fault_of(&db.open_spoke_journal("host-a", "journal", 0, 1).unwrap_err()),
        Some(HostOutboxFault::ReplayGap)
    );
    assert_eq!(
        fault_of(&db.open_spoke_journal("host-a", "journal", 1, 3).unwrap_err()),
        Some(HostOutboxFault::ReplayGap)
    );
    let again = db.open_spoke_journal("host-a", "journal", 1, 1).unwrap();
    assert_eq!(again.received, 1);
    assert_eq!(again.generation, 2);
}

#[test]
fn a_new_journal_that_compacted_past_the_first_record_is_a_replay_gap() {
    let db = db();
    assert_eq!(
        fault_of(&db.open_spoke_journal("host-a", "journal", 4, 2).unwrap_err()),
        Some(HostOutboxFault::ReplayGap)
    );
    assert!(db.spoke_journal("host-a").unwrap().is_none());
}

#[test]
fn a_new_journal_that_still_retains_its_first_record_replays_from_the_start() {
    let db = db();
    let admission = db.open_spoke_journal("host-a", "journal", 4, 1).unwrap();
    assert_eq!(admission.generation, 1);
    assert_eq!(admission.received, 0);
    for sequence in 1..=4 {
        assert_eq!(
            db.commit_spoke_record(commit("host-a", &admission, sequence, b"frame", &[], None))
                .unwrap(),
            sequence
        );
    }
}

#[test]
fn frames_are_contiguous_immutable_and_owned() {
    let db = spoke_db();
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = key(&owned);
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    assert_eq!(
        db.commit_spoke_record(commit(
            "host-a",
            &admission,
            1,
            b"one",
            std::slice::from_ref(&run),
            None
        ))
        .unwrap(),
        1
    );
    assert_eq!(
        db.commit_spoke_record(commit(
            "host-a",
            &admission,
            1,
            b"one",
            std::slice::from_ref(&run),
            None
        ))
        .unwrap(),
        1
    );
    assert_eq!(
        fault_of(
            &db.commit_spoke_record(commit(
                "host-a",
                &admission,
                1,
                b"other",
                std::slice::from_ref(&run),
                None
            ))
            .unwrap_err()
        ),
        Some(HostOutboxFault::Denied)
    );
    assert_eq!(
        fault_of(
            &db.commit_spoke_record(commit(
                "host-a",
                &admission,
                3,
                b"three",
                std::slice::from_ref(&run),
                None
            ))
            .unwrap_err()
        ),
        Some(HostOutboxFault::ReplayGap)
    );
    let mut foreign = run.clone();
    foreign.spawn_token = "forged".into();
    assert_eq!(
        fault_of(
            &db.commit_spoke_record(commit(
                "host-a",
                &admission,
                2,
                b"two",
                std::slice::from_ref(&foreign),
                None
            ))
            .unwrap_err()
        ),
        Some(HostOutboxFault::Denied)
    );
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
    assert_eq!(
        db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(),
        Some(&b"one"[..])
    );
}

#[test]
fn a_fenced_generation_cannot_commit() {
    let db = db();
    let first = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    let second = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    assert_eq!(
        fault_of(
            &db.commit_spoke_record(commit("host-a", &first, 1, b"one", &[], None))
                .unwrap_err()
        ),
        Some(HostOutboxFault::Denied)
    );
    assert_eq!(
        db.commit_spoke_record(commit("host-a", &second, 1, b"one", &[], None))
            .unwrap(),
        1
    );
}

#[test]
fn a_result_survives_reopen_after_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("work.db");
    let run;
    {
        let db = WorkDb::open(path.clone()).unwrap();
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        run = key(&owned);
        db.append_host_command("host-a", "c1", Some(&run), RunAdmission::Start, b"req", &|seq| {
            seq.to_be_bytes().to_vec()
        })
        .unwrap();
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        let result = SpokeIngressResult {
            command_id: "c1",
            run: Some(&run),
            bytes: b"ok",
        };
        db.commit_spoke_record(commit(
            "host-a",
            &admission,
            1,
            b"frame",
            std::slice::from_ref(&run),
            Some(result),
        ))
        .unwrap();
    }
    let db = WorkDb::open(path).unwrap();
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
    assert_eq!(
        db.host_command_result("host-a", "c1").unwrap().as_deref(),
        Some(&b"ok"[..])
    );
    assert_eq!(
        db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(),
        Some(&b"frame"[..])
    );
}

#[test]
fn a_new_result_cannot_be_receipted_without_storage() {
    let db = db();
    db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = key(&owned);
    db.append_host_command("host-a", "c1", Some(&run), RunAdmission::Start, b"req", &|seq| {
        seq.to_be_bytes().to_vec()
    })
    .unwrap();
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    let mut record = commit(
        "host-a",
        &admission,
        1,
        b"frame",
        &[],
        Some(SpokeIngressResult {
            command_id: "c1",
            run: Some(&run),
            bytes: b"ok",
        }),
    );
    record.retain = false;
    assert_eq!(
        fault_of(&db.commit_spoke_record(record).unwrap_err()),
        Some(HostOutboxFault::Denied)
    );
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 0);
    assert!(db.host_command_result("host-a", "c1").unwrap().is_none());
    assert!(db.spoke_ingress_frame("host-a", 1).unwrap().is_none());
}

#[test]
fn quota_counts_encoded_frame_bytes() {
    let db = db();
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    let huge = vec![0u8; MAX_HOST_INGRESS_BYTES as usize + 1];
    assert_eq!(
        fault_of(
            &db.commit_spoke_record(commit("host-a", &admission, 1, &huge, &[], None))
                .unwrap_err()
        ),
        Some(HostOutboxFault::Full)
    );
    assert!(db.spoke_ingress_frame("host-a", 1).unwrap().is_none());
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 0);
}

#[test]
fn quota_is_o1_exact_at_the_boundary_and_ignores_duplicates() {
    let db = db();
    let quota = IngressQuota { records: 2, bytes: 10 };
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    let put = |sequence: u64, frame: &[u8]| {
        db.commit_spoke_record_within(commit("host-a", &admission, sequence, frame, &[], None), quota)
    };
    assert_eq!(put(1, b"aaaa").unwrap(), 1);
    // An identical duplicate stores nothing and consumes no quota.
    assert_eq!(put(1, b"aaaa").unwrap(), 1);
    assert_eq!(put(1, b"aaaa").unwrap(), 1);
    assert_eq!(put(2, b"bbbbbb").unwrap(), 2); // exactly 10 bytes
    assert_eq!(fault_of(&put(3, b"c").unwrap_err()), Some(HostOutboxFault::Full));
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 2);
}

#[test]
fn a_host_at_its_quota_recovers_after_pruning() {
    let db = db();
    let quota = IngressQuota { records: 2, bytes: 100 };
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    let put = |sequence: u64| {
        db.commit_spoke_record_within(commit("host-a", &admission, sequence, b"frame", &[], None), quota)
    };
    put(1).unwrap();
    put(2).unwrap();
    assert_eq!(fault_of(&put(3).unwrap_err()), Some(HostOutboxFault::Full));
    assert_eq!(db.prune_spoke_ingress("host-a", 1).unwrap(), 1);
    // Pruning past the receipt clamps to it; repeating is a no-op.
    assert_eq!(db.prune_spoke_ingress("host-a", 1).unwrap(), 0);
    assert_eq!(put(3).unwrap(), 3);
    assert_eq!(fault_of(&put(4).unwrap_err()), Some(HostOutboxFault::Full));
    assert_eq!(db.prune_spoke_ingress("host-a", 99).unwrap(), 2);
    assert_eq!(put(4).unwrap(), 4);
    // A retransmission of a pruned frame is acknowledged without storing it.
    assert_eq!(put(1).unwrap(), 4);
    assert!(db.spoke_ingress_frame("host-a", 1).unwrap().is_none());
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 4);
    // A reconnect after pruning still resumes at the receipt.
    let again = db.open_spoke_journal("host-a", "journal", 4, 1).unwrap();
    assert_eq!(again.received, 4);
}

#[test]
fn a_result_at_a_pruned_sequence_is_not_accepted_unless_already_settled() {
    let db = db();
    db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = key(&owned);
    db.append_host_command("host-a", "c1", Some(&run), RunAdmission::Start, b"req", &|seq| {
        seq.to_be_bytes().to_vec()
    })
    .unwrap();
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    db.commit_spoke_record(commit("host-a", &admission, 1, b"capacity", &[], None))
        .unwrap();
    db.prune_spoke_ingress("host-a", 1).unwrap();
    let result = || SpokeIngressResult {
        command_id: "c1",
        run: Some(&run),
        bytes: b"ok",
    };
    let replay = |result| {
        db.commit_spoke_record(commit(
            "host-a",
            &admission,
            1,
            b"fabricated",
            std::slice::from_ref(&run),
            Some(result),
        ))
    };
    assert_eq!(fault_of(&replay(result()).unwrap_err()), Some(HostOutboxFault::Denied));
    assert!(db.host_command_result("host-a", "c1").unwrap().is_none());
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
    // Once the result is genuinely durable, a retransmission is acknowledged.
    db.commit_spoke_record(commit(
        "host-a",
        &admission,
        2,
        b"real",
        std::slice::from_ref(&run),
        Some(result()),
    ))
    .unwrap();
    db.prune_spoke_ingress("host-a", 2).unwrap();
    assert_eq!(replay(result()).unwrap(), 2);
    let other = SpokeIngressResult {
        command_id: "c1",
        run: Some(&run),
        bytes: b"different",
    };
    assert_eq!(fault_of(&replay(other).unwrap_err()), Some(HostOutboxFault::Denied));
    let mut forged = run.clone();
    forged.spawn_token = "forged".into();
    let wrong_run = SpokeIngressResult {
        command_id: "c1",
        run: Some(&forged),
        bytes: b"ok",
    };
    assert_eq!(fault_of(&replay(wrong_run).unwrap_err()), Some(HostOutboxFault::Denied));
    assert_eq!(
        db.host_command_result("host-a", "c1").unwrap().as_deref(),
        Some(&b"ok"[..])
    );
}

#[test]
fn ephemeral_frames_do_not_occupy_quota_and_replay_as_receipts() {
    let db = db();
    db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = key(&owned);
    let quota = IngressQuota { records: 1, bytes: 8 };
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    let put = |sequence: u64, frame: &[u8], retain: bool| {
        db.commit_spoke_record_within(
            SpokeIngressCommit::builder()
                .host_id("host-a")
                .generation(admission.generation)
                .journal_id(&admission.journal_id)
                .sequence(sequence)
                .frame(frame)
                .epoch(1)
                .features("execution=1")
                .observed_at_ms(1)
                .runs(std::slice::from_ref(&run))
                .retain(retain)
                .build(),
            quota,
        )
    };
    assert_eq!(put(1, b"capture-bytes-too-big-for-quota", false).unwrap(), 1);
    assert_eq!(db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(), Some(&b""[..]));
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
    // A retransmission, even with different bytes, is already receipted.
    assert_eq!(put(1, b"changed", false).unwrap(), 1);
    // A retained frame still fits: the capture did not consume quota.
    assert_eq!(put(2, b"kept", true).unwrap(), 2);
    assert_eq!(
        db.spoke_ingress_frame("host-a", 2).unwrap().as_deref(),
        Some(&b"kept"[..])
    );
    assert_eq!(
        fault_of(&put(3, b"full", true).unwrap_err()),
        Some(HostOutboxFault::Full)
    );
    // Another capture still receipts without filling the quota.
    assert_eq!(put(3, b"another-capture", false).unwrap(), 3);
    assert_eq!(db.spoke_ingress_frame("host-a", 3).unwrap().as_deref(), Some(&b""[..]));
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 3);
}

#[test]
fn a_retained_replay_at_an_unstored_sequence_is_denied() {
    let db = db();
    db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = key(&owned);
    let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
    let put = |sequence: u64, frame: &[u8], retain: bool| {
        db.commit_spoke_record(
            SpokeIngressCommit::builder()
                .host_id("host-a")
                .generation(admission.generation)
                .journal_id(&admission.journal_id)
                .sequence(sequence)
                .frame(frame)
                .epoch(1)
                .features("execution=1")
                .observed_at_ms(1)
                .runs(std::slice::from_ref(&run))
                .retain(retain)
                .build(),
        )
    };
    assert_eq!(put(1, b"capture", false).unwrap(), 1);
    assert_eq!(db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(), Some(&b""[..]));
    assert_eq!(
        fault_of(&put(1, b"lifecycle", true).unwrap_err()),
        Some(HostOutboxFault::Denied)
    );
    assert_eq!(put(1, b"changed-capture", false).unwrap(), 1);
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
}
