//! Reproducible, standalone invocation recipe for the six-PR review-guide
//! validation sample (see the design's "Reproducible six-PR sample" and the
//! "Validate the production prompt on the six saved comparisons" task at
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`).
//!
//! This binary drives the exact production collector
//! (`boss_pr_review_sources::collect_pinned_source_packet`) and the exact
//! production prompt/validation crate (`boss_review_guide`) outside the
//! running engine, so the six saved comparisons can be reconstructed from
//! GitHub and re-prompted at a fixed model/effort without a live Boss
//! instance, database, or driver dispatch. It does not call any model
//! itself — that step shells out to the local `codex` CLI with the exact
//! production model/effort override (`-m gpt-6-astra -c
//! model_reasoning_effort=high`), matching
//! `boss_engine_driver::codex::build_codex_command`. See
//! `tools/boss/docs/evaluations/automatic-pr-review-guides/README.md` for
//! the end-to-end recipe this binary is one step of.
//!
//! Subcommands:
//! - `collect --pr-url <url> --base-sha <sha> --head-sha <sha> --out <path>`
//!   Collects an immutable [`SourcePacket`] via the production collector and
//!   writes it as pretty JSON. `merge_base_sha` is always recomputed live
//!   from GitHub (see [`boss_pr_review_sources::collect_pinned_source_packet`]);
//!   it is not a caller-supplied input.
//! - `prompt --packet <path> --git-dir <path> --out <path> [--baseline]`
//!   Renders the exact prompt text a production execution would compose
//!   (`boss_review_guide::render_prompt` +
//!   `boss_review_guide::render_source_context`), or the historical
//!   evaluated-baseline slice with `--baseline`
//!   (`boss_review_guide::render_baseline_prompt`).
//! - `validate --packet <path> --markdown <path>`
//!   Runs the exact production output validator
//!   (`boss_review_guide::validate_guide_output`) against a raw model
//!   response and prints `VALID` or every [`GuideValidationIssue`] found,
//!   with a non-zero exit code on any issue.
//! - `refs --markdown <path> --out <path>`
//!   Extracts every `github.com` reference from a rendered guide
//!   (`boss_review_guide::guide_references`) and writes it as a pretty JSON
//!   array of `{section, href}`, in document order with duplicates removed.
//!   Feeds the navigation/grounding review template — see
//!   `tools/boss/docs/evaluations/automatic-pr-review-guides/README.md`.

use std::path::PathBuf;
use std::{fs, process};

use anyhow::{Context, Result, bail};
use boss_pr_review_sources::{
    PinnedComparison, SourcePacket, SourceSide, collect_pinned_source_packet, validate_pinned_reference,
};
use boss_review_guide::{
    GuideReference, MAX_PROMPT_CHARS, PROMPT_TEMPLATE_SHA256, PROMPT_VERSION, PromptMetadata, guide_references,
    render_baseline_prompt, render_prompt, render_source_context, validate_guide_output_with_resolver,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args).await {
        Ok(()) => process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("corpus-tool: {error:#}");
            process::ExitCode::FAILURE
        }
    }
}

async fn run(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.get(1) else {
        bail!(usage());
    };
    match subcommand.as_str() {
        "collect" => run_collect(&args[2..]).await,
        "prompt" => run_prompt(&args[2..]),
        "validate" => run_validate(&args[2..]),
        "refs" => run_refs(&args[2..]),
        "metadata" => write_json(
            &PathBuf::from(flag(&args[2..], "out")?),
            &serde_json::json!({"prompt_version": PROMPT_VERSION, "prompt_template_sha256": PROMPT_TEMPLATE_SHA256}),
        ),
        other => bail!("unknown subcommand `{other}`\n\n{}", usage()),
    }
}

fn usage() -> String {
    "usage:\n\
     \x20 corpus-tool collect --pr-url <url> --base-sha <sha> --head-sha <sha> --out <path>\n\
     \x20 corpus-tool prompt --packet <path> --git-dir <path> --out <path> [--baseline]\n\
     \x20 corpus-tool validate --packet <path> (--markdown <path> | --body <text>) [--workspace <path>]\n\
     \x20 corpus-tool refs --markdown <path> --out <path>"
        .to_owned()
}

/// Minimal `--flag value` parser — this binary has few enough flags that a
/// dependency on `clap` would outweigh the value; every flag here is
/// required and order-independent.
fn flag(args: &[String], name: &str) -> Result<String> {
    let needle = format!("--{name}");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if *arg == needle {
            return iter
                .next()
                .cloned()
                .with_context(|| format!("--{name} requires a value"));
        }
    }
    bail!("missing required --{name}")
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == &format!("--{name}"))
}

async fn run_collect(args: &[String]) -> Result<()> {
    let pr_url = flag(args, "pr-url")?;
    let base_sha = flag(args, "base-sha")?;
    let head_sha = flag(args, "head-sha")?;
    let out = PathBuf::from(flag(args, "out")?);

    let observed = PinnedComparison { base_sha, head_sha };
    eprintln!("corpus-tool: collecting {pr_url} via the production collector (this hits live GitHub)...");
    let packet = collect_pinned_source_packet(&pr_url, &observed, None)
        .await
        .with_context(|| format!("collect_pinned_source_packet({pr_url})"))?;

    if !packet.is_complete() {
        eprintln!(
            "corpus-tool: WARNING — packet for {pr_url} is not complete ({} omission(s)); see the packet's `omissions` \
             field before treating this as an acceptance case",
            packet.omissions.len()
        );
    }
    write_json(&out, &packet)?;
    eprintln!(
        "corpus-tool: wrote {} ({} file(s), {} omission(s), content hash {})",
        out.display(),
        packet.files.len(),
        packet.omissions.len(),
        packet.content_hash()?,
    );
    Ok(())
}

