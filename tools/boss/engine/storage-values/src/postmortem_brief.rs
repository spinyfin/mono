use boss_protocol::Project;

/// Compose the postmortem task's `description` — the remit brief that
/// surfaces to the worker via `runner::work_item_details`'s `- details:`
/// block. Lists every PR the review must cover so the worker doesn't have
/// to rediscover the wave from scratch.
pub fn compose_postmortem_brief(project: &Project, merged_prs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Design postmortem for project \"{}\": review the PRs merged since the last postmortem (or since the project began, if this is the first) and update the project's design doc to reflect what actually shipped — decisions that diverged, scope added or dropped, and contracts that evolved during implementation. Also flag any uncompleted work the review surfaces (see the required structured-output section in your instructions below) so the engine can schedule it.\n\n",
        project.name
    ));
    if let Some(path) = project.design_doc_path.as_deref().filter(|p| !p.is_empty()) {
        out.push_str(&format!("Design doc: `{path}`\n"));
    }
    out.push_str("Merged PRs to review:\n");
    for (name, url) in merged_prs {
        out.push_str(&format!("- {name}: {url}\n"));
    }
    out
}
