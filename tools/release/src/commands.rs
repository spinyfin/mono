//! Stateful command orchestration for the release CLI.
//!
//! The resolver and decision modules deliberately remain deterministic. This
//! module is their narrow boundary to the checkout, GitHub CLI, and Buildkite
//! metadata service.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use git_changelog::{
    ChangelogRenderer, ExtractionConfig, GithubMarkdownRenderer, derive_paths_from_project, extract_changelog,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir, tempdir};

use crate::draft_claim::{DraftClaim, strip_claim};
use crate::draft_recovery::{
    HeadDraftOutcome, RecoveryContext, abandon_stale_drafts, fresh_claim, handle_head_draft, refresh_claim,
    resolve_family_drafts,
};
use crate::version::matching_tag_counter;
use crate::{
    Command, CommandOutput, CommandRunner, GitHubRelease, NotesSource, ReleaseConfig, SkipDecision, Trigger,
    assert_allowed_trigger, compute_next_version, query_release_state, should_skip,
};

/// Buildkite's per-build key that passes a release tag from `prepare` to the
/// build and publish phases. Pipelines are isolated, so one fixed key is safe.
pub const BUILDKITE_TAG_KEY: &str = "release-tag";

const UPLOAD_ATTEMPTS: usize = 3;
const VERIFY_DOWNLOAD_ATTEMPTS: usize = 3;
const VERSION_PLACEHOLDER: &str = "{version}";

/// Time every verification download gets before any allowance for its size.
const DOWNLOAD_BASE_TIMEOUT: Duration = Duration::from_secs(120);
/// The slowest sustained rate a verification download may run at, 1 Mbit/s,
/// before it is treated as stalled and tried again on a new connection.
const DOWNLOAD_MIN_BYTES_PER_SECOND: u64 = 125_000;

/// The visible result of a prepare invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    Created {
        tag: String,
    },
    Resumed {
        tag: String,
    },
    /// An unpublished draft for this commit exists, and the run that claimed
    /// it may still be building it. This run did nothing to it.
    InProgress {
        tag: String,
    },
    Skipped,
}

/// What `prepare` is told about the run that invoked it.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrepareRequest<'a> {
    /// Buildkite's source for the build: `schedule`, `ui` or `api`.
    pub trigger_source: &'a str,
    pub buildkite_job_id: Option<&'a str>,
    /// A link to the CI build. It is recorded on the draft so that a later run
    /// can say which run left the draft behind.
    pub build_url: Option<&'a str>,
    /// Set from `RELEASE_RESUME_DRAFT=1`: resume a draft for this commit from a
    /// scheduled build, whatever its claim and however many scheduled attempts
    /// it has used.
    pub resume_draft: bool,
}

/// A release asset supplied by a product-specific build phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub source: PathBuf,
}

impl Asset {
    /// Parses the CLI's `<release-name>=<local-path>` syntax.
    pub fn parse(value: &str) -> Result<Self> {
        let (name, source) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("asset {value:?} must use <name>=<path>"))?;
        validate_asset_name(name)?;
        if source.is_empty() {
            bail!("asset {name:?} has an empty source path");
        }
        Ok(Self {
            name: name.to_owned(),
            source: PathBuf::from(source),
        })
    }
}

/// Loads and validates the release manifest passed to every subcommand.
pub fn load_config(path: &Path) -> Result<ReleaseConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("could not read release config {}", path.display()))?;
    ReleaseConfig::parse(&contents).with_context(|| format!("invalid release config {}", path.display()))
}

/// Resolves a relative config from Bazel's workspace when the CLI is launched
/// through `bazel run`; direct `bin/release` invocations retain their current
/// working-directory behavior.
pub fn resolve_config_path(path: &Path, bazel_workspace: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    bazel_workspace.map_or_else(|| path.to_path_buf(), |workspace| workspace.join(path))
}

/// Performs the prepare phase: resolve state, optionally create the tag and
/// draft, then hand the tag to later Buildkite phases.
///
/// An unpublished draft for this commit is resumed, waited on, or refused, and
/// never passed over. Drafts for older commits are deleted once this run has a
/// release of its own, so a run that skips leaves them for the next run that
/// releases. `draft_recovery` holds the rules.
pub fn prepare(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    request: PrepareRequest<'_>,
) -> Result<PrepareOutcome> {
    let trigger = assert_allowed_trigger(request.trigger_source)?;
    let buildkite_job_id = request.buildkite_job_id;
    fetch_tags(runner)?;

    let repo_root = resolve_repo_root(runner)?;
    let manifest_contents = read_manifest(config, &repo_root)?;
    let state = query_release_state(runner, config, manifest_contents.as_deref())?;
    let head_sha = head_sha(runner)?;
    let last_published_sha = resolve_tag_sha(runner, config, state.last.published.as_ref())?;
    let drafts = resolve_family_drafts(runner, config, manifest_contents.as_deref(), &state.releases)?;
    let recovery = RecoveryContext {
        runner,
        config,
        buildkite_job_id,
        build_url: request.build_url,
        head_sha: &head_sha,
        now: unix_now(runner),
    };

    // Drafts are ordered from the highest counter down, so this finds the
    // newest draft for the commit being released.
    if let Some(draft) = drafts
        .iter()
        .find(|draft| draft.commit.as_deref() == Some(head_sha.as_str()))
    {
        let tag = draft.release.tag_name.clone();
        return match handle_head_draft(&recovery, draft, trigger, request.resume_draft)? {
            HeadDraftOutcome::LeftInProgress => Ok(PrepareOutcome::InProgress { tag }),
            HeadDraftOutcome::Resumed => {
                record_tag(runner, buildkite_job_id, &tag)?;
                abandon_stale_drafts(&recovery, &drafts, &state.releases, draft.counter, &tag);
                Ok(PrepareOutcome::Resumed { tag })
            }
        };
    }

    let changed_paths =
        if trigger == Trigger::Schedule && state.last.published.is_some() && last_published_sha.is_some() {
            changed_paths(runner, last_published_sha.as_deref().expect("checked above"), &head_sha)?
        } else {
            Vec::new()
        };
    if let SkipDecision::Skip(reason) = should_skip(crate::SkipInput {
        trigger,
        head_sha: &head_sha,
        last_published: state.last.published.as_ref(),
        last_published_sha: last_published_sha.as_deref(),
        change_paths: &config.change_paths,
        changed_paths: &changed_paths,
    }) {
        eprintln!("release skipped: {reason}");
        return Ok(PrepareOutcome::Skipped);
    }

    let next = compute_next_version(
        &config.version,
        &config.tag_prefix,
        manifest_contents.as_deref(),
        &state.remote_tags,
    )?;
    create_draft(
        runner,
        DraftRequest {
            config,
            repo_root: &repo_root,
            notes_start_tag: state.last.published.as_ref().map(|release| release.tag_name.as_str()),
            head_sha: &head_sha,
            tag: &next.tag,
            version: &next.version,
            buildkite_job_id,
            claim: &fresh_claim(&recovery),
        },
    )?;

    // Only now that the new tag is on the remote is it safe to delete older
    // ones: the next version is always computed from the highest remote tag.
    if let Some(counter) = matching_tag_counter(
        &config.version,
        &config.tag_prefix,
        manifest_contents.as_deref(),
        &next.tag,
    )? {
        abandon_stale_drafts(&recovery, &drafts, &state.releases, counter, &next.tag);
    }

    Ok(PrepareOutcome::Created { tag: next.tag })
}

/// Reports on the build page that `phase` failed for `tag`, whether the
/// release is still an unpublished draft, and what will happen to it next.
/// Does nothing outside Buildkite, where the error on stderr is the whole
/// report.
pub fn report_unpublished(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    buildkite_job_id: Option<&str>,
    tag: &str,
    phase: &str,
    error: &anyhow::Error,
) {
    crate::draft_recovery::report_unpublished(runner, config, buildkite_job_id, tag, phase, error);
}

/// Reads a tag written by `prepare`.
///
/// Returns `Ok(None)` when prepare recorded nothing (it skipped or did not
/// run) — that is a normal scheduled-run outcome, not an error. A missing
/// Buildkite job remains a hard failure: without a job id there is no
/// meta-data key to read.
pub fn read_tag(runner: &impl CommandRunner, buildkite_job_id: Option<&str>) -> Result<Option<String>> {
    let Some(job_id) = nonempty(buildkite_job_id) else {
        bail!("no release tag is available outside a Buildkite job");
    };
    let output = run_checked(
        runner,
        Command::new(
            "buildkite-agent",
            [
                "meta-data".to_owned(),
                "get".to_owned(),
                BUILDKITE_TAG_KEY.to_owned(),
                "--default".to_owned(),
                String::new(),
            ],
        ),
    )
    .with_context(|| format!("could not read {BUILDKITE_TAG_KEY} for Buildkite job {job_id}"))?;
    let tag = output.stdout.trim();
    if tag.is_empty() {
        return Ok(None);
    }
    Ok(Some(tag.to_owned()))
}

