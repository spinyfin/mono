//! Short-lived pane capture subscriptions for the hub's read-only viewer
//! (design §2.8). The spoke holds only what a viewer is looking at right now:
//!
//! * A `Capture` command opens or renews a subscription, named by its
//!   `subscription_id`. It lives `ttl_seconds` past the last renewal (default
//!   30, at most 120) and is cancelled at once by a TTL of one second or less.
//!   The hub renews while a viewer is visible and stops renewing when it
//!   leaves. Disconnect clears all subscriptions immediately.
//! * Each due subscription appends a `PaneCapture` record carrying a bounded
//!   text snapshot (at most the requested bytes, never more than 64 KiB, the
//!   newest lines first). Captures are admitted below every other record
//!   class, so pressure on the spool stops them first.
//! * Subscriptions are memory only. A restarted spoke has none, which is right:
//!   the hub's next renewal reopens whatever is still being watched.
//!
//! Nothing here can write to a pane or reach anything but the text of a worker
//! whose token-named session this spoke supervises.
use std::collections::HashMap;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::Result;
use boss_host_execution::capture::{PaneRead, PaneSource, bound_snapshot};
use boss_host_execution::worker::{ExecutionRecord, Phase};
use boss_protocol_spoke::limits::{
    CANCEL_CAPTURE_TTL_SECONDS, DEFAULT_CAPTURE_INTERVAL_MS, DEFAULT_CAPTURE_TTL_SECONDS, MAX_CAPTURE_BYTES,
    MAX_CAPTURE_INTERVAL_MS, MAX_CAPTURE_SUBSCRIPTIONS, MAX_CAPTURE_TTL_SECONDS, MIN_CAPTURE_INTERVAL_MS,
};
use boss_protocol_spoke::v1::{Capture, Command, CommandResult, PaneCapture, Record, ResultCode, Run, Stamp, record};
use boss_spoke_journal::{Admission, Fault};
use boss_spoke_outbound::SharedJournal;

/// How often the loop looks for due subscriptions; finer than the fastest cadence.
const TICK: Duration = Duration::from_millis(250);

struct Subscription {
    generation: u64,
    run: Run,
    max_bytes: usize,
    schedule: Schedule,
}

struct Schedule {
    interval: Duration,
    expires: Instant,
    next_due: Instant,
}

/// What a `Capture` command did to the subscription table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Opened,
    Renewed,
    /// Also the answer for a subscription that was already gone.
    Cancelled,
    Refused(&'static str),
}

/// A subscription that is due now.
#[derive(Debug, Clone, PartialEq)]
pub struct Due {
    generation: u64,
    pub subscription_id: String,
    pub run: Run,
    pub max_bytes: usize,
}

#[derive(Default)]
pub struct Captures {
    session: Mutex<Option<u64>>,
    generation: AtomicU64,
    subscriptions: Mutex<HashMap<String, Subscription>>,
}

impl boss_spoke_outbound::SessionObserver for Captures {
    fn connected(&self, pane_capture: bool, received_commands: u64) {
        let mut session = self.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.table().clear();
        *session = pane_capture.then_some(received_commands);
    }

    fn disconnected(&self) {
        let mut session = self.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *session = None;
        self.table().clear();
    }
}

