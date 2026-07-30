//! Accounting for dispatch-JSONL lines that are not exactly one record.
//!
//! Every reader of the dispatch stream used to handle a line that failed to
//! parse the same way: print a warning to stderr and move on. That is wrong in
//! two independent ways, and this module exists to fix both.
//!
//! **The events were usually still there.** The writer's historical defect
//! (fixed in `boss_engine_jsonl_append` — see that crate's docs) issued a
//! record's body and its trailing newline as two separate `write()` calls.
//! `O_APPEND` makes each individual write atomic but not the pair, so two
//! concurrent appenders *in the same process* produced `bodyA` `bodyB` `\n`
//! `\n` on disk: ONE line
//! holding two complete records, followed by a blank line. A JSON parser reads
//! that as `trailing characters at line 1 column <len(bodyA)+1>` — which is
//! exactly the error, and exactly the column distribution (clustered at
//! whole-record lengths, never mid-token), observed in production. Both
//! records are fully present; dropping the line discarded recoverable events.
//! [`salvage_damaged_line`] recovers them.
//!
//! **A warning is not an accounting.** A diagnostic tool whose whole purpose
//! is to answer "why did nothing happen here" cannot treat "some records in
//! this file were unreadable" as a side note: an absence in the timeline is
//! its primary evidence, and unreadable records make absence unprovable. So
//! every line that was not exactly one clean record is reported as a
//! structured [`DamagedLine`] — carrying where it was, how big it was, how
//! much was recovered, how much is genuinely gone, and the timestamps of the
//! records either side of it, so a caller can say which part of a timeline is
//! untrustworthy rather than which stderr line scrolled past.
//!
//! **Recovery must not invent.** Resync deliberately anchors on a bare `{`, and
//! `DispatchEvent` deserialization is non-strict, so a `{` reached inside a
//! truncated record's bytes can land on a nested `details` payload and parse.
//! Every record recovered *after* a resync therefore passes
//! [`dispatch_record_is_plausible`] before it is accepted, and rejected bytes
//! stay in `lost_bytes`: a fabricated event in a forensic timeline is worse than
//! a reported loss, because it reads as real.
//!
//! The salvage machinery is generic ([`salvage_records_with`]) so the *other*
//! evidence stream a diagnose reads — engine-trace JSONL, whose records are open
//! JSON objects — gets the same recovery and the same accounting instead of
//! silently skipping its torn lines.

use std::path::{Path, PathBuf};

use boss_dispatch_events::{DispatchEvent, Outcome, Stage};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Longest excerpt of unrecoverable bytes carried on a [`DamagedLine`].
/// Enough to recognise the shape of what was lost without reproducing a whole
/// record's `details` payload in a report.
pub const LOST_EXCERPT_MAX_BYTES: usize = 120;

/// Cap on resync attempts within one damaged line. A line whose bytes defeat
/// this many restarts is reported as unrecoverable rather than scanned
/// quadratically; in the dominant real case (whole records concatenated) the
/// first attempt consumes the entire line.
const MAX_RESYNC_ATTEMPTS: usize = 64;

/// Floor on a plausible `ts_epoch_ms` (2020-01-01T00:00:00Z). Boss did not
/// exist before this, so a smaller value is not a dispatch timestamp — it is a
/// number that happened to sit under a `ts_epoch_ms` key inside some other
/// object's payload.
const MIN_PLAUSIBLE_TS_EPOCH_MS: u128 = 1_577_836_800_000;

/// Ceiling on a plausible `ts_epoch_ms` (2100-01-01T00:00:00Z). Same argument
/// from the other side: real records are not centuries in the future, and a
/// nested counter or byte offset read as a timestamp usually lands far outside
/// the range rather than just inside it.
const MAX_PLAUSIBLE_TS_EPOCH_MS: u128 = 4_102_444_800_000;