/// Stages and uploads built assets and checksum sidecars for one draft release.
pub fn upload(runner: &impl CommandRunner, config: &ReleaseConfig, tag: &str, assets: &[Asset]) -> Result<()> {
    if tag.is_empty() {
        bail!("release tag must not be empty");
    }
    if assets.is_empty() {
        bail!("at least one --asset is required");
    }
    let assets = expand_upload_assets(config, tag, assets)?;
    validate_upload_assets(config, tag, &assets)?;

    // `--clobber` deletes an asset before it uploads the replacement. On a
    // published release that would take a verified asset away from users, so
    // uploads only ever go to a draft. A published release here means another
    // run finished this version while this one was still building.
    let view = view_release(runner, config, tag)
        .with_context(|| format!("could not read release {tag} before uploading to it"))?;
    if !view.is_draft {
        bail!("release {tag} is already published; not replacing the assets of a published release");
    }
    keep_claim_fresh(runner, config, tag, &view);

    let stage = stage_assets(&assets)?;
    let mut args = vec!["release".to_owned(), "upload".to_owned(), tag.to_owned()];
    for asset in assets {
        args.push(stage.path().join(&asset.name).display().to_string());
        args.push(
            stage
                .path()
                .join(format!("{}.sha256", asset.name))
                .display()
                .to_string(),
        );
    }
    args.extend(["--repo".to_owned(), config.repo.clone(), "--clobber".to_owned()]);

    let command = Command::new("gh", args);
    retry_command(runner, &command, UPLOAD_ATTEMPTS, "asset upload", || {})
}

/// Verifies the remote assets of a draft release, then publishes it.
pub fn publish(runner: &impl CommandRunner, config: &ReleaseConfig, tag: &str) -> Result<()> {
    if tag.is_empty() {
        bail!("release tag must not be empty");
    }
    let view = view_release(runner, config, tag)
        .with_context(|| format!("could not list assets for {tag}; leaving the release draft"))?;
    if view.is_draft {
        keep_claim_fresh(runner, config, tag, &view);
    }
    let remote_assets = view
        .assets
        .iter()
        .map(|asset| (asset.name.as_str(), asset))
        .collect::<HashMap<_, _>>();

    let required_assets = expand_asset_list(&config.required_assets, tag, &config.tag_prefix)?;
    let optional_assets = expand_asset_list(&config.optional_assets, tag, &config.tag_prefix)?;

    let mut missing = Vec::new();
    for asset in &required_assets {
        for required_name in [asset.as_str(), &format!("{asset}.sha256")] {
            if !remote_assets.contains_key(required_name) {
                missing.push(required_name.to_owned());
            }
        }
    }
    if !missing.is_empty() {
        bail!(
            "release {tag} is missing required assets: {}; leaving the release draft",
            missing.join(", ")
        );
    }

    let mut to_verify = required_assets.clone();
    for asset in &optional_assets {
        if remote_assets.contains_key(asset.as_str()) {
            let checksum = format!("{asset}.sha256");
            if !remote_assets.contains_key(checksum.as_str()) {
                bail!("optional asset {asset} is present without {checksum}; leaving the release draft");
            }
            to_verify.push(asset.clone());
        }
    }

    let stage = tempdir().context("could not create asset verification directory")?;
    for asset in &to_verify {
        let size = remote_assets.get(asset.as_str()).map_or(0, |asset| asset.size);
        verify_remote_asset(runner, config, tag, stage.path(), asset, size)?;
    }

    // Verification can take minutes. Read the release again, so that what is
    // published is what was verified and the notes written back are the ones
    // the release holds now. Another run that resumed this draft would have
    // replaced an asset in the meantime, which gives it a new identity.
    let latest = view_release(runner, config, tag).with_context(|| {
        format!("verification passed, but {tag} could not be read again before publishing; leaving the release draft")
    })?;
    // Every configured asset, required or optional, including those that were
    // absent when verification started. An optional file uploaded during the
    // download would otherwise be published without a checksum check.
    for asset in required_assets.iter().chain(optional_assets.iter()) {
        for name in [asset.clone(), format!("{asset}.sha256")] {
            let verified = remote_assets.get(name.as_str()).copied();
            let current = latest.assets.iter().find(|asset| asset.name == name);
            if verified != current {
                bail!(
                    "{name} on {tag} changed while it was being verified, so another run is uploading to this release; leaving the release draft"
                );
            }
        }
    }

    let mut args = vec![
        "release".to_owned(),
        "edit".to_owned(),
        tag.to_owned(),
        "--repo".to_owned(),
        config.repo.clone(),
        "--draft=false".to_owned(),
    ];
    // The claim in a draft's notes is bookkeeping for release runs. It is
    // removed in the same edit that publishes, so it never becomes public and
    // a failed edit leaves the draft exactly as it was.
    let mut notes_file = None;
    if let Some(notes) = latest.body.as_deref().and_then(strip_claim) {
        let mut file = NamedTempFile::new().context("could not create release-notes file")?;
        file.write_all(notes.as_bytes())
            .context("could not write release notes")?;
        args.extend(["--notes-file".to_owned(), file.path().display().to_string()]);
        notes_file = Some(file);
    }
    let result = run_checked(runner, Command::new("gh", args));
    drop(notes_file);
    result.with_context(|| format!("verification passed, but publishing {tag} failed; the release remains a draft"))?;
    Ok(())
}

/// What `gh release view` reports about a release, draft or published.
#[derive(Debug, Deserialize)]
pub(crate) struct ReleaseView {
    #[serde(default)]
    pub assets: Vec<ReleaseViewAsset>,
    #[serde(default)]
    pub body: Option<String>,
    /// The release's address in the REST API, which ends in its id.
    #[serde(rename = "apiUrl")]
    pub api_url: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
}

impl ReleaseView {
    /// The id the REST API addresses the release by, or `0` when the address
    /// does not end in one. It is read from `apiUrl` because every `gh` that
    /// can print a release as JSON reports that field, while `databaseId`
    /// only exists in newer versions.
    pub(crate) fn release_id(&self) -> u64 {
        self.api_url
            .rsplit('/')
            .next()
            .and_then(|id| id.parse().ok())
            .unwrap_or_default()
    }
}

/// One uploaded asset. Replacing an asset gives it a new `id`, so two views
/// that agree on every field here are looking at the same upload.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct ReleaseViewAsset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: Option<String>,
}

/// Reads a release by tag. Unlike GitHub's lookup by tag, this finds drafts.
pub(crate) fn view_release(runner: &impl CommandRunner, config: &ReleaseConfig, tag: &str) -> Result<ReleaseView> {
    let output = retry_checked(
        runner,
        &Command::new(
            "gh",
            [
                "release".to_owned(),
                "view".to_owned(),
                tag.to_owned(),
                "--repo".to_owned(),
                config.repo.clone(),
                "--json".to_owned(),
                "apiUrl,assets,body,isDraft".to_owned(),
            ],
        ),
        UPLOAD_ATTEMPTS,
        "release view",
        || {},
    )?;
    serde_json::from_str(&output.stdout)
        .with_context(|| format!("could not read GitHub's description of release {tag}"))
}

/// Dates the claim on a draft to now, so other runs can see that this one is
/// working on it and neither resume nor abandon it meanwhile.
///
/// This is not allowed to fail the phase. A draft whose claim could not be
/// dated is no worse protected than before the attempt, and the upload or
/// verification that follows is the work that matters.
fn keep_claim_fresh(runner: &impl CommandRunner, config: &ReleaseConfig, tag: &str, view: &ReleaseView) {
    if let Err(error) = refresh_claim(
        runner,
        &config.repo,
        view.release_id(),
        tag,
        view.body.as_deref().unwrap_or_default(),
        unix_now(runner),
    ) {
        eprintln!(
            "warning: could not record that release {tag} is being worked on; another run may take it for abandoned: {error:#}"
        );
    }
}

