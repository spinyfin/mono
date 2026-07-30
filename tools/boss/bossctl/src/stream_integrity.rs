//! Reporting for dispatch-stream records `bossctl` could not read.
//!
//! `bossctl dispatch diagnose` is the tool of record for "why did this work
//! item not dispatch". Its primary evidence is frequently an *absence* — no
//! terminal stage, no paired convergence event, no `request_recorded` — so a
//! record it could not read is not a cosmetic wart: it is a hole in the
//! evidence, and a timeline printed over that hole reads as complete when it is
//! not. The reader now recovers what it can and reports the rest (see
//! `boss_dispatch_reader::integrity`); this module is what makes the remainder
//! impossible to miss:
//!
//! - a banner **above** the timeline, so it is read before any conclusion,
//! - a marker **inside** the timeline at the point records were unreadable,
//! - an explicit qualification on any finding whose conclusion rests on
//!   absence, when damage overlaps the window that conclusion is about,
//! - a footer naming every file and line, and a machine-readable
//!   `stream_integrity` block in `--json`,
//! - a distinct process exit code when records were genuinely lost.
//!
//! Deliberately NOT here: any way to suppress the above. There is no `--quiet`
//! and nothing is routed to a channel a caller can drop. If the stream is
//! damaged, every consumer of this output learns about it.
//!
//! **Both evidence streams, not just one.** `dispatch diagnose` derives
//! findings from two files: the dispatch JSONL *and* `engine-trace.jsonl` (SIG-4b
//! / SIG-5 / SIG-5c / SIG-G read only the latter). A clean dispatch stream over a
//! torn trace is exactly the same defect as the reverse — the report looks
//! complete while evidence was dropped, and a trace-only signature that never
//! fired is indistinguishable from one that had nothing to match. So
//! [`IntegrityReport`] carries the engine-trace scan's damage too, including the
//! case where the whole scan failed, and every surface above accounts for it:
//! [`IntegrityReport::is_intact`] is false, the banner and footer name it, the
//! `--json` block reports it under `trace_integrity`, and lost trace records
//! drive the same [`EXIT_UNRECOVERED_RECORDS`] exit status. There is no stderr
//! warning path left for a dropped evidence source to hide in.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use boss_engine::dispatch_reader::{DamageShape, DamagedLine};
use serde_json::{Value, json};

/// Exit code when a command read an evidence stream from which records were
/// genuinely lost (not merely concatenated and recovered).
///
/// Distinct from `1`, which means the command itself failed: the report was
/// produced and is usable, it just cannot be treated as complete. Automated
/// callers that only inspect the exit status still cannot miss it.
///
/// **Scope is "this report", not "this id".** The status latches on any file the
/// command read, which for `dispatch diagnose <id>` includes the fleet-wide
/// `current.jsonl` and the engine-trace segments — so it can fire for damage
/// that is not attributable to `<id>`. That is intentional and it is the same
/// claim the banner makes: the *report* is incomplete, because the command
/// derived cross-execution findings (SIG-2 recurrence, the trace signatures)
/// from those shared files. Per-execution attribution is a separate, finer
/// question and lives elsewhere: `timelines.<id>.complete` in `--json`, the
/// in-timeline markers, and the per-execution rule that decides whether an
/// absence-based finding gets [`ABSENCE_CAVEAT`]. A script that wants "is
/// `<id>`'s own timeline complete" must read that field, not this exit code.
pub const EXIT_UNRECOVERED_RECORDS: u8 = 3;

/// Set when any command in this process read a stream with unrecoverable
/// records, so `main` can pick the exit code up without every verb's signature
/// having to carry it. A process-wide flag rather than a return value because
/// `bossctl` runs exactly one verb per invocation, and threading an exit code
/// through a 400-arm command match to serve one verb would obscure it rather
/// than clarify it.
static LOST_RECORDS_SEEN: AtomicBool = AtomicBool::new(false);

