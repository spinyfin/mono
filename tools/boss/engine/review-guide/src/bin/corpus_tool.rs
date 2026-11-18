//! Standalone CLI for the review-guide prompt evaluation corpus.
//! See tools/boss/docs/evaluations/automatic-pr-review-guides/README.md.

use std::path::PathBuf;
use std::{fs, process};

use anyhow::{Context, Result, bail};
use boss_pr_review_sources::{
    PinnedComparison, SourcePacket, SourceSide, collect_pinned_source_packet, validate_pinned_reference,
};
use boss_review_guide::{
    GuideContext, GuideReference, PROMPT_TEMPLATE_SHA256, PROMPT_VERSION, PromptMetadata, RootTask, compose_prompt,
    guide_references, linked_range_ok, render_prompt, valid_linked_path, validate_guide_output_with_resolver,
};

/// Historical four-section request, preserved independently of production revisions.
const BASELINE_PROMPT: &str = r#"I want you to provide me a guided summary of the changes in {{PR_URL}}. The summary should break down as:

1. a general overview of the problem being solved.
2. a general overview of the core fix / implementation.
3. a runthrough of major changes to logic and architecture in the change, with a primary focus on the core change that fixed the problem / implemented the solution.
4. a summary of what tests were added, and what test infrastructure was modified to support it.

This is meant to function as a human guide to code review, so it should reference and include code snippets, but not giant diffs.

Make the core fix concrete with one worked example. Give the input and relevant state, trace the decisive old and new behavior, and show the observable result. Choose an example supported by the implementation or tests; label invented inputs as illustrative. Include a contrasting boundary or failure case only when it helps explain the changed contract. For changes without a runtime behavior, use an equivalent concrete before/after scenario. Check every step against the actual code."#;

/// Render the historical baseline with the current pinned-workspace access context.
fn render_baseline_prompt(metadata: &PromptMetadata<'_>) -> Result<String> {
    let full = render_prompt(metadata);
    let context = full
        .split_once("\n\nReview context:")
        .context("production template lacks review context")?
        .1;
    let context = context
        .split_once("\n\nGround the guide")
        .context("production template lacks grounding boundary")?
        .0;
    Ok(format!(
        "{}\n\nReview context:{context}",
        BASELINE_PROMPT.replace("{{PR_URL}}", metadata.pr_url)
    ))
}

const MAX_PROMPT_CHARS: usize = 1_000_000;

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
    // Pinned PR metadata substitutes for unavailable historical engine records.
    let context = GuideContext::builder()
        .root(
            RootTask::builder()
                .name(&packet.title)
                .maybe_description(packet.body.clone())
                .build(),
        )
        .build();
    let template = if baseline {
        render_baseline_prompt(&metadata)?
    } else {
        render_prompt(&metadata)
    };
    let prompt = compose_prompt(
        template,
        &packet,
        &context,
        "No screenshots are registered for this task or its revisions.\n",
    );

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
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) || !valid_linked_path(path) {
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
    output.status.success() && linked_range_ok(path, start, end, &output.stdout)
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