fn unix_now(runner: &impl CommandRunner) -> u64 {
    runner
        .now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn fetch_tags(runner: &impl CommandRunner) -> Result<()> {
    run_checked(
        runner,
        Command::new("git", ["fetch", "--tags", "--prune", "--prune-tags", "origin"]),
    )
    .context("git fetch --tags failed; refusing to resolve a release from stale tags")?;
    Ok(())
}

/// Resolves the git work-tree root so manifest and changelog paths are stable
/// regardless of the process working directory.
fn resolve_repo_root(runner: &impl CommandRunner) -> Result<PathBuf> {
    let output = run_checked(runner, Command::new("git", ["rev-parse", "--show-toplevel"]))
        .context("could not resolve repository root via git rev-parse --show-toplevel")?;
    let root = PathBuf::from(output.stdout.trim());
    if root.as_os_str().is_empty() {
        bail!("git rev-parse --show-toplevel returned an empty path");
    }
    root.canonicalize()
        .with_context(|| format!("could not resolve repository root {}", root.display()))
}

fn read_manifest(config: &ReleaseConfig, repo_root: &Path) -> Result<Option<String>> {
    config
        .version
        .manifest
        .as_ref()
        .map(|path| {
            let path = repo_root.join(path);
            fs::read_to_string(&path).with_context(|| format!("could not read version manifest {}", path.display()))
        })
        .transpose()
}

fn head_sha(runner: &impl CommandRunner) -> Result<String> {
    let output = run_checked(runner, Command::new("git", ["rev-parse", "HEAD"]))?;
    let sha = output.stdout.trim();
    if sha.is_empty() {
        bail!("git rev-parse HEAD returned an empty commit SHA");
    }
    Ok(sha.to_owned())
}

pub(crate) fn resolve_tag_sha(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    release: Option<&GitHubRelease>,
) -> Result<Option<String>> {
    let Some(release) = release else {
        return Ok(None);
    };
    let local = Command::new("git", ["rev-list", "-n", "1", release.tag_name.as_str()]);
    if let Ok(output) = runner.run(&local)
        && output.success
        && !output.stdout.trim().is_empty()
    {
        return Ok(Some(output.stdout.trim().to_owned()));
    }

    let remote = Command::new(
        "gh",
        [
            "api".to_owned(),
            format!("repos/{}/commits/{}", config.repo, release.tag_name),
            "--jq".to_owned(),
            ".sha".to_owned(),
        ],
    );
    if let Ok(output) = runner.run(&remote)
        && output.success
        && !output.stdout.trim().is_empty()
    {
        return Ok(Some(output.stdout.trim().to_owned()));
    }
    Ok(None)
}

/// Fetches the rest of the history when the checkout is shallow, so that
/// comparisons between commits see every commit between them.
pub(crate) fn ensure_full_history(runner: &impl CommandRunner, purpose: &str) -> Result<()> {
    let shallow = run_checked(runner, Command::new("git", ["rev-parse", "--is-shallow-repository"]))?;
    if shallow.stdout.trim() == "true" {
        run_checked(runner, Command::new("git", ["fetch", "--unshallow", "origin"]))
            .with_context(|| format!("could not unshallow checkout before {purpose}"))?;
    }
    Ok(())
}

fn changed_paths(runner: &impl CommandRunner, from: &str, head: &str) -> Result<Vec<String>> {
    ensure_full_history(runner, "release change detection")?;
    let output = run_checked(
        runner,
        Command::new("git", ["diff", "--name-only", &format!("{from}..{head}")]),
    )?;
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

struct DraftRequest<'a> {
    config: &'a ReleaseConfig,
    repo_root: &'a Path,
    notes_start_tag: Option<&'a str>,
    head_sha: &'a str,
    tag: &'a str,
    version: &'a str,
    buildkite_job_id: Option<&'a str>,
    /// This run's claim on the draft, stored in its notes from the start.
    claim: &'a DraftClaim,
}

fn create_draft(runner: &impl CommandRunner, request: DraftRequest<'_>) -> Result<()> {
    fetch_tags(runner)?;
    let collision = runner
        .run(&Command::new(
            "git",
            ["rev-parse", "--verify", &format!("refs/tags/{}", request.tag)],
        ))
        .context("could not check for a release-tag collision")?;
    if collision.success {
        bail!(
            "computed tag {} already exists after re-fetching tags; refusing to overwrite it",
            request.tag
        );
    }

    let mut local_tag_created = false;
    let mut tag_pushed = false;
    let result = (|| {
        run_checked(runner, Command::new("git", ["tag", request.tag, request.head_sha]))?;
        local_tag_created = true;
        run_checked(
            runner,
            Command::new("git", ["push", "origin", &format!("refs/tags/{}", request.tag)]),
        )?;
        tag_pushed = true;

        create_release(
            runner,
            request.config,
            request.repo_root,
            request.notes_start_tag,
            request.tag,
            request.version,
            request.claim,
        )?;
        tag_pushed = false;
        record_tag(runner, request.buildkite_job_id, request.tag)?;
        Ok(())
    })();
    if result.is_err() {
        cleanup_tag(runner, request.tag, local_tag_created, tag_pushed);
    }
    result
}

fn create_release(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    repo_root: &Path,
    notes_start_tag: Option<&str>,
    tag: &str,
    version: &str,
    claim: &DraftClaim,
) -> Result<()> {
    let mut args = vec![
        "release".to_owned(),
        "create".to_owned(),
        tag.to_owned(),
        "--repo".to_owned(),
        config.repo.clone(),
        "--draft".to_owned(),
        "--title".to_owned(),
        format!("{} {version}", config.title_prefix),
    ];
    let mut notes_file = None;
    match config.notes.source {
        NotesSource::Changelog => {
            let notes = claim.write_into(&changelog_notes(runner, config, repo_root, notes_start_tag, tag)?);
            let mut file = NamedTempFile::new().context("could not create release-notes file")?;
            file.write_all(notes.as_bytes())
                .context("could not write release notes")?;
            args.extend(["--notes-file".to_owned(), file.path().display().to_string()]);
            notes_file = Some(file);
        }
        NotesSource::GithubGenerated => {
            args.extend(github_generated_notes_args(
                &config.title_prefix,
                notes_start_tag,
                claim,
            ));
        }
    }
    let result = run_checked(runner, Command::new("gh", args));
    drop(notes_file);
    result.context("could not create draft GitHub release")?;
    Ok(())
}

/// CLI args that bound GitHub-generated notes to this tool's own previous tag.
///
/// `--generate-notes` without `--notes-start-tag` lets GitHub pick the newest
/// release in the repository. A first release for the prefix cannot use that
/// API without inventing a cross-tool range, so it gets a short initial note.
///
/// The run's claim travels in `--notes` either way. Beside `--generate-notes`
/// GitHub places those notes ahead of the ones it generates.
fn github_generated_notes_args(title_prefix: &str, start_tag: Option<&str>, claim: &DraftClaim) -> Vec<String> {
    match start_tag {
        Some(tag) => vec![
            "--generate-notes".to_owned(),
            "--notes-start-tag".to_owned(),
            tag.to_owned(),
            "--notes".to_owned(),
            claim.write_into(""),
        ],
        None => vec![
            "--notes".to_owned(),
            claim.write_into(&initial_release_notes(title_prefix)),
        ],
    }
}

fn initial_release_notes(title_prefix: &str) -> String {
    format!("Initial {title_prefix} release.\n")
}

fn changelog_notes(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    repo_root: &Path,
    notes_start_tag: Option<&str>,
    tag: &str,
) -> Result<String> {
    let Some(from_tag) = notes_start_tag else {
        return Ok(initial_release_notes(&config.title_prefix));
    };
    ensure_full_history(runner, "generating release notes")?;
    let project = config.notes.project.as_ref().expect("validated changelog project");
    let project = repo_root.join(project);
    let paths = derive_paths_from_project(&project, repo_root)
        .with_context(|| format!("could not derive changelog paths from {}", project.display()))?;
    let range = extract_changelog(&ExtractionConfig {
        repo_path: repo_root.to_owned(),
        from_tag: from_tag.to_owned(),
        to_tag: tag.to_owned(),
        path_globs: paths,
        repo_slug: config.repo.clone(),
        enrich: true,
    })
    .context("could not generate changelog release notes")?;
    Ok(GithubMarkdownRenderer.render(&range))
}

fn record_tag(runner: &impl CommandRunner, buildkite_job_id: Option<&str>, tag: &str) -> Result<()> {
    if nonempty(buildkite_job_id).is_none() {
        return Ok(());
    }
    run_checked(
        runner,
        Command::new("buildkite-agent", ["meta-data", "set", BUILDKITE_TAG_KEY, tag]),
    )
    .context("could not record release tag in Buildkite metadata")?;
    Ok(())
}

pub(crate) fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

fn cleanup_tag(runner: &impl CommandRunner, tag: &str, local_tag_created: bool, tag_pushed: bool) {
    if tag_pushed {
        let _ = runner.run(&Command::new("git", ["push", "origin", &format!(":refs/tags/{tag}")]));
    }
    if local_tag_created {
        let _ = runner.run(&Command::new("git", ["tag", "-d", tag]));
    }
}

fn expand_upload_assets(config: &ReleaseConfig, tag: &str, assets: &[Asset]) -> Result<Vec<Asset>> {
    assets
        .iter()
        .map(|asset| {
            Ok(Asset {
                name: expand_asset_name(&asset.name, tag, &config.tag_prefix)?,
                source: asset.source.clone(),
            })
        })
        .collect()
}

fn expand_asset_list(names: &[String], tag: &str, tag_prefix: &str) -> Result<Vec<String>> {
    names
        .iter()
        .map(|name| expand_asset_name(name, tag, tag_prefix))
        .collect()
}

/// Substitutes `{version}` with the version encoded in `tag` (the tag with
/// `tag_prefix` stripped). Names without the placeholder are unchanged.
fn expand_asset_name(name: &str, tag: &str, tag_prefix: &str) -> Result<String> {
    let expanded = if name.contains(VERSION_PLACEHOLDER) {
        let version = version_from_tag(tag, tag_prefix)?;
        name.replace(VERSION_PLACEHOLDER, version)
    } else {
        name.to_owned()
    };
    validate_asset_name(&expanded)?;
    Ok(expanded)
}

fn version_from_tag<'a>(tag: &'a str, tag_prefix: &str) -> Result<&'a str> {
    let version = tag
        .strip_prefix(tag_prefix)
        .ok_or_else(|| anyhow!("release tag {tag} does not start with configured tag_prefix {tag_prefix}"))?;
    if version.is_empty() {
        bail!("release tag {tag} has an empty version after stripping prefix {tag_prefix}");
    }
    Ok(version)
}