impl Captures {
    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<String, Subscription>> {
        self.subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn len(&self) -> usize {
        self.table().len()
    }

    pub fn is_empty(&self) -> bool {
        self.table().is_empty()
    }

    /// Open, renew or cancel `capture.subscription_id` for `run`. A new
    /// subscription is due immediately so a viewer's first look is not a full
    /// interval away.
    pub fn apply(&self, run: &Run, capture: &Capture, now: Instant) -> Applied {
        let mut table = self.table();
        if let Some(existing) = table.get(&capture.subscription_id)
            && existing.run != *run
        {
            return Applied::Refused("subscription belongs to another run");
        }
        let ttl = match capture.ttl_seconds {
            0 => DEFAULT_CAPTURE_TTL_SECONDS,
            ttl => ttl.min(MAX_CAPTURE_TTL_SECONDS),
        };
        if ttl <= CANCEL_CAPTURE_TTL_SECONDS {
            table.remove(&capture.subscription_id);
            return Applied::Cancelled;
        }
        let interval = match capture.interval_ms {
            0 => DEFAULT_CAPTURE_INTERVAL_MS,
            ms => ms.clamp(MIN_CAPTURE_INTERVAL_MS, MAX_CAPTURE_INTERVAL_MS),
        };
        let max_bytes = match capture.max_bytes as usize {
            0 => MAX_CAPTURE_BYTES,
            bytes => bytes.min(MAX_CAPTURE_BYTES),
        };
        let expires = now + Duration::from_secs(u64::from(ttl));
        if let Some(existing) = table.get_mut(&capture.subscription_id) {
            existing.schedule.interval = Duration::from_millis(u64::from(interval));
            existing.max_bytes = max_bytes;
            existing.schedule.expires = expires;
            return Applied::Renewed;
        }
        table.retain(|_, subscription| subscription.schedule.expires > now);
        if table.len() >= MAX_CAPTURE_SUBSCRIPTIONS {
            return Applied::Refused("too many capture subscriptions");
        }
        table.insert(
            capture.subscription_id.clone(),
            Subscription {
                generation: self.generation.fetch_add(1, Ordering::Relaxed),
                run: run.clone(),
                max_bytes,
                schedule: Schedule {
                    interval: Duration::from_millis(u64::from(interval)),
                    expires,
                    next_due: now,
                },
            },
        );
        Applied::Opened
    }

    /// Drop expired subscriptions and return, in id order, those due at `now`,
    /// scheduling each one's next look a full interval later.
    pub fn due(&self, now: Instant) -> Vec<Due> {
        let mut table = self.table();
        table.retain(|_, subscription| subscription.schedule.expires > now);
        let mut due: Vec<Due> = table
            .iter_mut()
            .filter(|(_, subscription)| subscription.schedule.next_due <= now)
            .map(|(id, subscription)| {
                subscription.schedule.next_due = now + subscription.schedule.interval;
                Due {
                    generation: subscription.generation,
                    subscription_id: id.clone(),
                    run: subscription.run.clone(),
                    max_bytes: subscription.max_bytes,
                }
            })
            .collect();
        due.sort_by(|a, b| a.subscription_id.cmp(&b.subscription_id));
        due
    }

    fn current(table: &HashMap<String, Subscription>, due: &Due, now: Instant) -> bool {
        table
            .get(&due.subscription_id)
            .is_some_and(|s| s.generation == due.generation && s.schedule.expires > now)
    }

    fn remove_current(&self, due: &Due) {
        let mut table = self.table();
        if table
            .get(&due.subscription_id)
            .is_some_and(|s| s.generation == due.generation)
        {
            table.remove(&due.subscription_id);
        }
    }

    pub fn remove(&self, subscription_id: &str) {
        self.table().remove(subscription_id);
    }

    /// Run a `Capture` command. The worker must be one this spoke supervises
    /// and has not finished; a command past its deadline is refused rather than
    /// reopening a subscription nobody is waiting for.
    pub fn command(
        &self,
        journal: &SharedJournal,
        command: &Command,
        capture: &Capture,
        now_ms: i64,
        now: Instant,
    ) -> CommandResult {
        let reply = |code: ResultCode, reason: &str| CommandResult {
            command_id: command.command_id.clone(),
            code: code.into(),
            reason: reason.to_owned(),
            value: None,
        };
        let Some(run) = command.run.as_ref() else {
            return reply(ResultCode::Rejected, "run identity required");
        };
        if command.not_after_unix_ms <= now_ms {
            return reply(ResultCode::Rejected, "command expired");
        }
        // Hold this guard through apply so disconnect cannot race an open.
        let session = self.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !session.is_some_and(|through| command.sequence > through) {
            return reply(ResultCode::Rejected, "capture connection ended or not negotiated");
        }
        let cancelling = capture.ttl_seconds != 0 && capture.ttl_seconds <= CANCEL_CAPTURE_TTL_SECONDS;
        if !cancelling {
            match live_record(journal, run) {
                Ok(Some(_)) => {}
                Ok(None) => return reply(ResultCode::Absent, "no live worker for this run"),
                Err(error) => return reply(ResultCode::OutcomeUncertain, &format!("{error:#}")),
            }
        }
        match self.apply(run, capture, now) {
            Applied::Opened | Applied::Renewed | Applied::Cancelled => reply(ResultCode::Ok, ""),
            Applied::Refused(reason) => reply(ResultCode::Rejected, reason),
        }
    }
}

/// The execution record for `run`, when it is this run and not yet finished.
fn live_record(journal: &SharedJournal, run: &Run) -> Result<Option<ExecutionRecord>> {
    let bytes = journal
        .lock()
        .map_err(|_| anyhow::Error::from(Fault::LockPoisoned))?
        .load_execution(&run.run_id, &run.spawn_token)?;
    let Some(bytes) = bytes else { return Ok(None) };
    let record: ExecutionRecord = serde_json::from_slice(&bytes)?;
    Ok((record.execution_id == run.execution_id
        && record.assignment_generation == run.assignment_generation
        && record.phase != Phase::Terminal)
        .then_some(record))
}

/// Look at every due subscription once. Separate from [`serve`] so a test can
/// drive time.
pub async fn tick(
    journal: &SharedJournal,
    captures: &Captures,
    source: &impl PaneSource,
    stamp: &impl Fn() -> Result<Stamp>,
    now: Instant,
) -> Result<()> {
    for due in captures.due(now) {
        if !Captures::current(&captures.table(), &due, now.max(Instant::now())) {
            continue;
        }
        let record = match live_record(journal, &due.run) {
            Ok(Some(record)) => record,
            Ok(None) => {
                captures.remove_current(&due);
                continue;
            }
            Err(error) => {
                tracing::warn!(run = due.run.run_id, %error, "capture skipped: execution record unreadable");
                continue;
            }
        };
        let text = match source.read(&record.session, &record.spawn_token).await {
            PaneRead::Text(text) => text,
            PaneRead::Gone => {
                captures.remove_current(&due);
                continue;
            }
            PaneRead::Unreadable(reason) => {
                tracing::debug!(run = due.run.run_id, reason, "capture skipped: pane unreadable");
                continue;
            }
        };
        let (text, truncated) = bound_snapshot(&text, due.max_bytes);
        let capture = Record {
            observed: Some(stamp()?),
            run: Some(due.run.clone()),
            body: Some(record::Body::Capture(PaneCapture {
                subscription_id: due.subscription_id.clone(),
                text_utf8: text.into_bytes(),
                truncated,
            })),
            ..Default::default()
        };
        let table = captures.table();
        if !Captures::current(&table, &due, now.max(Instant::now())) {
            continue;
        }
        let appended = journal
            .lock()
            .map_err(|_| anyhow::Error::from(Fault::LockPoisoned))?
            .append(capture, Admission::Capture);
        drop(table);
        match appended {
            Ok(_) => {}
            // Captures are the first thing the spool sheds; the next look retries.
            Err(error) if Fault::is_full(&error) => {
                tracing::debug!(run = due.run.run_id, "capture dropped: spool pressure");
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The capture loop. Runs beside the command loop; it only ever appends
/// `PaneCapture` records for subscriptions a command opened.
pub async fn serve(
    journal: SharedJournal,
    captures: std::sync::Arc<Captures>,
    source: impl PaneSource,
    stamp: impl Fn() -> Result<Stamp>,
) -> Result<()> {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if captures.is_empty() {
            continue;
        }
        tick(&journal, &captures, &source, &stamp, Instant::now()).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_spoke_journal::{Journal, Limits};
    use prost::Message;
    use std::sync::Arc;

    fn run() -> Run {
        Run {
            execution_id: "execution".into(),
            run_id: "run".into(),
            spawn_token: "token".into(),
            assignment_generation: 1,
        }
    }
    fn subscribe(id: &str, ttl: u32, interval_ms: u32) -> Capture {
        Capture {
            subscription_id: id.into(),
            interval_ms,
            max_bytes: 0,
            ttl_seconds: ttl,
        }
    }
    fn stamp() -> Result<Stamp> {
        Ok(Stamp {
            boot_id: "boot".into(),
            wall_unix_ms: 1,
            awake_ns: 1,
            elapsed_ns: 1,
        })
    }
    fn journal(root: &std::path::Path) -> SharedJournal {
        let limits = Limits {
            spool_bytes: 4 * 1024 * 1024,
            lifecycle_reserve: 64 * 1024,
        };
        Arc::new(Mutex::new(Journal::open(&root.join("state"), "host", limits).unwrap()))
    }
    fn save(journal: &SharedJournal, phase: Phase) {
        let record = ExecutionRecord::builder()
            .execution_id("execution")
            .run_id("run")
            .spawn_token("token")
            .assignment_generation(1)
            .lease_id("lease")
            .workspace_id("workspace")
            .session("session")
            .phase(phase)
            .boot_id("boot")
            .build();
        journal
            .lock()
            .unwrap()
            .save_execution("run", "token", &serde_json::to_vec(&record).unwrap())
            .unwrap();
    }
    fn captured(journal: &SharedJournal) -> Vec<PaneCapture> {
        let journal = journal.lock().unwrap();
        let (mut after, mut found) = (0, Vec::new());
        while let Some(stored) = journal.next_record(after).unwrap() {
            after = stored.sequence;
            if let Some(record::Body::Capture(capture)) = Record::decode(stored.bytes.as_slice()).unwrap().body {
                found.push(capture);
            }
        }
        found
    }

    /// Replies from a script, then `Gone`.
    struct Script(Mutex<Vec<PaneRead>>);
    impl PaneSource for Script {
        async fn read(&self, session: &str, token: &str) -> PaneRead {
            assert_eq!((session, token), ("session", "token"));
            let mut script = self.0.lock().unwrap();
            if script.is_empty() {
                PaneRead::Gone
            } else {
                script.remove(0)
            }
        }
    }

    #[test]
    fn open_renew_and_cancel() {
        let captures = Captures::default();
        let now = Instant::now();
        assert_eq!(captures.apply(&run(), &subscribe("s", 0, 0), now), Applied::Opened);
        assert_eq!(captures.apply(&run(), &subscribe("s", 0, 0), now), Applied::Renewed);
        assert_eq!(captures.len(), 1);
        assert_eq!(captures.apply(&run(), &subscribe("s", 1, 0), now), Applied::Cancelled);
        assert!(captures.is_empty());
        // Cancelling what is not there is the same answer.
        assert_eq!(captures.apply(&run(), &subscribe("s", 1, 0), now), Applied::Cancelled);
    }

    #[test]
    fn a_subscription_belongs_to_its_run() {
        let captures = Captures::default();
        let now = Instant::now();
        captures.apply(&run(), &subscribe("s", 30, 0), now);
        let other = Run {
            run_id: "other".into(),
            ..run()
        };
        for ttl in [30, 1] {
            assert_eq!(
                captures.apply(&other, &subscribe("s", ttl, 0), now),
                Applied::Refused("subscription belongs to another run")
            );
        }
        assert_eq!(captures.len(), 1);
    }

    #[test]
    fn requests_are_clamped_and_expire_without_renewal() {
        let captures = Captures::default();
        let start = Instant::now();
        // A 10 ms cadence is raised to the floor; a 1 hour lifetime is lowered.
        captures.apply(&run(), &subscribe("s", 3600, 10), start);
        assert_eq!(captures.due(start).len(), 1, "first look is immediate");
        assert!(captures.due(start + Duration::from_millis(999)).is_empty());
        assert_eq!(captures.due(start + Duration::from_millis(1000)).len(), 1);
        let end = start + Duration::from_secs(u64::from(MAX_CAPTURE_TTL_SECONDS));
        assert!(
            captures.due(end).is_empty(),
            "past the capped TTL the subscription is gone"
        );
        assert!(captures.is_empty());
    }

    #[test]
    fn a_renewal_keeps_it_alive_and_the_table_is_bounded() {
        let captures = Captures::default();
        let start = Instant::now();
        captures.apply(&run(), &subscribe("s", 30, 5000), start);
        captures.apply(&run(), &subscribe("s", 30, 5000), start + Duration::from_secs(20));
        assert_eq!(captures.due(start + Duration::from_secs(40)).len(), 1);
        for index in 0..MAX_CAPTURE_SUBSCRIPTIONS {
            captures.apply(&run(), &subscribe(&format!("n{index}"), 30, 0), start);
        }
        assert_eq!(
            captures.apply(&run(), &subscribe("one-too-many", 30, 0), start),
            Applied::Refused("too many capture subscriptions")
        );
    }

    #[test]
    fn commands_need_a_live_unexpired_worker() {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path());
        let captures = Captures::default();
        boss_spoke_outbound::SessionObserver::connected(&captures, true, 0);
        let mut command = Command {
            sequence: 1,
            command_id: "c1".into(),
            run: Some(run()),
            not_after_unix_ms: 100,
            ..Default::default()
        };
        let ask = |captures: &Captures, command: &Command, ttl, now_ms| {
            captures.command(&journal, command, &subscribe("s", ttl, 0), now_ms, Instant::now())
        };
        assert_eq!(ask(&captures, &command, 30, 1).code, ResultCode::Absent as i32);
        save(&journal, Phase::Alive);
        assert_eq!(ask(&captures, &command, 30, 1).code, ResultCode::Ok as i32);
        assert_eq!(captures.len(), 1);
        assert_eq!(ask(&captures, &command, 30, 100).reason, "command expired");
        command.run.as_mut().unwrap().assignment_generation = 2;
        assert_eq!(ask(&captures, &command, 30, 1).code, ResultCode::Absent as i32);
        command.run.as_mut().unwrap().assignment_generation = 1;
        // A cancel needs no live worker: leaving must always work.
        save(&journal, Phase::Terminal);
        assert_eq!(ask(&captures, &command, 30, 1).code, ResultCode::Absent as i32);
        assert_eq!(ask(&captures, &command, 1, 1).code, ResultCode::Ok as i32);
        assert!(captures.is_empty());
    }

    #[tokio::test]
    async fn due_subscriptions_append_bounded_capture_records() {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path());
        save(&journal, Phase::Alive);
        let captures = Captures::default();
        let start = Instant::now();
        let mut request = subscribe("s", 30, 1000);
        request.max_bytes = 16;
        captures.apply(&run(), &request, start);
        let source = Script(Mutex::new(vec![
            PaneRead::Text("fresh\n\n\n".into()),
            PaneRead::Unreadable("tmux hiccup".into()),
            PaneRead::Text("older line\nnewer line that is long\nnewest\n".into()),
        ]));
        tick(&journal, &captures, &source, &stamp, start).await.unwrap();
        // Not due again yet: nothing is read, nothing appended.
        tick(&journal, &captures, &source, &stamp, start + Duration::from_millis(500))
            .await
            .unwrap();
        // An unreadable pane appends nothing and keeps the subscription.
        tick(&journal, &captures, &source, &stamp, start + Duration::from_secs(1))
            .await
            .unwrap();
        tick(&journal, &captures, &source, &stamp, start + Duration::from_secs(2))
            .await
            .unwrap();
        let found = captured(&journal);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].text_utf8, b"fresh");
        assert!(!found[0].truncated);
        assert_eq!(found[1].subscription_id, "s");
        assert!(found[1].truncated && found[1].text_utf8.len() <= 16);
        assert_eq!(String::from_utf8(found[1].text_utf8.clone()).unwrap(), "newest");
        assert_eq!(captures.len(), 1);
    }

    #[tokio::test]
    async fn a_vanished_pane_or_finished_worker_ends_the_subscription() {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path());
        save(&journal, Phase::Alive);
        let captures = Captures::default();
        let start = Instant::now();
        captures.apply(&run(), &subscribe("gone", 30, 1000), start);
        tick(&journal, &captures, &Script(Mutex::new(vec![])), &stamp, start)
            .await
            .unwrap();
        assert!(captures.is_empty(), "a session that is positively gone ends it");
        captures.apply(&run(), &subscribe("done", 30, 1000), start);
        save(&journal, Phase::Terminal);
        tick(&journal, &captures, &Script(Mutex::new(vec![])), &stamp, start)
            .await
            .unwrap();
        assert!(captures.is_empty());
        assert!(captured(&journal).is_empty());
    }

