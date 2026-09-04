//! `boss idea …` handlers: markdown drafts over the engine's `ideas` table
//! (Create/List/Show/Update/Delete/Graduate).

use crate::*;

// ---------------------------------------------------------------------------
// Selector resolution
// ---------------------------------------------------------------------------

/// Parsed form of an idea selector.
#[derive(Debug)]
pub(crate) enum IdeaSelector {
    /// `idea_…` canonical id — used directly without a product lookup.
    PrimaryId(String),
    /// `I<n>` / `i<n>` (or plain integer) — short id within a product.
    ShortId(i64),
}

pub(crate) fn parse_idea_selector(s: &str) -> Result<IdeaSelector, CliError> {
    let s = s.trim();
    if s.starts_with("idea_") {
        return Ok(IdeaSelector::PrimaryId(s.to_owned()));
    }
    // `I<n>` or `i<n>`
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'I' || first == b'i')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return Ok(IdeaSelector::ShortId(n));
        }
    }
    // Plain positive integer → short id
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return Ok(IdeaSelector::ShortId(n));
    }
    Err(CliError::usage(format!(
        "idea selector must be I<n> (e.g. I1) or an idea_… id; got {s:?}"
    )))
}

/// Resolve an idea selector to a full `Idea` row.
///
/// For `idea_…` ids the product is not needed. For `I<n>` selectors a
/// product must be provided (resolved by the caller beforehand). There is
/// no wire RPC for short-id lookup, so `I<n>` resolves by listing the
/// product's ideas (including graduated/archived, so `show`/`update` of a
/// non-draft idea still works).
pub(crate) async fn resolve_idea(
    client: &mut BossClient,
    selector: &str,
    product: Option<&Product>,
) -> Result<Idea, CliError> {
    match parse_idea_selector(selector)? {
        IdeaSelector::PrimaryId(id) => get_idea(client, &id).await,
        IdeaSelector::ShortId(n) => {
            let product = product
                .ok_or_else(|| CliError::usage("I<n> selectors require --product to identify the idea namespace"))?;
            let ideas = list_ideas(client, &product.id, None).await?;
            ideas
                .into_iter()
                .find(|i| i.short_id == Some(n))
                .ok_or_else(|| CliError::not_found(format!("no idea I{n} found in product '{}'", product.slug)))
        }
    }
}

/// Resolve `--body` / `--body-file` (both optional) into `Option<String>`.
/// `None` when neither flag was given.
fn resolve_optional_body(text: Option<String>, file: Option<PathBuf>) -> Result<Option<String>, CliError> {
    match (text, file) {
        (Some(text), None) => Ok(Some(text)),
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|err| CliError::usage(format!("could not read --body-file {}: {err}", path.display()))),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => unreachable!("clap conflicts_with rules out --body and --body-file together"),
    }
}

// ---------------------------------------------------------------------------
// RPC helpers
// ---------------------------------------------------------------------------

pub(crate) async fn create_idea(client: &mut BossClient, input: CreateIdeaInput) -> Result<Idea, CliError> {
    rpc_call!(
        client,
        FrontendRequest::CreateIdea { input },
        "idea create",
        FrontendEvent::IdeaCreated { idea } => idea,
    )
}

pub(crate) async fn list_ideas(
    client: &mut BossClient,
    product_id: &str,
    status: Option<boss_protocol::IdeaStatus>,
) -> Result<Vec<Idea>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListIdeas {
            product_id: product_id.to_owned(),
            status,
        },
        "idea list",
        FrontendEvent::IdeasList { ideas, .. } => ideas,
    )
}

pub(crate) async fn get_idea(client: &mut BossClient, id: &str) -> Result<Idea, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetIdea { id: id.to_owned() },
        "idea show",
        FrontendEvent::IdeaResult { idea } => idea,
    )
}

pub(crate) async fn update_idea(client: &mut BossClient, id: &str, patch: IdeaPatch) -> Result<Idea, CliError> {
    rpc_call!(
        client,
        FrontendRequest::UpdateIdea { id: id.to_owned(), patch },
        "idea update",
        FrontendEvent::IdeaUpdated { idea } => idea,
    )
}

pub(crate) async fn delete_idea(client: &mut BossClient, id: &str) -> Result<(), CliError> {
    rpc_call!(
        client,
        FrontendRequest::DeleteIdea { id: id.to_owned() },
        "idea delete",
        FrontendEvent::IdeaDeleted { .. } => (),
    )
}

pub(crate) struct IdeaGraduationOutcome {
    pub(crate) idea: Idea,
    pub(crate) chore: Option<Box<Task>>,
    pub(crate) project: Option<Box<Project>>,
}