/// How far *behind* the preceding record a salvaged record's timestamp may sit
/// and still be believed.
///
/// The stream is near-but-not-exactly time-ordered: `ts_epoch_ms` is stamped
/// when the event is built, while the append is serialized later, so two events
/// built microseconds apart can land in either order. A minute is far more slack
/// than that race needs, and still rules out a "timestamp" pulled from an
/// embedded payload describing something that happened much earlier.
const TS_BACKWARD_TOLERANCE_MS: u128 = 60_000;

/// What a damaged line turned out to be, once salvage had run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DamageShape {
    /// Two or more complete records on one line, every byte accounted for.
    /// The historical two-write interleave produces exactly this. No event is
    /// lost — but the line is still reported, because a stream that can
    /// interleave is a stream whose completeness must be stated, not assumed.
    Concatenated,
    /// Some records recovered; some bytes could not be turned into a record.
    PartiallyRecovered,
    /// Nothing on the line parsed as a record. This is the only shape that
    /// means events are definitively missing from the timeline.
    Unrecoverable,
}

impl DamageShape {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Concatenated => "concatenated",
            Self::PartiallyRecovered => "partially_recovered",
            Self::Unrecoverable => "unrecoverable",
        }
    }

    /// True when at least one record on this line is gone for good.
    pub fn lost_records(self) -> bool {
        matches!(self, Self::PartiallyRecovered | Self::Unrecoverable)
    }
}

/// One JSONL line that did not parse as exactly one dispatch record.
///
/// `prev_ts_epoch_ms` / `next_ts_epoch_ms` bracket the damage in time, taken
/// from the records parsed immediately before and after this line in the same
/// file. They are what lets a caller decide whether damage is relevant to a
/// particular conclusion: a timeline claim about a window that does not
/// overlap `[prev, next]` is unaffected by this line, and one that does
/// overlap it cannot be stated as fact. `None` means the damage sits at the
/// start (or end) of the file with no neighbouring record to anchor it, which
/// is the *least* bounded case, not the most benign — treat it as overlapping
/// everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub struct DamagedLine {
    pub path: PathBuf,
    /// 1-based line number, matching what an editor or `sed -n Np` shows.
    pub line_number: u64,
    pub byte_len: usize,
    /// Records salvaged from this line.
    pub recovered: usize,
    /// Bytes on this line that no resync could turn into a record.
    pub lost_bytes: usize,
    /// Control-stripped, clipped sample of those bytes.
    pub lost_excerpt: String,
    pub shape: DamageShape,
    pub prev_ts_epoch_ms: Option<u128>,
    pub next_ts_epoch_ms: Option<u128>,
}

impl DamagedLine {
    /// True when this damage could hide an event at `ts`. Unbracketed damage
    /// (no neighbouring record on one side) covers everything on that side —
    /// deliberately conservative: over-qualifying a conclusion costs an
    /// operator a sentence, under-qualifying one costs them the incident.
    pub fn could_hide_ts(&self, ts: u128) -> bool {
        self.prev_ts_epoch_ms.is_none_or(|prev| ts >= prev) && self.next_ts_epoch_ms.is_none_or(|next| ts <= next)
    }

    /// True when this damage overlaps the inclusive window `[from, to]`.
    pub fn overlaps_window(&self, from: u128, to: u128) -> bool {
        let start = self.prev_ts_epoch_ms.unwrap_or(0);
        let end = self.next_ts_epoch_ms.unwrap_or(u128::MAX);
        start <= to && from <= end
    }
}

/// Events read from one or more dispatch-JSONL files, together with every line
/// that could not be read cleanly.
///
/// Returning damage alongside events — rather than warning about it on stderr
/// — is the whole point: a caller physically cannot render a timeline from
/// this type without having been handed the reasons it might be incomplete.
#[derive(Debug, Clone, Default)]
pub struct StreamRead {
    pub events: Vec<DispatchEvent>,
    pub damage: Vec<DamagedLine>,
}

impl StreamRead {
    /// Fold `other` in, keeping event order (callers read files
    /// oldest-segment-first) and accumulating damage.
    pub fn absorb(&mut self, other: StreamRead) {
        self.events.extend(other.events);
        self.damage.extend(other.damage);
    }

