//! table / detail rendering, formatting, lint, and local commands
//!
//! Extracted from the former monolithic `main.rs` (mechanical split; behavior unchanged).

use crate::*;

/// Parsed `--repo <selector>` filter. Per design Q3 +
/// `tools/boss/docs/designs/multi-repo-work-modeling.md` R10:
///   - reject selectors shorter than 2 chars,
///   - match against the *resolved* repo on every row (task override
///     ?? parent product default), not just the override column,
///   - selectors that look like a full URL match the canonicalised
///     URL exactly (case-insensitive),
///   - otherwise treat the selector as a short name and match as
///     case-insensitive prefix of `short_name_for(url)`.
pub(crate) struct RepoSelector {
    /// Lowercased selector — used for both comparison branches.
    pub(crate) lc: String,
    /// `true` when the selector looks like a full URL (contains a
    /// scheme separator or a `git@…:` prefix). URL form ⇒ exact
    /// case-insensitive match; otherwise short-name prefix match.
    pub(crate) is_url_form: bool,
}

impl RepoSelector {
    pub(crate) fn parse(raw: &str) -> Result<Self, CliError> {
        let trimmed = raw.trim();
        if trimmed.len() < 2 {
            return Err(CliError::usage(
                "--repo selector must be at least 2 characters (avoids spurious short-name matches)",
            ));
        }
        let is_url_form = trimmed.contains("://") || trimmed.starts_with("git@");
        Ok(Self {
            lc: trimmed.to_ascii_lowercase(),
            is_url_form,
        })
    }

    /// Match against an effective repo URL. `None` (work item has no
    /// resolution) never matches — `--repo` is a positive filter.
    pub(crate) fn matches(&self, resolved_url: Option<&str>) -> bool {
        let Some(url) = resolved_url else { return false };
        let lc_url = url.to_ascii_lowercase();
        if self.is_url_form {
            return lc_url == self.lc;
        }
        let short = short_name_for(&lc_url);
        short.starts_with(&self.lc)
    }
}

/// Resolve a task / chore's effective repo: its own override wins;
/// fall back to the product's default. Used by the `--repo` filter
/// so `--repo nimbus` finds inherited matches too (design R10 / Q3).
pub(crate) fn resolved_repo_for_task<'a>(task: &'a Task, product_repo: Option<&'a str>) -> Option<&'a str> {
    task.repo_remote_url.as_deref().or(product_repo)
}

/// Criteria for `apply_task_list_filters`, bundled to keep the function's
/// argument count under clippy's `too_many_arguments` threshold.
///
/// Uses `#[derive(bon::Builder)]` per the repo's giant-struct convention
/// (more than five named fields). The slice/flag fields default to the
/// "no filter" state so callers only set the dimension they narrow.
#[derive(bon::Builder)]
pub(crate) struct TaskListCriteria<'a> {
    #[builder(default)]
    pub(crate) statuses: &'a [TaskStatusArg],
    #[builder(default)]
    pub(crate) priorities: &'a [TaskPriority],
    pub(crate) match_term: Option<&'a str>,
    #[builder(default)]
    pub(crate) ids: &'a [String],
    pub(crate) limit: Option<usize>,
    /// When `false`, `archived` rows are hidden unless `statuses`
    /// explicitly asks for them — mirrors the deleted/restore contract
    /// (hidden by default, visible on request) rather than the
    /// show-everything default other statuses get.
    #[builder(default)]
    pub(crate) include_archived: bool,
}

pub(crate) fn apply_task_list_filters(
    items: Vec<Task>,
    criteria: TaskListCriteria<'_>,
    repo: Option<&RepoSelector>,
    product_repo: Option<&str>,
) -> Vec<Task> {
    let allowed_statuses: Vec<&str> = criteria.statuses.iter().map(|s| s.as_str()).collect();
    let allowed_priorities: Vec<&str> = criteria.priorities.iter().map(|p| p.as_str()).collect();
    let id_set: std::collections::HashSet<&str> = criteria.ids.iter().map(String::as_str).collect();
    let lc_term = criteria.match_term.map(str::to_lowercase);
    let show_archived = criteria.include_archived || allowed_statuses.contains(&"archived");
    items
        .into_iter()
        .filter(|task| {
            if !show_archived && task.status.as_str() == "archived" {
                return false;
            }
            if !allowed_statuses.is_empty() && !allowed_statuses.contains(&task.status.as_str()) {
                return false;
            }
            if !allowed_priorities.is_empty() && !allowed_priorities.contains(&task.priority.as_str()) {
                return false;
            }
            if !id_set.is_empty() && !id_set.contains(task.id.as_str()) {
                return false;
            }
            if let Some(term) = &lc_term {
                let name = task.name.to_lowercase();
                let desc = task.description.to_lowercase();
                if !name.contains(term.as_str()) && !desc.contains(term.as_str()) {
                    return false;
                }
            }
            if let Some(selector) = repo
                && !selector.matches(resolved_repo_for_task(task, product_repo))
            {
                return false;
            }
            true
        })
        .take(criteria.limit.unwrap_or(usize::MAX))
        .collect()
}

