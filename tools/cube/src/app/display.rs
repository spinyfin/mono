//! Human-readable rendering of workspace rows for the terminal.

use std::path::Path;

use console::{Style, style};

use crate::metadata::{WorkspaceHealth, WorkspaceRecord, WorkspaceState};

/// Returns the human-readable effective status string for a workspace,
/// combining the lease state with the last-known health status. Free
/// workspaces with a recorded health issue show `free-dirty` or
/// `free-conflicted` so operators can see at a glance which slots are
/// usable without `cd`-ing into each one.
pub(super) fn effective_state_display(record: &WorkspaceRecord) -> String {
    match record.state {
        WorkspaceState::Leased => "leased".to_string(),
        WorkspaceState::Free => match record.health_status {
            Some(WorkspaceHealth::Dirty) => "free-dirty".to_string(),
            Some(WorkspaceHealth::Conflicted) => "free-conflicted".to_string(),
            Some(WorkspaceHealth::Quarantined) => "free-quarantined".to_string(),
            _ => "free".to_string(),
        },
    }
}

/// Render a duration the way an operator reads a retention age: `3h`, `2.4d`.
pub(super) fn format_age(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 90 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{:.1}h", secs as f64 / 3600.0)
    } else {
        format!("{:.1}d", secs as f64 / 86_400.0)
    }
}

/// How much of the pool is being withheld, why, and for how long.
///
/// Retention going unbounded is what turned a healthy 173-workspace free pool
/// into an effective free pool of 3 and took dispatch down with it. That is a
/// condition an operator should be able to see coming from `cube workspace
/// list`, so it is summarised inline rather than left to be reconstructed from
/// the audit log after the fact.
#[derive(Debug, Default, Clone, serde::Serialize, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct RetentionSummary {
    /// Free workspaces withheld from leasing because they are unhealthy.
    pub(super) retained: usize,
    /// Of those, the ones held specifically because they carry unpushed work.
    pub(super) unpushed_work_preserved: usize,
    /// Of those, the ones the dirty-reclaim guard quarantined.
    pub(super) quarantined: usize,
    /// Retained longer than the configured TTL — awaiting the next GC pass.
    pub(super) past_ttl: usize,
    /// Age of the oldest retained workspace, in seconds.
    pub(super) oldest_retained_secs: i64,
    /// Free workspaces that are actually available to lease right now.
    pub(super) effective_free: usize,
    pub(super) ttl_secs: i64,
}

pub(super) fn retention_summary(records: &[WorkspaceRecord], now_epoch_s: i64, ttl_secs: i64) -> RetentionSummary {
    let mut summary = RetentionSummary {
        ttl_secs,
        ..Default::default()
    };
    for record in records {
        if record.state != WorkspaceState::Free {
            continue;
        }
        match record.health_status {
            Some(WorkspaceHealth::Dirty) | Some(WorkspaceHealth::Conflicted) | Some(WorkspaceHealth::Quarantined) => {}
            _ => {
                summary.effective_free += 1;
                continue;
            }
        }
        summary.retained += 1;
        if record.health_status == Some(WorkspaceHealth::Quarantined) {
            summary.quarantined += 1;
        }
        if record.last_release_reason.as_deref() == Some("unpushed_work_preserved") {
            summary.unpushed_work_preserved += 1;
        }
        if let Some(since) = record.unhealthy_since_epoch_s {
            let age = now_epoch_s.saturating_sub(since);
            summary.oldest_retained_secs = summary.oldest_retained_secs.max(age);
            if age >= ttl_secs {
                summary.past_ttl += 1;
            }
        }
    }
    summary
}

/// The trailing block `cube workspace list` prints under the rows. Returns
/// `None` when nothing is being withheld — there is no condition to report.
pub(super) fn format_retention_summary(summary: &RetentionSummary) -> Option<String> {
    if summary.retained == 0 {
        return None;
    }
    let dim = Style::new().dim();
    let mut reasons = Vec::new();
    if summary.unpushed_work_preserved > 0 {
        reasons.push(format!("{} holding unpushed work", summary.unpushed_work_preserved));
    }
    if summary.quarantined > 0 {
        reasons.push(format!("{} quarantined", summary.quarantined));
    }
    let reason_text = if reasons.is_empty() {
        String::new()
    } else {
        format!(" ({})", reasons.join(", "))
    };
    let headline = format!(
        "Retention: {} workspace(s) withheld{}, oldest {}; {} free to lease.",
        summary.retained,
        reason_text,
        format_age(summary.oldest_retained_secs),
        summary.effective_free,
    );
    let ttl_line = format!(
        "           TTL {}; {} past it, awaiting the next gc pass. \
         Reclaimed work is salvaged first — see `cube workspace salvage`.",
        format_age(summary.ttl_secs),
        summary.past_ttl,
    );
    let headline = if summary.effective_free == 0 {
        style(headline).red().bold().to_string()
    } else {
        style(headline).yellow().to_string()
    };
    Some(format!("\n{headline}\n{}", dim.apply_to(ttl_line)))
}

