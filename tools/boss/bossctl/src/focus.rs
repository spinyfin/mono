//! `bossctl dispatch focus set|add|remove|clear|show`: restrict automatic
//! dispatch to a chosen set of projects and products.
//!
//! Focus mode is purely a dispatch-admission gate. Out-of-scope rows stay
//! queued with the `focus_mode` hold reason and nothing about them is
//! rewritten, so clearing focus leaves every row exactly as it was. The
//! engine owns the semantics; this file is the command surface.

use anyhow::{Context, Result, bail};
use boss_protocol::{DispatchFocusAction, DispatchFocusState, FrontendEvent, FrontendRequest};

use super::connect;

#[derive(clap::Subcommand, Debug)]
pub(crate) enum FocusAction {
    /// Restrict automatic dispatch to exactly these projects and products,
    /// replacing any current focus set. Targets are project or product ids,
    /// project short ids (`P<n>`), or product slugs.
    Set {
        #[arg(required = true, value_name = "PROJECT_OR_PRODUCT")]
        targets: Vec<String>,
    },
    /// Add projects or products to the focus set (turning focus mode on if it
    /// was off).
    Add {
        #[arg(required = true, value_name = "PROJECT_OR_PRODUCT")]
        targets: Vec<String>,
    },
    /// Remove projects or products from the focus set. Removing the last one
    /// turns focus mode off.
    Remove {
        #[arg(required = true, value_name = "PROJECT_OR_PRODUCT")]
        targets: Vec<String>,
    },
    /// Empty the focus set and turn focus mode off. Rows it was holding
    /// dispatch normally; nothing was rewritten, so nothing needs undoing.
    Clear,
    /// Print the focus set and how many ready executions it is holding.
    Show,
}

impl FocusAction {
    fn request(self) -> FrontendRequest {
        let (action, targets) = match self {
            FocusAction::Set { targets } => (DispatchFocusAction::Set, targets),
            FocusAction::Add { targets } => (DispatchFocusAction::Add, targets),
            FocusAction::Remove { targets } => (DispatchFocusAction::Remove, targets),
            FocusAction::Clear => (DispatchFocusAction::Clear, Vec::new()),
            FocusAction::Show => return FrontendRequest::GetDispatchFocus,
        };
        FrontendRequest::UpdateDispatchFocus { action, targets }
    }
}

fn state_from_event(event: FrontendEvent, what: &str) -> Result<DispatchFocusState> {
    match event {
        FrontendEvent::DispatchFocusResult { focus } => Ok(focus),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected {what}: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// The focus state as the engine reports it.
pub(crate) async fn get_focus(socket_path: &Option<String>) -> Result<DispatchFocusState> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetDispatchFocus)
        .await
        .context("sending GetDispatchFocus")?;
    state_from_event(response, "GetDispatchFocus")
}

/// Human-readable lines for a focus state, shared by every surface that
/// prints it (`focus show`, `dispatch state`, `state`).
pub(crate) fn focus_lines(focus: &DispatchFocusState) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(err) = &focus.restore_error {
        lines.push(format!(
            "WARNING: the persisted focus set could not be loaded at engine start, so dispatch started \
             unrestricted ({err}). Re-apply it with `bossctl dispatch focus set`."
        ));
    }
    if !focus.active {
        lines.push("focus: off — every project and product is in scope".to_owned());
        return lines;
    }
    lines.push(format!(
        "focus: on — automatic dispatch is restricted to {} target(s); rows outside it are held with reason `{}`",
        focus.targets.len(),
        boss_protocol::DISPATCH_WAIT_REASON_FOCUS_MODE,
    ));
    for target in &focus.targets {
        let short = target.short_id.map(|n| format!("P{n} ")).unwrap_or_default();
        let name = if target.name.is_empty() {
            "(no longer exists)"
        } else {
            target.name.as_str()
        };
        lines.push(format!("  {} {short}{name} ({})", target.kind.as_str(), target.id));
    }
    lines.push(format!("  held by focus: {} ready execution(s)", focus.held_count));
    lines
}

pub(crate) fn focus_json(focus: &DispatchFocusState) -> serde_json::Value {
    serde_json::to_value(focus).unwrap_or(serde_json::Value::Null)
}