pub(crate) fn apply_project_list_filters(
    items: Vec<Project>,
    statuses: &[ProjectStatusArg],
    match_term: Option<&str>,
    ids: &[String],
    limit: Option<usize>,
    repo: Option<&RepoSelector>,
    product_repo: Option<&str>,
) -> Vec<Project> {
    let allowed_statuses: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
    let id_set: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let lc_term = match_term.map(str::to_lowercase);
    items
        .into_iter()
        .filter(|project| {
            if !allowed_statuses.is_empty() && !allowed_statuses.contains(&project.status.as_str()) {
                return false;
            }
            if !id_set.is_empty() && !id_set.contains(project.id.as_str()) {
                return false;
            }
            if let Some(term) = &lc_term {
                let name = project.name.to_lowercase();
                let desc = project.description.to_lowercase();
                if !name.contains(term.as_str()) && !desc.contains(term.as_str()) {
                    return false;
                }
            }
            if let Some(selector) = repo {
                // Projects have no repo column today; they resolve
                // through their parent product, so every project under
                // a given product shares the same effective repo.
                if !selector.matches(product_repo) {
                    return false;
                }
            }
            true
        })
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

pub(crate) fn print_entity<T, F>(ctx: &RunContext, json_value: &T, human: F) -> Result<(), CliError>
where
    T: Serialize,
    F: FnOnce(),
{
    match ctx.output_mode {
        OutputMode::Json => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            serde_json::to_writer_pretty(&mut lock, json_value).map_err(CliError::internal)?;
            writeln!(lock).map_err(CliError::internal)?;
        }
        OutputMode::Human => human(),
    }
    Ok(())
}

pub(crate) fn print_products_table(products: &[Product]) {
    let show_default_model = products.iter().any(|p| p.default_model.is_some());
    let mut header = vec!["ID", "SLUG", "NAME", "STATUS", "REPO"];
    if show_default_model {
        header.push("DEFAULT MODEL");
    }
    let mut table = new_dynamic_table(header);
    for product in products {
        let mut row = vec![
            product.id.as_str(),
            product.slug.as_str(),
            product.name.as_str(),
            product.status.as_str(),
            product.repo_remote_url.as_deref().unwrap_or(""),
        ];
        if show_default_model {
            row.push(product.default_model.as_deref().unwrap_or(""));
        }
        table.add_row(row);
    }
    print_table(table);
}

pub(crate) fn print_projects_table(projects: &[Project], with_primary_id: bool) {
    let show_short_id = projects.iter().any(|p| p.short_id.is_some());
    let mut header: Vec<&str> = Vec::new();
    if show_short_id {
        header.push("#");
    }
    if !show_short_id || with_primary_id {
        header.push("ID");
    }
    header.extend_from_slice(&["SLUG", "NAME", "STATUS", "PRIORITY", "GOAL"]);
    let mut table = new_dynamic_table(header);
    for project in projects {
        let mut row: Vec<String> = Vec::new();
        if show_short_id {
            let friendly = project.short_id.map(|n| format!("P{n}")).unwrap_or_default();
            row.push(friendly);
        }
        if !show_short_id || with_primary_id {
            row.push(project.id.clone());
        }
        row.push(project.slug.clone());
        row.push(project.name.clone());
        row.push(project.status.to_string());
        row.push(project.priority.clone());
        row.push(project.goal.clone());
        table.add_row(row);
    }
    print_table(table);
}

pub(crate) fn print_tasks_table(tasks: &[Task], with_primary_id: bool) {
    // Only render the EFFORT column when at least one row in the
    // view carries a level — keeps the common case (legacy rows)
    // narrow but surfaces the new field as soon as it lands on
    // anything. JSON output always carries the field; this is a
    // human-readability nicety only.
    let show_effort = tasks.iter().any(|t| t.effort_level.is_some());
    let show_short_id = tasks.iter().any(|t| t.short_id.is_some());
    // Surface the soft-delete tombstone only when a row actually carries
    // one — i.e. when the caller passed `--deleted`. Keeps the common
    // live-only listing unchanged. Mirrors the `show_effort` pattern.
    let show_deleted = tasks.iter().any(|t| t.deleted_at.is_some());
    let mut header: Vec<&str> = Vec::new();
    if show_short_id {
        header.push("#");
    }
    if !show_short_id || with_primary_id {
        header.push("ID");
    }
    header.extend_from_slice(&["NAME", "STATUS", "PRIORITY"]);
    if show_effort {
        header.push("EFFORT");
    }
    header.extend_from_slice(&["PROJECT", "ORDINAL", "PR URL"]);
    if show_deleted {
        header.push("DELETED");
    }
    let mut table = new_dynamic_table(header);
    for task in tasks {
        let ordinal = task.ordinal.map(|value| value.to_string()).unwrap_or_default();
        let friendly = boss_protocol::short_id_label(task.short_id).unwrap_or_default();
        let effort_str = task.effort_level.map(|l| l.as_str().to_owned()).unwrap_or_default();
        let mut row: Vec<String> = Vec::new();
        if show_short_id {
            row.push(friendly);
        }
        if !show_short_id || with_primary_id {
            row.push(task.id.clone());
        }
        row.push(task.name.clone());
        row.push(task.status.display_label().to_owned());
        row.push(task.priority.clone());
        if show_effort {
            row.push(effort_str);
        }
        row.push(task.project_id.clone().unwrap_or_default());
        row.push(ordinal);
        row.push(task.pr_url.clone().unwrap_or_default());
        if show_deleted {
            row.push(task.deleted_at.clone().unwrap_or_default());
        }
        table.add_row(row);
    }
    print_table(table);
}

