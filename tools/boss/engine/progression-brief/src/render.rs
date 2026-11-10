//! Markdown rendering of a [`ProgressionBriefPacket`].

use crate::catalogue::{render_question_criteria, render_verb_catalogue};
use crate::packet::{Neighbour, ProgressionBriefPacket};

/// Render the diagnosis packet as the progression worker's prompt body.
pub fn render_progression_brief(packet: &ProgressionBriefPacket) -> String {
    let mut out = String::new();
    out.push_str("# Progression diagnosis\n\n");
    out.push_str(
        "You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item \
         from the packet below and act with `boss propose` verbs. Stored attempts are supplemented \
         by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` \
         if you need a live check.\n\n",
    );
    render_item(&mut out, packet);
    render_ledger(&mut out, packet);
    render_dependencies(&mut out, packet);
    render_executions(&mut out, packet);
    render_declared_blocker(&mut out, packet);
    render_pr(&mut out, packet);
    render_attentions(&mut out, packet);
    render_questions(&mut out, packet);
    render_history(&mut out, packet);
    out.push_str(&render_verb_catalogue(&packet.budget));
    out.push('\n');
    out.push_str(render_question_criteria());
    out
}

fn render_item(out: &mut String, packet: &ProgressionBriefPacket) {
    let item = &packet.item;
    out.push_str("## Item\n\n");
    line(out, "id", &item.id);
    line(out, "name", &item.name);
    line(out, "kind", item.kind.as_str());
    line(out, "status", item.status.as_str());
    line(out, "product", &packet.product_name);
    line(out, "project", packet.project_name.as_deref().unwrap_or("(none)"));
    line(out, "stuck", &packet.stuck_kind);
    match packet.stuck_since {
        Some(since) => line(out, "stuck since", &since.to_string()),
        None => line(out, "stuck since", "(unknown)"),
    }
    line(
        out,
        "blocked_reason",
        item.blocked_reason.as_deref().unwrap_or("(none)"),
    );
    if let Some(detail) = item.blocked_detail.as_deref() {
        line(out, "blocked_detail", detail);
    }
    line(
        out,
        "dispatch_failed_reason",
        item.dispatch_failed_reason.as_deref().unwrap_or("(none)"),
    );
    if let Some(error) = item.dispatch_failed_error.as_deref() {
        line(out, "dispatch_failed_error", error);
    }
    if let Some(basis) = packet.failure_basis.as_deref() {
        line(out, "failure_basis", basis);
    }
    line(out, "pr_url", item.pr_url.as_deref().unwrap_or("(none)"));
    out.push('\n');
}

fn render_ledger(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## Ledger (last 3 status changes and the row that entered the stuck state)\n\n");
    if packet.ledger.is_empty() {
        out.push_str("(none)\n\n");
        return;
    }
    for row in &packet.ledger {
        let old = row.old_value.as_deref().unwrap_or("∅");
        let new = row.new_value.as_deref().unwrap_or("∅");
        let basis = row.basis.as_deref().unwrap_or("(no basis)");
        out.push_str(&format!(
            "- {property} {old} → {new} by {actor} at {at} ({basis})\n",
            property = row.property,
            actor = row.actor,
            at = row.changed_at,
        ));
    }
    out.push('\n');
}

fn render_dependencies(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## Dependencies\n\n");
    out.push_str("Prerequisites:\n");
    render_neighbours(out, &packet.prerequisites);
    out.push_str("Dependents:\n");
    render_neighbours(out, &packet.dependents);
    out.push('\n');
}

fn render_neighbours(out: &mut String, neighbours: &[Neighbour]) {
    if neighbours.is_empty() {
        out.push_str("- (none)\n");
        return;
    }
    for neighbour in neighbours {
        let live = match &neighbour.live_execution {
            Some((id, kind)) => format!(" live={kind}/{id}"),
            None => String::new(),
        };
        out.push_str(&format!(
            "- {id} ({kind}, {status}, {relation}){live}\n",
            id = neighbour.edge.id,
            kind = neighbour.edge.kind,
            status = neighbour.edge.status,
            relation = neighbour.edge.relation,
        ));
    }
}

fn render_executions(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## Executions (item and revision chain)\n\n");
    out.push_str(&format!(
        "Churn-window terminal count (1h, excluding progression): {}\n\n",
        packet.churn_window_terminal_count
    ));
    if packet.executions.is_empty() {
        out.push_str("(none)\n\n");
        return;
    }
    for row in &packet.executions {
        let via = row.created_via.as_deref().unwrap_or("(unset)");
        let error = row
            .last_error
            .as_deref()
            .or(row.latest_run_error.as_deref())
            .unwrap_or("(none)");
        let done = row.run_done_outcome.as_deref().unwrap_or("(none)");
        out.push_str(&format!(
            "- {id} item={item} kind={kind} status={status} via={via} run_done={done} error={error}\n",
            id = row.id,
            item = row.work_item_id,
            kind = row.kind,
            status = row.status,
        ));
    }
    out.push('\n');
}

