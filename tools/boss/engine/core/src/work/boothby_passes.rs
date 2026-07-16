//! `boothby_passes` CRUD — the lifecycle half of Boothby's schema (the
//! journal-write half lives in `boothby.rs`). One row per wake-up; see
//! `boss_protocol::boothby` for the shape and `boothby.md` §"Pass lifecycle".
//!
//! At most one pass may be open (`finished_at IS NULL`) at a time — enforced
//! by the `boothby_passes_single_open_idx` partial unique index from
//! `migrations_boothby.rs`. [`WorkDb::open_boothby_pass`] respects that
//! invariant by checking first rather than racing the constraint: a caller
//! that gets `Ok(None)` back knows a pass is already in flight, distinct from
//! a real error.

use super::*;

const BOOTHBY_PASS_SELECT: &str = "
    SELECT id, trigger, started_at, finished_at, outcome,
           actions_count, proposals_count, findings_count,
           summary, session_id, transcript_path
    FROM boothby_passes";

fn map_boothby_pass(row: &Row) -> rusqlite::Result<BoothbyPass> {
    Ok(BoothbyPass::builder()
        .id(row.get::<_, String>(0)?)
        .trigger(row.get::<_, String>(1)?)
        .started_at(row.get::<_, String>(2)?)
        .maybe_finished_at(row.get::<_, Option<String>>(3)?)
        .maybe_outcome(row.get::<_, Option<String>>(4)?)
        .actions_count(row.get::<_, i64>(5)?)
        .proposals_count(row.get::<_, i64>(6)?)
        .findings_count(row.get::<_, i64>(7)?)
        .maybe_summary(row.get::<_, Option<String>>(8)?)
        .maybe_session_id(row.get::<_, Option<String>>(9)?)
        .maybe_transcript_path(row.get::<_, Option<String>>(10)?)
        .build())
}

fn query_boothby_pass(conn: &Connection, id: &str) -> Result<Option<BoothbyPass>> {
    let sql = format!("{BOOTHBY_PASS_SELECT} WHERE id = ?1");
    conn.query_row(&sql, [id], map_boothby_pass)
        .optional()
        .map_err(Into::into)
}

/// Every trigger value the design allows: `'schedule'`, `'manual'`, or the
/// open-ended `'event:<name>'`. The `event:` case has no fixed suffix set
/// (see `migrations_boothby.rs`'s deliberate absence of a `CHECK` on this
/// column), so this only validates the two closed literals plus the prefix.
fn validate_boothby_trigger(trigger: &str) -> Result<()> {
    if trigger == BOOTHBY_TRIGGER_SCHEDULE
        || trigger == BOOTHBY_TRIGGER_MANUAL
        || trigger.starts_with(BOOTHBY_TRIGGER_EVENT_PREFIX)
    {
        return Ok(());
    }
    bail!("invalid boothby pass trigger: {trigger:?}");
}

fn validate_boothby_outcome(outcome: &str) -> Result<()> {
    const VALID: &[&str] = &[
        BOOTHBY_OUTCOME_COMPLETED,
        BOOTHBY_OUTCOME_NOTHING_TO_DO,
        BOOTHBY_OUTCOME_TIMED_OUT,
        BOOTHBY_OUTCOME_FAILED,
        BOOTHBY_OUTCOME_CAPPED,
    ];
    if VALID.contains(&outcome) {
        return Ok(());
    }
    bail!("invalid boothby pass outcome: {outcome:?}");
}