pub(crate) fn print_product_details(title: &str, product: &Product) {
    println!("{title}");
    println!("ID: {}", product.id);
    println!("Name: {}", product.name);
    println!("Slug: {}", product.slug);
    println!("Status: {}", product.status);
    println!("Repo: {}", product.repo_remote_url.as_deref().unwrap_or(""));
    if let Some(design_repo) = product.design_repo.as_deref() {
        println!("Design repo: {design_repo}");
    }
    if let Some(docs_repo) = product.docs_repo.as_deref() {
        println!("Docs repo: {docs_repo}");
    }
    if let Some(prefix) = product.worker_branch_prefix.as_deref() {
        println!("Worker branch prefix: {prefix}");
    }
    if let Some(model) = product.default_model.as_deref() {
        println!("Default model: {model}");
    }
    if let Some(preamble) = product.dispatch_preamble.as_deref() {
        println!("Dispatch preamble: {preamble}");
    }
    if let Some(rules) = product.editorial_rules.as_ref() {
        println!("Editorial rules:");
        let branch_str = match &rules.branch_naming {
            boss_protocol::BranchNaming::BossExecPrefix => "boss-exec-prefix (default)".to_owned(),
            boss_protocol::BranchNaming::OpaqueHash => "opaque-hash".to_owned(),
            boss_protocol::BranchNaming::CustomPrefix { prefix } => {
                format!("custom-prefix ({prefix})")
            }
        };
        println!("  Branch naming: {branch_str}");
        let template_str = match rules.template_policy {
            boss_protocol::TemplatePolicy::Off => "off (default)",
            boss_protocol::TemplatePolicy::Advise => "advise",
            boss_protocol::TemplatePolicy::Enforce => "enforce",
        };
        println!("  Template policy: {template_str}");
        let trailer_str = match rules.commit_trailer_policy {
            boss_protocol::TrailerPolicy::Default => "default",
            boss_protocol::TrailerPolicy::NoAiTrailer => "no-ai-trailer",
        };
        println!("  Commit trailer: {trailer_str}");
        if !rules.redactions.is_empty() {
            println!("  Redactions: {} rule(s)", rules.redactions.len());
        }
        if let Some(instructions) = rules.instructions.as_deref() {
            println!("  Instructions: {instructions}");
        }
        if product.dispatch_preamble.is_some() && rules.instructions.is_some() {
            println!(
                "  [note] Both dispatch_preamble and editorial_rules.instructions are set — consider consolidating into editorial_rules.instructions (R11)."
            );
        }
    }
    if let Some(kind) = product.external_tracker_kind.as_deref() {
        println!("External tracker:");
        println!("  Kind: {kind}");
        if let Some(config) = product.external_tracker_config.as_ref() {
            if kind == "github" {
                if let Some(org) = config["org"].as_str() {
                    println!("  Org: {org}");
                }
                if let Some(repo) = config["repo"].as_str() {
                    println!("  Repo: {repo}");
                }
                if let Some(project_number) = config["project_number"].as_u64() {
                    println!("  Project: {project_number}");
                }
                let reverse_close = config["reverse_close"].as_bool().unwrap_or(false);
                println!("  Reverse-close: {reverse_close}");
            } else {
                println!("  Config: {config}");
            }
        }
    }
    if !product.description.is_empty() {
        println!("Description: {}", product.description);
    }
}

/// Render the trailing portion of the `Repo:` line emitted by `boss
/// <kind> show` — i.e. everything after the `Repo: ` prefix. Mirrors
/// the engine's `resolve_repo_for_work_item`: per-row override wins,
/// otherwise the product default, otherwise "(none — work item cannot
/// dispatch)".
///
/// `override_url` is the work item's own `repo_remote_url` column.
/// Projects always pass `None` since they don't carry their own
/// override column today; the parenthetical "(inherited from product
/// `<slug>`)" is the only non-`none` shape projects can produce.
pub(crate) fn format_repo_line(override_url: Option<&str>, product: &Product) -> String {
    if let Some(url) = override_url.filter(|s| !s.is_empty()) {
        return format!("{url} (override on this work item)");
    }
    if let Some(url) = product.repo_remote_url.as_deref().filter(|s| !s.is_empty()) {
        return format!("{url} (inherited from product `{}`)", product.slug);
    }
    "(none — work item cannot dispatch)".to_owned()
}

pub(crate) fn print_project_details(
    title: &str,
    project: &Project,
    parent_product: Option<&Product>,
    with_primary_id: bool,
) {
    println!("{title}");
    if let Some(n) = project.short_id {
        if with_primary_id {
            println!("P{n}  \x1b[2m{}\x1b[0m", project.id);
        } else {
            println!("P{n}");
        }
    } else {
        println!("ID: {}", project.id);
    }
    println!("Product ID: {}", project.product_id);
    println!("Name: {}", project.name);
    println!("Slug: {}", project.slug);
    println!("Status: {}", project.status);
    if let Some(product) = parent_product {
        // Projects have no per-row override column today, so the
        // override slot is always `None`; the line reduces to the
        // product-inherited or "none" shape.
        println!("Repo: {}", format_repo_line(None, product));
    }
    println!("Priority: {}", project.priority);
    if !project.goal.is_empty() {
        println!("Goal: {}", project.goal);
    }
    if !project.description.is_empty() {
        println!("Description: {}", project.description);
    }
}

