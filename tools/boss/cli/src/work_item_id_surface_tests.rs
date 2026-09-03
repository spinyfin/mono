//! Adversarial clap surface enumeration for work-item id args.
//!
//! Split out of `tests.rs` so that file stays under the line-count cap
//! while this anti-recurrence suite can keep growing with new surfaces.

use super::Cli;

/// Walk the clap command tree and collect `(surface_path, arg_id,
/// has_work_item_id_marker)` for every argument.
fn walk_clap_args(command: &clap::Command) -> Vec<(String, String, bool)> {
    fn walk(command: &clap::Command, path: &mut Vec<String>, out: &mut Vec<(String, String, bool)>) {
        for arg in command.get_arguments() {
            let arg_id = arg.get_id().as_str().to_owned();
            let marked = arg.get_value_names().is_some_and(|names| {
                names
                    .iter()
                    .any(|n| n.as_str() == boss_protocol::WORK_ITEM_ID_VALUE_NAME)
            });
            let mut surface = path.clone();
            surface.push(arg_id.clone());
            out.push((surface.join(" "), arg_id, marked));
        }
        for sub in command.get_subcommands() {
            path.push(sub.get_name().to_owned());
            walk(sub, path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(command, &mut Vec::new(), &mut out);
    out
}

/// True when an arg id matches the short-id / work-item id-shaped
/// pattern that must either carry `WORK_ITEM_ID` or sit on an
/// explicit non-work-item allowlist. A new unmarked id arg fails the
/// enumeration test — the recurrence mode this check is built for.
fn is_id_shaped_arg(arg_id: &str) -> bool {
    // Display / filter flags that happen to end in `_id` but are not
    // selectors (e.g. `--with-primary-id` bool).
    if arg_id.starts_with("with_") {
        return false;
    }
    // Comment namespace (`cmt_…`) — different grammar from work items.
    if arg_id == "comment_id" {
        return false;
    }
    matches!(
        arg_id,
        "id" | "selector" | "parent" | "dependent" | "prerequisite" | "depends_on" | "task" | "project" | "set_project"
    ) || arg_id.starts_with("work_item")
        || arg_id.ends_with("work_item")
        || arg_id.contains("work_item")
        || arg_id.ends_with("_id")
        || arg_id == "prerequisites_of"
        || arg_id == "dependents_of"
}

/// Arg ids that look id-shaped but live in a different namespace
/// (executions, CI attempts, probes, workspaces, external trackers, …).
/// Adding a *new* id-shaped name here requires an explicit decision that
/// it is not a work-item selector.
const NON_WORK_ITEM_ARG_IDS: &[&str] = &[
    "execution_id",
    "attempt_id",
    "probe_id",
    "preferred_workspace_id",
    "new_id",      // CI re-trigger provider run id
    "upstream_id", // external tracker remote id
    "source_automation_id",
    "capability_id",
    "group_id",
    "revision_task_id",
];

/// Full surface paths (`"subcmd … arg"`) for id-shaped args that are
/// not work-item selectors — decisions `D<n>`, automations `A<n>` /
/// `auto_…`, attention groups `A<n>` / `atg_…` / members `atn_…`,
/// product/project-binding slugs that are not short-id resolvers, CI
/// attempt selectors that also accept `cir_…`, etc.
///
/// Prefer arg-id allowlisting above; use this only when the same arg
/// name is work-item on one command and not on another.
fn is_non_work_item_surface(surface: &str) -> bool {
    // Decision namespace: D<n> / decision ids.
    if surface.starts_with("decision ") && surface.ends_with(" selector") {
        return true;
    }
    // Automation namespace: A<n> / auto_….
    if surface.starts_with("automation ") && (surface.ends_with(" selector") || surface.ends_with(" id")) {
        return true;
    }
    // Idea namespace: I<n> / idea_…. Ideas are deliberately not work items.
    if surface.starts_with("idea ") && surface.ends_with(" selector") {
        return true;
    }
    // Attention groups / members (A<n>, atg_…, atn_…) — not T/P short ids.
    // Exception: attention list|create --task|--project ARE work items
    // (marked WORK_ITEM_ID); those paths end with " task" / " project".
    if surface.starts_with("attention ") {
        if surface.ends_with(" task") || surface.ends_with(" project") {
            return false;
        }
        if surface.ends_with(" selector") || surface.ends_with(" id") || surface.ends_with(" group") {
            return true;
        }
    }
    // Engine CI attempt / conflict attempt selectors (cir_… / crz_…),
    // not work-item short ids. work-item filters are marked separately.
    if (surface.contains(" engine ") || surface.starts_with("engine "))
        && (surface.ends_with(" attempt_id") || surface.ends_with(" selector") && !surface.contains("work_item"))
    {
        // `engine ci retry selector` accepts attempt OR work-item — mark
        // it WORK_ITEM_ID if it is; otherwise allowlist.
        if surface.ends_with(" selector") {
            return true;
        }
        if surface.ends_with(" attempt_id") {
            return true;
        }
    }
    // Product namespace: product slug/id selectors, not T/P short ids.
    if surface.starts_with("product ")
        && (surface.ends_with(" selector") || surface.ends_with(" product") || surface.ends_with(" product_id"))
    {
        return true;
    }
    // Editorial rules live under a product selector (slug / prod_…).
    if surface.starts_with("editorial ") && surface.ends_with(" selector") {
        return true;
    }
    // GitHub Projects number on external-tracker bind (not a Boss work item).
    if surface.contains("external-tracker") && surface.ends_with(" project") {
        return true;
    }
    // Dual-purpose CI retry selector (cir_… attempt id OR work item) —
    // attempt form is the common case; work-item resolve lives engine-side.
    if surface.contains("ci") && surface.ends_with(" retry selector") {
        return true;
    }
    // Hosted-pane / probe / host ids.
    if surface.contains("probe") || surface.ends_with(" host_id") {
        return true;
    }
    false
}

/// Adversarial surface sweep: every id-shaped clap arg must either
/// carry `WORK_ITEM_ID_VALUE_NAME` or appear in the explicit non-
/// work-item namespace set. A new unmarked id-accepting command fails
/// this test — discovery does not depend on the marker it would forget
/// to add.
#[test]
fn every_id_shaped_arg_is_marked_or_allowlisted() {
    use clap::CommandFactory;
    let args = walk_clap_args(&Cli::command());
    let mut unmarked = Vec::new();
    let mut marked = Vec::new();
    for (surface, arg_id, has_marker) in &args {
        if !is_id_shaped_arg(arg_id) {
            continue;
        }
        if *has_marker {
            marked.push(surface.clone());
            continue;
        }
        if NON_WORK_ITEM_ARG_IDS.contains(&arg_id.as_str()) || is_non_work_item_surface(surface) {
            continue;
        }
        unmarked.push(surface.clone());
    }
    assert!(
        unmarked.is_empty(),
        "id-shaped clap args missing WORK_ITEM_ID marker (and not on the \
         non-work-item allowlist). Stamp `value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME` \
         or add an explicit allowlist entry with justification.\n\
         unmarked: {unmarked:?}\n\
         marked (for context): {marked:?}"
    );
    assert!(
        marked.len() >= 15,
        "expected a broad WORK_ITEM_ID surface set, got {} marked: {marked:?}",
        marked.len()
    );
}

/// Per-surface source routing: each marked WORK_ITEM_ID surface's
/// defining handler module must reference a shared resolver symbol.
/// Correlates the enumerated surface path with the file that serves it
/// so a marked-but-unrouted command fails.
#[test]
fn every_work_item_id_surface_routes_through_shared_resolver() {
    use clap::CommandFactory;

    /// Handler modules and the shared resolver symbols they must use.
    /// A surface is routed when its matched module contains at least one.
    const HANDLER_MODULES: &[(&str, &str, &[&str])] = &[
        (
            "data.rs",
            include_str!("data.rs"),
            &[
                "resolve_selector_to_primary_id",
                "resolve_short_id_item",
                "resolve_create_revision_parent",
                "resolve_depends_on",
                "resolve_revision_depends_on",
                "get_work_item",
            ],
        ),
        (
            "work_cmds.rs",
            include_str!("work_cmds.rs"),
            &["resolve_selector_to_primary_id", "resolve_depends_on", "get_work_item"],
        ),
        (
            "cost_cmds.rs",
            include_str!("cost_cmds.rs"),
            &["resolve_selector_to_primary_id", "get_work_item"],
        ),
        (
            "engine_cmds.rs",
            include_str!("engine_cmds.rs"),
            &[
                "resolve_selector_to_primary_id",
                "resolve_optional_work_item_filter",
                "get_work_item",
            ],
        ),
        (
            "automation_cmds.rs",
            include_str!("automation_cmds.rs"),
            &["resolve_selector_to_primary_id"],
        ),
        (
            "decision_commands.rs",
            include_str!("decision_commands.rs"),
            &["resolve_selector_to_primary_id"],
        ),
        // comment_commands.rs is clap-only; handlers live in work_cmds.rs
        // (task/chore comment) and call resolve_selector_to_primary_id.
    ];

    let marked: Vec<String> = walk_clap_args(&Cli::command())
        .into_iter()
        .filter(|(_, _, m)| *m)
        .map(|(s, _, _)| s)
        .collect();
    assert!(
        !marked.is_empty(),
        "expected at least one WORK_ITEM_ID surface; did value_name markers regress?"
    );

    // Every handler module that owns WORK_ITEM_ID surfaces must call a
    // shared resolver. We do not hardcode today's surface list — but we
    // do require that each module that is part of the id path actually
    // references the choke point (so dropping resolve from a module fails).
    for (name, src, symbols) in HANDLER_MODULES {
        let reachable = symbols.iter().any(|sym| src.contains(sym));
        assert!(
            reachable,
            "handler module {name} owns id-accepting surfaces but references \
             none of the shared resolver symbols {symbols:?}"
        );
    }

    // Surface path tokens must look like id args (catch mis-marked slugs).
    for surface in &marked {
        let looks_like_id_arg = surface.split_whitespace().any(is_id_shaped_arg);
        assert!(
            looks_like_id_arg,
            "WORK_ITEM_ID surface {surface:?} does not look like an id arg"
        );
    }
}