fn render_declared_blocker(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## Declared blocker (2.9)\n\n");
    match &packet.declared_blocker {
        Some(blocker) => {
            out.push_str(&format!("Producer: {}\n\n", blocker.producer));
            if let Some(note) = blocker.sweep_note.as_deref() {
                out.push_str(&format!("Engine sweep note (not the worker's words): {note}\n\n"));
            }
            out.push_str("Verbatim:\n\n");
            out.push_str("```\n");
            out.push_str(blocker.verbatim.trim_end());
            out.push_str("\n```\n\n");
        }
        None => out.push_str("(none)\n\n"),
    }
}

fn render_pr(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## PR\n\n");
    if let Some(owner) = packet.pr_owner.as_deref() {
        out.push_str(&format!(
            "Parent PR (owned by chain root {owner}); this item is a revision.\n\n"
        ));
    }
    match packet.pr_url.as_deref() {
        None | Some("") => {
            out.push_str("(no PR bound)\n\n");
            render_conflict_attempts(out, packet);
            render_ci_attempts(out, packet);
            return;
        }
        Some(url) => line(out, "url", url),
    }
    line(
        out,
        "mergeable",
        packet.pr_mergeable.as_deref().unwrap_or("(unobserved)"),
    );
    line(
        out,
        "merge_state_status",
        packet.pr_merge_state_status.as_deref().unwrap_or("(unobserved)"),
    );
    line(out, "head_sha", packet.pr_head_sha.as_deref().unwrap_or("(unobserved)"));
    line(
        out,
        "observed_at",
        packet.pr_observed_at.as_deref().unwrap_or("(unobserved)"),
    );
    line(
        out,
        "ci_required_state",
        packet.ci_required_state.as_deref().unwrap_or("(unobserved)"),
    );
    line(
        out,
        "review_required_state",
        packet.review_required_state.as_deref().unwrap_or("(unobserved)"),
    );
    out.push('\n');
    render_conflict_attempts(out, packet);
    render_ci_attempts(out, packet);
    render_check_history(out, packet);
}

fn render_conflict_attempts(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("### Conflict resolutions\n\n");
    if packet.conflict_attempts.is_empty() {
        out.push_str("(none)\n\n");
        return;
    }
    for attempt in &packet.conflict_attempts {
        out.push_str(&format!(
            "- {id} status={status} failure={failure} class={class}\n",
            id = attempt.id,
            status = attempt.status,
            failure = attempt.failure_reason.as_deref().unwrap_or("(none)"),
            class = attempt.conflict_class.as_deref().unwrap_or("(none)"),
        ));
        match attempt.conflict_diagnosis.as_deref() {
            Some(json) => {
                out.push_str("  diagnosis:\n");
                render_conflict_diagnosis(out, json, packet.pr_head_sha.as_deref());
            }
            None => out.push_str("  diagnosis: (not stored; agent may `gh pr view`)\n"),
        }
    }
    out.push('\n');
}

fn render_conflict_diagnosis(out: &mut String, json: &str, current_head: Option<&str>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        out.push_str("  (unparseable diagnosis JSON)\n");
        return;
    };
    if let (Some(stored), Some(current)) = (value.get("head_sha").and_then(|v| v.as_str()), current_head)
        && stored != current
    {
        out.push_str(&format!(
            "  STALE: stored for head {stored}; the PR head is now {current}\n"
        ));
    }
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        out.push_str(&format!("  probe error: {error}\n"));
    }
    if let Some(files) = value.get("files").and_then(|v| v.as_array()) {
        if files.is_empty() {
            out.push_str("  files: (none)\n");
        }
        for file in files {
            let path = file.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let shape = file.get("shape").and_then(|v| v.as_str()).unwrap_or("?");
            out.push_str(&format!("  - {path} ({shape})\n"));
        }
    }
}

fn render_ci_attempts(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("### CI remediations\n\n");
    if packet.ci_attempts.is_empty() {
        out.push_str("(none)\n\n");
        return;
    }
    for attempt in &packet.ci_attempts {
        out.push_str(&format!(
            "- {id} status={status} kind={kind} failure={failure} checks={checks}\n",
            id = attempt.id,
            status = attempt.status,
            kind = attempt.attempt_kind,
            failure = attempt.failure_reason.as_deref().unwrap_or("(none)"),
            checks = attempt.failed_checks,
        ));
        if let Some(current) = packet.pr_head_sha.as_deref()
            && attempt.head_sha_at_trigger != current
        {
            out.push_str(&format!(
                "  STALE: attempt ran on head {}; the PR head is now {current}\n",
                attempt.head_sha_at_trigger
            ));
        }
        match attempt.log_excerpt.as_deref() {
            Some(excerpt) => {
                out.push_str("  log excerpt:\n```\n");
                out.push_str(excerpt.trim_end());
                out.push_str("\n```\n");
            }
            None => out.push_str("  log excerpt: (not stored; agent may `gh pr checks`)\n"),
        }
    }
    out.push('\n');
}