/// What `bossctl dispatch diagnose` reports about focus mode for the
/// executions it is looking at. Read straight from `state.db`, so it works
/// with the engine down, like the rest of `diagnose`.
pub(crate) struct FocusDiagnosis {
    pub(crate) focus: boss_protocol::DispatchFocus,
    pub(crate) targets: Vec<boss_protocol::DispatchFocusTarget>,
    /// `(execution id, work item id)` of each in-scope-ness verdict for a
    /// `ready` execution: `true` when focus mode is holding it.
    pub(crate) ready: Vec<(String, String, bool)>,
}

impl FocusDiagnosis {
    pub(crate) fn load(db: &boss_engine::work::WorkDb, execution_ids: &[String]) -> Result<Self> {
        let focus = db.load_dispatch_focus()?;
        let targets = db.describe_dispatch_focus(&focus)?;
        let mut ready = Vec::new();
        let mut executions = Vec::new();
        for id in execution_ids {
            if let Ok(execution) = db.get_execution(id)
                && execution.status == boss_protocol::ExecutionStatus::Ready
            {
                executions.push(execution);
            }
        }
        let held = db.executions_outside_dispatch_focus(&executions, &focus)?;
        for execution in executions {
            let is_held = held.contains(&execution.id);
            ready.push((execution.id, execution.work_item_id, is_held));
        }
        Ok(Self { focus, targets, ready })
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "active": self.focus.is_active(),
            "targets": self.targets,
            "ready_executions": self.ready.iter().map(|(execution_id, work_item_id, held)| serde_json::json!({
                "execution_id": execution_id,
                "work_item_id": work_item_id,
                "held_by_focus": held,
                "hold_reason": held.then_some(boss_protocol::DISPATCH_WAIT_REASON_FOCUS_MODE),
            })).collect::<Vec<_>>(),
        })
    }

    pub(crate) fn lines(&self) -> Vec<String> {
        let state = DispatchFocusState {
            active: self.focus.is_active(),
            targets: self.targets.clone(),
            held_count: self.ready.iter().filter(|(_, _, held)| *held).count(),
            restore_error: None,
        };
        let mut lines = focus_lines(&state);
        // The held count above is scoped to the executions diagnosed, not
        // the whole queue; say so rather than let it read as fleet-wide.
        if state.active {
            lines.pop();
        }
        for (execution_id, work_item_id, held) in &self.ready {
            if *held {
                lines.push(format!(
                    "  HELD by focus mode: {execution_id} ({work_item_id}) is outside the focus set — \
                     wait reason `{}`; it dispatches once focus is cleared or its project/product is added \
                     (`bossctl agents launch {execution_id}` bypasses focus)",
                    boss_protocol::DISPATCH_WAIT_REASON_FOCUS_MODE,
                ));
            } else if state.active {
                lines.push(format!(
                    "  in focus: {execution_id} ({work_item_id}) is in scope and dispatches normally"
                ));
            }
        }
        lines
    }
}