fn validate_upload_assets(config: &ReleaseConfig, tag: &str, assets: &[Asset]) -> Result<()> {
    let known_assets = expand_asset_list(&config.required_assets, tag, &config.tag_prefix)?
        .into_iter()
        .chain(expand_asset_list(&config.optional_assets, tag, &config.tag_prefix)?)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    for asset in assets {
        if !known_assets.contains(asset.name.as_str()) {
            bail!("asset {} is not declared in the release config", asset.name);
        }
        if !seen.insert(asset.name.as_str()) {
            bail!("asset {} was specified more than once", asset.name);
        }
        let metadata = fs::metadata(&asset.source)
            .with_context(|| format!("could not read asset source {}", asset.source.display()))?;
        if !metadata.is_file() {
            bail!("asset source {} is not a regular file", asset.source.display());
        }
    }
    Ok(())
}

fn stage_assets(assets: &[Asset]) -> Result<TempDir> {
    let stage = tempdir().context("could not create release asset staging directory")?;
    for asset in assets {
        let staged = stage.path().join(&asset.name);
        fs::copy(&asset.source, &staged).with_context(|| {
            format!(
                "could not stage asset {} as {}",
                asset.source.display(),
                staged.display()
            )
        })?;
        let checksum = sha256_file(&staged)?;
        fs::write(
            stage.path().join(format!("{}.sha256", asset.name)),
            format!("{checksum}  {}\n", asset.name),
        )
        .with_context(|| format!("could not write checksum sidecar for {}", asset.name))?;
    }
    Ok(stage)
}

/// Runs a network transfer up to `attempts` times, waiting 15 seconds longer
/// before each further try. `before_retry` clears whatever a failed try left
/// behind.
fn retry_command(
    runner: &impl CommandRunner,
    command: &Command,
    attempts: usize,
    what: &str,
    before_retry: impl FnMut(),
) -> Result<()> {
    retry_checked(runner, command, attempts, what, before_retry).map(|_| ())
}

/// Like [`retry_command`], but returns the successful output.
fn retry_checked(
    runner: &impl CommandRunner,
    command: &Command,
    attempts: usize,
    what: &str,
    mut before_retry: impl FnMut(),
) -> Result<CommandOutput> {
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        match runner.run(command) {
            Ok(output) if output.success => return Ok(output),
            Ok(output) => last_error = command_failure(command, &output),
            Err(error) => last_error = error.to_string(),
        }
        if attempt < attempts {
            let delay = Duration::from_secs((attempt * 15) as u64);
            eprintln!(
                "release {what} attempt {attempt}/{attempts} failed: {last_error}; retrying in {} seconds",
                delay.as_secs()
            );
            runner.sleep(delay);
            before_retry();
        }
    }
    bail!("{what} failed after {attempts} attempts: {last_error}")
}

/// How long one verification download of an asset of `size` bytes may take.
///
/// A download that stalls is otherwise only ended by the far side, which can
/// take most of an hour. Bounding it by the asset's size lets a stalled
/// attempt be dropped and retried on a new connection without cutting off a
/// large asset on a slow link.
fn download_timeout(size: u64) -> Duration {
    DOWNLOAD_BASE_TIMEOUT + Duration::from_secs(size / DOWNLOAD_MIN_BYTES_PER_SECOND)
}

/// Downloads `asset` and its sidecar from the draft and compares checksums.
///
/// The download is bounded and retried because a failed one leaves the release
/// unpublished. A checksum mismatch is not retried: it means the release holds
/// the wrong bytes, which another download cannot fix.
fn verify_remote_asset(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    tag: &str,
    stage: &Path,
    asset: &str,
    size: u64,
) -> Result<()> {
    let asset_path = stage.join(asset);
    let sidecar_path = stage.join(format!("{asset}.sha256"));
    let download = Command::new(
        "gh",
        [
            "release".to_owned(),
            "download".to_owned(),
            tag.to_owned(),
            "--repo".to_owned(),
            config.repo.clone(),
            "--dir".to_owned(),
            stage.display().to_string(),
            "--clobber".to_owned(),
            "--pattern".to_owned(),
            asset.to_owned(),
            "--pattern".to_owned(),
            format!("{asset}.sha256"),
        ],
    )
    .with_timeout(download_timeout(size));
    retry_command(
        runner,
        &download,
        VERIFY_DOWNLOAD_ATTEMPTS,
        "verification download",
        || {
            let _ = fs::remove_file(&asset_path);
            let _ = fs::remove_file(&sidecar_path);
        },
    )
    .with_context(|| format!("failed to download {asset} and its sidecar from {tag}; leaving the release draft"))?;
    if !asset_path.is_file() || !sidecar_path.is_file() {
        bail!("{asset} or its checksum sidecar did not download from {tag}; leaving the release draft");
    }
    let expected = checksum_from_sidecar(&sidecar_path, asset)?;
    let actual = sha256_file(&asset_path)?;
    if expected != actual {
        bail!(
            "checksum mismatch for {asset} on {tag}: sidecar says {expected}, downloaded asset hashes to {actual}; leaving the release draft"
        );
    }
    Ok(())
}

fn checksum_from_sidecar(path: &Path, asset: &str) -> Result<String> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("could not read checksum sidecar {}", path.display()))?;
    let mut lines = contents.lines();
    let line = lines
        .next()
        .ok_or_else(|| anyhow!("checksum sidecar {} is empty", path.display()))?;
    if lines.any(|line| !line.is_empty()) {
        bail!("checksum sidecar {} must contain one checksum", path.display());
    }
    let (checksum, recorded_name) = line
        .split_once("  ")
        .ok_or_else(|| anyhow!("checksum sidecar {} is not in sha256sum -c format", path.display()))?;
    if recorded_name != asset {
        bail!(
            "checksum sidecar {} names {recorded_name:?}, expected {asset:?}",
            path.display()
        );
    }
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("checksum sidecar {} has an invalid SHA-256 digest", path.display());
    }
    Ok(checksum.to_ascii_lowercase())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("could not open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 32 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("could not read {} for hashing", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_asset_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        bail!("asset name {name:?} must be a non-empty file name");
    }
    Ok(())
}

pub(crate) fn run_checked(runner: &impl CommandRunner, command: Command) -> Result<CommandOutput> {
    let output = runner
        .run(&command)
        .map_err(|error| anyhow!("could not run {}: {error}", display_command(&command)))?;
    if !output.success {
        bail!(command_failure(&command, &output));
    }
    Ok(output)
}

fn command_failure(command: &Command, output: &CommandOutput) -> String {
    let detail = output.stderr.trim();
    if detail.is_empty() {
        format!("{} exited unsuccessfully", display_command(command))
    } else {
        format!("{} failed: {detail}", display_command(command))
    }
}

fn display_command(command: &Command) -> String {
    std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use super::*;
    use crate::{CommandOutput, RunnerError};

    const CONFIG: &str = r#"
repo = "example/project"
tag_prefix = "demo-v"
title_prefix = "Demo"
change_paths = ["tool/"]
required_assets = ["demo-linux"]
optional_assets = ["demo-darwin"]

[version]
scheme = "patch-counter"
major_minor = "1.0"

[notes]
source = "github-generated"
"#;

    /// The claim a fixture run records: made at `NOW`, by no known build.
    const FIXTURE_CLAIM: &str = "<!-- release-tool: claimed-at=1800000000 scheduled-resumes=0 -->";

    fn request<'a>(trigger_source: &'a str, buildkite_job_id: Option<&'a str>) -> PrepareRequest<'a> {
        PrepareRequest {
            trigger_source,
            buildkite_job_id,
            ..PrepareRequest::default()
        }
    }

    /// `gh release view` output for a draft holding `assets`, each a few
    /// bytes long, and `notes`.
    fn release_view(assets: &[&str], notes: &str) -> FixtureResponse {
        success(&view_json(assets, notes, true, 1).to_string())
    }

    /// `upload` counts as a new generation of an asset: it changes the id.
    fn view_json(assets: &[&str], notes: &str, draft: bool, generation: u32) -> serde_json::Value {
        let assets = assets
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "size": 24,
                    "id": format!("{name}-upload-{generation}"),
                    "updatedAt": "2026-11-19T00:00:00Z",
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "apiUrl": "https://api.github.com/repos/example/project/releases/7",
            "assets": assets,
            "body": notes,
            "isDraft": draft,
        })
    }

    /// What a publish of a draft asks for, in order, when nothing goes wrong:
    /// the release, the claim write, the release again, and the edit.
    fn publish_responses(assets: &[&str], notes: &str) -> [FixtureResponse; 4] {
        [
            release_view(assets, notes),
            success(""),
            release_view(assets, notes),
            success(""),
        ]
    }

    fn is_claim_write(call: &Command) -> bool {
        call.args
            .starts_with(&["api", "-X", "PATCH", "repos/example/project/releases/7"].map(str::to_owned))
    }

    const CHANGELOG_CONFIG: &str = r#"
