//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Scan `execution`'s Stop-boundary transcript for `[effort-escalation]`
    /// / `[blocked]` markers and file a (content-deduplicated) attention
    /// item for each one found. Called once per genuinely-terminal Stop,
    /// before any nudge decision — `nudge_or_park` reads the same store to
    /// decide whether to suppress the "produce a PR" loop.
    ///
    /// A malformed marker is still filed (with a parse warning) rather than
    /// dropped: a garbled escalation the coordinator can read by hand beats
    /// the engine silently pretending nothing happened, which is the
    /// incident this exists to fix. `heuristic_blocker_detection` (DEFAULT
    /// OFF) additionally scans for guidance-ask prose when no explicit
    /// marker was found — the documented marker is the contract; the
    /// heuristic is a best-effort net under it.
    ///
    /// When `worker_signal_proposals_seam` is on, each detected signal is
    /// read proposals-first: if `execution` already carries a
    /// `worker_proposals` row of the matching kind *and matching `reason=`*
    /// (see [`Self::execution_has_worker_signal_proposal`]),
    /// `WorkDb::submit_worker_proposal`'s apply pipeline
    /// ([`crate::work::proposal_apply`]) already filed that signal's
    /// attention item synchronously at submission time, so the legacy
    /// marker is skipped rather than re-filed. Only when no matching
    /// proposal exists does the legacy parser run, and every time it does,
    /// the seam's `worker_proposals.fallback_hit.*` counter increments and a
    /// WARN logs — this is the seam's exit criterion for eventually
    /// deleting the parser. With the flag off, the marker parsers run
    /// unconditionally and nothing is counted or skipped.
    pub(super) async fn detect_and_file_worker_signals(&self, execution: &crate::work::WorkExecution) {
        let Some(text) = self.read_final_triage_message(&execution.id).await.into_message() else {
            return;
        };
        let mut signals = worker_escalation::detect_worker_signals(&text);
        if signals.is_empty()
            && self.feature_flags.is_enabled("heuristic_blocker_detection")
            && let Some(signal) = worker_escalation::detect_heuristic_blocker(&text)
        {
            signals.push(signal);
        }
        // `worker_proposals` is the master kill switch for every proposal-backed
        // seam (design §"Gating": "worker_proposals master flag + per-seam
        // flags"); `worker_signal_proposals_seam` is this seam's own flag. Both
        // must be on for the proposals-first read to engage, so flipping the
        // master flag off disables every seam at once regardless of each
        // seam's individual rollout state.
        let proposals_first = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("worker_signal_proposals_seam");
        for signal in &signals {
            if proposals_first && self.execution_has_worker_signal_proposal(execution, signal) {
                // Already filed via the proposal apply pipeline — the legacy
                // marker documents the same event, not a new one.
                continue;
            }
            let filed = self.file_worker_signal_attention(execution, signal).await;
            // Only count a fallback hit when the marker actually filed a new
            // attention item. `detect_and_file_worker_signals` runs on every
            // terminal Stop and the marker line never disappears from the
            // transcript once emitted, so without this an execution that
            // survives N Stops after one uncovered marker would increment
            // the exit-criterion counter N times instead of once.
            if proposals_first && filed {
                self.record_worker_signal_fallback_hit(execution, signal);
            }
        }
    }

    /// Whether `execution` already carries a `worker_proposals` row of
    /// `signal.kind` whose `reason` matches `signal`'s own `reason=` field —
    /// the proposals-first check [`Self::detect_and_file_worker_signals`]
    /// uses to skip *that specific signal's* legacy marker once the worker
    /// has used `boss propose` instead.
    ///
    /// This is content-aware, not just kind-scoped: the Stop-boundary
    /// transcript is cumulative, so an execution that proposed `blocked`
    /// early and then, after `boss propose` became unreachable, fell back to
    /// the `[blocked]` bootstrap marker with a *different* `reason=` must
    /// still have that second, distinct signal filed — kind-scoped skipping
    /// would silently discard it, defeating the bootstrap fallback the
    /// worker directive exists to provide. When the marker itself carries no
    /// comparable reason (a malformed bare marker with no `reason=`), it is
    /// never treated as covered by an existing proposal — `validate_blocked_fields`
    /// already treats a reason-less `[blocked]` as a real signal the worker
    /// meant to send, so it must fall through to
    /// [`Self::file_worker_signal_attention`] rather than being silently
    /// skipped here; that function's own marker-line content dedup still
    /// keeps a truly redundant bare marker from double-filing.
    ///
    /// A storage error fails open (`false`, so the legacy parser still
    /// runs) rather than silently dropping a real signal — the same
    /// "surface it, never swallow it" discipline
    /// [`worker_escalation`]'s module doc describes for malformed markers.
    ///
    /// Field comparisons are trimmed on both sides so a marker whose fields
    /// differ from the proposal only in incidental leading/trailing
    /// whitespace still counts as covered, rather than double-recording (see
    /// [`Self::execution_has_matching_proposal`]).
    pub(super) fn execution_has_worker_signal_proposal(
        &self,
        execution: &crate::work::WorkExecution,
        signal: &WorkerSignal,
    ) -> bool {
        let proposal_kind = match signal.kind {
            WorkerSignalKind::EffortEscalation => ProposalKind::EffortEscalation,
            WorkerSignalKind::Blocked => ProposalKind::Blocked,
        };
        let marker_reason = worker_escalation::extract_quoted(&signal.marker_line, "reason").map(str::trim);
        self.execution_has_matching_proposal(execution, proposal_kind, "worker_signal_proposals_seam", |payload| {
            let proposal_reason = payload.get("reason").and_then(|r| r.as_str()).map(str::trim);
            match (marker_reason, proposal_reason) {
                (Some(m), Some(p)) => m == p,
                // A reason-less marker (malformed, or a payload some
                // future kind omits `reason` from) is never treated as
                // covered by an existing proposal — it falls through to
                // `file_worker_signal_attention`, whose own marker-line
                // content dedup still prevents a truly redundant bare
                // marker from double-filing.
                _ => false,
            }
        })
    }

    /// Shared proposals-first check every proposal-backed seam
    /// (`worker_signal_proposals_seam`, `deferred_scope_proposals_seam`, and
    /// any seam migrated after them per the design's migration map) uses to
    /// decide whether `execution` already carries a `worker_proposals` row
    /// of `kind` that covers a just-detected legacy marker. Holds the db
    /// lookup, the JSON parse, the `.any()` scan, and the fail-open WARN —
    /// each seam supplies only `matches`, its own payload-field predicate
    /// (see [`Self::execution_has_worker_signal_proposal`] and
    /// [`Self::execution_has_deferred_scope_proposal`] for the two current
    /// callers).
    ///
    /// A storage error fails open (`false`, so the legacy parser still
    /// runs) rather than silently dropping a real signal.
    pub(super) fn execution_has_matching_proposal(
        &self,
        execution: &crate::work::WorkExecution,
        kind: ProposalKind,
        seam: &str,
        matches: impl Fn(&serde_json::Value) -> bool,
    ) -> bool {
        match self.work_db.list_worker_proposals_for_execution(&execution.id, kind) {
            Ok(proposals) => proposals.iter().any(|proposal| {
                serde_json::from_str::<serde_json::Value>(&proposal.payload_json)
                    .ok()
                    .is_some_and(|payload| matches(&payload))
            }),
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    ?err,
                    seam,
                    "failed to check for an existing proposal; falling back to the legacy marker \
                     parser for this signal",
                );
                false
            }
        }
    }

    /// Count one legacy-parser hit for `signal.kind`'s seam and log a WARN.
    /// Called only when `worker_signal_proposals_seam` is on, no proposal
    /// covered the signal, and [`Self::file_worker_signal_attention`] actually
    /// filed a new attention item for it — i.e. the legacy path just did the
    /// work the proposal API was supposed to, for the first time. Skipping
    /// this when the marker was already-seen keeps the counter from
    /// re-incrementing on every subsequent terminal Stop of the same
    /// cumulative transcript. See the counter declarations above for what
    /// "exit criterion" means here.
    pub(super) fn record_worker_signal_fallback_hit(
        &self,
        execution: &crate::work::WorkExecution,
        signal: &WorkerSignal,
    ) {
        let (counter, kind) = match signal.kind {
            WorkerSignalKind::EffortEscalation => (&WORKER_SIGNAL_FALLBACK_HIT_EFFORT_ESCALATION, "effort_escalation"),
            WorkerSignalKind::Blocked => (&WORKER_SIGNAL_FALLBACK_HIT_BLOCKED, "blocked"),
        };
        self.record_proposal_fallback_hit(
            execution,
            counter,
            "worker_signal_proposals_seam",
            kind,
            &signal.marker_line,
        );
    }

    /// Shared fallback-hit recorder every proposal-backed seam uses: bumps
    /// `counter` and logs a WARN carrying the seam name, the signal/marker
    /// kind, and the marker line that triggered the legacy path. Each seam's
    /// `record_*_fallback_hit` supplies only its own counter handle and kind
    /// label (see [`Self::record_worker_signal_fallback_hit`] and
    /// [`Self::record_deferred_scope_fallback_hit`]).
    pub(super) fn record_proposal_fallback_hit(
        &self,
        execution: &crate::work::WorkExecution,
        counter: &'static crate::metrics::CounterHandle,
        seam: &str,
        kind: &str,
        marker_line: &str,
    ) {
        counter.inc(&self.metrics);
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            seam,
            kind,
            marker_line,
            "no worker_proposals row found for this execution/kind; legacy marker parser fired \
             instead of the proposal path",
        );
    }

    /// File one [`WorkerSignal`] as a `work_attention_items` row, unless an
    /// item already exists (open or resolved) for this execution carrying
    /// the exact same marker line. Content-keying on the marker line — not
    /// just the kind — keeps this idempotent across the many Stops a single
    /// execution's cumulative transcript survives (the marker line never
    /// disappears from the transcript once emitted) while still letting a
    /// *new*, distinct marker from the same worker surface as its own item,
    /// and letting the coordinator's resolution
    /// ([`WorkDb::resolve_worker_signal_attentions_for_execution`]) actually
    /// stick instead of being immediately re-filed from the same stale line.
    ///
    /// Returns whether a new attention item was actually filed (`false` on
    /// the already-seen early return) so callers — specifically
    /// [`Self::detect_and_file_worker_signals`]'s fallback-hit counting — can
    /// tell a genuinely new signal from a stale marker resurfacing on a
    /// later Stop of the same cumulative transcript.
    pub(super) async fn file_worker_signal_attention(
        &self,
        execution: &crate::work::WorkExecution,
        signal: &WorkerSignal,
    ) -> bool {
        let kind = signal.kind.attention_kind();
        let already_seen = self
            .work_db
            .list_attention_items(&execution.id)
            .map(|items| {
                items
                    .iter()
                    .any(|i| i.kind == kind && i.body_markdown.contains(&signal.marker_line))
            })
            .unwrap_or(false);
        if already_seen {
            return false;
        }

        let (title, label) = match signal.kind {
            WorkerSignalKind::EffortEscalation => {
                ("Worker requested an effort escalation", "an [effort-escalation] marker")
            }
            WorkerSignalKind::Blocked => ("Worker reported a blocker", "a [blocked] marker"),
        };
        let mut body = format!(
            "Worker emitted {label} on its Stop boundary.\n\n\
             - execution: `{execution_id}`\n\
             - work item: `{work_item_id}`\n\n\
             Marker (verbatim):\n\n```\n{marker_line}\n```",
            execution_id = execution.id,
            work_item_id = execution.work_item_id,
            marker_line = signal.marker_line,
        );
        if let Some(warning) = signal.parse_warning.as_deref() {
            body.push_str(&format!(
                "\n\n**Parse warning:** {warning} — the marker is malformed; process it by hand \
                 per the escalation protocol rather than trusting an automated field extraction."
            ));
        }
        body.push_str(
            "\n\nThe auto-nudge \"produce a PR\" loop is paused for this execution while this item \
             is unresolved. Acking the worker (e.g. `bossctl probe`) resolves it and resumes normal \
             nudging.",
        );

        match self.work_db.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: kind.to_owned(),
            status: None,
            title: title.to_owned(),
            body_markdown: body,
            resolved_at: None,
        }) {
            Ok(item) => {
                if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                    let product_id = work_item.product_id().to_string();
                    self.publisher
                        .publish_frontend_event_on_product(&product_id, FrontendEvent::AttentionItemCreated { item })
                        .await;
                }
            }
            Err(err) => {
                // Loud on purpose: a worker's `[blocked]` marker was correctly recognized and
                // parsed, but the attention item never landed and the
                // failure sat at `warn` — invisible enough that root-causing
                // the missed suppression cost a full trace reconstruction.
                // A recognized-but-unfiled marker means the auto-nudge loop
                // will NOT be suppressed even though the worker did the
                // right thing and asked for help; that is exactly the
                // failure mode this whole module exists to prevent, so it
                // gets `error` + an unmistakable prefix instead of a `warn`
                // easily lost in routine log volume.
                tracing::error!(
                    execution_id = %execution.id,
                    kind,
                    marker_line = %signal.marker_line,
                    ?err,
                    "[engine-reconcile] worker escalation: RECOGNIZED marker failed to file as an \
                     attention item — the auto-nudge loop will NOT be suppressed for this execution; \
                     the marker is otherwise lost until a human reads the transcript by hand",
                );
            }
        }
        true
    }

    /// Scan `execution`'s Stop-boundary transcript for `[deferred-scope]`
    /// markers and durably record each one found. See
    /// [`crate::deferred_scope`] for the marker contract and the incident
    /// this exists to fix. Best-effort: recording failures are logged and
    /// swallowed, never block completion.
    ///
    /// When `deferred_scope_proposals_seam` is on (design implementation
    /// task 9, following the recipe `worker_signal_proposals_seam`
    /// established for effort-escalation/blocked), each detected marker is
    /// read proposals-first: if `execution` already carries a
    /// `worker_proposals` row of kind `deferred_scope` matching this
    /// marker's `summary=`/`reason=` (see
    /// [`Self::execution_has_deferred_scope_proposal`]),
    /// `WorkDb::submit_worker_proposal`'s apply pipeline
    /// ([`crate::work::proposal_apply`]) already wrote the audit line and
    /// attention item synchronously at submission time, so the legacy
    /// marker is skipped rather than re-recorded. Only when no matching
    /// proposal exists does the legacy parser run, and every time it does,
    /// `worker_proposals.fallback_hit.deferred_scope` increments and a WARN
    /// logs — this seam's exit criterion for eventually deleting the
    /// parser. With the flag off, the marker parser runs unconditionally
    /// and nothing is counted or skipped.
    pub(super) async fn detect_and_record_deferred_scope(&self, execution: &crate::work::WorkExecution) {
        let Some(text) = self.read_final_triage_message(&execution.id).await.into_message() else {
            return;
        };
        let proposals_first = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("deferred_scope_proposals_seam");
        for item in crate::deferred_scope::detect_deferred_scope_items(&text) {
            if proposals_first && self.execution_has_deferred_scope_proposal(execution, &item) {
                // Already recorded via the proposal apply pipeline — the
                // legacy marker documents the same event, not a new one.
                continue;
            }
            let recorded = self.record_deferred_scope_item(execution, &item).await;
            if proposals_first && recorded {
                self.record_deferred_scope_fallback_hit(execution, &item);
            }
        }
    }

    /// Whether `execution` already carries a `worker_proposals` row of kind
    /// `deferred_scope` whose `summary`/`reason` both match `item`'s own
    /// `summary=`/`reason=` fields — the proposals-first check
    /// [`Self::detect_and_record_deferred_scope`] uses to skip *that
    /// specific marker's* legacy handling once the worker has used
    /// `boss propose` instead. Content-aware, not just kind-scoped, mirroring
    /// [`Self::execution_has_worker_signal_proposal`]'s discipline: a
    /// malformed marker missing either field is never treated as covered
    /// (falls through to [`Self::record_deferred_scope_item`], whose own
    /// marker-line content dedup still prevents a truly redundant marker
    /// from double-recording).
    ///
    /// A storage error fails open (`false`, so the legacy parser still
    /// runs) rather than silently dropping a real signal.
    ///
    /// Field comparisons are trimmed on both sides so a marker whose fields
    /// differ from the proposal only in incidental leading/trailing
    /// whitespace still counts as covered, rather than double-recording (see
    /// [`Self::execution_has_matching_proposal`]).
    pub(super) fn execution_has_deferred_scope_proposal(
        &self,
        execution: &crate::work::WorkExecution,
        item: &crate::deferred_scope::DeferredScopeItem,
    ) -> bool {
        let (marker_summary, marker_reason) = crate::deferred_scope::summary_and_reason(&item.marker_line);
        let marker_summary = marker_summary.as_deref().map(str::trim);
        let marker_reason = marker_reason.as_deref().map(str::trim);
        self.execution_has_matching_proposal(
            execution,
            ProposalKind::DeferredScope,
            "deferred_scope_proposals_seam",
            |payload| {
                let proposal_summary = payload.get("summary").and_then(|v| v.as_str()).map(str::trim);
                let proposal_reason = payload.get("reason").and_then(|v| v.as_str()).map(str::trim);
                match (marker_summary, marker_reason) {
                    (Some(s), Some(r)) => proposal_summary == Some(s) && proposal_reason == Some(r),
                    // A marker missing either field is never treated as
                    // covered by an existing proposal.
                    _ => false,
                }
            },
        )
    }

    /// Count one legacy-parser hit for the deferred-scope seam and log a
    /// WARN. Called only when `deferred_scope_proposals_seam` is on, no
    /// proposal covered the marker, and [`Self::record_deferred_scope_item`]
    /// actually recorded a new item for it — mirrors
    /// [`Self::record_worker_signal_fallback_hit`]'s discipline of not
    /// re-incrementing on every subsequent terminal Stop of the same
    /// cumulative transcript.
    pub(super) fn record_deferred_scope_fallback_hit(
        &self,
        execution: &crate::work::WorkExecution,
        item: &crate::deferred_scope::DeferredScopeItem,
    ) {
        self.record_proposal_fallback_hit(
            execution,
            &DEFERRED_SCOPE_FALLBACK_HIT,
            "deferred_scope_proposals_seam",
            "deferred_scope",
            &item.marker_line,
        );
    }

    /// Durably record one [`crate::deferred_scope::DeferredScopeItem`],
    /// unless an attention item already exists for this execution carrying
    /// the exact same marker line (content-keyed dedup, mirroring
    /// [`Self::file_worker_signal_attention`] — keeps this idempotent across
    /// the many Stops a single execution's cumulative transcript survives).
    ///
    /// Recording has two durable halves: an `[engine-reconcile]`-style audit
    /// line appended to the work item's own description (survives even if
    /// the transcript is later pruned) and a `work_attention_items` row that
    /// surfaces to the coordinator, exactly as effort-escalation/blocked
    /// signals do. The attention item's body explicitly frames the decision
    /// left for a human: create a followup task for the deferred item, or
    /// consciously accept the deferral.
    ///
    /// Returns whether a new attention item was actually filed (`false` on
    /// the already-seen early return) so callers — specifically
    /// [`Self::detect_and_record_deferred_scope`]'s fallback-hit counting —
    /// can tell a genuinely new marker from a stale one resurfacing on a
    /// later Stop of the same cumulative transcript.
    pub(super) async fn record_deferred_scope_item(
        &self,
        execution: &crate::work::WorkExecution,
        item: &crate::deferred_scope::DeferredScopeItem,
    ) -> bool {
        let kind = crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND;
        let already_seen = self
            .work_db
            .list_attention_items(&execution.id)
            .map(|items| {
                items
                    .iter()
                    .any(|i| i.kind == kind && i.body_markdown.contains(&item.marker_line))
            })
            .unwrap_or(false);
        if already_seen {
            return false;
        }

        let epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        let audit_line = crate::deferred_scope::render_audit_line(epoch, item);
        if let Err(err) =
            crate::reconcile_audit::append_description_line(&self.work_db, &execution.work_item_id, &audit_line)
        {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                ?err,
                "deferred-scope: failed to append audit line to description (non-fatal)",
            );
        }

        let mut body = format!(
            "Worker deferred part of this task's scope on its Stop boundary.\n\n\
             - execution: `{execution_id}`\n\
             - work item: `{work_item_id}`\n\n\
             Marker (verbatim):\n\n```\n{marker_line}\n```",
            execution_id = execution.id,
            work_item_id = execution.work_item_id,
            marker_line = item.marker_line,
        );
        if let Some(warning) = item.parse_warning.as_deref() {
            body.push_str(&format!(
                "\n\n**Parse warning:** {warning} — the marker is malformed; read it by hand to \
                 recover the deferred summary/reason."
            ));
        }
        body.push_str(
            "\n\nThis is NOT yet tracked work — the worker has no ability to file a task itself. \
             Decide whether to create a followup task for the deferred item, or consciously accept \
             the deferral (e.g. it is genuinely out of scope for this task). Either way, resolving \
             this item records that a human made that call, rather than the remainder silently \
             vanishing.",
        );

        match self.work_db.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: kind.to_owned(),
            status: None,
            title: "Worker deferred scope".to_owned(),
            body_markdown: body,
            resolved_at: None,
        }) {
            Ok(item) => {
                if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                    let product_id = work_item.product_id().to_string();
                    self.publisher
                        .publish_frontend_event_on_product(&product_id, FrontendEvent::AttentionItemCreated { item })
                        .await;
                }
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    ?err,
                    "deferred-scope: failed to file attention item (non-fatal)",
                );
            }
        }
        true
    }

    /// `Some(reason)` when `execution` has at least one *unresolved*
    /// (`status != "resolved"`) worker-escalation/blocker attention item —
    /// the condition [`Self::nudge_or_park`] uses to suppress the
    /// "produce a PR" auto-nudge loop. `None` when there is none (never
    /// filed, or filed-and-resolved).
    pub(super) fn unresolved_worker_signal_reason(&self, execution: &crate::work::WorkExecution) -> Option<String> {
        let items = self.work_db.list_attention_items(&execution.id).ok()?;
        let open: Vec<&str> = items
            .iter()
            .filter(|i| {
                (i.kind == worker_escalation::WORKER_ESCALATION_ATTENTION_KIND
                    || i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
                    && i.status != "resolved"
            })
            .map(|i| i.kind.as_str())
            .collect();
        if open.is_empty() {
            return None;
        }
        Some(format!(
            "{} unresolved worker signal(s) pending coordinator action ({})",
            open.len(),
            open.join(", ")
        ))
    }

    /// Whether the worker emitted the sanctioned [`NO_CHANGES_NEEDED`
    /// marker](crate::no_op_signal::NO_CHANGES_NEEDED_MARKER) in its final
    /// assistant prose — its unambiguous signal that the assigned work is
    /// already done and there is genuinely nothing to commit/push/open a PR
    /// for. Reads the run transcript (reusing the triage-marker reader) and
    /// scans it for an own-line emission of the marker.
    ///
    /// Returns `false` on any read failure or when no transcript is recorded
    /// — absence of the marker must never be guessed at: a worker that
    /// stopped without the explicit signal is treated as "gave up / not done"
    /// and falls through to the normal produce-a-PR nudge.
    pub(super) async fn worker_signalled_no_op(&self, execution_id: &str) -> bool {
        match self.read_final_triage_message(execution_id).await.into_message() {
            Some(text) => crate::no_op_signal::transcript_signals_no_op(&text),
            None => false,
        }
    }
}