pub(crate) async fn run(socket_path: &Option<String>, json: bool, action: FocusAction) -> Result<()> {
    let shows = matches!(action, FocusAction::Show);
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&action.request())
        .await
        .context("sending dispatch focus request")?;
    let focus = state_from_event(
        response,
        if shows {
            "GetDispatchFocus"
        } else {
            "UpdateDispatchFocus"
        },
    )?;
    if json {
        println!("{}", focus_json(&focus));
    } else {
        for line in focus_lines(&focus) {
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use boss_protocol::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, CreateProjectInput, DispatchFocus,
        DispatchFocusTarget, DispatchFocusTargetKind, ExecutionKind, ExecutionStatus,
    };
    use clap::Parser;

    use super::*;
    use crate::{Cli, Command, DispatchAction};

    fn parse(args: &[&str]) -> FocusAction {
        let mut argv = vec!["bossctl", "dispatch", "focus"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Dispatch {
                action: DispatchAction::Focus { action },
            } => action,
            other => panic!("expected dispatch focus, got {other:?}"),
        }
    }

    #[test]
    fn set_add_and_remove_take_one_or_more_targets_and_clear_and_show_take_none() {
        match parse(&["set", "P12", "boss"]).request() {
            FrontendRequest::UpdateDispatchFocus { action, targets } => {
                assert_eq!(action, DispatchFocusAction::Set);
                assert_eq!(targets, vec!["P12", "boss"]);
            }
            other => panic!("unexpected request {other:?}"),
        }
        assert!(matches!(
            parse(&["add", "proj_1"]).request(),
            FrontendRequest::UpdateDispatchFocus {
                action: DispatchFocusAction::Add,
                ..
            }
        ));
        assert!(matches!(
            parse(&["remove", "proj_1"]).request(),
            FrontendRequest::UpdateDispatchFocus {
                action: DispatchFocusAction::Remove,
                ..
            }
        ));
        match parse(&["clear"]).request() {
            FrontendRequest::UpdateDispatchFocus { action, targets } => {
                assert_eq!(action, DispatchFocusAction::Clear);
                assert!(targets.is_empty());
            }
            other => panic!("unexpected request {other:?}"),
        }
        assert!(matches!(parse(&["show"]).request(), FrontendRequest::GetDispatchFocus));
        for verb in ["set", "add", "remove"] {
            assert!(
                Cli::try_parse_from(["bossctl", "dispatch", "focus", verb]).is_err(),
                "`{verb}` without a target must fail clap parsing"
            );
        }
    }

    #[test]
    fn lines_name_the_targets_the_hold_reason_and_the_held_count() {
        let off = DispatchFocusState {
            active: false,
            targets: Vec::new(),
            held_count: 0,
            restore_error: None,
        };
        let mut failed = off.clone();
        failed.restore_error = Some("db locked".to_owned());
        assert!(
            focus_lines(&failed)[0].starts_with("WARNING:"),
            "{:?}",
            focus_lines(&failed)
        );
        assert!(focus_lines(&failed)[0].contains("db locked"));
        assert_eq!(focus_lines(&off).len(), 1);
        assert!(focus_lines(&off)[0].starts_with("focus: off"));

        let on = DispatchFocusState {
            active: true,
            targets: vec![
                DispatchFocusTarget {
                    kind: DispatchFocusTargetKind::Project,
                    id: "proj_1".into(),
                    short_id: Some(12),
                    name: "Focus mode".into(),
                },
                DispatchFocusTarget {
                    kind: DispatchFocusTargetKind::Product,
                    id: "prod_1".into(),
                    short_id: None,
                    name: "boss".into(),
                },
            ],
            held_count: 7,
            restore_error: None,
        };
        let text = focus_lines(&on).join("\n");
        assert!(text.contains("focus: on"), "{text}");
        assert!(text.contains("`focus_mode`"), "the hold reason is named: {text}");
        assert!(text.contains("project P12 Focus mode (proj_1)"), "{text}");
        assert!(text.contains("product boss (prod_1)"), "{text}");
        assert!(text.contains("held by focus: 7"), "{text}");
        assert_eq!(focus_json(&on)["held_count"], 7);
    }

    #[test]
    fn diagnose_says_which_ready_executions_focus_is_holding() {
        let db = boss_engine::work::WorkDb::open(":memory:".into()).unwrap();
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("diagnose-focus")
                    .repo_remote_url("git@github.com:test/diagnose-focus.git")
                    .build(),
            )
            .unwrap();
        let focused = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Focused".into(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: true,
                design_reasoning_effort_xhigh: false,
            })
            .unwrap();
        let in_scope = db
            .create_task(
                boss_protocol::CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(focused.id.clone())
                    .name("In scope")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let outside = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Outside")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let ready = |work_item_id: &str, kind| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .kind(kind)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap()
            .id
        };
        let in_scope_exec = ready(&in_scope.id, ExecutionKind::TaskImplementation);
        let outside_exec = ready(&outside.id, ExecutionKind::ChoreImplementation);
        let ids = vec![in_scope_exec.clone(), outside_exec.clone()];

        // Focus off: nothing is held and nothing is printed about it.
        let off = FocusDiagnosis::load(&db, &ids).unwrap();
        assert!(off.ready.iter().all(|(_, _, held)| !held));

        let mut focus = DispatchFocus::default();
        focus.insert(DispatchFocusTargetKind::Project, focused.id.clone());
        db.save_dispatch_focus(&focus).unwrap();
        let on = FocusDiagnosis::load(&db, &ids).unwrap();
        let text = on.lines().join("\n");
        assert!(text.contains("focus: on"), "{text}");
        assert!(text.contains(&format!("HELD by focus mode: {outside_exec}")), "{text}");
        assert!(text.contains(&format!("in focus: {in_scope_exec}")), "{text}");
        let json = on.to_json();
        assert_eq!(json["active"], true);
        let held: Vec<_> = json["ready_executions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["held_by_focus"] == true)
            .collect();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0]["execution_id"], outside_exec.as_str());
        assert_eq!(held[0]["hold_reason"], "focus_mode");
    }
}