pub(super) fn format_workspace_list(records: &[WorkspaceRecord], now_epoch_s: i64) -> String {
    if records.is_empty() {
        return "No workspaces match.".to_string();
    }

    let names: Vec<String> = records
        .iter()
        .map(|r| format!("{}/{}", r.repo, r.workspace_id))
        .collect();
    let paths: Vec<String> = records.iter().map(|r| abbreviate_path(&r.workspace_path)).collect();
    let effective_states: Vec<String> = records.iter().map(effective_state_display).collect();
    let name_w = names.iter().map(|s| s.len()).max().unwrap_or(0);
    let state_w = effective_states.iter().map(|s| s.len()).max().unwrap_or(0);

    let label_w = "holder".len();
    let dim = Style::new().dim();
    let mut lines = Vec::with_capacity(records.len());
    for (((record, name), path), eff_state) in records.iter().zip(&names).zip(&paths).zip(&effective_states) {
        let name_pad = format!("{name:<name_w$}");
        let state_pad = format!("{eff_state:<state_w$}");
        let state_styled = match (record.state, record.health_status) {
            // Quarantined workspaces need an operator decision (`cube
            // workspace force-release`) before they can be leased again —
            // call that out distinctly from ordinary free/dirty/conflicted.
            (WorkspaceState::Free, Some(WorkspaceHealth::Quarantined)) => style(state_pad).red().bold(),
            (WorkspaceState::Free, _) => style(state_pad).green(),
            (WorkspaceState::Leased, _) => style(state_pad).yellow(),
        };
        lines.push(format!(
            "{}  {}  {}",
            style(name_pad).cyan().bold(),
            state_styled,
            dim.apply_to(path),
        ));

        // Why a free workspace is being withheld, and for how long. Without
        // this the operator sees `free-dirty` and has no way to tell a
        // workspace released ten minutes ago from one that has been out of the
        // pool for days.
        if record.state == WorkspaceState::Free
            && let Some(since) = record.unhealthy_since_epoch_s
        {
            let reason = record.last_release_reason.as_deref().unwrap_or("unhealthy");
            lines.push(format!(
                "    {}  {} for {}",
                dim.apply_to(format!("{:<label_w$}", "retained")),
                reason,
                format_age(now_epoch_s.saturating_sub(since)),
            ));
        }

        if record.state == WorkspaceState::Leased {
            if let Some(holder) = &record.holder {
                lines.push(format!(
                    "    {}  {}",
                    dim.apply_to(format!("{:<label_w$}", "holder")),
                    holder,
                ));
            }
            if let Some(task) = &record.task {
                lines.push(format!(
                    "    {}  {}",
                    dim.apply_to(format!("{:<label_w$}", "task")),
                    task,
                ));
            }
            if let Some(lease) = &record.lease_id {
                lines.push(format!(
                    "    {}  {}",
                    dim.apply_to(format!("{:<label_w$}", "lease")),
                    dim.apply_to(lease),
                ));
            }
        }
    }
    lines.join("\n")
}

pub(super) fn human_workspace_detail(record: &crate::metadata::WorkspaceRecord, jj_status: &str) -> String {
    let dim = Style::new().dim();
    let mut lines = vec![
        format!("{} {}", dim.apply_to("repo:"), record.repo),
        format!(
            "{} {}",
            dim.apply_to("workspace_id:"),
            style(&record.workspace_id).cyan().bold(),
        ),
        format!(
            "{} {}",
            dim.apply_to("workspace_path:"),
            abbreviate_path(&record.workspace_path),
        ),
        format!("{} {}", dim.apply_to("state:"), style_state(record.state),),
    ];
    if let Some(lease_id) = &record.lease_id {
        lines.push(format!("{} {}", dim.apply_to("lease_id:"), dim.apply_to(lease_id),));
    }
    if let Some(holder) = &record.holder {
        lines.push(format!("{} {holder}", dim.apply_to("holder:")));
    }
    if let Some(task) = &record.task {
        lines.push(format!("{} {task}", dim.apply_to("task:")));
    }
    if let Some(head_commit) = &record.head_commit {
        lines.push(format!(
            "{} {}",
            dim.apply_to("head_commit:"),
            dim.apply_to(head_commit),
        ));
    }
    lines.push(dim.apply_to("jj_status:").to_string());
    lines.push(jj_status.to_string());
    lines.join("\n")
}

fn style_state(state: WorkspaceState) -> console::StyledObject<&'static str> {
    match state {
        WorkspaceState::Free => style(state.as_str()).green(),
        WorkspaceState::Leased => style(state.as_str()).yellow(),
    }
}

pub(super) fn abbreviate_path(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            if s == home.as_ref() {
                return "~".to_string();
            }
            if let Some(rest) = s.strip_prefix(home.as_ref())
                && rest.starts_with('/')
            {
                return format!("~{rest}");
            }
        }
    }
    s
}