fn render_check_history(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("### Failing-check history (other open PRs in this product)\n\n");
    out.push_str("Stored cross-PR observations follow. Engine probes below supply recent main-branch outcomes before spawn; missing observations remain unknown.\n\n");
    let failing_here = failing_check_names(packet.ci_required_detail.as_deref());
    if failing_here.is_empty() {
        out.push_str("(no failing required checks recorded on this item)\n\n");
        return;
    }
    let unobserved = packet
        .other_pr_ci
        .iter()
        .filter(|snap| snap.ci_required_detail.is_none())
        .count();
    for check in &failing_here {
        let others: Vec<&str> = packet
            .other_pr_ci
            .iter()
            .filter(|snap| check_failed_on(&snap.ci_required_detail, check))
            .map(|snap| snap.name.as_str())
            .collect();
        if others.is_empty() {
            out.push_str(&format!(
                "- `{check}` failed on this PR; no other PR with stored detail shows it failing \
                 ({unobserved} other PRs have no stored detail), main not in stored snapshot → attribution unknown until engine probe.\n"
            ));
        } else {
            out.push_str(&format!(
                "- `{check}` failed on this PR and on {} other open/recent PRs ({}) → likely unrelated or flaky.\n",
                others.len(),
                others.join(", ")
            ));
        }
    }
    out.push('\n');
}

/// Names of the checks in a stored `ci_required_detail`. The merge poller
/// stores only failing required checks, with GitHub's uppercase conclusion
/// (`FAILURE`, `TIMED_OUT`, `ERROR`, `STARTUP_FAILURE`, …), so a check counts as
/// failing unless its conclusion is one of the pass values.
fn failing_check_names(detail: Option<&str>) -> Vec<String> {
    let Some(detail) = detail else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(detail) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?;
            let conclusion = item.get("conclusion").and_then(|v| v.as_str()).unwrap_or("");
            let passing = matches!(
                conclusion.to_ascii_uppercase().as_str(),
                "SUCCESS" | "NEUTRAL" | "SKIPPED"
            );
            (!passing).then(|| name.to_owned())
        })
        .collect()
}

fn check_failed_on(detail: &Option<String>, check: &str) -> bool {
    failing_check_names(detail.as_deref()).iter().any(|name| name == check)
}

fn render_attentions(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## Open attentions\n\n");
    let open: Vec<_> = packet
        .attentions
        .iter()
        .filter(|(_, item)| item.status == "open")
        .collect();
    if open.is_empty() {
        out.push_str("(none)\n\n");
        return;
    }
    for (owner, item) in open {
        out.push_str(&format!(
            "- {id} on={owner} kind={kind} title={title}\n  {body}\n",
            id = item.id,
            kind = item.kind,
            title = item.title,
            body = item.body_markdown.replace('\n', " "),
        ));
    }
    out.push('\n');
}

fn render_questions(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## Operator questions\n\n");
    if packet.questions.is_empty() {
        out.push_str("(none)\n\n");
        return;
    }
    for question in &packet.questions {
        out.push_str(&format!(
            "- {id} on={owner} status={status:?} {text}\n",
            id = question.question.id,
            owner = question.work_item_id,
            status = question.status,
            text = question.question.text,
        ));
    }
    out.push('\n');
}

fn render_history(out: &mut String, packet: &ProgressionBriefPacket) {
    out.push_str("## Progression history\n\n");
    out.push_str(&format!("Current fingerprint: `{}`\n\n", packet.fingerprint));
    if packet.prior_progression.is_empty() {
        out.push_str("Prior progression runs: (none)\n");
    } else {
        out.push_str("Prior progression runs:\n");
        for run in &packet.prior_progression {
            out.push_str(&format!(
                "- {id} status={status} done={done} error={error}\n",
                id = run.execution_id,
                status = run.status,
                done = run.run_done_outcome.as_deref().unwrap_or("(none)"),
                error = run.last_error.as_deref().unwrap_or("(none)"),
            ));
            out.push_str(&format!(
                "  fingerprint={} matches_current={}\n",
                run.fingerprint.as_deref().unwrap_or("(unrecorded)"),
                run.fingerprint.as_deref() == Some(&packet.fingerprint)
            ));
            for action in &run.actions {
                out.push_str(&format!("  action: {action}\n"));
            }
        }
    }
    out.push_str("Prior fingerprint actions: see applied proposals above.\n\n");
}

fn line(out: &mut String, key: &str, value: &str) {
    out.push_str("- ");
    out.push_str(key);
    out.push_str(": ");
    out.push_str(value);
    out.push('\n');
}
