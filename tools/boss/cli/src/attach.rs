//! `boss attach <path>` / `boss attach --list` — hand a screenshot to the
//! engine as evidence a human reviewer can actually look at.
//!
//! Committing capture PNGs to the branch is forbidden by repo policy (and
//! rots — links against a merged, deleted branch 404). This verb is the
//! alternative: the engine stores the image outside the ephemeral cube
//! workspace and serves it from a loopback gallery, and this command prints
//! the URL to paste into the PR body.
//!
//! Pure CLI + wire glue. Everything that could reject a submission — path
//! confinement, format sniffing, size and dimension caps, per-run and
//! per-work-item counts — is decided engine-side and comes back as the same
//! typed [`FrontendEvent::ProposalRejected`] the proposal verbs use, so a
//! worker fixes and retries mid-run rather than discovering at Stop that its
//! evidence never landed.
//!
//! Design: `tools/boss/docs/designs/worker-screenshot-evidence-attachments.md`.

use std::path::PathBuf;

use boss_protocol::{FrontendEvent, FrontendRequest, WorkAttachment, attachment_gallery_url, attachment_image_url};
use clap::Args;

use crate::{
    CliError, RunContext, connect_for_work, new_dynamic_table, print_entity, print_table, propose, unexpected_event,
};

/// `boss attach <path> [--caption "…"]`, or `boss attach --list`.
///
/// Submission is synchronous: the engine validates and stores the bytes
/// before the command exits, then prints the gallery URL. Idempotent by
/// content — re-attaching the same image inside one run replays the existing
/// record rather than duplicating it, so a retried command is safe.
#[derive(Debug, Clone, Args)]
pub(crate) struct AttachArgs {
    /// Path to a PNG or JPEG to attach. Must be a file this run wrote —
    /// inside your workspace, or a temp dir (e.g. the `--capture-to` target,
    /// or a test's `TEST_UNDECLARED_OUTPUTS_DIR` render).
    #[arg(value_name = "PATH", required_unless_present = "list")]
    path: Option<PathBuf>,

    /// One line describing what the image shows. Strongly encouraged: it is
    /// the caption the gallery renders, and "what am I looking at" is the
    /// reviewer's first question.
    #[arg(long, conflicts_with = "list")]
    caption: Option<String>,

    /// List every attachment stored against your own work item — across all
    /// its executions, including prior runs — instead of attaching one.
    #[arg(long)]
    list: bool,
}

pub(crate) async fn run_attach_command(args: AttachArgs, ctx: &RunContext) -> Result<(), CliError> {
    if args.list {
        return run_attach_list(ctx).await;
    }
    let path = args.path.expect("clap requires PATH unless --list");
    run_attach_submit(ctx, path, args.caption).await
}

/// Absolutise against the *worker's* cwd before sending.
///
/// The engine opens the path itself and its cwd is not the worker's, so a
/// relative path would resolve somewhere neither party meant. Doing it here
/// rather than engine-side keeps the resolution anchored to the process that
/// actually knows what the path was relative to.
fn absolutise(path: PathBuf) -> Result<String, CliError> {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|err| CliError::usage(format!("cannot resolve `{}`: {err}", path.display())))?
            .join(path)
    };
    Ok(absolute.to_string_lossy().into_owned())
}

async fn run_attach_submit(ctx: &RunContext, path: PathBuf, caption: Option<String>) -> Result<(), CliError> {
    let run_id = require_run_id()?;
    let path = absolutise(path)?;

    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::SubmitAttachment { run_id, path, caption })
        .await
        .map_err(|err| {
            CliError::engine_unavailable(format!("boss attach lost the engine connection mid-request: {err}"))
        })?;

    match response {
        FrontendEvent::AttachmentStored {
            attachment,
            already_stored,
            evidence_base_url,
        } => {
            let image_url = evidence_base_url
                .as_deref()
                .map(|base| attachment_image_url(base, &attachment.id));
            let gallery_url = evidence_base_url
                .as_deref()
                .map(|base| attachment_gallery_url(base, &attachment.work_item_id));
            print_entity(
                ctx,
                &serde_json::json!({
                    "attachment": attachment,
                    "already_stored": already_stored,
                    "image_url": image_url,
                    "gallery_url": gallery_url,
                }),
                || {
                    if ctx.quiet {
                        return;
                    }
                    let verb = if already_stored { "already stored" } else { "stored" };
                    println!(
                        "{} {verb} ({}x{}, {} bytes).",
                        attachment.id, attachment.pixel_width, attachment.pixel_height, attachment.size_bytes
                    );
                    match (&image_url, &gallery_url) {
                        (Some(image), Some(gallery)) => {
                            println!("  image:   {image}");
                            println!("  gallery: {gallery}");
                            println!(
                                "Put the gallery link in your PR body under an `## Evidence` heading so the \
                                 reviewer can see it."
                            );
                        }
                        // Stored, but nothing is serving it — say so instead
                        // of printing a URL that would be a dead link in a
                        // PR body forever.
                        _ => println!(
                            "The evidence server is not listening, so there is no link to paste. The image is \
                             stored; an operator can list it with `bossctl attachments list`."
                        ),
                    }
                },
            )
        }
        FrontendEvent::ProposalRejected { error } => Err(propose::render_proposal_rejection(None, error)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attach", &other)),
    }
}