impl WorkDb {
    /// Open a new pass for `trigger` at `now` (UTC epoch seconds, as a
    /// string per the schema's TEXT-timestamp convention). Returns `Ok(None)`
    /// — not an error — when a pass is already open, so callers (the
    /// scheduler, the manual-run RPC handler) can treat "busy" as an
    /// ordinary control-flow branch rather than matching on a DB error.
    pub fn open_boothby_pass(&self, trigger: &str, now: &str) -> Result<Option<BoothbyPass>> {
        validate_boothby_trigger(trigger)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let already_open: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM boothby_passes WHERE finished_at IS NULL)",
            [],
            |row| row.get(0),
        )?;
        if already_open {
            return Ok(None);
        }

        let id = next_id("bp");
        tx.execute(
            "INSERT INTO boothby_passes (id, trigger, started_at) VALUES (?1, ?2, ?3)",
            params![id, trigger, now],
        )?;
        let pass = query_boothby_pass(&tx, &id)?.with_context(|| format!("missing boothby pass after insert: {id}"))?;
        tx.commit()?;
        Ok(Some(pass))
    }

    /// The currently in-flight pass (`finished_at IS NULL`), if any.
    /// Well-defined because `boothby_passes_single_open_idx` makes a second
    /// concurrent open pass impossible.
    pub fn get_open_boothby_pass(&self) -> Result<Option<BoothbyPass>> {
        let conn = self.connect()?;
        let sql = format!("{BOOTHBY_PASS_SELECT} WHERE finished_at IS NULL");
        conn.query_row(&sql, [], map_boothby_pass)
            .optional()
            .map_err(Into::into)
    }

    /// Close a pass with a terminal `outcome`, stamping `finished_at = now`.
    /// `summary`/`session_id`/`transcript_path` are optional — the
    /// `nothing_to_do` short-circuit (no session ever spawned) leaves all
    /// three `None`.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_boothby_pass(
        &self,
        id: &str,
        now: &str,
        outcome: &str,
        summary: Option<&str>,
        session_id: Option<&str>,
        transcript_path: Option<&str>,
    ) -> Result<BoothbyPass> {
        validate_boothby_outcome(outcome)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE boothby_passes
             SET finished_at = ?2, outcome = ?3, summary = ?4, session_id = ?5, transcript_path = ?6
             WHERE id = ?1 AND finished_at IS NULL",
            params![id, now, outcome, summary, session_id, transcript_path],
        )?;
        if updated == 0 {
            bail!("boothby pass {id} not found or already finished");
        }
        let pass = query_boothby_pass(&tx, id)?.with_context(|| format!("missing boothby pass after finish: {id}"))?;
        tx.commit()?;
        Ok(pass)
    }

    pub fn get_boothby_pass(&self, id: &str) -> Result<Option<BoothbyPass>> {
        let conn = self.connect()?;
        query_boothby_pass(&conn, id)
    }

    /// Most recent passes first, capped at `limit`.
    pub fn list_boothby_passes(&self, limit: i64) -> Result<Vec<BoothbyPass>> {
        let conn = self.connect()?;
        let sql = format!("{BOOTHBY_PASS_SELECT} ORDER BY started_at DESC LIMIT ?1");
        let mut stmt = conn.prepare(&sql)?;
        collect_rows(stmt.query_map([limit], map_boothby_pass)?)
    }

    /// The most recent *finished* pass (`finished_at IS NOT NULL`), or `None`
    /// if none has ever finished. Distinct from [`Self::list_boothby_passes`]
    /// (which is unfiltered and so returns the in-flight pass itself
    /// whenever one is open) — `GetBoothbyState`'s `last_pass` field needs
    /// the last *finished* pass so the Boothby tab can render "current pass +
    /// previous outcome" side by side.
    pub fn last_finished_boothby_pass(&self) -> Result<Option<BoothbyPass>> {
        let conn = self.connect()?;
        let sql = format!("{BOOTHBY_PASS_SELECT} WHERE finished_at IS NOT NULL ORDER BY started_at DESC LIMIT 1");
        conn.query_row(&sql, [], map_boothby_pass)
            .optional()
            .map_err(Into::into)
    }

    /// `started_at` (parsed as epoch seconds) of the most recent
    /// `trigger = 'schedule'` pass, or `None` if Boothby has never fired a
    /// scheduled pass. The scheduler anchors its cron-occurrence math on
    /// this rather than a persisted `next_due_at` column — passes are
    /// stateless-by-design (`boothby.md` §"Idempotence & convergence"), so
    /// "when is the next scheduled fire" is derived purely from pass history
    /// plus the cron expression, with no separate mutable schedule row to
    /// keep in sync.
    pub fn last_boothby_schedule_pass_started_at(&self) -> Result<Option<i64>> {
        let conn = self.connect()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT started_at FROM boothby_passes WHERE trigger = ?1 ORDER BY started_at DESC LIMIT 1",
                [BOOTHBY_TRIGGER_SCHEDULE],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| s.parse().ok()))
    }

    /// `started_at` (parsed as epoch seconds) of the most recent pass of any
    /// trigger, or `None` if Boothby has never run a pass. Used to enforce
    /// `boothby.min_pass_gap_secs` regardless of what triggered the prior
    /// pass.
    pub fn last_boothby_pass_started_at(&self) -> Result<Option<i64>> {
        let conn = self.connect()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT started_at FROM boothby_passes ORDER BY started_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| s.parse().ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_db;

    #[test]
    fn open_pass_then_get_open_round_trips() {
        let (_d, db) = open_db();
        let opened = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        assert_eq!(opened.trigger, BOOTHBY_TRIGGER_SCHEDULE);
        assert_eq!(opened.started_at, "1000");
        assert!(opened.finished_at.is_none());
        assert!(opened.outcome.is_none());

        let open = db.get_open_boothby_pass().unwrap().unwrap();
        assert_eq!(open.id, opened.id);
    }

    #[test]
    fn opening_a_second_pass_while_one_is_open_returns_none() {
        let (_d, db) = open_db();
        db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        let second = db.open_boothby_pass(BOOTHBY_TRIGGER_MANUAL, "1001").unwrap();
        assert!(second.is_none(), "a second open pass must be refused, not inserted");
    }

    #[test]
    fn finishing_a_pass_clears_get_open_and_persists_outcome() {
        let (_d, db) = open_db();
        let pass = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();

        let finished = db
            .finish_boothby_pass(
                &pass.id,
                "1050",
                BOOTHBY_OUTCOME_NOTHING_TO_DO,
                Some("no candidates"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(finished.outcome.as_deref(), Some(BOOTHBY_OUTCOME_NOTHING_TO_DO));
        assert_eq!(finished.finished_at.as_deref(), Some("1050"));
        assert_eq!(finished.summary.as_deref(), Some("no candidates"));

        assert!(db.get_open_boothby_pass().unwrap().is_none());
        // A new pass can now open.
        assert!(
            db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1100")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn finishing_an_already_finished_pass_errors() {
        let (_d, db) = open_db();
        let pass = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        db.finish_boothby_pass(&pass.id, "1050", BOOTHBY_OUTCOME_COMPLETED, None, None, None)
            .unwrap();
        let err = db
            .finish_boothby_pass(&pass.id, "1060", BOOTHBY_OUTCOME_FAILED, None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("not found or already finished"));
    }

    #[test]
    fn invalid_trigger_is_rejected() {
        let (_d, db) = open_db();
        let err = db.open_boothby_pass("not_a_real_trigger", "1000").unwrap_err();
        assert!(err.to_string().contains("invalid boothby pass trigger"));
    }

    #[test]
    fn event_prefixed_trigger_is_accepted() {
        let (_d, db) = open_db();
        let pass = db
            .open_boothby_pass("event:dead_pid_reconcile", "1000")
            .unwrap()
            .unwrap();
        assert_eq!(pass.trigger, "event:dead_pid_reconcile");
    }

    #[test]
    fn invalid_outcome_is_rejected() {
        let (_d, db) = open_db();
        let pass = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        let err = db
            .finish_boothby_pass(&pass.id, "1050", "not_a_real_outcome", None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("invalid boothby pass outcome"));
    }

    #[test]
    fn list_passes_orders_freshest_first_and_respects_limit() {
        let (_d, db) = open_db();
        for (trigger, started_at) in [
            (BOOTHBY_TRIGGER_SCHEDULE, "1000"),
            (BOOTHBY_TRIGGER_MANUAL, "2000"),
            (BOOTHBY_TRIGGER_SCHEDULE, "3000"),
        ] {
            let pass = db.open_boothby_pass(trigger, started_at).unwrap().unwrap();
            db.finish_boothby_pass(&pass.id, started_at, BOOTHBY_OUTCOME_NOTHING_TO_DO, None, None, None)
                .unwrap();
        }

        let all = db.list_boothby_passes(10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].started_at, "3000");
        assert_eq!(all[2].started_at, "1000");

        let limited = db.list_boothby_passes(2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn last_schedule_pass_started_at_ignores_manual_and_event_passes() {
        let (_d, db) = open_db();
        let manual = db.open_boothby_pass(BOOTHBY_TRIGGER_MANUAL, "5000").unwrap().unwrap();
        db.finish_boothby_pass(&manual.id, "5000", BOOTHBY_OUTCOME_NOTHING_TO_DO, None, None, None)
            .unwrap();
        assert_eq!(db.last_boothby_schedule_pass_started_at().unwrap(), None);

        let scheduled = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "6000").unwrap().unwrap();
        db.finish_boothby_pass(&scheduled.id, "6000", BOOTHBY_OUTCOME_NOTHING_TO_DO, None, None, None)
            .unwrap();
        assert_eq!(db.last_boothby_schedule_pass_started_at().unwrap(), Some(6000));
    }

    #[test]
    fn last_finished_pass_ignores_the_open_pass() {
        let (_d, db) = open_db();
        let finished = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        db.finish_boothby_pass(&finished.id, "1050", BOOTHBY_OUTCOME_NOTHING_TO_DO, None, None, None)
            .unwrap();
        let open_pass = db.open_boothby_pass(BOOTHBY_TRIGGER_MANUAL, "2000").unwrap().unwrap();

        let last_finished = db.last_finished_boothby_pass().unwrap().unwrap();
        assert_eq!(last_finished.id, finished.id);
        assert_ne!(
            open_pass.id, last_finished.id,
            "the open pass must not be returned as the last finished pass"
        );
    }

    #[test]
    fn last_finished_pass_is_none_when_nothing_has_finished() {
        let (_d, db) = open_db();
        db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        assert_eq!(db.last_finished_boothby_pass().unwrap(), None);
    }

    #[test]
    fn last_pass_started_at_considers_every_trigger() {
        let (_d, db) = open_db();
        assert_eq!(db.last_boothby_pass_started_at().unwrap(), None);
        let pass = db.open_boothby_pass(BOOTHBY_TRIGGER_MANUAL, "7000").unwrap().unwrap();
        db.finish_boothby_pass(&pass.id, "7000", BOOTHBY_OUTCOME_NOTHING_TO_DO, None, None, None)
            .unwrap();
        assert_eq!(db.last_boothby_pass_started_at().unwrap(), Some(7000));
    }
}