    #[tokio::test]
    async fn disconnect_and_capability_loss_stop_captures_and_fence_old_commands() {
        use boss_spoke_outbound::SessionObserver;
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path());
        save(&journal, Phase::Alive);
        let captures = Captures::default();
        let command = Command {
            sequence: 1,
            command_id: "c".into(),
            run: Some(run()),
            not_after_unix_ms: 100,
            ..Default::default()
        };
        let now = Instant::now();
        captures.connected(true, 0);
        assert_eq!(
            captures
                .command(&journal, &command, &subscribe("s", 120, 1000), 1, now)
                .code,
            ResultCode::Ok as i32
        );
        captures.disconnected();
        tick(
            &journal,
            &captures,
            &Script(Mutex::new(vec![PaneRead::Text("stale".into())])),
            &stamp,
            now,
        )
        .await
        .unwrap();
        assert!(captured(&journal).is_empty());
        captures.connected(true, 1);
        assert_eq!(
            captures
                .command(&journal, &command, &subscribe("s", 120, 1000), 1, now)
                .code,
            ResultCode::Rejected as i32
        );
        captures.connected(false, 1);
        let command = Command { sequence: 2, ..command };
        assert_eq!(
            captures
                .command(&journal, &command, &subscribe("s", 120, 1000), 1, now)
                .code,
            ResultCode::Rejected as i32
        );
        tick(
            &journal,
            &captures,
            &Script(Mutex::new(vec![])),
            &stamp,
            now + Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(captured(&journal).is_empty());
    }