/// The exit code for a successful run, accounting for any stream damage seen.
pub fn exit_code() -> ExitCode {
    if LOST_RECORDS_SEEN.load(Ordering::Relaxed) {
        ExitCode::from(EXIT_UNRECOVERED_RECORDS)
    } else {
        ExitCode::SUCCESS
    }
}

/// Deduplicated integrity report over the damage a command collected.
///
/// The same file is often read twice in one command (`diagnose` reads the flat
/// stream for its fleet scan, and again to reconstruct a missing mirror), so
/// damage is deduplicated by `(path, line)` — a caller must never be told a
/// stream is twice as broken as it is.
#[derive(Debug, Clone, Default)]
pub struct IntegrityReport {
    damage: Vec<DamagedLine>,
    trace: TraceIntegrity,
}

/// Integrity of the engine-trace scan, kept beside the dispatch-stream damage
/// rather than pooled into it.
///
/// Separate because the two answer different questions. Dispatch damage is
/// time-bracketed and attributable to an execution's timeline, so it can
/// qualify one finding and leave another alone. Trace damage is neither: the
/// segments are fleet-wide and their `timestamp` is an RFC3339 string this
/// binary has no clock library to convert, so there is no epoch-millisecond
/// window to test a finding against. It therefore qualifies the findings that
/// were *derived from the trace* and nothing else — which is more useful than
/// pooling it would be, and more honest than dropping it.
#[derive(Debug, Clone, Default)]
pub struct TraceIntegrity {
    /// Torn engine-trace lines, with whatever was salvaged from them.
    pub damage: Vec<DamagedLine>,
    /// Set when the scan could not be completed at all (an unreadable segment).
    /// This used to be an `eprintln!` the caller carried on past — the loudest
    /// possible way to lose a whole evidence source quietly.
    pub scan_error: Option<String>,
}

impl TraceIntegrity {
    pub fn is_intact(&self) -> bool {
        self.damage.is_empty() && self.scan_error.is_none()
    }

    /// Damaged trace lines from which at least one record is definitively gone.
    pub fn lost_lines(&self) -> impl Iterator<Item = &DamagedLine> {
        self.damage.iter().filter(|line| line.shape.lost_records())
    }

    pub fn recovered_records(&self) -> usize {
        self.damage.iter().map(|line| line.recovered).sum()
    }

    /// True when trace evidence is missing — records lost, or the scan failed.
    /// A concatenated-but-fully-recovered trace line does not degrade evidence,
    /// so it does not count here (it is still reported).
    pub fn evidence_degraded(&self) -> bool {
        self.scan_error.is_some() || self.lost_lines().next().is_some()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "intact": self.is_intact(),
            "scan_failed": self.scan_error.is_some(),
            "scan_error": self.scan_error.clone().map(Value::String).unwrap_or(Value::Null),
            "unreadable_lines": self.damage.len(),
            "recovered_records": self.recovered_records(),
            "lines_with_lost_records": self.lost_lines().count(),
            "evidence_degraded": self.evidence_degraded(),
            "lines": self.damage,
        })
    }
}

impl IntegrityReport {
    pub fn new(damage: impl IntoIterator<Item = DamagedLine>) -> Self {
        let mut seen: BTreeSet<(std::path::PathBuf, u64)> = BTreeSet::new();
        let mut out = Vec::new();
        for line in damage {
            if seen.insert((line.path.clone(), line.line_number)) {
                out.push(line);
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line_number.cmp(&b.line_number)));
        let report = Self {
            damage: out,
            trace: TraceIntegrity::default(),
        };
        report.latch_lost_records();
        report
    }

    /// Attach the engine-trace scan's integrity, so every surface on this type
    /// covers both evidence streams.
    pub fn with_trace(mut self, trace: TraceIntegrity) -> Self {
        self.trace = trace;
        self.latch_lost_records();
        self
    }

    pub fn trace(&self) -> &TraceIntegrity {
        &self.trace
    }