repo = "example/project"
tag_prefix = "demo-v"
title_prefix = "Demo"
change_paths = ["tool/"]
required_assets = ["demo-linux"]
optional_assets = ["demo-darwin"]

[version]
scheme = "patch-counter"
major_minor = "1.0"

[notes]
source = "changelog"
project = "tool/PROJECT.yaml"
"#;

    type FixtureResponse = std::result::Result<CommandOutput, RunnerError>;

    /// How `FixtureRunner` materializes `gh release download` into the stage dir.
    #[derive(Clone, Copy)]
    enum DownloadMaterialize {
        /// Leave downloads to the response queue (or fail if none is queued).
        None,
        /// Write the asset and a matching sidecar digest.
        Matching,
        /// Write the asset and a well-formed but wrong 64-hex sidecar digest.
        Mismatched,
    }

    struct FixtureRunner {
        responses: RefCell<VecDeque<FixtureResponse>>,
        calls: RefCell<Vec<Command>>,
        sleeps: RefCell<Vec<Duration>>,
        download: DownloadMaterialize,
        /// Downloads that fail before the materialized ones start succeeding.
        download_failures: Cell<usize>,
        notes_file: RefCell<Option<String>>,
    }

    /// The fixed time fixtures report, so claims in notes are predictable.
    const NOW: u64 = 1_800_000_000;

    impl FixtureRunner {
        fn new(responses: impl IntoIterator<Item = FixtureResponse>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
                sleeps: RefCell::new(Vec::new()),
                download: DownloadMaterialize::None,
                download_failures: Cell::new(0),
                notes_file: RefCell::new(None),
            }
        }

        fn failing_the_first_downloads(self, count: usize) -> Self {
            self.download_failures.set(count);
            self
        }

        fn with_matching_download(mut self) -> Self {
            self.download = DownloadMaterialize::Matching;
            self
        }

        fn with_mismatched_download(mut self) -> Self {
            self.download = DownloadMaterialize::Mismatched;
            self
        }
    }

    impl CommandRunner for FixtureRunner {
        fn run(&self, command: &Command) -> std::result::Result<CommandOutput, RunnerError> {
            self.calls.borrow_mut().push(command.clone());
            if let Some(path) = command
                .args
                .windows(2)
                .find(|args| args[0] == "--notes-file")
                .map(|args| &args[1])
            {
                *self.notes_file.borrow_mut() = Some(fs::read_to_string(path).expect("read notes file"));
            }
            if !matches!(self.download, DownloadMaterialize::None)
                && command.program == "gh"
                && command.args.starts_with(&["release".to_owned(), "download".to_owned()])
            {
                if self.download_failures.get() > 0 {
                    self.download_failures.set(self.download_failures.get() - 1);
                    return failure("stream error: stream ID 1; PROTOCOL_ERROR; received from peer");
                }
                materialize_download(command, self.download);
                return success("");
            }
            self.responses
                .borrow_mut()
                .pop_front()
                .expect("fixture response for command")
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
        }

        fn now(&self) -> std::time::SystemTime {
            UNIX_EPOCH + Duration::from_secs(NOW)
        }
    }

    fn materialize_download(command: &Command, mode: DownloadMaterialize) {
        let directory = command
            .args
            .windows(2)
            .find(|args| args[0] == "--dir")
            .map(|args| PathBuf::from(&args[1]))
            .expect("download directory");
        let asset = command
            .args
            .windows(2)
            .find(|args| args[0] == "--pattern")
            .map(|args| args[1].clone())
            .expect("asset pattern");
        let bytes = b"downloaded release asset";
        fs::write(directory.join(&asset), bytes).expect("write downloaded asset");
        let checksum = match mode {
            DownloadMaterialize::None => unreachable!("download intercept disabled"),
            DownloadMaterialize::Matching => sha256_file(&directory.join(&asset)).expect("hash downloaded asset"),
            // Well-formed 64-hex digest that will not match the written bytes.
            DownloadMaterialize::Mismatched => "0".repeat(64),
        };
        fs::write(
            directory.join(format!("{asset}.sha256")),
            format!("{checksum}  {asset}\n"),
        )
        .expect("write downloaded sidecar");
    }

    fn success(stdout: &str) -> FixtureResponse {
        Ok(CommandOutput {
            success: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    fn failure(stderr: &str) -> FixtureResponse {
        Ok(CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        })
    }

    fn repo_root_response(root: &Path) -> FixtureResponse {
        success(&format!("{}\n", root.display()))
    }

    #[test]
    fn parses_asset_assignment() {
        assert_eq!(
            Asset::parse("demo-linux=/tmp/demo").expect("valid asset"),
            Asset {
                name: "demo-linux".to_owned(),
                source: PathBuf::from("/tmp/demo"),
            }
        );
        assert!(Asset::parse("demo-linux").is_err());
        assert!(Asset::parse("../demo=/tmp/demo").is_err());
    }

    #[test]
    fn resolves_relative_config_from_a_bazel_workspace() {
        assert_eq!(
            resolve_config_path(Path::new("release.toml"), Some(Path::new("/repo"))),
            PathBuf::from("/repo/release.toml")
        );
        assert_eq!(
            resolve_config_path(Path::new("release.toml"), None),
            PathBuf::from("release.toml")
        );
        assert_eq!(
            resolve_config_path(Path::new("/tmp/release.toml"), Some(Path::new("/repo"))),
            PathBuf::from("/tmp/release.toml")
        );
    }

    #[test]
    fn upload_stages_a_sha256sum_sidecar_and_clobbers_remote_asset() {
        let source_dir = tempdir().expect("source dir");
        let source = source_dir.path().join("binary");
        fs::write(&source, b"release bytes").expect("write source");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let asset = Asset {
            name: "demo-linux".to_owned(),
            source,
        };
        let stage = stage_assets(std::slice::from_ref(&asset)).expect("stage assets");
        let sidecar = fs::read_to_string(stage.path().join("demo-linux.sha256")).expect("read sidecar");
        assert_eq!(
            sidecar,
            "ff7a5e6429d2c8511521e4abf41cd54a3e525ef4a1f24f8d1c67ede9d17874dd  demo-linux\n"
        );

        let runner = FixtureRunner::new([release_view(&[], "notes\n"), success(""), success("")]);

        upload(&runner, &config, "demo-v1.0.0", &[asset]).expect("upload");

        let calls = runner.calls.into_inner();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].args[0..2], ["release", "view"]);
        assert!(
            is_claim_write(&calls[1]),
            "the claim is dated before the upload starts: {calls:?}"
        );
        assert_eq!(calls[2].program, "gh");
        assert_eq!(calls[2].args[0..2], ["release", "upload"]);
        assert!(
            calls[2]
                .args
                .ends_with(&["--repo", "example/project", "--clobber"].map(str::to_owned))
        );
        assert!(calls[2].args.iter().any(|arg| arg.ends_with("demo-linux.sha256")));
    }

    #[test]
    fn upload_does_not_replace_the_assets_of_a_published_release() {
        let source_dir = tempdir().expect("source dir");
        let source = source_dir.path().join("binary");
        fs::write(&source, b"release bytes").expect("write source");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let published = view_json(&["demo-linux", "demo-linux.sha256"], "notes\n", false, 1);
        let runner = FixtureRunner::new([success(&published.to_string())]);

        let error = upload(
            &runner,
            &config,
            "demo-v1.0.0",
            &[Asset {
                name: "demo-linux".to_owned(),
                source,
            }],
        )
        .expect_err("a published release keeps its verified assets");

        assert!(error.to_string().contains("already published"), "{error:#}");
        assert_eq!(runner.calls.borrow().len(), 1, "nothing was written or uploaded");
    }

    #[test]
    fn upload_goes_ahead_when_the_claim_cannot_be_dated() {
        let source_dir = tempdir().expect("source dir");
        let source = source_dir.path().join("binary");
        fs::write(&source, b"release bytes").expect("write source");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([release_view(&[], "notes\n"), failure("HTTP 502"), success("")]);

        upload(
            &runner,
            &config,
            "demo-v1.0.0",
            &[Asset {
                name: "demo-linux".to_owned(),
                source,
            }],
        )
        .expect("bookkeeping does not fail an upload");

        assert_eq!(runner.calls.borrow().len(), 3);
    }

    #[test]
    fn upload_retries_with_the_script_backoff_schedule() {
        let source_dir = tempdir().expect("source dir");
        let source = source_dir.path().join("binary");
        fs::write(&source, b"release bytes").expect("write source");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            release_view(&[], "notes\n"),
            success(""),
            failure("first"),
            failure("second"),
            failure("third"),
        ]);

        let error = upload(
            &runner,
            &config,
            "demo-v1.0.0",
            &[Asset {
                name: "demo-linux".to_owned(),
                source,
            }],
        )
        .expect_err("all retries fail");

        assert!(error.to_string().contains("after 3 attempts"), "{error:#}");
        assert_eq!(runner.calls.borrow().len(), 5);
        assert_eq!(
            *runner.sleeps.borrow(),
            vec![Duration::from_secs(15), Duration::from_secs(30)]
        );
    }

    const BOSS_CONFIG: &str = r#"