/// Severity classification for `boss project lint-design-docs`.
/// `Broken` entries drive the verb's non-zero exit code so the lint
/// is usable from CI; `Missing` / `Unverified` are advisory only and
/// only appear when the matching `--include-…` flag is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LintSeverity {
    /// The resolver returned `Broken`, or the resolved path doesn't
    /// exist on disk in the leased workspace.
    Broken,
    /// No pointer set on the project at all. Advisory; only included
    /// when `--include-missing` is passed.
    Missing,
    /// Resolver returned `Resolved` but no workspace was leased for
    /// the repo, so the file's existence couldn't be confirmed.
    /// Advisory; only included when `--include-unverified` is passed.
    Unverified,
}

/// One row in the `lint-design-docs` report. Carries enough state for
/// the human to act on the finding without re-resolving: project id +
/// slug for identification, product slug for the grouping context,
/// the current pointer fields (so the user can see what's set), the
/// reason the finding fired, and a copy-pasteable `suggested_fix`
/// CLI invocation.
#[derive(bon::Builder, Debug, Clone, Serialize)]
#[builder(on(String, into))]
pub(crate) struct LintDesignDocEntry {
    pub(crate) project_id: String,
    pub(crate) project_slug: String,
    pub(crate) project_name: String,
    pub(crate) product_id: String,
    pub(crate) product_slug: String,
    pub(crate) severity: LintSeverity,
    /// Current `design_doc_path` value on the project row, if any.
    pub(crate) design_doc_path: Option<String>,
    /// Current `design_doc_repo_remote_url` override, if any. `None`
    /// means the project inherits from `product.repo_remote_url`.
    pub(crate) design_doc_repo_remote_url: Option<String>,
    /// Current `design_doc_branch` override, if any. `None` means the
    /// branch falls back to `"main"`.
    pub(crate) design_doc_branch: Option<String>,
    /// Human-readable explanation of why this entry was flagged. The
    /// table renderer prints this verbatim; the JSON form carries it
    /// for programmatic consumers.
    pub(crate) reason: String,
    /// A `boss project ...` invocation the user can run to repair the
    /// finding. For `Broken` / `Missing` it's a `set-design-doc`
    /// template with the project selector pre-filled; the user fills
    /// in the new path. For `Unverified` it's `open-design --print
    /// --web` so the user can manually confirm the doc still exists.
    pub(crate) suggested_fix: String,
}

/// Pure classifier used by `boss project lint-design-docs`. Returns
/// `None` when the project is healthy (or its finding doesn't match
/// the caller's `--include-…` flags); returns `Some(entry)` when the
/// project should appear in the lint report. `file_check` is the
/// filesystem-probe callback (typically [`check_design_doc_file_exists`],
/// stubbed in unit tests).
pub(crate) fn classify_lint_finding<F>(
    product: &Product,
    project: &Project,
    state: Option<&ProjectDesignDocState>,
    file_check: F,
    include_missing: bool,
    include_unverified: bool,
) -> Option<LintDesignDocEntry>
where
    F: FnOnce(&str, &str) -> bool,
{
    let selector = format!("{}/{}", product.slug, project.slug);
    match state {
        None => {
            // `design_doc_path` is NULL — project has no pointer.
            if !include_missing {
                return None;
            }
            Some(LintDesignDocEntry {
                project_id: project.id.clone(),
                project_slug: project.slug.clone(),
                project_name: project.name.clone(),
                product_id: product.id.clone(),
                product_slug: product.slug.clone(),
                severity: LintSeverity::Missing,
                design_doc_path: None,
                design_doc_repo_remote_url: None,
                design_doc_branch: None,
                reason: "no design-doc pointer set".to_owned(),
                suggested_fix: format!("boss project set-design-doc {selector} --path <repo-relative-path>"),
            })
        }
        Some(ProjectDesignDocState::NotSet) => {
            // Should be unreachable when the caller only resolves
            // projects with `design_doc_path` set — but treat it as
            // equivalent to the `None` arm for robustness.
            if !include_missing {
                return None;
            }
            Some(LintDesignDocEntry {
                project_id: project.id.clone(),
                project_slug: project.slug.clone(),
                project_name: project.name.clone(),
                product_id: product.id.clone(),
                product_slug: product.slug.clone(),
                severity: LintSeverity::Missing,
                design_doc_path: None,
                design_doc_repo_remote_url: None,
                design_doc_branch: None,
                reason: "no design-doc pointer set".to_owned(),
                suggested_fix: format!("boss project set-design-doc {selector} --path <repo-relative-path>"),
            })
        }
        Some(ProjectDesignDocState::Broken { reason }) => Some(LintDesignDocEntry {
            project_id: project.id.clone(),
            project_slug: project.slug.clone(),
            project_name: project.name.clone(),
            product_id: product.id.clone(),
            product_slug: product.slug.clone(),
            severity: LintSeverity::Broken,
            design_doc_path: project.design_doc_path.clone(),
            design_doc_repo_remote_url: project.design_doc_repo_remote_url.clone(),
            design_doc_branch: project.design_doc_branch.clone(),
            reason: reason.clone(),
            suggested_fix: format!("boss project set-design-doc {selector} --path <p> --repo <repo-url>"),
        }),
        Some(ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            ..
        }) => match workspace_path.as_deref() {
            Some(workspace) => {
                if file_check(workspace, &resolved.path) {
                    None
                } else {
                    Some(LintDesignDocEntry {
                        project_id: project.id.clone(),
                        project_slug: project.slug.clone(),
                        project_name: project.name.clone(),
                        product_id: product.id.clone(),
                        product_slug: product.slug.clone(),
                        severity: LintSeverity::Broken,
                        design_doc_path: Some(resolved.path.clone()),
                        design_doc_repo_remote_url: project.design_doc_repo_remote_url.clone(),
                        design_doc_branch: project.design_doc_branch.clone(),
                        reason: format!(
                            "file not found at {}/{} (pointer may be stale after a rename)",
                            workspace, resolved.path,
                        ),
                        suggested_fix: format!("boss project set-design-doc {selector} --path <new-path>"),
                    })
                }
            }
            None => {
                if !include_unverified {
                    return None;
                }
                Some(LintDesignDocEntry {
                    project_id: project.id.clone(),
                    project_slug: project.slug.clone(),
                    project_name: project.name.clone(),
                    product_id: product.id.clone(),
                    product_slug: product.slug.clone(),
                    severity: LintSeverity::Unverified,
                    design_doc_path: Some(resolved.path.clone()),
                    design_doc_repo_remote_url: project.design_doc_repo_remote_url.clone(),
                    design_doc_branch: project.design_doc_branch.clone(),
                    reason: format!(
                        "no leased workspace for {} — cannot verify file exists",
                        resolved.repo_remote_url,
                    ),
                    suggested_fix: format!("boss project open-design {selector} --print --web"),
                })
            }
        },
    }
}