    fn latch_lost_records(&self) {
        if self.lost_lines().next().is_some() || self.trace.evidence_degraded() {
            LOST_RECORDS_SEEN.store(true, Ordering::Relaxed);
        }
    }

    /// True when every line of every evidence stream this report covers was read
    /// cleanly. False for engine-trace damage as well as dispatch damage — the
    /// callers that gate a caveat on this (`render_findings`'s "no matches",
    /// `NO_EVENTS_CAVEAT`, the banner) are making a claim about the whole report.
    pub fn is_intact(&self) -> bool {
        self.damage.is_empty() && self.trace.is_intact()
    }

    /// True when the *dispatch* stream specifically was read cleanly, ignoring
    /// the trace. For the few places that are talking about the timeline rather
    /// than the report.
    pub fn dispatch_stream_is_intact(&self) -> bool {
        self.damage.is_empty()
    }

    pub fn lines(&self) -> &[DamagedLine] {
        &self.damage
    }

    /// Damaged lines from which at least one record is definitively gone.
    pub fn lost_lines(&self) -> impl Iterator<Item = &DamagedLine> {
        self.damage.iter().filter(|line| line.shape.lost_records())
    }

    pub fn recovered_records(&self) -> usize {
        self.damage.iter().map(|line| line.recovered).sum()
    }

    /// True when any unrecoverable damage overlaps `[from, to]` — i.e. when an
    /// event in that window could have existed and been lost.
    pub fn could_hide_event_in(&self, from: u128, to: u128) -> bool {
        self.lost_lines().any(|line| line.overlaps_window(from, to))
    }

    /// One line of provenance for a damaged line, for a marker or footer.
    fn describe(line: &DamagedLine) -> String {
        let where_ = format!(
            "{}:{}",
            line.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
            line.line_number
        );
        match line.shape {
            DamageShape::Concatenated => format!(
                "{where_}  {} record(s) recovered from one {}-byte line (interleaved write; nothing lost)",
                line.recovered, line.byte_len
            ),
            DamageShape::PartiallyRecovered => format!(
                "{where_}  {} record(s) recovered, {} byte(s) UNRECOVERABLE: {}",
                line.recovered, line.lost_bytes, line.lost_excerpt
            ),
            DamageShape::Unrecoverable => format!(
                "{where_}  NO record recovered, {} byte(s) UNRECOVERABLE: {}",
                line.lost_bytes, line.lost_excerpt
            ),
        }
    }