    /// True when every line in every file scanned was exactly one record.
    pub fn is_intact(&self) -> bool {
        self.damage.is_empty()
    }

    /// Damaged lines from which at least one record is definitively gone.
    pub fn lines_with_lost_records(&self) -> impl Iterator<Item = &DamagedLine> {
        self.damage.iter().filter(|d| d.shape.lost_records())
    }

    pub fn recovered_records(&self) -> usize {
        self.damage.iter().map(|d| d.recovered).sum()
    }

    /// Discard the integrity report and keep only the events.
    ///
    /// Named to be unpleasant on purpose. It is correct for callers that are
    /// not presenting evidence to a human — the engine's own stall sweeps,
    /// which reduce a timeline to "has this stage moved recently" and re-run
    /// every 15 seconds — and wrong for anything that prints a timeline.
    pub fn into_events_ignoring_damage(self) -> Vec<DispatchEvent> {
        self.events
    }
}

/// Recovered records and unrecoverable remainder from one damaged line.
///
/// Generic over the record type so the same resync-and-account machinery serves
/// both evidence streams a diagnose reads: dispatch JSONL (`DispatchEvent`) and
/// engine-trace JSONL (open `serde_json::Value` objects). Sharing it is what
/// keeps the two streams' integrity reporting honest in the same way — a torn
/// engine-trace line is as much a hole in the evidence as a torn dispatch line.
#[derive(Debug, Clone)]
pub struct Salvage<T = DispatchEvent> {
    pub records: Vec<T>,
    pub lost_bytes: usize,
    pub lost_excerpt: String,
}

impl<T> Default for Salvage<T> {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            lost_bytes: 0,
            lost_excerpt: String::new(),
        }
    }
}

impl<T> Salvage<T> {
    pub fn shape(&self) -> DamageShape {
        match (self.records.len(), self.lost_bytes) {
            (0, _) => DamageShape::Unrecoverable,
            (_, 0) => DamageShape::Concatenated,
            _ => DamageShape::PartiallyRecovered,
        }
    }
}

/// True when a record recovered by *resyncing into* a damaged line's bytes is
/// plausible enough to put in a forensic timeline.
///
/// Resync anchors on a bare `{` on purpose (see [`next_record_start`]), and
/// `DispatchEvent` deserialization is non-strict: no `deny_unknown_fields`, and
/// only four required fields. Together those mean a `{` reached in the middle of
/// a truncated record can land on a *nested* object — dispatch `details`
/// payloads do embed event-shaped objects, e.g. `redundant_spawn`'s
/// `live_execution_id` block or a `stage_stalled` descriptor — and yield a
/// "record" the writer never wrote. A fabricated event in a forensic timeline is
/// strictly worse than a reported loss: it is evidence that reads as real.
///
/// So the weak anchor keeps its robustness and this gate supplies the missing
/// half, checking the recovered value against what the writer can actually
/// produce. Rejected bytes are counted in `lost_bytes` like any other
/// unrecoverable span, so the accounting does not improve by discarding them.
///
/// `prev_ts_epoch_ms` is the timestamp of the last record read before this line
/// in the same file, when there is one — the cheap neighbour check the reviewer
/// of a synthesized record would do by eye.
fn dispatch_record_is_plausible(event: &DispatchEvent, prev_ts_epoch_ms: Option<u128>) -> bool {
    if event.execution_id.is_empty() {
        return false;
    }
    // Unknown stage/outcome strings are the strongest single signal: the writer
    // only ever emits `Stage`/`Outcome` variants. This does mean an OLDER
    // bossctl reading a NEWER engine's stream would reject a brand-new stage —
    // but only on the resync path, and it reports the bytes as lost rather than
    // claiming completeness, which is the honest failure direction.
    if serde_json::from_value::<Stage>(serde_json::Value::String(event.stage.clone())).is_err() {
        return false;
    }
    if serde_json::from_value::<Outcome>(serde_json::Value::String(event.outcome.clone())).is_err() {
        return false;
    }
    if !(MIN_PLAUSIBLE_TS_EPOCH_MS..=MAX_PLAUSIBLE_TS_EPOCH_MS).contains(&event.ts_epoch_ms) {
        return false;
    }
    prev_ts_epoch_ms.is_none_or(|prev| event.ts_epoch_ms + TS_BACKWARD_TOLERANCE_MS >= prev)
}