/// Filesystem probe used by the real CLI handler — `true` when the
/// resolved doc exists as a regular file inside the leased
/// workspace. Symlinks resolve through; broken symlinks return
/// `false`. The pure classifier takes this as an injectable callback
/// so the unit tests don't have to touch disk.
pub(crate) fn check_design_doc_file_exists(workspace_path: &str, repo_relative_path: &str) -> bool {
    PathBuf::from(workspace_path).join(repo_relative_path).is_file()
}

pub(crate) fn print_lint_design_docs_table(entries: &[LintDesignDocEntry]) {
    if entries.is_empty() {
        println!("No design-doc pointer issues found.");
        return;
    }
    let mut table = new_dynamic_table(vec!["SEVERITY", "PROJECT", "PATH", "REASON"]);
    for entry in entries {
        table.add_row(vec![
            lint_severity_label(entry.severity).to_owned(),
            format!("{}/{}", entry.product_slug, entry.project_slug),
            entry.design_doc_path.clone().unwrap_or_default(),
            entry.reason.clone(),
        ]);
    }
    print_table(table);
    println!();
    println!("Suggested fixes:");
    for entry in entries {
        println!(
            "  [{}] {}/{}: {}",
            lint_severity_label(entry.severity),
            entry.product_slug,
            entry.project_slug,
            entry.suggested_fix,
        );
    }
    println!();
    println!("{}", lint_summary_line(entries));
}

pub(crate) fn print_plan_project_result(result: &PlanProjectResult) {
    println!("{}: {}", result.outcome, result.message);
    if let Some(run_id) = &result.run_id {
        println!("Planner run: {run_id}");
    }
    if let Some(proposal) = &result.proposal {
        print_planner_proposal(proposal);
    }
}

pub(crate) fn print_planner_proposal(proposal: &PlannerOutput) {
    if proposal.tasks.is_empty() {
        return;
    }
    let mut table = new_dynamic_table(vec!["HANDLE", "NAME", "KIND", "EFFORT"]);
    for task in &proposal.tasks {
        table.add_row(vec![
            task.handle.clone(),
            task.name.clone(),
            task.kind.as_str().to_owned(),
            task.effort.as_str().to_owned(),
        ]);
    }
    print_table(table);
    if !proposal.edges.is_empty() {
        println!("Dependency edges:");
        for edge in &proposal.edges {
            println!("  {} depends on {}", edge.dependent, edge.prerequisite);
        }
    }
    if !proposal.merge_order_hints.is_empty() {
        println!("Merge-order hints (soft, do not gate dispatch):");
        for hint in &proposal.merge_order_hints {
            println!("  {} <-> {} ({})", hint.task_a, hint.task_b, hint.reason);
        }
    }
    println!("Confidence: {}", proposal.confidence);
    if !proposal.notes.is_empty() {
        println!("Notes: {}", proposal.notes);
    }
}

pub(crate) fn print_planner_runs_table(runs: &[PlannerRun]) {
    if runs.is_empty() {
        println!("No planner runs recorded for this project.");
        return;
    }
    let mut table = new_dynamic_table(vec!["ID", "CALLER", "OUTCOME", "RESULT", "CREATED_AT"]);
    for run in runs {
        table.add_row(vec![
            run.id.clone(),
            run.caller.clone(),
            run.outcome.clone(),
            run.result_summary.clone().unwrap_or_default(),
            run.created_at.clone(),
        ]);
    }
    print_table(table);
}