pub(crate) async fn graduate_idea(
    client: &mut BossClient,
    id: &str,
    target: boss_protocol::IdeaGraduationKind,
    name: Option<String>,
    effort_level: Option<boss_protocol::EffortLevel>,
    reasoning: Option<boss_protocol::ReasoningMode>,
) -> Result<IdeaGraduationOutcome, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GraduateIdea {
            id: id.to_owned(),
            target,
            name,
            effort_level,
            reasoning,
        },
        "idea graduate",
        FrontendEvent::IdeaGraduated { idea, chore, project } => IdeaGraduationOutcome { idea, chore, project },
    )
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

pub(crate) fn print_idea_details(label: &str, idea: &Idea) {
    println!("{label}:");
    println!("  ID:            {} ({})", idea.id, idea.display_label());
    println!("  Product:       {}", idea.product_id);
    println!("  Status:        {}", idea.status);
    println!("  Name:          {}", idea.name);
    println!("  Body:\n{}", idea.body);
    if let Some(graduated_to_id) = &idea.graduated_to_id {
        println!("  Graduated to:  {graduated_to_id}");
    }
    println!("  Created via:   {}", idea.created_via);
    println!("  Created:       {}", idea.created_at);
    println!("  Updated:       {}", idea.updated_at);
}

pub(crate) fn print_ideas_table(ideas: &[Idea]) {
    let mut table = new_dynamic_table(["ID", "STATUS", "NAME", "CREATED"]);
    for idea in ideas {
        table.add_row([
            idea.display_label(),
            idea.status.to_string(),
            idea.name.clone(),
            idea.created_at.clone(),
        ]);
    }
    print_table(table);
}

// ---------------------------------------------------------------------------
// Command handler
// ---------------------------------------------------------------------------

