use super::*;

/// Create the four Boothby tables — `boothby_passes`, `boothby_actions`,
/// `boothby_findings`, `boothby_cursors`.
///
/// Boothby is Boss's autonomous groundskeeper. A *pass* is one wake-up;
/// every mutation it makes during that pass is journalled as an *action*
/// carrying pre/post images (the undo payload); what it mines out of logs
/// and transcripts lands as *findings*, deduped by fingerprint; *cursors*
/// record how far each mining source has been read.
///
/// DDL follows `tools/boss/docs/designs/boothby.md` §"Audit & undo data
/// model" column-for-column. Where this migration adds a `CHECK` the design
/// does not spell out, it is only for genuinely closed vocabularies listed
/// in that section — `trigger` is deliberately left open past its `event:`
/// prefix, since the design defines it as `'schedule' | 'event:<name>' |
/// 'manual'` with an open-ended event name.
///
/// Purely additive and fully idempotent — every statement is
/// `IF NOT EXISTS` and no existing table or row is touched. Ships dark: the
/// executor that writes these tables is task 2 of the design's breakdown,
/// so nothing populates them yet and no existing behaviour can change.
///
/// Ordering within the batch matters despite `IF NOT EXISTS`:
/// `boothby_actions` carries a real FK to `boothby_passes`, so the parent
/// must exist first.
pub(crate) fn migrate_boothby_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS boothby_passes (
             id              TEXT PRIMARY KEY,
             -- 'schedule' | 'event:<name>' | 'manual'. Left unconstrained
             -- past the documented shapes: the event name is open-ended, so
             -- a CHECK here would reject triggers the design allows.
             trigger         TEXT NOT NULL,
             started_at      TEXT NOT NULL,
             -- NULL while the pass is in flight; set with `outcome`.
             finished_at     TEXT,
             outcome         TEXT
                                 CHECK (outcome IS NULL OR outcome IN
                                     ('completed', 'nothing_to_do', 'timed_out', 'failed', 'capped')),
             actions_count   INTEGER NOT NULL DEFAULT 0,
             proposals_count INTEGER NOT NULL DEFAULT 0,
             findings_count  INTEGER NOT NULL DEFAULT 0,
             -- Agent-authored, written by the `pass-summary` verb.
             summary         TEXT,
             session_id      TEXT,
             transcript_path TEXT,
             -- A pass is finished exactly when it has an outcome. Without
             -- this a crashed pass could sit in flight forever holding an
             -- outcome, or report `completed` with no end time.
             CHECK ((outcome IS NULL) = (finished_at IS NULL))
         );

         -- At most one pass runs at a time. Not merely hygiene: the mutation
         -- layer resolves an action's owning pass by looking up *the* open
         -- pass in-transaction, which is only well-defined because this
         -- index makes a second concurrent pass impossible. Partial index
         -- keyed on a constant, so finished passes are unconstrained.
         CREATE UNIQUE INDEX IF NOT EXISTS boothby_passes_single_open_idx
             ON boothby_passes((1))
             WHERE finished_at IS NULL;

         CREATE INDEX IF NOT EXISTS boothby_passes_started_idx
             ON boothby_passes(started_at DESC);

         CREATE TABLE IF NOT EXISTS boothby_actions (
             id            TEXT PRIMARY KEY,
             -- NOT NULL per the design: an action is always part of a pass.
             -- ON DELETE CASCADE so the retention prune of old passes takes
             -- their journal detail with them (design §Retention).
             pass_id       TEXT NOT NULL REFERENCES boothby_passes(id) ON DELETE CASCADE,
             -- Ordinal within the pass; `(pass_id, seq)` is the read order.
             seq           INTEGER NOT NULL,
             -- Catalogue slug, e.g. 'close_stale_task'. Supplied by the
             -- executor's verb catalogue (task 2), not inferred here: the
             -- mutation layer sees a column delta, never the intent behind
             -- it, and a guessed verb in an audit trail is worse than none.
             verb          TEXT NOT NULL,
             -- task | project | attention | attention_item | execution |
             -- lease | workspace | file | issue. Unconstrained: the
             -- operational verbs (task 9) target kinds that are not WorkDb
             -- rows at all, and the catalogue is the authority on the set.
             target_kind   TEXT NOT NULL,
             target_id     TEXT NOT NULL,
             -- JSON: the verb's inputs.
             params        TEXT,
             -- Agent-supplied one-liner, required by the design — an
             -- unexplained autonomous mutation is exactly what the journal
             -- exists to prevent.
             rationale     TEXT NOT NULL,
             -- JSON of the mutated fields before / after. Restricted to the
             -- columns the mutation actually touched, so replaying
             -- `pre_image` reverts exactly what Boothby changed and cannot
             -- clobber a column another writer has moved since. `pre_image`
             -- is NULL for I-class (irreversible) actions, which journal
             -- `params` + evidence instead.
             pre_image     TEXT,
             -- Also the undo conflict check: undo compares the row's
             -- current state against this before restoring `pre_image`.
             post_image    TEXT,
             reversibility TEXT NOT NULL
                               CHECK (reversibility IN ('reversible', 'semi', 'irreversible')),
             undo_state    TEXT NOT NULL DEFAULT 'none'
                               CHECK (undo_state IN ('none', 'undoable', 'undone', 'expired', 'conflicted')),
             undone_at     TEXT,
             -- Undo is human-only; the Boothby session has no undo verb, so
             -- it cannot launder its own mistakes.
             undone_by     TEXT,
             created_at    TEXT NOT NULL
         );

         CREATE UNIQUE INDEX IF NOT EXISTS boothby_actions_by_pass
             ON boothby_actions(pass_id, seq);

         -- Drives 'what has Boothby done to this row?' and the undo lookup.
         CREATE INDEX IF NOT EXISTS boothby_actions_by_target
             ON boothby_actions(target_kind, target_id);

         CREATE TABLE IF NOT EXISTS boothby_findings (
             id                TEXT PRIMARY KEY,
             -- Content-derived dedup key, and the memory that makes 'this
             -- has happened 40 times' legible without a GROUP BY over
             -- history. Also what a human veto suppresses.
             fingerprint       TEXT NOT NULL UNIQUE,
             kind              TEXT NOT NULL
                                   CHECK (kind IN ('error', 'anomaly', 'perf', 'friction', 'taxonomy')),
             -- JSON refs: log span / transcript span / row ids.
             subject           TEXT NOT NULL,
             first_seen        TEXT NOT NULL,
             last_seen         TEXT NOT NULL,
             occurrences       INTEGER NOT NULL DEFAULT 1 CHECK (occurrences >= 1),
             status            TEXT NOT NULL
                                   CHECK (status IN ('open', 'filed', 'resolved', 'suppressed')),
             filed_kind        TEXT CHECK (filed_kind IS NULL OR filed_kind IN ('chore', 'github_issue')),
             -- Task id or issue URL, per `filed_kind`.
             filed_ref         TEXT,
             suppressed_reason TEXT
         );

         CREATE INDEX IF NOT EXISTS boothby_findings_status_idx
             ON boothby_findings(status, last_seen DESC);

         -- One row per mining source; `source` IS the key, so no surrogate
         -- id. Findings outlive passes (they are the dedup memory), so
         -- neither this nor boothby_findings references boothby_passes.
         CREATE TABLE IF NOT EXISTS boothby_cursors (
             -- e.g. 'engine-trace', 'dispatch-events', 'transcript:<session>'.
             source     TEXT PRIMARY KEY,
             -- JSON: segment/offset or timestamp high-water mark.
             position   TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}