    /// Prominent block that belongs *before* any timeline or finding, so damage
    /// is read before the conclusions it undermines.
    ///
    /// Rendered rather than printed so a test can assert its content; every
    /// renderer here returns the empty string for an intact stream, which is
    /// what makes the call sites unconditional.
    pub fn render_banner(&self) -> String {
        if self.is_intact() {
            return String::new();
        }
        let lost = self.lost_lines().count();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "!! ── STREAM INTEGRITY ─────────────────────────────────────────────"
        );
        if !self.damage.is_empty() {
            let _ = writeln!(
                out,
                "!! {} unreadable line(s) in the dispatch stream; {} record(s) recovered, {} line(s) UNRECOVERABLE",
                self.damage.len(),
                self.recovered_records(),
                lost,
            );
        }
        if let Some(err) = &self.trace.scan_error {
            let _ = writeln!(out, "!! engine-trace scan FAILED: {err}");
            let _ = writeln!(
                out,
                "!! Trace-only signatures (SIG-4b / SIG-5 / SIG-5c / SIG-G) did not run over"
            );
            let _ = writeln!(
                out,
                "!! this evidence at all. Their absence below means NOTHING was checked."
            );
        }
        if !self.trace.damage.is_empty() {
            let _ = writeln!(
                out,
                "!! {} unreadable line(s) in engine-trace; {} record(s) recovered, {} line(s) UNRECOVERABLE",
                self.trace.damage.len(),
                self.trace.recovered_records(),
                self.trace.lost_lines().count(),
            );
        }
        if lost > 0 {
            let _ = writeln!(
                out,
                "!! Records are missing from the timeline below. The ABSENCE of an event"
            );
            let _ = writeln!(out, "!! in this report is NOT evidence that it never happened.");
        } else if !self.damage.is_empty() {
            let _ = writeln!(
                out,
                "!! Every dispatch record was recovered, so the timeline below is complete —"
            );
            let _ = writeln!(
                out,
                "!! but the stream interleaved, which means the writer needs attention."
            );
        }
        if self.trace.evidence_degraded() {
            let _ = writeln!(
                out,
                "!! Engine-trace evidence is incomplete, so a trace-only signature that did"
            );
            let _ = writeln!(out, "!! not fire may simply never have seen its record.");
        }
        let _ = writeln!(
            out,
            "!! ─────────────────────────────────────────────────────────────────"
        );
        out
    }

    pub fn print_banner(&self) {
        print!("{}", self.render_banner());
    }

    /// Full per-line accounting, for after the findings.
    pub fn render_footer(&self) -> String {
        if self.is_intact() {
            return String::new();
        }
        let mut out = String::new();
        let _ = writeln!(
            out,
            "\nstream integrity: {} unreadable line(s)",
            self.damage.len() + self.trace.damage.len()
        );
        let mut by_file: BTreeMap<&std::path::Path, Vec<&DamagedLine>> = BTreeMap::new();
        for line in self.lines().iter().chain(self.trace.damage.iter()) {
            by_file.entry(line.path.as_path()).or_default().push(line);
        }
        for (path, lines) in by_file {
            let _ = writeln!(out, "  {}", path.display());
            for line in lines {
                let _ = writeln!(out, "    {}", Self::describe(line));
            }
        }
        if let Some(err) = &self.trace.scan_error {
            let _ = writeln!(out, "  engine-trace scan FAILED, no trace evidence was read: {err}");
        }
        let lost = self.lost_lines().count() + self.trace.lost_lines().count();
        if lost > 0 || self.trace.scan_error.is_some() {
            let _ = writeln!(
                out,
                "  exit status {EXIT_UNRECOVERED_RECORDS}: {lost} line(s) lost records; \
                 treat this report as incomplete"
            );
        }
        out
    }

    pub fn print_footer(&self) {
        print!("{}", self.render_footer());
    }

    /// One-line notice for verbs that print a listing rather than a timeline
    /// (`dispatch stats`, `dispatch tail`, `dispatch ghost-active`, `dispatch
    /// state --history`). Short, but never absent.
    pub fn render_notice(&self) -> String {
        if self.is_intact() {
            return String::new();
        }
        let lost = self.lost_lines().count();
        let mut out = String::new();
        if let Some(err) = &self.trace.scan_error {
            let _ = writeln!(out, "!! engine-trace scan FAILED: {err}");
        }
        if self.damage.is_empty() {
            // Trace-only damage: nothing to say about this listing's own stream.
            return out;
        }
        if lost > 0 {
            let _ = writeln!(
                out,
                "!! stream integrity: {} unreadable line(s), {lost} with UNRECOVERABLE records — \
                 this listing may be incomplete (`bossctl dispatch diagnose <id>` reports the detail)",
                self.damage.len()
            );
        } else {
            let _ = writeln!(
                out,
                "!! stream integrity: {} interleaved line(s), all {} record(s) recovered",
                self.damage.len(),
                self.recovered_records()
            );
        }
        out
    }

    pub fn print_notice(&self) {
        print!("{}", self.render_notice());
    }

    pub fn to_json(&self) -> Value {
        let incomplete = self.lost_lines().next().is_some() || self.trace.evidence_degraded();
        json!({
            "intact": self.is_intact(),
            "dispatch_stream_intact": self.dispatch_stream_is_intact(),
            "unreadable_lines": self.damage.len(),
            "recovered_records": self.recovered_records(),
            "lines_with_lost_records": self.lost_lines().count(),
            "exit_code": if incomplete { EXIT_UNRECOVERED_RECORDS } else { 0 },
            "lines": self.damage,
            // Sibling block rather than pooled into `lines`: the trace is a
            // second evidence source with its own failure modes (including a
            // whole-scan failure, which has no line number), and an automated
            // caller must be able to tell "the timeline has holes" from "the
            // trace signatures never ran".
            "trace_integrity": self.trace.to_json(),
        })
    }
}

