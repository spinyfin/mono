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

pub(super) fn format_workspace_list(records: &[WorkspaceRecord]) -> String {
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