async fn run_attach_list(ctx: &RunContext) -> Result<(), CliError> {
    let run_id = require_run_id()?;
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::ListAttachments { run_id })
        .await
        .map_err(|err| {
            CliError::engine_unavailable(format!("boss attach lost the engine connection mid-request: {err}"))
        })?;

    match response {
        FrontendEvent::AttachmentsList {
            work_item_id,
            attachments,
            evidence_base_url,
        } => print_entity(
            ctx,
            &serde_json::json!({
                "work_item_id": work_item_id,
                "attachments": attachments,
                "gallery_url": evidence_base_url
                    .as_deref()
                    .map(|base| attachment_gallery_url(base, &work_item_id)),
            }),
            || {
                if ctx.quiet {
                    return;
                }
                if attachments.is_empty() {
                    println!("No attachments stored against {work_item_id}.");
                    return;
                }
                print_attachments_table(&attachments, evidence_base_url.as_deref());
                if let Some(base) = evidence_base_url.as_deref() {
                    println!("Gallery: {}", attachment_gallery_url(base, &work_item_id));
                }
            },
        ),
        FrontendEvent::ProposalRejected { error } => Err(propose::render_proposal_rejection(None, error)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attach --list", &other)),
    }
}

fn print_attachments_table(attachments: &[WorkAttachment], evidence_base_url: Option<&str>) {
    let mut table = new_dynamic_table(["ID", "RUN", "CAPTION", "SIZE", "STATE", "URL"]);
    for attachment in attachments {
        let caption = if attachment.caption.is_empty() {
            attachment.source_name.clone()
        } else {
            attachment.caption.clone()
        };
        table.add_row([
            attachment.id.clone(),
            attachment.execution_id.clone(),
            caption,
            format!("{}x{}", attachment.pixel_width, attachment.pixel_height),
            if attachment.is_reclaimed() {
                "reclaimed".to_owned()
            } else {
                "stored".to_owned()
            },
            evidence_base_url
                .map(|base| attachment_image_url(base, &attachment.id))
                .unwrap_or_default(),
        ]);
    }
    print_table(table);
}

fn require_run_id() -> Result<String, CliError> {
    std::env::var("BOSS_RUN_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            CliError::usage("BOSS_RUN_ID is not set — `boss attach` only works inside a Boss worker session.")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Commands};
    use clap::Parser;

    fn parse_attach(args: &[&str]) -> AttachArgs {
        let mut full = vec!["boss", "attach"];
        full.extend_from_slice(args);
        match Cli::parse_from(full).command {
            Commands::Attach(args) => args,
            other => panic!("expected Commands::Attach, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_path_with_a_caption() {
        let args = parse_attach(&["/tmp/shot.png", "--caption", "after the fix"]);
        assert_eq!(args.path.as_deref(), Some(std::path::Path::new("/tmp/shot.png")));
        assert_eq!(args.caption.as_deref(), Some("after the fix"));
        assert!(!args.list);
    }

    #[test]
    fn list_needs_no_path() {
        let args = parse_attach(&["--list"]);
        assert!(args.list);
        assert!(args.path.is_none());
    }

    #[test]
    fn a_bare_invocation_is_a_usage_error() {
        // Neither a path nor --list: the command has nothing to do, and
        // silently succeeding would look like the evidence landed.
        assert!(Cli::try_parse_from(["boss", "attach"]).is_err());
    }

    #[test]
    fn relative_paths_resolve_against_the_workers_cwd() {
        let resolved = absolutise(PathBuf::from("shot.png")).unwrap();
        let expected = std::env::current_dir().unwrap().join("shot.png");
        assert_eq!(resolved, expected.to_string_lossy());
        // Absolute paths pass through untouched.
        assert_eq!(absolutise(PathBuf::from("/tmp/a.png")).unwrap(), "/tmp/a.png");
    }
}