fn run_prompt(args: &[String]) -> Result<()> {
    let packet_path = PathBuf::from(flag(args, "packet")?);
    let out = PathBuf::from(flag(args, "out")?);
    let baseline = has_flag(args, "baseline");
    let git_dir = flag(args, "git-dir")?;

    let packet = read_packet(&packet_path)?;
    let metadata = PromptMetadata {
        pr_url: &packet.canonical_pr_url,
        repository: &packet.base_repository,
        pr_title: &packet.title,
        base_sha: &packet.merge_base_sha,
        head_sha: &packet.head_sha,
        git_dir: &git_dir,
    };
    // Matches `runner::review_guide_prompt::compose_review_guide_prompt`
    // exactly: the versioned template plus the embedded source context, with
    // no extra system-prompt addenda (WorkerKind::ReviewGuide never receives
    // one — see `boss_engine_driver::codex`).
    let mut prompt = if baseline {
        render_baseline_prompt(&metadata)
    } else {
        render_prompt(&metadata)
    };
    prompt.push_str("\n\n");
    prompt.push_str(&render_source_context(&packet));
    // These pinned GitHub comparisons have no repository-resident screenshot
    // attachments. Match the engine's empty attachment manifest explicitly.
    prompt.push_str("\n### Registered screenshots\n\nNo screenshots are registered for this task or its revisions.\n");

    fs::write(&out, &prompt).with_context(|| format!("write {}", out.display()))?;
    let chars = prompt.chars().count();
    eprintln!(
        "corpus-tool: wrote {} ({} byte(s), {} char(s), {})",
        out.display(),
        prompt.len(),
        chars,
        if baseline {
            "baseline prompt"
        } else {
            "production prompt"
        },
    );
    if chars > MAX_PROMPT_CHARS {
        // Matches boss_review_guide::exceeds_prompt_budget's threshold
        // (checked here against the actual composed string rather than via
        // that helper, since this covers both the production and
        // `--baseline` renderings it doesn't itself distinguish). See
        // MAX_PROMPT_CHARS's doc: retrying an over-budget comparison at the
        // same model/effort/packet always fails identically, so callers
        // should treat this line, not a wasted generation attempt, as the
        // authoritative signal.
        println!("PROMPT_TOO_LARGE chars={chars} budget={MAX_PROMPT_CHARS}");
    }
    Ok(())
}

fn run_validate(args: &[String]) -> Result<()> {
    let packet_path = PathBuf::from(flag(args, "packet")?);
    let packet = read_packet(&packet_path)?;
    let raw = if has_flag(args, "body") {
        flag(args, "body")?
    } else {
        let path = PathBuf::from(flag(args, "markdown")?);
        fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
    };
    let workspace = if has_flag(args, "workspace") {
        Some(flag(args, "workspace")?)
    } else {
        None
    };

    // The same production validator controls structure and pinned revisions.
    // A standalone jj checkout supplies unchanged context too; without one,
    // validation is deliberately limited to the captured packet.
    match validate_guide_output_with_resolver(&raw, &packet, |sha, path, start, end| {
        if let Some(workspace) = &workspace {
            return workspace_range_exists(workspace, sha, path, start, end);
        }
        [SourceSide::Before, SourceSide::After].into_iter().any(|side| {
            let expected = match side {
                SourceSide::Before => &packet.merge_base_sha,
                SourceSide::After => &packet.head_sha,
            };
            sha == expected && validate_pinned_reference(&packet, side, path, start, end).is_ok()
        })
    }) {
        Ok(guide) => {
            println!("VALID ({} byte(s))", guide.markdown.len());
            Ok(())
        }
        Err(issues) => {
            eprintln!("INVALID — {} issue(s):", issues.len());
            for issue in &issues {
                eprintln!("  - {issue}");
            }
            bail!("guide validation failed");
        }
    }
}

fn workspace_range_exists(workspace: &str, sha: &str, path: &str, start: u32, end: u32) -> bool {
    if path.is_empty()
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return false;
    }
    // jj reads the pinned object without snapshotting or changing the checkout.
    // Keep this adapter separate from the engine's cube-backed git transport.
    let Ok(output) = process::Command::new("jj")
        .args(["--ignore-working-copy", "-R", workspace, "file", "show", "-r", sha])
        .arg(PathBuf::from(workspace).join(path))
        .output()
    else {
        return false;
    };
    if !output.status.success() || output.stdout.len() > 16 * 1024 * 1024 {
        return false;
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return false;
    };
    !text.contains('\0')
        && ((start == 0 && end == 0) || (start > 0 && end >= start && end as usize <= text.lines().count()))
}

fn run_refs(args: &[String]) -> Result<()> {
    let markdown_path = PathBuf::from(flag(args, "markdown")?);
    let out = PathBuf::from(flag(args, "out")?);

    let raw = fs::read_to_string(&markdown_path).with_context(|| format!("read {}", markdown_path.display()))?;
    let refs: Vec<GuideReference> = guide_references(&raw);
    write_json(&out, &refs)?;
    eprintln!(
        "corpus-tool: wrote {} ({} reference(s) from {})",
        out.display(),
        refs.len(),
        markdown_path.display(),
    );
    Ok(())
}

fn read_packet(path: &PathBuf) -> Result<SourcePacket> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {} as a SourcePacket", path.display()))
}

fn write_json<T: serde::Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value).context("serialize")?;
    fs::write(path, json).with_context(|| format!("write {}", path.display()))
}