repo = "example/project"
tag_prefix = "boss-v"
title_prefix = "Boss"
change_paths = ["tools/boss/"]
required_assets = ["Boss-{version}.zip"]
optional_assets = []

[version]
scheme = "patch-counter"
major_minor = "1.0"

[notes]
source = "github-generated"
"#;

    #[test]
    fn expands_version_placeholder_from_the_release_tag() {
        assert_eq!(
            expand_asset_name("Boss-{version}.zip", "boss-v1.0.10", "boss-v").expect("expand"),
            "Boss-1.0.10.zip"
        );
        assert_eq!(
            expand_asset_name("demo-linux", "demo-v1.0.0", "demo-v").expect("unchanged"),
            "demo-linux"
        );
        let error =
            expand_asset_name("Boss-{version}.zip", "checkleft-v1.0.0", "boss-v").expect_err("tag must match prefix");
        assert!(error.to_string().contains("does not start with"), "{error:#}");
    }

    #[test]
    fn upload_expands_versioned_asset_names_from_config_and_cli() {
        let source_dir = tempdir().expect("source dir");
        let source = source_dir.path().join("binary");
        fs::write(&source, b"release bytes").expect("write source");
        let config = ReleaseConfig::parse(BOSS_CONFIG).expect("config");
        let runner = FixtureRunner::new([release_view(&[], "notes\n"), success(""), success("")]);

        upload(
            &runner,
            &config,
            "boss-v1.0.10",
            &[Asset {
                name: "Boss-{version}.zip".to_owned(),
                source: source.clone(),
            }],
        )
        .expect("placeholder on the CLI");

        let calls = runner.calls.into_inner();
        assert!(
            calls[2].args.iter().any(|arg| arg.ends_with("Boss-1.0.10.zip")),
            "{:?}",
            calls[2].args
        );
        assert!(
            calls[2].args.iter().any(|arg| arg.ends_with("Boss-1.0.10.zip.sha256")),
            "{:?}",
            calls[2].args
        );

        let runner = FixtureRunner::new([release_view(&[], "notes\n"), success(""), success("")]);
        upload(
            &runner,
            &config,
            "boss-v1.0.10",
            &[Asset {
                name: "Boss-1.0.10.zip".to_owned(),
                source,
            }],
        )
        .expect("already-expanded CLI name");
        let calls = runner.calls.into_inner();
        assert!(calls[2].args.iter().any(|arg| arg.ends_with("Boss-1.0.10.zip")));
    }

    #[test]
    fn publish_verifies_the_expanded_versioned_asset_name() {
        let config = ReleaseConfig::parse(BOSS_CONFIG).expect("config");
        let runner = FixtureRunner::new(publish_responses(
            &["Boss-1.0.10.zip", "Boss-1.0.10.zip.sha256"],
            "notes\n",
        ))
        .with_matching_download();

        publish(&runner, &config, "boss-v1.0.10").expect("publish versioned zip");

        let calls = runner.calls.into_inner();
        assert_eq!(calls[2].args[0..2], ["release", "download"]);
        assert!(
            calls[2]
                .args
                .windows(2)
                .any(|window| window == ["--pattern", "Boss-1.0.10.zip"]),
            "{:?}",
            calls[2].args
        );
    }

    #[test]
    fn prepare_creates_a_draft_and_records_the_tag_only_in_buildkite() {
        let root = tempdir().expect("repo root");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            success(""),
            repo_root_response(root.path()),
            success(r#"[{"tag_name":"boss-v1.0.608","draft":false}]"#),
            success(""),
            success("head\n"),
            success(""),
            failure("not found"),
            success(""),
            success(""),
            success(""),
            success(""),
        ]);

        let outcome = prepare(&runner, &config, request("ui", Some("job-1"))).expect("prepare");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.0".to_owned()
            }
        );
        let calls = runner.calls.into_inner();
        let create = release_create_args(&calls);
        assert!(
            create.contains(&"--notes".to_owned()),
            "first release must not let GitHub infer a previous tag: {create:?}"
        );
        let notes = &create[create.iter().position(|arg| arg == "--notes").expect("notes flag") + 1];
        assert_eq!(
            notes,
            &format!("Initial Demo release.\n\n{FIXTURE_CLAIM}\n"),
            "a new draft carries its run's claim from the moment it exists"
        );
        assert_eq!(strip_claim(notes).as_deref(), Some("Initial Demo release.\n"));
        assert!(
            !create
                .iter()
                .any(|arg| arg == "--generate-notes" || arg == "--notes-start-tag"),
            "first release must not generate a changelog range: {create:?}"
        );
        assert!(calls.iter().any(|call| {
            call.program == "buildkite-agent"
                && call.args == ["meta-data", "set", BUILDKITE_TAG_KEY, "demo-v1.0.0"].map(str::to_owned)
        }));
        assert!(
            calls.iter().any(|call| {
                call.program == "git" && call.args == ["rev-parse", "--show-toplevel"].map(str::to_owned)
            })
        );
    }

    #[test]
    fn github_generated_notes_use_an_explicit_prefix_start_tag() {
        let claim = DraftClaim {
            claimed_at: NOW,
            scheduled_resumes: 0,
            build_url: None,
        };

        assert_eq!(
            github_generated_notes_args("checkleft", Some("checkleft-v0.1.0-alpha.122"), &claim),
            [
                "--generate-notes",
                "--notes-start-tag",
                "checkleft-v0.1.0-alpha.122",
                "--notes",
                &format!("{FIXTURE_CLAIM}\n"),
            ]
            .map(str::to_owned)
        );
        assert_eq!(
            github_generated_notes_args("release", None, &claim),
            ["--notes", &format!("Initial release release.\n\n{FIXTURE_CLAIM}\n")].map(str::to_owned)
        );
    }

    #[test]
    fn prepare_bounds_generated_notes_to_the_previous_prefix_tag() {
        let root = tempdir().expect("repo root");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            success(""),
            repo_root_response(root.path()),
            success(
                r#"[{"tag_name":"boss-v1.0.608","draft":false},{"tag_name":"demo-v1.0.5","draft":false},{"tag_name":"demo-v1.0.4","draft":false}]"#,
            ),
            success("sha\trefs/tags/demo-v1.0.5\nsha\trefs/tags/demo-v1.0.4\n"),
            success("head\n"),
            success("oldsha\n"),
            success(""),
            failure("not found"),
            success(""),
            success(""),
            success(""),
            success(""),
        ]);

        let outcome = prepare(&runner, &config, request("ui", Some("job-1"))).expect("prepare");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.6".to_owned()
            }
        );
        let calls = runner.calls.into_inner();
        let create = release_create_args(&calls);
        assert!(
            create
                .windows(2)
                .any(|window| window == ["--notes-start-tag", "demo-v1.0.5"]),
            "notes must start at the previous demo-v tag, not boss-v1.0.608: {create:?}"
        );
        assert!(
            create.contains(&"--generate-notes".to_owned()),
            "subsequent github-generated notes still use GitHub's generator: {create:?}"
        );
    }

    #[test]
    fn prepare_changelog_notes_emit_initial_text_when_the_prefix_has_no_prior_tag() {
        let root = tempdir().expect("repo root");
        let config = ReleaseConfig::parse(CHANGELOG_CONFIG).expect("config");
        let runner = FixtureRunner::new([
            success(""),
            repo_root_response(root.path()),
            success(r#"[{"tag_name":"boss-v1.0.608","draft":false}]"#),
            success(""),
            success("head\n"),
            success(""),
            failure("not found"),
            success(""),
            success(""),
            success(""),
            success(""),
        ]);

        let outcome = prepare(&runner, &config, request("ui", Some("job-1"))).expect("prepare");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.0".to_owned()
            }
        );
        let calls = runner.calls.into_inner();
        let create = release_create_args(&calls);
        assert!(
            create.contains(&"--notes-file".to_owned()),
            "changelog notes are passed as a file: {create:?}"
        );
        let notes = runner.notes_file.into_inner().expect("captured notes");
        assert_eq!(
            strip_claim(&notes).as_deref(),
            Some("Initial Demo release.\n"),
            "a prefix with no prior published tag must not invent a changelog range"
        );
        assert!(notes.ends_with(&format!("{FIXTURE_CLAIM}\n")), "{notes}");
    }

    #[test]
    fn prepare_changelog_notes_start_at_this_prefix_previous_tag() {
        let root = changelog_git_repo(["demo-v1.0.5", "demo-v1.0.6"]);
        let config = ReleaseConfig::parse(CHANGELOG_CONFIG).expect("config");
        let runner = FixtureRunner::new([
            success(""),
            repo_root_response(root.path()),
            success(
                r#"[{"tag_name":"boss-v1.0.608","draft":false},{"tag_name":"demo-v1.0.5","draft":false},{"tag_name":"demo-v1.0.4","draft":false}]"#,
            ),
            success("sha\trefs/tags/demo-v1.0.5\nsha\trefs/tags/demo-v1.0.4\n"),
            success("head\n"),
            success("oldsha\n"),
            success(""),
            failure("not found"),
            success(""),
            success(""),
            success("false\n"),
            success(""),
            success(""),
        ]);

        let outcome = prepare(&runner, &config, request("ui", Some("job-1"))).expect("prepare");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.6".to_owned()
            }
        );
        let calls = runner.calls.into_inner();
        let create = release_create_args(&calls);
        assert!(
            create.contains(&"--notes-file".to_owned()),
            "changelog notes are passed as a file: {create:?}"
        );
        let notes = runner.notes_file.into_inner().expect("captured changelog notes");
        assert!(
            notes.contains("https://github.com/example/project/compare/demo-v1.0.5...demo-v1.0.6"),
            "changelog range must start at this prefix's previous tag, not boss-v1.0.608: {notes}"
        );
        assert!(
            !notes.contains("boss-v1.0.608"),
            "newest repo release from another prefix must not bound notes: {notes}"
        );
    }

    #[test]
    fn draft_creation_failure_deletes_the_pushed_and_local_tag() {
        let root = tempdir().expect("repo root");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            success(""),
            repo_root_response(root.path()),
            success("[]"),
            success(""),
            success("head\n"),
            success(""),
            failure("not found"),
            success(""),
            success(""),
            failure("GitHub unavailable"),
            success(""),
            success(""),
        ]);

        prepare(&runner, &config, request("ui", None)).expect_err("release create fails");

        let calls = runner.calls.into_inner();
        assert!(
            calls
                .iter()
                .any(|call| call.args == ["push", "origin", ":refs/tags/demo-v1.0.0"].map(str::to_owned))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.args == ["tag", "-d", "demo-v1.0.0"].map(str::to_owned))
        );
    }

    #[test]
    fn manual_prepare_resumes_a_draft_for_head_and_claims_it() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let root = tempdir().expect("repo root");
        let runner = FixtureRunner::new([
            success(""),
            repo_root_response(root.path()),
            success(r#"[{"id":7,"tag_name":"demo-v1.0.0","draft":true,"body":"notes\n"}]"#),
            success("head refs/tags/demo-v1.0.0\n"),
            success("head\n"),
            success("head\n"),
            // The draft is read again before its notes are written.
            success(r#"{"id":7,"tag_name":"demo-v1.0.0","draft":true,"body":"notes\n"}"#),
            success(""),
            success(""),
        ]);

        let outcome = prepare(&runner, &config, request("ui", Some("job-1"))).expect("manual resume");

        assert_eq!(
            outcome,
            PrepareOutcome::Resumed {
                tag: "demo-v1.0.0".to_owned()
            }
        );
        let calls = runner.calls.into_inner();
        assert!(
            calls.iter().any(|call| call
                .args
                .starts_with(&["api", "-X", "PATCH", "repos/example/project/releases/7"].map(str::to_owned))),
            "the resuming run records its claim on the draft: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| { call.args == ["meta-data", "set", BUILDKITE_TAG_KEY, "demo-v1.0.0"].map(str::to_owned) })
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.args.first().is_some_and(|arg| arg == "annotate")),
            "a resume a person asked for needs no warning on the build page: {calls:?}"
        );
    }

    #[test]
    fn publish_verifies_the_downloaded_required_asset_before_editing_the_release() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new(publish_responses(&["demo-linux", "demo-linux.sha256"], "notes\n"))
            .with_matching_download();

        publish(&runner, &config, "demo-v1.0.0").expect("publish verified release");

        let calls = runner.calls.into_inner();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[0].args[0..2], ["release", "view"]);
        assert!(
            is_claim_write(&calls[1]),
            "the claim is dated before verification starts: {calls:?}"
        );
        assert_eq!(calls[2].args[0..2], ["release", "download"]);
        assert_eq!(
            calls[3].args[0..2],
            ["release", "view"],
            "the release is read again once verification is done"
        );
        assert_eq!(
            calls[4].args,
            [
                "release",
                "edit",
                "demo-v1.0.0",
                "--repo",
                "example/project",
                "--draft=false"
            ]
            .map(str::to_owned)
        );
    }

    #[test]
    fn publish_leaves_the_release_draft_when_a_required_asset_is_missing() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([release_view(&["demo-linux"], "notes\n"), success("")]);

        let error = publish(&runner, &config, "demo-v1.0.0").expect_err("missing sidecar blocks publish");

        assert!(error.to_string().contains("demo-linux.sha256"), "{error:#}");
        assert_eq!(runner.calls.borrow().len(), 2);
        assert!(!published_draft(&runner));
    }

    #[test]
    fn publish_leaves_the_release_draft_on_checksum_mismatch() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            release_view(&["demo-linux", "demo-linux.sha256"], "notes\n"),
            success(""),
        ])
        .with_mismatched_download();

        let error = publish(&runner, &config, "demo-v1.0.0").expect_err("checksum mismatch blocks publish");

        assert!(error.to_string().contains("checksum mismatch"), "{error:#}");
        assert!(!published_draft(&runner));
    }

    #[test]
    fn publish_leaves_the_release_draft_when_optional_asset_lacks_sidecar() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            release_view(&["demo-linux", "demo-linux.sha256", "demo-darwin"], "notes\n"),
            success(""),
        ]);

        let error =
            publish(&runner, &config, "demo-v1.0.0").expect_err("optional asset without sidecar blocks publish");

        assert!(error.to_string().contains("without demo-darwin.sha256"), "{error:#}");
        assert_eq!(runner.calls.borrow().len(), 2);
        assert!(!published_draft(&runner));
    }

    #[test]
    fn publish_retries_a_failed_verification_download_before_giving_up_on_the_release() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new(publish_responses(&["demo-linux", "demo-linux.sha256"], "notes\n"))
            .with_matching_download()
            .failing_the_first_downloads(2);

        publish(&runner, &config, "demo-v1.0.0").expect("the third download verifies and publishes");

        let downloads = runner
            .calls
            .borrow()
            .iter()
            .filter(|call| call.args.starts_with(&["release".to_owned(), "download".to_owned()]))
            .count();
        assert_eq!(downloads, 3);
        assert_eq!(
            *runner.sleeps.borrow(),
            vec![Duration::from_secs(15), Duration::from_secs(30)]
        );
        assert!(published_draft(&runner));
    }

    #[test]
    fn publish_leaves_the_release_draft_when_every_verification_download_fails() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            release_view(&["demo-linux", "demo-linux.sha256"], "notes\n"),
            success(""),
        ])
        .with_matching_download()
        .failing_the_first_downloads(3);

        let error = publish(&runner, &config, "demo-v1.0.0").expect_err("an unverified release must stay a draft");

        let rendered = format!("{error:#}");
        assert!(rendered.contains("failed after 3 attempts"), "{rendered}");
        assert!(rendered.contains("PROTOCOL_ERROR"), "{rendered}");
        assert!(rendered.contains("leaving the release draft"), "{rendered}");
        assert!(!published_draft(&runner));
    }

    #[test]
    fn verification_downloads_are_bounded_by_the_size_of_the_asset() {
        assert_eq!(download_timeout(0), Duration::from_secs(120));
        // A 64.6 MB Boss.app zip: two minutes plus the time 1 Mbit/s needs.
        assert_eq!(download_timeout(64_606_777), Duration::from_secs(120 + 516));

        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let view = serde_json::json!({
            "assets": [{ "name": "demo-linux", "size": 1_250_000 }, { "name": "demo-linux.sha256", "size": 84 }],
            "body": "notes\n",
            "apiUrl": "https://api.github.com/repos/example/project/releases/7",
            "isDraft": true,
        })
        .to_string();
        let runner =
            FixtureRunner::new([success(&view), success(""), success(&view), success("")]).with_matching_download();

        publish(&runner, &config, "demo-v1.0.0").expect("publish");

        let calls = runner.calls.into_inner();
        assert_eq!(calls[2].args[0..2], ["release", "download"]);
        assert_eq!(calls[2].timeout, Some(Duration::from_secs(130)));
        assert!(
            calls
                .iter()
                .enumerate()
                .all(|(index, call)| index == 2 || call.timeout.is_none()),
            "only the transfer is bounded: {calls:?}"
        );
    }

    #[test]
    fn publish_removes_the_claim_from_the_notes_in_the_edit_that_publishes() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let notes = format!("## Changes\n\n* one\n\n{FIXTURE_CLAIM}\n");
        // Notes edited while the assets were being verified must survive: the
        // edit that publishes starts from the notes as read just before it.
        let edited = format!("## Changes\n\n* one\n* two\n\n{FIXTURE_CLAIM}\n");
        let runner = FixtureRunner::new([
            release_view(&["demo-linux", "demo-linux.sha256"], &notes),
            success(""),
            release_view(&["demo-linux", "demo-linux.sha256"], &edited),
            success(""),
        ])
        .with_matching_download();

        publish(&runner, &config, "demo-v1.0.0").expect("publish");

        assert_eq!(
            runner.notes_file.borrow().as_deref(),
            Some("## Changes\n\n* one\n* two\n"),
            "the published notes are the latest notes without the claim"
        );
        let calls = runner.calls.into_inner();
        let edit = calls.last().expect("edit call");
        assert_eq!(
            edit.args[..6],
            [
                "release",
                "edit",
                "demo-v1.0.0",
                "--repo",
                "example/project",
                "--draft=false"
            ]
            .map(str::to_owned)
        );
        assert_eq!(edit.args[6], "--notes-file");
        assert_eq!(edit.args.len(), 8);
    }

    #[test]
    fn publish_reports_an_unreadable_asset_list_instead_of_publishing() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([success("demo-linux\ndemo-linux.sha256\n")]);

        let error = publish(&runner, &config, "demo-v1.0.0").expect_err("a list that cannot be read blocks publish");

        assert!(
            format!("{error:#}").contains("could not list assets for demo-v1.0.0"),
            "{error:#}"
        );
        assert!(!published_draft(&runner));
    }

    #[test]
    fn the_release_id_is_read_from_the_address_every_gh_version_reports() {
        let view: ReleaseView = serde_json::from_value(view_json(&[], "notes\n", true, 1)).expect("view");
        assert_eq!(view.release_id(), 7);

        let odd: ReleaseView = serde_json::from_value(serde_json::json!({
            "apiUrl": "https://api.github.com/repos/example/project/releases/latest",
            "isDraft": true,
        }))
        .expect("view");
        assert_eq!(odd.release_id(), 0, "an address without an id is not guessed at");
    }

    #[test]
    fn publish_stops_when_an_asset_is_replaced_while_it_is_being_verified() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let assets = ["demo-linux", "demo-linux.sha256"];
        let runner = FixtureRunner::new([
            success(&view_json(&assets, "notes\n", true, 1).to_string()),
            success(""),
            success(&view_json(&assets, "notes\n", true, 2).to_string()),
        ])
        .with_matching_download();

        let error = publish(&runner, &config, "demo-v1.0.0")
            .expect_err("bytes that were not the ones verified must not be published");

        assert!(
            error.to_string().contains("changed while it was being verified"),
            "{error:#}"
        );
        assert!(!published_draft(&runner));
    }

    #[test]
    fn publish_leaves_the_release_draft_when_an_optional_asset_appears_during_verification() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let required = ["demo-linux", "demo-linux.sha256"];
        let with_optional = ["demo-linux", "demo-linux.sha256", "demo-darwin", "demo-darwin.sha256"];
        let runner = FixtureRunner::new([
            success(&view_json(&required, "notes\n", true, 1).to_string()),
            success(""),
            success(&view_json(&with_optional, "notes\n", true, 1).to_string()),
        ])
        .with_matching_download();

        let error = publish(&runner, &config, "demo-v1.0.0")
            .expect_err("an optional asset that was not verified must not be published");

        assert!(
            error
                .to_string()
                .contains("demo-darwin on demo-v1.0.0 changed while it was being verified"),
            "{error:#}"
        );
        assert!(!published_draft(&runner));
    }

    #[test]
    fn publish_leaves_the_release_draft_when_an_optional_asset_appears_without_its_sidecar() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let required = ["demo-linux", "demo-linux.sha256"];
        let optional_alone = ["demo-linux", "demo-linux.sha256", "demo-darwin"];
        let runner = FixtureRunner::new([
            success(&view_json(&required, "notes\n", true, 1).to_string()),
            success(""),
            success(&view_json(&optional_alone, "notes\n", true, 1).to_string()),
        ])
        .with_matching_download();

        let error = publish(&runner, &config, "demo-v1.0.0")
            .expect_err("an optional asset without a sidecar must not be published");

        assert!(
            error
                .to_string()
                .contains("demo-darwin on demo-v1.0.0 changed while it was being verified"),
            "{error:#}"
        );
        assert!(!published_draft(&runner));
    }

    #[test]
    fn upload_retries_a_failed_release_view() {
        let source_dir = tempdir().expect("source dir");
        let source = source_dir.path().join("binary");
        fs::write(&source, b"release bytes").expect("write source");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([
            failure("HTTP 502"),
            release_view(&[], "notes\n"),
            success(""),
            success(""),
        ]);

        upload(
            &runner,
            &config,
            "demo-v1.0.0",
            &[Asset {
                name: "demo-linux".to_owned(),
                source,
            }],
        )
        .expect("the second view succeeds");

        let views = runner
            .calls
            .borrow()
            .iter()
            .filter(|call| call.args.starts_with(&["release".to_owned(), "view".to_owned()]))
            .count();
        assert_eq!(views, 2);
        assert_eq!(*runner.sleeps.borrow(), vec![Duration::from_secs(15)]);
        assert!(
            runner
                .calls
                .borrow()
                .iter()
                .any(|call| call.args.starts_with(&["release".to_owned(), "upload".to_owned()]))
        );
    }

    #[test]
    fn publish_retries_a_failed_release_view() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let assets = ["demo-linux", "demo-linux.sha256"];
        let runner = FixtureRunner::new([
            failure("HTTP 502"),
            release_view(&assets, "notes\n"),
            success(""),
            release_view(&assets, "notes\n"),
            success(""),
        ])
        .with_matching_download();

        publish(&runner, &config, "demo-v1.0.0").expect("the second view succeeds");

        let views = runner
            .calls
            .borrow()
            .iter()
            .filter(|call| call.args.starts_with(&["release".to_owned(), "view".to_owned()]))
            .count();
        assert_eq!(views, 3, "one retried view before verification, one after");
        assert_eq!(*runner.sleeps.borrow(), vec![Duration::from_secs(15)]);
        assert!(published_draft(&runner));
    }

    #[test]
    fn publishing_a_release_that_is_already_published_writes_no_claim() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let published = view_json(&["demo-linux", "demo-linux.sha256"], "notes\n", false, 1).to_string();
        let runner =
            FixtureRunner::new([success(&published), success(&published), success("")]).with_matching_download();

        publish(&runner, &config, "demo-v1.0.0").expect("a second publish verifies and changes nothing");

        let calls = runner.calls.into_inner();
        assert!(
            !calls.iter().any(is_claim_write),
            "published notes are not touched: {calls:?}"
        );
        assert_eq!(calls.last().expect("edit").args.len(), 6, "no notes are rewritten");
    }

    #[test]
    fn read_tag_returns_none_when_prepare_recorded_nothing() {
        let runner = FixtureRunner::new([success("")]);

        let tag = read_tag(&runner, Some("job-1")).expect("empty meta-data is success");

        assert_eq!(tag, None);
    }

    #[test]
    fn read_tag_returns_the_recorded_tag() {
        let runner = FixtureRunner::new([success("demo-v1.0.0\n")]);

        let tag = read_tag(&runner, Some("job-1")).expect("recorded tag");

        assert_eq!(tag.as_deref(), Some("demo-v1.0.0"));
    }

    #[test]
    fn read_tag_requires_a_buildkite_job() {
        let runner = FixtureRunner::new([]);

        let error = read_tag(&runner, None).expect_err("missing job is a hard error");

        assert!(error.to_string().contains("outside a Buildkite job"), "{error:#}");
        assert!(runner.calls.borrow().is_empty());
    }

    fn published_draft(runner: &FixtureRunner) -> bool {
        runner.calls.borrow().iter().any(|call| {
            call.program == "gh"
                && call.args.windows(2).any(|window| window == ["edit", "demo-v1.0.0"])
                && call.args.iter().any(|arg| arg == "--draft=false")
        })
    }

    fn release_create_args(calls: &[Command]) -> &[String] {
        calls
            .iter()
            .find(|call| call.program == "gh" && call.args.starts_with(&["release".to_owned(), "create".to_owned()]))
            .map(|call| call.args.as_slice())
            .expect("gh release create")
    }

    fn changelog_git_repo(tags: [&str; 2]) -> TempDir {
        let root = tempdir().expect("changelog repo");
        let tool = root.path().join("tool");
        fs::create_dir_all(&tool).expect("tool dir");
        fs::write(tool.join("PROJECT.yaml"), "name: demo\npaths:\n  - tool/\n").expect("project yaml");
        git_in(root.path(), &["init"]);
        git_in(root.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_in(root.path(), &["config", "user.email", "release@example.test"]);
        git_in(root.path(), &["config", "user.name", "Release Fixture"]);
        git_in(root.path(), &["config", "commit.gpgsign", "false"]);
        git_in(root.path(), &["add", "tool/PROJECT.yaml"]);
        git_in(root.path(), &["commit", "-m", "seed"]);
        git_in(root.path(), &["tag", tags[0]]);
        fs::write(tool.join("note.txt"), "scoped change").expect("tool change");
        git_in(root.path(), &["add", "tool/note.txt"]);
        git_in(root.path(), &["commit", "-m", "Scoped tool change"]);
        git_in(root.path(), &["tag", tags[1]]);
        root
    }

    fn git_in(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