pub(crate) fn lint_severity_label(severity: LintSeverity) -> &'static str {
    match severity {
        LintSeverity::Broken => "broken",
        LintSeverity::Missing => "missing",
        LintSeverity::Unverified => "unverified",
    }
}

/// One-line tally of the lint findings, broken down by severity, for
/// the human report footer (the JSON form already carries
/// `broken_count`). Only severities actually present are listed, so a
/// run that surfaces nothing but stale pointers reads "2 finding(s): 2
/// broken" rather than padding the line with zero counts. Callers
/// invoke this only when `entries` is non-empty — the empty case is
/// handled earlier with a dedicated "no issues" message.
pub(crate) fn lint_summary_line(entries: &[LintDesignDocEntry]) -> String {
    let count = |severity| entries.iter().filter(|e| e.severity == severity).count();
    let parts: Vec<String> = [
        (LintSeverity::Broken, "broken"),
        (LintSeverity::Missing, "missing"),
        (LintSeverity::Unverified, "unverified"),
    ]
    .into_iter()
    .filter_map(|(severity, label)| match count(severity) {
        0 => None,
        n => Some(format!("{n} {label}")),
    })
    .collect();
    format!("{} finding(s): {}", entries.len(), parts.join(", "))
}

/// Format the "Design doc:" line appended by `boss project show` /
/// `boss project set-design-doc`. `None` means "no line should be
/// emitted" — used by `Show` so the unset case stays silent rather
/// than printing "Design doc: (not set)" on every project that
/// hasn't been pointed yet. The set / broken cases produce a
/// concrete line so the user can see at a glance which path the doc
/// resolves to and whether the pointer is healthy.
pub(crate) fn format_project_design_doc_line(state: &ProjectDesignDocState) -> Option<String> {
    match state {
        ProjectDesignDocState::NotSet => None,
        ProjectDesignDocState::Resolved { resolved, web_url, .. } => Some(format!("{} ({})", resolved.path, web_url)),
        ProjectDesignDocState::Broken { reason } => Some(format!("(broken) {reason}")),
    }
}

/// What `boss project open-design` should do once the engine has
/// resolved the pointer. Built by [`decide_open_design_action`] from
/// the engine's `ProjectDesignDocState` + the `--web` flag; consumed
/// by the handler to either print or launch the right target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenDesignAction {
    /// Open a local file inside a leased workspace (the
    /// same-product fast path). The path is workspace-relative and
    /// gets joined to whatever cube currently has leased — but the
    /// CLI doesn't talk to cube, so we surface the doc's repo-relative
    /// path and let the editor / opener resolve it from the user's
    /// cwd (i.e. the worker's leased workspace).
    LocalFile { path: PathBuf, web_url: String },
    /// Open the GitHub web URL. Used for `External` pointers, for
    /// `SameProduct`/`OtherProduct` when no workspace is leased, and
    /// whenever `--web` is explicit.
    Web { url: String },
}

impl OpenDesignAction {
    pub(crate) fn human_summary(&self) -> String {
        match self {
            Self::LocalFile { path, .. } => format!("Opening {} in $EDITOR", path.display()),
            Self::Web { url } => format!("Opening {url} in browser"),
        }
    }

    pub(crate) fn as_json(&self) -> serde_json::Value {
        match self {
            Self::LocalFile { path, web_url } => serde_json::json!({
                "kind": "local_file",
                "path": path.to_string_lossy(),
                "web_url": web_url,
            }),
            Self::Web { url } => serde_json::json!({
                "kind": "web",
                "url": url,
            }),
        }
    }

    pub(crate) fn launch(&self) -> Result<(), CliError> {
        match self {
            Self::LocalFile { path, web_url } => match std::env::var_os("EDITOR") {
                Some(editor) => spawn_opener(editor, [path.as_os_str()]),
                None => {
                    eprintln!("warning: $EDITOR not set; falling back to web URL ({web_url})",);
                    spawn_opener_for_url(web_url)
                }
            },
            Self::Web { url } => spawn_opener_for_url(url),
        }
    }
}

pub(crate) fn spawn_opener<I, A>(program: I, args: A) -> Result<(), CliError>
where
    I: AsRef<std::ffi::OsStr>,
    A: IntoIterator,
    A::Item: AsRef<std::ffi::OsStr>,
{
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|err| CliError::internal(anyhow::anyhow!("failed to launch opener: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::internal(anyhow::anyhow!(
            "opener exited with status {status}",
        )))
    }
}

pub(crate) fn spawn_opener_for_url(url: &str) -> Result<(), CliError> {
    #[cfg(target_os = "macos")]
    let program: &str = "open";
    #[cfg(not(target_os = "macos"))]
    let program: &str = "xdg-open";
    spawn_opener(program, [url])
}