/// Pull every dispatch record salvageable from `line`, which is known not to
/// parse as exactly one record.
///
/// `prev_ts_epoch_ms` is the timestamp of the last record read before this line
/// in the same file; it tightens the plausibility gate applied to records
/// recovered by resync (see [`dispatch_record_is_plausible`]). `None` — damage
/// at the head of a file — simply skips that one check.
pub fn salvage_damaged_line(line: &str, prev_ts_epoch_ms: Option<u128>) -> Salvage<DispatchEvent> {
    salvage_records_with(line, |event: &DispatchEvent| {
        dispatch_record_is_plausible(event, prev_ts_epoch_ms)
    })
}

/// Pull every record salvageable from `line`, which is known not to parse as
/// exactly one record.
///
/// Walks `line` with a streaming deserializer, which handles the dominant
/// corruption shape (complete records concatenated) in a single pass. On a
/// parse error it resyncs to the next plausible record start and tries again,
/// so a truncated record followed by a whole one still yields the whole one.
/// Every byte not consumed by an *accepted* record is counted in `lost_bytes` —
/// salvage never quietly improves the accounting for itself.
///
/// `plausible` gates records recovered **after a resync**, and only those. The
/// distinction is the point: the first streaming pass starts at byte 0 of the
/// line, which is a genuine record boundary (it is where the writer's `write()`
/// began), so records it yields need no second-guessing and a forward-compatible
/// stream is never rejected for the dominant concatenated case. Every later pass
/// starts at a `{` this code *guessed* at inside damaged bytes, and a guess that
/// happens to parse is exactly how a phantom record enters a timeline.
pub fn salvage_records_with<T, F>(line: &str, plausible: F) -> Salvage<T>
where
    T: DeserializeOwned,
    F: Fn(&T) -> bool,
{
    let mut records = Vec::new();
    let mut lost: Vec<&str> = Vec::new();
    let mut cursor = 0usize;

    for attempt in 0..MAX_RESYNC_ATTEMPTS {
        if cursor >= line.len() {
            break;
        }
        let after_resync = attempt > 0;
        let rest = &line[cursor..];
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<T>();
        // Bytes of `rest` consumed by records that actually parsed. Advanced
        // only on a successful yield, so an error leaves it pointing at the end
        // of the last good record rather than wherever the parser gave up.
        let mut consumed = 0usize;
        let mut failed = false;
        loop {
            match stream.next() {
                Some(Ok(record)) => {
                    let end = stream.byte_offset();
                    if after_resync && !plausible(&record) {
                        // Parsed, but not something the writer could have
                        // written here. Account for its bytes rather than
                        // emitting it: a reported loss beats a phantom event.
                        lost.push(&rest[consumed..end]);
                    } else {
                        records.push(record);
                    }
                    consumed = end;
                }
                Some(Err(_)) => {
                    failed = true;
                    break;
                }
                None => break,
            }
        }

        let stopped_at = cursor + consumed;
        if !failed {
            // The stream ended cleanly. Anything after the last record is
            // trailing whitespace (harmless) or bytes serde skipped over.
            let tail = &line[stopped_at..];
            if !tail.trim().is_empty() {
                lost.push(tail);
            }
            cursor = line.len();
            break;
        }

        // Resync: give up on the bytes from the last good record to the next
        // plausible record start, and restart the stream there.
        match next_record_start(line, stopped_at) {
            Some(next) => {
                lost.push(&line[stopped_at..next]);
                cursor = next;
            }
            None => {
                lost.push(&line[stopped_at..]);
                cursor = line.len();
                break;
            }
        }
    }

    // Whatever the attempt cap left unexamined is lost too, by definition.
    if cursor < line.len() {
        lost.push(&line[cursor..]);
    }

    let lost_bytes = lost.iter().map(|span| span.trim().len()).sum();
    Salvage {
        records,
        lost_bytes,
        lost_excerpt: clip_excerpt(&lost),
    }
}