/// A damage marker to interleave into a rendered timeline, and where it goes.
///
/// `after_ts_epoch_ms` is the timestamp of the last record read before the
/// damage, so the marker lands at the position in the timeline where the
/// unreadable bytes actually sat rather than being appended at the end.
#[derive(Debug, Clone)]
pub struct TimelineMarker {
    pub after_ts_epoch_ms: Option<u128>,
    pub text: String,
}

/// Build the in-timeline markers for `damage`.
///
/// Every damaged line gets one, including fully-recovered ones: "records here
/// were unreadable" is a fact about the timeline whether or not this reader
/// happened to be able to reconstruct them.
pub fn timeline_markers(damage: &[DamagedLine]) -> Vec<TimelineMarker> {
    let mut markers: Vec<TimelineMarker> = damage
        .iter()
        .map(|line| TimelineMarker {
            after_ts_epoch_ms: line.prev_ts_epoch_ms,
            text: format!(
                "!! UNREADABLE  {}  [{}]",
                IntegrityReport::describe(line),
                line.shape.as_str()
            ),
        })
        .collect();
    markers.sort_by_key(|marker| marker.after_ts_epoch_ms.unwrap_or(0));
    markers
}

/// Sentence appended to a finding whose conclusion rests on an event not being
/// present, when unreadable records overlap the window that conclusion covers.
pub const ABSENCE_CAVEAT: &str = "UNRELIABLE: unreadable records overlap the window this conclusion \
                                  covers, so the absence it rests on is not established — a lost \
                                  record could be the event this finding says is missing. Fix the \
                                  stream damage (see the stream integrity block) before acting on \
                                  this as fact.";

/// Variant of [`ABSENCE_CAVEAT`] for a finding derived from engine-trace, whose
/// damage cannot be time-windowed (see [`TraceIntegrity`]) — so the caveat is
/// about the source rather than about an overlap.
pub const TRACE_ABSENCE_CAVEAT: &str = "UNRELIABLE: engine-trace evidence was incomplete (unreadable \
                                        records, or the scan failed), and this conclusion rests on \
                                        an event not being present in it. Fix the trace damage (see \
                                        the stream integrity block) before acting on this as fact.";

/// Sentence printed in place of a bare "no matches" when an evidence stream was
/// damaged. "No matches" over a stream with holes in it is not a clean bill of
/// health, and must not read as one. Covers engine-trace as well as dispatch:
/// the trace-only signatures cannot match a record the scan never read.
pub const NO_MATCHES_CAVEAT: &str = "…but records in the evidence this scan read were unreadable, so \
                                     \"no matches\" does NOT mean nothing matched — a signature that \
                                     depends on an event may simply not have seen it. See the stream \
                                     integrity block above.";