pub(crate) async fn run_idea_command(command: IdeaCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        IdeaCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Idea name", ctx)?;
            let body = resolve_optional_body(args.body, args.body_file)?;
            let idea = create_idea(
                &mut client,
                CreateIdeaInput::builder()
                    .product_id(product.id)
                    .name(name)
                    .maybe_body(body)
                    .created_via(CREATED_VIA_CLI)
                    .build(),
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "idea": idea }), || {
                print_idea_details("Created idea", &idea);
            })
        }

        IdeaCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let status = args.status.map(IdeaStatusArg::as_protocol);
            let ideas = list_ideas(&mut client, &product.id, status).await?;
            print_entity(ctx, &serde_json::json!({ "ideas": ideas }), || {
                if ideas.is_empty() {
                    println!("No ideas for product '{}'.", product.slug);
                } else {
                    print_ideas_table(&ideas);
                }
            })
        }

        IdeaCommand::Show(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let idea = resolve_idea(&mut client, &args.selector, product.as_ref()).await?;
            // Flat row object — no `{idea: …}` wrapper (matches task/chore/decision show).
            print_entity(ctx, &idea, || {
                print_idea_details("Idea", &idea);
            })
        }

        IdeaCommand::Update(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let idea = resolve_idea(&mut client, &args.selector, product.as_ref()).await?;
            let body = resolve_optional_body(args.body, args.body_file)?;
            let updated = update_idea(
                &mut client,
                &idea.id,
                IdeaPatch::builder()
                    .maybe_name(normalize_non_empty(args.name))
                    .maybe_body(body)
                    .build(),
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "idea": updated }), || {
                print_idea_details("Updated idea", &updated);
            })
        }

        IdeaCommand::Delete(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let idea = resolve_idea(&mut client, &args.selector, product.as_ref()).await?;
            delete_idea(&mut client, &idea.id).await?;
            print_entity(ctx, &serde_json::json!({ "deleted_idea_id": idea.id }), || {
                if !ctx.quiet {
                    println!("Deleted idea {}", idea.id);
                }
            })
        }

        IdeaCommand::Graduate(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let idea = resolve_idea(&mut client, &args.selector, product.as_ref()).await?;
            let target = args.target.as_protocol();
            if target == boss_protocol::IdeaGraduationKind::Project
                && (args.effort.is_some() || args.reasoning.is_some())
            {
                return Err(CliError::usage(
                    "--effort / --reasoning only apply with --as chore, not --as project",
                ));
            }
            let outcome = graduate_idea(
                &mut client,
                &idea.id,
                target,
                normalize_non_empty(args.name),
                args.effort.map(EffortLevelArg::into),
                args.reasoning.map(ReasoningArg::into),
            )
            .await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "idea": outcome.idea,
                    "chore": outcome.chore,
                    "project": outcome.project,
                }),
                || {
                    print_idea_details("Graduated idea", &outcome.idea);
                    if let Some(chore) = &outcome.chore {
                        println!("  -> chore {} ({})", chore.id, chore.short_label());
                    }
                    if let Some(project) = &outcome.project {
                        println!("  -> project {} ({})", project.id, project.slug);
                    }
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_idea_selector_accepts_primary_short_and_bare_forms() {
        match parse_idea_selector("idea_18c55").unwrap() {
            IdeaSelector::PrimaryId(id) => assert_eq!(id, "idea_18c55"),
            _ => panic!("expected primary id"),
        }
        match parse_idea_selector("I12").unwrap() {
            IdeaSelector::ShortId(n) => assert_eq!(n, 12),
            _ => panic!("expected short id"),
        }
        match parse_idea_selector("i3").unwrap() {
            IdeaSelector::ShortId(n) => assert_eq!(n, 3),
            _ => panic!("expected short id"),
        }
        match parse_idea_selector("7").unwrap() {
            IdeaSelector::ShortId(n) => assert_eq!(n, 7),
            _ => panic!("expected bare integer short id"),
        }
        assert!(parse_idea_selector("I0").is_err());
        assert!(parse_idea_selector("0").is_err());
        assert!(parse_idea_selector("-1").is_err());
        assert!(parse_idea_selector("not-an-idea").is_err());
    }

    #[test]
    fn parses_idea_create_command() {
        let cli = Cli::parse_from([
            "boss",
            "idea",
            "create",
            "--product",
            "boss",
            "--name",
            "A draft",
            "--body",
            "# Hello",
        ]);
        match cli.command {
            Commands::Idea {
                command: IdeaCommand::Create(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(args.name.as_deref(), Some("A draft"));
                assert_eq!(args.body.as_deref(), Some("# Hello"));
                assert!(args.body_file.is_none());
            }
            _ => panic!("expected idea create command"),
        }
    }

    #[test]
    fn parses_idea_list_show_update_delete() {
        let list = Cli::parse_from(["boss", "idea", "list", "--product", "boss", "--status", "draft"]);
        match list.command {
            Commands::Idea {
                command: IdeaCommand::List(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(args.status, Some(IdeaStatusArg::Draft));
            }
            _ => panic!("expected idea list"),
        }

        let show = Cli::parse_from(["boss", "idea", "show", "I1", "--product", "boss"]);
        match show.command {
            Commands::Idea {
                command: IdeaCommand::Show(args),
            } => {
                assert_eq!(args.selector, "I1");
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected idea show"),
        }

        let update = Cli::parse_from([
            "boss", "idea", "update", "idea_abc", "--name", "Renamed", "--body", "new body",
        ]);
        match update.command {
            Commands::Idea {
                command: IdeaCommand::Update(args),
            } => {
                assert_eq!(args.selector, "idea_abc");
                assert_eq!(args.name.as_deref(), Some("Renamed"));
                assert_eq!(args.body.as_deref(), Some("new body"));
            }
            _ => panic!("expected idea update"),
        }

        let delete = Cli::parse_from(["boss", "idea", "delete", "idea_abc"]);
        match delete.command {
            Commands::Idea {
                command: IdeaCommand::Delete(args),
            } => {
                assert_eq!(args.selector, "idea_abc");
                assert!(args.product.is_none());
            }
            _ => panic!("expected idea delete"),
        }
    }

    #[test]
    fn parses_idea_graduate_command() {
        let cli = Cli::parse_from([
            "boss",
            "idea",
            "graduate",
            "I1",
            "--product",
            "boss",
            "--as",
            "chore",
            "--name",
            "Shipped",
            "--effort",
            "small",
            "--reasoning",
            "investigation",
        ]);
        match cli.command {
            Commands::Idea {
                command: IdeaCommand::Graduate(args),
            } => {
                assert_eq!(args.selector, "I1");
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(args.target, IdeaGraduateAsArg::Chore);
                assert_eq!(args.name.as_deref(), Some("Shipped"));
                assert_eq!(args.effort, Some(EffortLevelArg::Small));
                assert_eq!(args.reasoning, Some(ReasoningArg::Investigation));
            }
            _ => panic!("expected idea graduate"),
        }

        let as_project = Cli::parse_from(["boss", "idea", "graduate", "I2", "--as", "project"]);
        match as_project.command {
            Commands::Idea {
                command: IdeaCommand::Graduate(args),
            } => {
                assert_eq!(args.target, IdeaGraduateAsArg::Project);
                assert!(args.effort.is_none());
            }
            _ => panic!("expected idea graduate as project"),
        }
    }
}