/// Decide which open action [`OpenDesignAction`] to take for a
/// resolved pointer. Pure function; unit-tested. Errors when the
/// pointer is `NotSet` (caller error — should not invoke
/// `open-design` on a project without a pointer) or `Broken` (the
/// pointer can't resolve to a target).
pub(crate) fn decide_open_design_action(
    state: &ProjectDesignDocState,
    force_web: bool,
) -> Result<OpenDesignAction, CliError> {
    match state {
        ProjectDesignDocState::NotSet => Err(CliError::not_found(
            "project has no design-doc pointer (set one with `boss project set-design-doc`)",
        )),
        ProjectDesignDocState::Broken { reason } => {
            Err(CliError::conflict(format!("design-doc pointer is broken: {reason}",)))
        }
        ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            web_url,
            ..
        } => {
            if force_web {
                return Ok(OpenDesignAction::Web { url: web_url.clone() });
            }
            let can_use_filesystem = matches!(
                resolved.kind,
                ResolvedDesignDocKind::SameProduct { .. } | ResolvedDesignDocKind::OtherProduct { .. },
            ) && workspace_path.is_some();
            if can_use_filesystem {
                Ok(OpenDesignAction::LocalFile {
                    path: PathBuf::from(&resolved.path),
                    web_url: web_url.clone(),
                })
            } else {
                Ok(OpenDesignAction::Web { url: web_url.clone() })
            }
        }
    }
}

pub(crate) fn print_task_details(title: &str, task: &Task, parent_product: Option<&Product>, with_primary_id: bool) {
    println!("{title}");
    if let Some(n) = task.short_id {
        if with_primary_id {
            println!("T{n}  \x1b[2m{}\x1b[0m", task.id);
        } else {
            println!("T{n}");
        }
    } else {
        println!("ID: {}", task.id);
    }
    println!("Product ID: {}", task.product_id);
    if let Some(project_id) = &task.project_id {
        println!("Project ID: {}", project_id);
    }
    println!("Name: {}", task.name);
    println!("Kind: {}", task.kind);
    println!("Status: {}", task.status.display_label());
    if let Some(reason) = task.archived_reason.as_deref() {
        println!("Archived reason: {reason}");
    }
    if let Some(product) = parent_product {
        println!("Repo: {}", format_repo_line(task.repo_remote_url.as_deref(), product),);
    }
    println!("Priority: {}", task.priority);
    if let Some(level) = task.effort_level {
        println!("Effort: {level}");
    }
    if let Some(model) = task.model_override.as_deref() {
        println!("Model override: {model}");
    }
    println!("Source: {}", task.created_via);
    if let Some(ordinal) = task.ordinal {
        println!("Ordinal: {}", ordinal);
    }
    if let Some(pr_url) = &task.pr_url {
        println!("PR URL: {}", pr_url);
    }
    if !task.description.is_empty() {
        println!("Description: {}", task.description);
    }
}

pub(crate) fn resolve_install_root() -> Result<PathBuf, CliError> {
    if let Ok(root) = std::env::var("BOSS_INSTALL_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| CliError::internal(anyhow::anyhow!("HOME is not set; cannot resolve install root")))?;
    Ok(PathBuf::from(home).join("Applications"))
}

pub(crate) fn resolve_state_root_for_uninstall() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss"))
}

pub(crate) fn confirm_interactive(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y")
}

pub(crate) async fn run_uninstall_command(args: UninstallArgs, flags: &GlobalFlags) -> Result<(), CliError> {
    let install_root = resolve_install_root()?;
    // True when no BOSS_INSTALL_ROOT override is in effect, meaning we are
    // operating on the canonical ~/Applications install. Only in that case
    // should we stop the engine — stopping the default pid file when the
    // caller set a sandbox install root would kill the host engine instead.
    let using_default_install_root = std::env::var("BOSS_INSTALL_ROOT").is_err();
    let app_path = install_root.join("Boss.app");

    if !app_path.exists() {
        if flags.json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "not_installed",
                    "message": "no installed Boss found",
                    "searched": app_path.display().to_string(),
                })
            );
        } else {
            eprintln!("boss uninstall: no installed Boss found at {}", app_path.display());
            eprintln!("If Boss is running from a dev build, uninstall is not applicable.");
        }
        return Err(CliError::internal(anyhow::anyhow!("no installed Boss to uninstall")));
    }

    let state_root = resolve_state_root_for_uninstall();

    if !flags.json {
        println!("This will remove:");
        println!("  {}", app_path.display());
        if args.purge_state
            && let Some(ref state) = state_root
        {
            println!("  {} (--purge-state)", state.display());
        }
    }

    if !args.yes {
        let confirmed = if flags.json {
            true
        } else {
            confirm_interactive("Proceed with uninstall?")
        };
        if !confirmed {
            if flags.json {
                println!(
                    "{}",
                    serde_json::json!({"status": "cancelled", "reason": "user declined"})
                );
            } else {
                println!("uninstall cancelled");
            }
            return Ok(());
        }
    }

    if using_default_install_root {
        let pid_path =
            std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| boss_client::DEFAULT_PID_PATH.to_owned());
        let _ = stop_engine(&pid_path).await;
    } else {
        eprintln!(
            "note: not stopping engine: BOSS_INSTALL_ROOT is set; \
             assuming the caller manages their own engine lifecycle"
        );
    }

    std::fs::remove_dir_all(&app_path)
        .map_err(|e| CliError::internal(anyhow::anyhow!("failed to remove {}: {e}", app_path.display())))?;

    let mut removed = vec![app_path.display().to_string()];

    if args.purge_state
        && let Some(state) = state_root
        && state.exists()
    {
        std::fs::remove_dir_all(&state)
            .map_err(|e| CliError::internal(anyhow::anyhow!("failed to remove {}: {e}", state.display())))?;
        removed.push(state.display().to_string());
    }

    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "status": "uninstalled",
                "removed": removed,
            })
        );
    } else {
        println!("Uninstalled Boss.");
        for path in &removed {
            println!("  removed: {path}");
        }
    }

    Ok(())
}