    #[tokio::test]
    async fn cancellation_or_replacement_during_read_discards_the_snapshot() {
        struct Blocking {
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }
        impl PaneSource for Blocking {
            async fn read(&self, _: &str, _: &str) -> PaneRead {
                self.entered.notify_one();
                self.release.notified().await;
                PaneRead::Text("stale".into())
            }
        }
        for action in ["cancel", "replace", "expire", "disconnect"] {
            let root = tempfile::tempdir().unwrap();
            let journal = journal(root.path());
            save(&journal, Phase::Alive);
            let captures = Captures::default();
            let now = Instant::now();
            captures.apply(&run(), &subscribe("a", 30, 1000), now);
            captures.apply(&run(), &subscribe("b", 30, 1000), now);
            let source = Blocking {
                entered: Default::default(),
                release: Default::default(),
            };
            let read = tick(&journal, &captures, &source, &stamp, now);
            let cancel = async {
                source.entered.notified().await;
                match action {
                    "expire" => {
                        for subscription in captures.table().values_mut() {
                            subscription.schedule.expires = Instant::now();
                        }
                    }
                    "disconnect" => boss_spoke_outbound::SessionObserver::disconnected(&captures),
                    _ => {
                        captures.remove("a");
                        captures.remove("b");
                        if action == "replace" {
                            captures.apply(&run(), &subscribe("a", 30, 1000), now);
                        }
                    }
                }
                source.release.notify_one();
            };
            let (result, ()) = tokio::join!(read, cancel);
            result.unwrap();
            assert!(captured(&journal).is_empty());
        }
    }

    #[tokio::test]
    async fn spool_pressure_sheds_captures_without_failing_the_loop() {
        let root = tempfile::tempdir().unwrap();
        let limits = Limits {
            spool_bytes: 128 * 1024,
            lifecycle_reserve: 64 * 1024,
        };
        let journal: SharedJournal = Arc::new(Mutex::new(
            Journal::open(&root.path().join("state"), "host", limits).unwrap(),
        ));
        save(&journal, Phase::Alive);
        let captures = Captures::default();
        let start = Instant::now();
        captures.apply(&run(), &subscribe("s", 120, 1000), start);
        let big = "x".repeat(MAX_CAPTURE_BYTES);
        let source = Script(Mutex::new((0..8).map(|_| PaneRead::Text(big.clone())).collect()));
        for second in 0..8 {
            tick(
                &journal,
                &captures,
                &source,
                &stamp,
                start + Duration::from_secs(second),
            )
            .await
            .unwrap();
        }
        // 75% of the 64 KiB ordinary budget admits no 64 KiB snapshot at all.
        assert!(captured(&journal).len() < 8);
        assert_eq!(captures.len(), 1, "pressure drops snapshots, not the subscription");
    }
}