/// Sentence printed when a diagnose found nothing at all over a damaged stream.
/// The strongest absence claim the tool makes, so the one that most needs
/// qualifying: "there is nothing here" and "I could not read what is here" look
/// identical to a reader unless the difference is spelled out.
pub const NO_EVENTS_CAVEAT: &str = "This is NOT \"nothing happened\": records for this id were \
                                    unreadable, so events may exist that this scan could not parse. \
                                    Fix the stream damage below and re-run before concluding the id \
                                    never dispatched.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn damaged(line_number: u64, shape: DamageShape, prev: Option<u128>, next: Option<u128>) -> DamagedLine {
        DamagedLine {
            path: PathBuf::from("/state/dispatch-events/current.jsonl"),
            line_number,
            byte_len: 418,
            recovered: if shape == DamageShape::Unrecoverable { 0 } else { 2 },
            lost_bytes: if shape == DamageShape::Concatenated { 0 } else { 200 },
            lost_excerpt: "{\"ts_epoch_ms\":1,\"stage\":\"worker_cla".to_owned(),
            shape,
            prev_ts_epoch_ms: prev,
            next_ts_epoch_ms: next,
        }
    }

    #[test]
    fn report_deduplicates_the_same_line_read_twice() {
        let report = IntegrityReport::new(vec![
            damaged(266198, DamageShape::Concatenated, Some(10), Some(20)),
            damaged(266198, DamageShape::Concatenated, Some(10), Some(20)),
            damaged(274670, DamageShape::Concatenated, Some(30), Some(40)),
        ]);
        assert_eq!(
            report.lines().len(),
            2,
            "one file+line is one finding, however often read"
        );
    }

    #[test]
    fn a_fully_recovered_stream_reports_no_lost_records() {
        let report = IntegrityReport::new(vec![damaged(1, DamageShape::Concatenated, Some(10), Some(20))]);
        assert!(!report.is_intact(), "an interleaved line is still reported");
        assert_eq!(report.lost_lines().count(), 0);
        assert_eq!(report.recovered_records(), 2);
        assert_eq!(report.to_json()["exit_code"], json!(0));
    }

    /// Note for future edits: `exit_code()` reads process-wide state that any
    /// `IntegrityReport::new` with lost records latches. Asserting it is
    /// `EXIT_UNRECOVERED_RECORDS` right after constructing such a report is
    /// order-independent; asserting it is `SUCCESS` anywhere would NOT be, since
    /// another test may have latched it first. Test the intact case through
    /// `to_json()["exit_code"]` instead, as `a_fully_recovered_stream_…` does.
    #[test]
    fn unrecoverable_damage_drives_the_distinct_exit_code() {
        let report = IntegrityReport::new(vec![damaged(1, DamageShape::Unrecoverable, Some(10), Some(20))]);
        assert_eq!(report.lost_lines().count(), 1);
        assert_eq!(report.to_json()["exit_code"], json!(EXIT_UNRECOVERED_RECORDS));
        assert_eq!(exit_code(), ExitCode::from(EXIT_UNRECOVERED_RECORDS));
    }

    #[test]
    fn could_hide_event_in_only_fires_for_overlapping_unrecoverable_damage() {
        let report = IntegrityReport::new(vec![damaged(1, DamageShape::Unrecoverable, Some(100), Some(200))]);
        assert!(report.could_hide_event_in(150, 400));
        assert!(!report.could_hide_event_in(300, 400));

        let recovered = IntegrityReport::new(vec![damaged(2, DamageShape::Concatenated, Some(100), Some(200))]);
        assert!(
            !recovered.could_hide_event_in(150, 400),
            "a recovered line hides nothing, so it must not qualify a conclusion"
        );
    }

    #[test]
    fn markers_are_ordered_by_where_the_damage_sat_in_the_timeline() {
        let markers = timeline_markers(&[
            damaged(9, DamageShape::Unrecoverable, Some(900), Some(1000)),
            damaged(2, DamageShape::Concatenated, Some(100), Some(200)),
        ]);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].after_ts_epoch_ms, Some(100));
        assert_eq!(markers[1].after_ts_epoch_ms, Some(900));
        assert!(markers[0].text.starts_with("!! UNREADABLE"));
        assert!(markers[1].text.contains("unrecoverable"));
    }

    #[test]
    fn describe_names_the_file_line_and_what_was_lost() {
        let text = IntegrityReport::describe(&damaged(266198, DamageShape::Unrecoverable, None, None));
        assert!(text.contains("current.jsonl:266198"), "{text}");
        assert!(text.contains("UNRECOVERABLE"), "{text}");
    }
}