/// Split a bug-report blob into a `(title, body)` pair.
///
/// The first non-blank line is the title (with a leading `# ` stripped
/// so a markdown H1 also works as the report heading). The remainder of
/// the file — minus the blank lines that immediately follow the title —
/// becomes the body. An empty blob is rejected by the caller; here we
/// just trust the input has at least one non-blank line.
pub(crate) fn split_shake_report(blob: &str) -> Option<(String, String)> {
    let mut lines = blob.lines();
    let title_line = lines.by_ref().find(|line| !line.trim().is_empty())?;
    let title = title_line
        .trim_start()
        .strip_prefix("# ")
        .unwrap_or(title_line)
        .trim()
        .to_owned();
    if title.is_empty() {
        return None;
    }

    let mut body_lines: Vec<&str> = lines.collect();
    while body_lines.first().is_some_and(|line| line.trim().is_empty()) {
        body_lines.remove(0);
    }
    while body_lines.last().is_some_and(|line| line.trim().is_empty()) {
        body_lines.pop();
    }
    let body = body_lines.join("\n");

    Some((title, body))
}

pub(crate) async fn run_shake_command(args: ShakeArgs, flags: &GlobalFlags) -> Result<(), CliError> {
    let blob = if args.file == "-" {
        let mut s = String::new();
        io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| CliError::internal(anyhow::anyhow!("read stdin: {e}")))?;
        s
    } else {
        std::fs::read_to_string(&args.file)
            .map_err(|e| CliError::usage(format!("cannot read bug report {}: {e}", args.file)))?
    };

    let (title, body) = if let Some(explicit_title) = args.title.as_deref() {
        let title = explicit_title.trim();
        if title.is_empty() {
            return Err(CliError::usage("--title cannot be blank".to_owned()));
        }
        (title.to_owned(), blob.trim_end_matches('\n').to_owned())
    } else {
        split_shake_report(&blob).ok_or_else(|| {
            CliError::usage("bug report is empty — need at least one non-blank line for a title".to_owned())
        })?
    };

    if args.dry_run {
        if flags.json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "dry_run",
                    "repo": args.repo,
                    "title": title,
                    "body": body,
                    "labels": args.labels,
                    "github_project": args.github_project,
                })
            );
        } else {
            println!("repo:  {}", args.repo);
            println!("title: {title}");
            if !args.labels.is_empty() {
                println!("labels: {}", args.labels.join(", "));
            }
            if !args.github_project.is_empty() {
                println!("github-project: {}", args.github_project);
            }
            println!("---");
            println!("{body}");
        }
        return Ok(());
    }

    let cfg = github_app::embedded_config().map_err(|e| CliError::application(e.to_string()))?;
    let api_base = std::env::var("BOSS_GITHUB_API_BASE").unwrap_or_else(|_| github_app::DEFAULT_API_BASE.to_owned());

    let issue = github_app::file_issue(&cfg, &api_base, &args.repo, &title, &body, &args.labels)
        .await
        .map_err(|e| CliError::application(format!("{e:#}")))?;

    // Associate the new issue with the configured GitHub Project so the
    // Boss importer (which scopes to that project) can reconcile it.
    // Skip if the caller explicitly passed an empty project node ID.
    if !args.github_project.is_empty() {
        github_app::add_issue_to_project_with_embedded_token(&cfg, &api_base, &args.github_project, &issue.node_id)
            .await
            .map_err(|e| CliError::application(format!("add issue to project: {e:#}")))?;
    }

    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "status": "filed",
                "repo": args.repo,
                "url": issue.html_url,
                "number": issue.number,
                "title": title,
            })
        );
    } else {
        println!(
            "filed issue against {}: {} (#{})",
            args.repo, issue.html_url, issue.number
        );
    }

    Ok(())
}

pub(crate) async fn run_release_command(flags: &GlobalFlags) -> Result<(), CliError> {
    let token = std::env::var("BK_API_TOKEN").map_err(|_| {
        CliError::application(
            "BK_API_TOKEN is not set. See tools/boss/docs/buildkite-release-setup.md \
             for provisioning instructions."
                .to_owned(),
        )
    })?;

    if token.is_empty() {
        return Err(CliError::application(
            "BK_API_TOKEN is set but empty. See tools/boss/docs/buildkite-release-setup.md \
             for provisioning instructions."
                .to_owned(),
        ));
    }

    let api_base = std::env::var("BOSS_BK_API_BASE").unwrap_or_else(|_| buildkite_release::DEFAULT_API_BASE.to_owned());

    let build = buildkite_release::trigger_release_build(&api_base, &token)
        .await
        .map_err(|e| CliError::application(format!("{e:#}")))?;

    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "status": "triggered",
                "build_url": build.web_url,
                "build_number": build.number,
            })
        );
    } else {
        println!("triggered release build #{}: {}", build.number, build.web_url);
    }

    Ok(())
}