/// First plausible start of a JSON object strictly after `from`.
///
/// "Plausible" is only `{` — deliberately weak, because a stricter anchor
/// (say, `{"ts_epoch_ms"`) would silently stop recovering the moment the
/// writer's field order changed. A `{` that turns out to be inside a truncated
/// record's string literal simply costs one more failed attempt.
fn next_record_start(line: &str, from: usize) -> Option<usize> {
    let search_from = line[from..].char_indices().nth(1).map(|(offset, _)| from + offset)?;
    line[search_from..].find('{').map(|offset| search_from + offset)
}

/// Join the unrecoverable spans into one clipped, control-free excerpt safe to
/// print to a terminal.
fn clip_excerpt(lost: &[&str]) -> String {
    let mut out = String::new();
    for span in lost {
        let span = span.trim();
        if span.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(" … ");
        }
        for ch in span.chars() {
            if out.len() >= LOST_EXCERPT_MAX_BYTES {
                break;
            }
            out.push(if ch.is_control() { '·' } else { ch });
        }
        if out.len() >= LOST_EXCERPT_MAX_BYTES {
            out.push('…');
            break;
        }
    }
    out
}

/// Build a [`DamagedLine`] for `line_number` in `path` from a [`Salvage`],
/// bracketed by the records parsed either side of it.
///
/// `events` is the accumulating output for this file and `events_before` is its
/// length as of just before this line was salvaged — which is all that is
/// needed to find both neighbours: the record before the damage is the last one
/// pushed before it, and the record after is the first one pushed after this
/// line's own recovered records.
pub(crate) fn damaged_line(
    path: &Path,
    line_number: u64,
    byte_len: usize,
    salvage: &Salvage,
    events: &[DispatchEvent],
    events_before: usize,
) -> DamagedLine {
    let prev_ts_epoch_ms = events_before
        .checked_sub(1)
        .and_then(|idx| events.get(idx))
        .map(|event| event.ts_epoch_ms);
    let next_ts_epoch_ms = events
        .get(events_before + salvage.records.len())
        .map(|event| event.ts_epoch_ms);
    DamagedLine {
        path: path.to_path_buf(),
        line_number,
        byte_len,
        recovered: salvage.records.len(),
        lost_bytes: salvage.lost_bytes,
        lost_excerpt: salvage.lost_excerpt.clone(),
        shape: salvage.shape(),
        prev_ts_epoch_ms,
        next_ts_epoch_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Base timestamp for fixtures. Real, in-range epoch milliseconds rather
    /// than 1/2/3: the plausibility gate on resynced records rejects values that
    /// are not credible dispatch timestamps, so a fixture using tiny counters
    /// would be testing the gate instead of the salvage.
    const T0: u128 = 1_770_000_000_000;

    fn record(ts: u128, stage: &str) -> String {
        record_full(ts, stage, "ok", "exec-1")
    }

    fn record_full(ts: u128, stage: &str, outcome: &str, execution_id: &str) -> String {
        format!(
            "{{\"ts_epoch_ms\":{ts},\"stage\":\"{stage}\",\"outcome\":\"{outcome}\",\
             \"execution_id\":\"{execution_id}\",\"work_item_id\":null,\"worker_id\":null,\
             \"cube_repo_id\":null,\"cube_lease_id\":null,\"cube_workspace_id\":null,\
             \"details\":null}}"
        )
    }

    /// The production corruption shape: two complete records on one line. Both
    /// must come back, and nothing may be reported lost.
    #[test]
    fn salvages_both_records_from_a_concatenated_line() {
        let line = format!(
            "{}{}",
            record(T0 + 1, "request_recorded"),
            record(T0 + 2, "worker_claimed")
        );
        let salvage = salvage_damaged_line(&line, None);
        assert_eq!(salvage.records.len(), 2);
        assert_eq!(salvage.records[0].stage, "request_recorded");
        assert_eq!(salvage.records[1].stage, "worker_claimed");
        assert_eq!(salvage.lost_bytes, 0);
        assert_eq!(salvage.shape(), DamageShape::Concatenated);
    }

    #[test]
    fn salvages_three_concatenated_records() {
        let line = format!(
            "{}{}{}",
            record(T0 + 1, "request_recorded"),
            record(T0 + 2, "worker_claimed"),
            record(T0 + 3, "pane_spawned")
        );
        let salvage = salvage_damaged_line(&line, None);
        assert_eq!(salvage.records.len(), 3);
        assert_eq!(salvage.lost_bytes, 0);
    }

    /// A truncated record followed by a whole one: the whole one is recoverable
    /// and must be recovered, and the truncated bytes must be reported lost
    /// rather than rounded away by the recovery succeeding.
    #[test]
    fn recovers_the_whole_record_after_a_truncated_one() {
        let whole = record(T0 + 9, "pane_spawned");
        let line = format!("{{\"ts_epoch_ms\":{},\"stage\":\"worker_cla{whole}", T0 + 8);
        let salvage = salvage_damaged_line(&line, None);
        assert_eq!(salvage.records.len(), 1, "the intact trailing record must survive");
        assert_eq!(salvage.records[0].ts_epoch_ms, T0 + 9);
        assert!(salvage.lost_bytes > 0, "the truncated prefix must be counted as lost");
        assert_eq!(salvage.shape(), DamageShape::PartiallyRecovered);
    }

    #[test]
    fn reports_a_trailing_truncated_record_as_partially_recovered() {
        let line = format!(
            "{}{{\"ts_epoch_ms\":{},\"stage\":\"pane_spa",
            record(T0 + 2, "worker_claimed"),
            T0 + 3
        );
        let salvage = salvage_damaged_line(&line, None);
        assert_eq!(salvage.records.len(), 1);
        assert!(salvage.lost_bytes > 0);
        assert_eq!(salvage.shape(), DamageShape::PartiallyRecovered);
    }

    #[test]
    fn reports_a_line_with_no_recoverable_record_as_unrecoverable() {
        let salvage = salvage_damaged_line("not-json-at-all", None);
        assert!(salvage.records.is_empty());
        assert_eq!(salvage.shape(), DamageShape::Unrecoverable);
        assert_eq!(salvage.lost_bytes, "not-json-at-all".len());
    }

    /// Well-formed JSON that is not a dispatch record must not be silently
    /// accepted as one, and must be accounted as lost.
    #[test]
    fn a_json_object_that_is_not_a_dispatch_record_is_lost_not_recovered() {
        let salvage = salvage_damaged_line("{\"unrelated\":true}{\"also\":1}", None);
        assert!(salvage.records.is_empty());
        assert_eq!(salvage.shape(), DamageShape::Unrecoverable);
        assert!(salvage.lost_bytes > 0);
    }

    /// The phantom-record case the `{`-anchored resync makes possible: a
    /// truncated record whose surviving tail contains a NESTED object carrying
    /// every field `DispatchEvent` requires. Resync walks into the middle of the
    /// truncated bytes, finds that object's `{`, and it parses — so without a
    /// plausibility gate salvage would emit an event the writer never wrote.
    /// A fabricated event in a forensic timeline is worse than a reported loss.
    #[test]
    fn a_nested_event_shaped_object_in_a_truncated_tail_is_not_synthesized() {
        // Shaped like a real `redundant_spawn` details payload: an inner object
        // that itself has ts_epoch_ms / stage / outcome / execution_id.
        let line = "{\"ts_epoch_ms\":1770000000001,\"stage\":\"host_selected\",\"outcome\":\"error\",\
                    \"execution_id\":\"exec-1\",\"details\":{\"reason\":\"redundant_spawn\",\
                    \"live\":{\"ts_epoch_ms\":12,\"stage\":\"not_a_stage\",\"outcome\":\"ok\",\
                    \"execution_id\":\"exec-9\"}},\"trunc";
        let salvage = salvage_damaged_line(line, None);
        assert!(
            salvage.records.is_empty(),
            "no record may be synthesized from a nested payload: {:?}",
            salvage.records.iter().map(|r| &r.stage).collect::<Vec<_>>()
        );
        assert_eq!(salvage.shape(), DamageShape::Unrecoverable);
        assert!(
            salvage.lost_bytes > 0,
            "rejected bytes stay in the accounting rather than vanishing"
        );
    }

    /// Same shape, but the nested object's `stage` IS a real wire stage, so the
    /// stage check alone would not catch it. The timestamp is what gives it away:
    /// a plain counter is not a credible epoch-millisecond value.
    #[test]
    fn a_nested_object_with_a_real_stage_but_an_implausible_ts_is_rejected() {
        let line = "{\"ts_epoch_ms\":1770000000001,\"stage\":\"stage_stalled\",\"outcome\":\"error\",\
                    \"execution_id\":\"exec-1\",\"details\":{\"stalled\":\
                    {\"ts_epoch_ms\":42,\"stage\":\"worker_claimed\",\"outcome\":\"ok\",\
                    \"execution_id\":\"exec-1\"}},\"trunc";
        let salvage = salvage_damaged_line(line, None);
        assert!(salvage.records.is_empty(), "{:?}", salvage.records);
        assert!(salvage.lost_bytes > 0);
    }

    /// The neighbour check: a resynced record dated well before the last clean
    /// record in the same file is not a record from this append-ordered stream.
    #[test]
    fn a_resynced_record_far_older_than_its_neighbour_is_rejected() {
        let stale = record(T0 - 10 * 60 * 1000, "pane_spawned");
        let line = format!("{{\"ts_epoch_ms\":{},\"stage\":\"worker_cla{stale}", T0 + 1);

        let ungated = salvage_damaged_line(&line, None);
        assert_eq!(
            ungated.records.len(),
            1,
            "with no neighbour there is nothing to compare to"
        );

        let gated = salvage_damaged_line(&line, Some(T0));
        assert!(
            gated.records.is_empty(),
            "a record predating the previous clean one by minutes is not plausible here"
        );
        assert!(gated.lost_bytes >= stale.len());
    }

    /// The gate must apply ONLY to the resync path. Byte 0 of a line is a real
    /// record boundary — it is where the writer's `write()` began — so the first
    /// streaming pass must accept what it parses, or a newer engine's brand-new
    /// stage would be reported lost from an ordinary concatenated line.
    #[test]
    fn the_first_pass_is_not_gated_so_a_future_stage_still_recovers() {
        let line = format!(
            "{}{}",
            record_full(T0 + 1, "a_stage_this_build_has_never_heard_of", "ok", "exec-1"),
            record(T0 + 2, "worker_claimed")
        );
        let salvage = salvage_damaged_line(&line, Some(T0));
        assert_eq!(
            salvage.records.len(),
            2,
            "forward compatibility is preserved on the un-guessed path"
        );
        assert_eq!(salvage.lost_bytes, 0);
        assert_eq!(salvage.shape(), DamageShape::Concatenated);
    }

    /// A resynced record with an empty `execution_id` cannot be attributed to any
    /// timeline, so it is not usable evidence even if it parses.
    #[test]
    fn a_resynced_record_with_no_execution_id_is_rejected() {
        let anonymous = record_full(T0 + 9, "pane_spawned", "ok", "");
        let line = format!("{{\"ts_epoch_ms\":{},\"stage\":\"worker_cla{anonymous}", T0 + 1);
        let salvage = salvage_damaged_line(&line, None);
        assert!(salvage.records.is_empty());
        assert!(salvage.lost_bytes > 0);
    }

    /// Bytes rejected by the gate must be counted, not dropped: the point of the
    /// gate is to prefer a reported loss over a phantom, which only holds if the
    /// loss is actually reported.
    #[test]
    fn rejected_bytes_are_counted_in_the_lost_accounting() {
        let implausible = record_full(T0 + 9, "not_a_real_stage", "ok", "exec-1");
        let line = format!("{{\"ts_epoch_ms\":{},\"stage\":\"worker_cla{implausible}", T0 + 1);
        let salvage = salvage_damaged_line(&line, None);
        assert!(salvage.records.is_empty());
        assert!(
            salvage.lost_bytes >= implausible.len(),
            "lost_bytes {} must cover the rejected record's {} bytes",
            salvage.lost_bytes,
            implausible.len()
        );
    }

    /// The generic entry point serves engine-trace lines, whose records are open
    /// `Value` objects rather than `DispatchEvent`s. Same recovery, same
    /// accounting, caller-supplied plausibility.
    #[test]
    fn salvage_records_with_recovers_concatenated_open_json_objects() {
        let line = "{\"timestamp\":\"2026-07-26T00:00:00Z\",\"level\":\"INFO\",\"fields\":{}}\
                    {\"timestamp\":\"2026-07-26T00:00:01Z\",\"level\":\"WARN\",\"fields\":{}}";
        let salvage: Salvage<serde_json::Value> =
            salvage_records_with(line, |value: &serde_json::Value| value.get("timestamp").is_some());
        assert_eq!(salvage.records.len(), 2);
        assert_eq!(salvage.lost_bytes, 0);
        assert_eq!(salvage.shape(), DamageShape::Concatenated);
    }

    #[test]
    fn lost_excerpt_is_clipped_and_control_free() {
        let noisy = format!("\u{1}\u{2}{}", "q".repeat(400));
        let salvage = salvage_damaged_line(&noisy, None);
        assert!(salvage.lost_excerpt.len() <= LOST_EXCERPT_MAX_BYTES + 4);
        assert!(!salvage.lost_excerpt.chars().any(char::is_control));
        assert!(salvage.lost_excerpt.contains('·'), "control bytes are substituted");
    }

    /// Salvage must terminate on adversarial input rather than scanning
    /// forever, and must still account for every byte.
    #[test]
    fn salvage_terminates_on_a_line_of_nothing_but_braces() {
        let line = "{".repeat(500);
        let salvage = salvage_damaged_line(&line, None);
        assert!(salvage.records.is_empty());
        assert_eq!(salvage.lost_bytes, line.len());
    }

    #[test]
    fn could_hide_ts_treats_an_unbracketed_side_as_open() {
        let mut damage = DamagedLine {
            path: PathBuf::from("current.jsonl"),
            line_number: 1,
            byte_len: 10,
            recovered: 0,
            lost_bytes: 10,
            lost_excerpt: String::new(),
            shape: DamageShape::Unrecoverable,
            prev_ts_epoch_ms: Some(100),
            next_ts_epoch_ms: Some(200),
        };
        assert!(damage.could_hide_ts(150));
        assert!(!damage.could_hide_ts(99));
        assert!(!damage.could_hide_ts(201));

        damage.next_ts_epoch_ms = None;
        assert!(damage.could_hide_ts(u128::MAX), "no following record means open-ended");
    }

    #[test]
    fn overlaps_window_is_inclusive_at_both_ends() {
        let damage = DamagedLine {
            path: PathBuf::from("current.jsonl"),
            line_number: 1,
            byte_len: 10,
            recovered: 0,
            lost_bytes: 10,
            lost_excerpt: String::new(),
            shape: DamageShape::Unrecoverable,
            prev_ts_epoch_ms: Some(100),
            next_ts_epoch_ms: Some(200),
        };
        assert!(damage.overlaps_window(200, 300));
        assert!(damage.overlaps_window(0, 100));
        assert!(!damage.overlaps_window(201, 300));
        assert!(!damage.overlaps_window(0, 99));
    }
}
