//! Stateful command orchestration for the release CLI.
//!
//! The resolver and decision modules deliberately remain deterministic. This
//! module is their narrow boundary to the checkout, GitHub CLI, and Buildkite
//! metadata service.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use git_changelog::{
    ChangelogRenderer, ExtractionConfig, GithubMarkdownRenderer, derive_paths_from_project, extract_changelog,
};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir, tempdir};

use crate::{
    Command, CommandOutput, CommandRunner, GitHubRelease, NotesSource, ReleaseConfig, SkipDecision, Trigger,
    assert_allowed_trigger, compute_next_version, query_release_state, should_skip,
};

/// Buildkite's per-build key that passes a release tag from `prepare` to the
/// build and publish phases. Pipelines are isolated, so one fixed key is safe.
pub const BUILDKITE_TAG_KEY: &str = "release-tag";

const UPLOAD_ATTEMPTS: usize = 3;
const VERSION_PLACEHOLDER: &str = "{version}";

/// The visible result of a prepare invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    Created { tag: String },
    Resumed { tag: String },
    Skipped,
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
pub fn prepare(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    trigger_source: &str,
    buildkite_job_id: Option<&str>,
    resume_draft: bool,
) -> Result<PrepareOutcome> {
    let trigger = assert_allowed_trigger(trigger_source)?;
    fetch_tags(runner)?;

    let repo_root = resolve_repo_root(runner)?;
    let manifest_contents = read_manifest(config, &repo_root)?;
    let state = query_release_state(runner, config, manifest_contents.as_deref())?;
    let head_sha = head_sha(runner)?;
    let last_published_sha = resolve_tag_sha(runner, config, state.last.published.as_ref())?;
    let last_draft_sha = resolve_tag_sha(runner, config, state.last.draft.as_ref())?;

    if let (Some(draft), Some(draft_sha)) = (&state.last.draft, last_draft_sha.as_deref())
        && draft_sha == head_sha
    {
        if trigger == Trigger::Schedule && !resume_draft {
            bail!(
                "release {} is an unpublished draft for HEAD; scheduled runs do not resume drafts automatically",
                draft.tag_name
            );
        }
        record_tag(runner, buildkite_job_id, &draft.tag_name)?;
        return Ok(PrepareOutcome::Resumed {
            tag: draft.tag_name.clone(),
        });
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
            last_published: state.last.published.as_ref(),
            head_sha: &head_sha,
            tag: &next.tag,
            version: &next.version,
            buildkite_job_id,
        },
    )?;

    Ok(PrepareOutcome::Created { tag: next.tag })
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

/// Stages and uploads built assets and checksum sidecars for one release.
pub fn upload(runner: &impl CommandRunner, config: &ReleaseConfig, tag: &str, assets: &[Asset]) -> Result<()> {
    if tag.is_empty() {
        bail!("release tag must not be empty");
    }
    if assets.is_empty() {
        bail!("at least one --asset is required");
    }
    let assets = expand_upload_assets(config, tag, assets)?;
    validate_upload_assets(config, tag, &assets)?;

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
    retry_upload(runner, &command)
}

/// Verifies the remote assets of a draft release, then publishes it.
pub fn publish(runner: &impl CommandRunner, config: &ReleaseConfig, tag: &str) -> Result<()> {
    if tag.is_empty() {
        bail!("release tag must not be empty");
    }
    let output = run_checked(
        runner,
        Command::new(
            "gh",
            [
                "release".to_owned(),
                "view".to_owned(),
                tag.to_owned(),
                "--repo".to_owned(),
                config.repo.clone(),
                "--json".to_owned(),
                "assets".to_owned(),
                "--jq".to_owned(),
                ".assets[].name".to_owned(),
            ],
        ),
    )
    .with_context(|| format!("could not list assets for {tag}; leaving the release draft"))?;
    let remote_assets = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect::<HashSet<_>>();

    let required_assets = expand_asset_list(&config.required_assets, tag, &config.tag_prefix)?;
    let optional_assets = expand_asset_list(&config.optional_assets, tag, &config.tag_prefix)?;

    let mut missing = Vec::new();
    for asset in &required_assets {
        for required_name in [asset.as_str(), &format!("{asset}.sha256")] {
            if !remote_assets.contains(required_name) {
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

    let mut to_verify = required_assets;
    for asset in &optional_assets {
        if remote_assets.contains(asset.as_str()) {
            let checksum = format!("{asset}.sha256");
            if !remote_assets.contains(checksum.as_str()) {
                bail!("optional asset {asset} is present without {checksum}; leaving the release draft");
            }
            to_verify.push(asset.clone());
        }
    }

    let stage = tempdir().context("could not create asset verification directory")?;
    for asset in &to_verify {
        verify_remote_asset(runner, config, tag, stage.path(), asset)?;
    }
    run_checked(
        runner,
        Command::new(
            "gh",
            [
                "release".to_owned(),
                "edit".to_owned(),
                tag.to_owned(),
                "--repo".to_owned(),
                config.repo.clone(),
                "--draft=false".to_owned(),
            ],
        ),
    )
    .with_context(|| format!("verification passed, but publishing {tag} failed; the release remains a draft"))?;
    Ok(())
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

fn resolve_tag_sha(
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

fn changed_paths(runner: &impl CommandRunner, from: &str, head: &str) -> Result<Vec<String>> {
    let shallow = run_checked(runner, Command::new("git", ["rev-parse", "--is-shallow-repository"]))?;
    if shallow.stdout.trim() == "true" {
        run_checked(runner, Command::new("git", ["fetch", "--unshallow", "origin"]))
            .context("could not unshallow checkout before release change detection")?;
    }
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
    last_published: Option<&'a GitHubRelease>,
    head_sha: &'a str,
    tag: &'a str,
    version: &'a str,
    buildkite_job_id: Option<&'a str>,
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
            request.last_published,
            request.tag,
            request.version,
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
    last_published: Option<&GitHubRelease>,
    tag: &str,
    version: &str,
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
            let notes = changelog_notes(runner, config, repo_root, last_published, tag)?;
            let mut file = NamedTempFile::new().context("could not create release-notes file")?;
            file.write_all(notes.as_bytes())
                .context("could not write release notes")?;
            args.extend(["--notes-file".to_owned(), file.path().display().to_string()]);
            notes_file = Some(file);
        }
        NotesSource::GithubGenerated => args.push("--generate-notes".to_owned()),
    }
    let result = run_checked(runner, Command::new("gh", args));
    drop(notes_file);
    result.context("could not create draft GitHub release")?;
    Ok(())
}

fn changelog_notes(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    repo_root: &Path,
    last_published: Option<&GitHubRelease>,
    tag: &str,
) -> Result<String> {
    let Some(last_published) = last_published else {
        return Ok(format!("Initial {} release.\n", config.title_prefix));
    };
    let shallow = run_checked(runner, Command::new("git", ["rev-parse", "--is-shallow-repository"]))?;
    if shallow.stdout.trim() == "true" {
        run_checked(runner, Command::new("git", ["fetch", "--unshallow", "origin"]))
            .context("could not unshallow checkout before generating release notes")?;
    }
    let project = config.notes.project.as_ref().expect("validated changelog project");
    let project = repo_root.join(project);
    let paths = derive_paths_from_project(&project, repo_root)
        .with_context(|| format!("could not derive changelog paths from {}", project.display()))?;
    let range = extract_changelog(&ExtractionConfig {
        repo_path: repo_root.to_owned(),
        from_tag: last_published.tag_name.clone(),
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

fn nonempty(value: Option<&str>) -> Option<&str> {
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

fn retry_upload(runner: &impl CommandRunner, command: &Command) -> Result<()> {
    let mut last_error = String::new();
    for attempt in 1..=UPLOAD_ATTEMPTS {
        match runner.run(command) {
            Ok(output) if output.success => return Ok(()),
            Ok(output) => last_error = command_failure(command, &output),
            Err(error) => last_error = error.to_string(),
        }
        if attempt < UPLOAD_ATTEMPTS {
            let delay = Duration::from_secs((attempt * 15) as u64);
            eprintln!(
                "release upload attempt {attempt}/{UPLOAD_ATTEMPTS} failed: {last_error}; retrying in {} seconds",
                delay.as_secs()
            );
            runner.sleep(delay);
        }
    }
    bail!("asset upload failed after {UPLOAD_ATTEMPTS} attempts: {last_error}")
}

fn verify_remote_asset(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    tag: &str,
    stage: &Path,
    asset: &str,
) -> Result<()> {
    run_checked(
        runner,
        Command::new(
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
        ),
    )
    .with_context(|| format!("failed to download {asset} and its sidecar from {tag}; leaving the release draft"))?;
    let asset_path = stage.join(asset);
    let sidecar_path = stage.join(format!("{asset}.sha256"));
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

fn run_checked(runner: &impl CommandRunner, command: Command) -> Result<CommandOutput> {
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
    use std::cell::RefCell;
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
    }

    impl FixtureRunner {
        fn new(responses: impl IntoIterator<Item = FixtureResponse>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
                sleeps: RefCell::new(Vec::new()),
                download: DownloadMaterialize::None,
            }
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
            if !matches!(self.download, DownloadMaterialize::None)
                && command.program == "gh"
                && command.args.starts_with(&["release".to_owned(), "download".to_owned()])
            {
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

        let runner = FixtureRunner::new([success("")]);

        upload(&runner, &config, "demo-v1.0.0", &[asset]).expect("upload");

        let calls = runner.calls.into_inner();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "gh");
        assert!(
            calls[0]
                .args
                .ends_with(&["--repo", "example/project", "--clobber"].map(str::to_owned))
        );
        assert!(calls[0].args.iter().any(|arg| arg.ends_with("demo-linux.sha256")));
    }

    #[test]
    fn upload_retries_with_the_script_backoff_schedule() {
        let source_dir = tempdir().expect("source dir");
        let source = source_dir.path().join("binary");
        fs::write(&source, b"release bytes").expect("write source");
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([failure("first"), failure("second"), failure("third")]);

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
        assert_eq!(runner.calls.borrow().len(), 3);
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
        let runner = FixtureRunner::new([success("")]);

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
            calls[0].args.iter().any(|arg| arg.ends_with("Boss-1.0.10.zip")),
            "{:?}",
            calls[0].args
        );
        assert!(
            calls[0].args.iter().any(|arg| arg.ends_with("Boss-1.0.10.zip.sha256")),
            "{:?}",
            calls[0].args
        );

        let runner = FixtureRunner::new([success("")]);
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
        assert!(calls[0].args.iter().any(|arg| arg.ends_with("Boss-1.0.10.zip")));
    }

    #[test]
    fn publish_verifies_the_expanded_versioned_asset_name() {
        let config = ReleaseConfig::parse(BOSS_CONFIG).expect("config");
        let runner = FixtureRunner::new([success("Boss-1.0.10.zip\nBoss-1.0.10.zip.sha256\n"), success("")])
            .with_matching_download();

        publish(&runner, &config, "boss-v1.0.10").expect("publish versioned zip");

        let calls = runner.calls.into_inner();
        assert_eq!(calls[1].args[0..2], ["release", "download"]);
        assert!(
            calls[1]
                .args
                .windows(2)
                .any(|window| window == ["--pattern", "Boss-1.0.10.zip"]),
            "{:?}",
            calls[1].args
        );
    }

    #[test]
    fn prepare_creates_a_draft_and_records_the_tag_only_in_buildkite() {
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
            success(""),
            success(""),
        ]);

        let outcome = prepare(&runner, &config, "ui", Some("job-1"), false).expect("prepare");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.0".to_owned()
            }
        );
        let calls = runner.calls.into_inner();
        assert!(
            calls
                .iter()
                .any(|call| call.program == "gh" && call.args.starts_with(&["release".to_owned(), "create".to_owned()]))
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

        prepare(&runner, &config, "ui", None, false).expect_err("release create fails");

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
    fn manual_prepare_resumes_a_draft_for_head_but_a_schedule_refuses_it() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let root = tempdir().expect("repo root");
        let responses = [
            success(""),
            repo_root_response(root.path()),
            success(r#"[{"tag_name":"demo-v1.0.0","draft":true}]"#),
            success("head refs/tags/demo-v1.0.0\n"),
            success("head\n"),
            success("head\n"),
            success(""),
        ];
        let runner = FixtureRunner::new(responses);

        let outcome = prepare(&runner, &config, "ui", Some("job-1"), false).expect("manual resume");

        assert_eq!(
            outcome,
            PrepareOutcome::Resumed {
                tag: "demo-v1.0.0".to_owned()
            }
        );
        assert!(
            runner
                .calls
                .borrow()
                .iter()
                .any(|call| { call.args == ["meta-data", "set", BUILDKITE_TAG_KEY, "demo-v1.0.0"].map(str::to_owned) })
        );

        let schedule_runner = FixtureRunner::new([
            success(""),
            repo_root_response(root.path()),
            success(r#"[{"tag_name":"demo-v1.0.0","draft":true}]"#),
            success("head refs/tags/demo-v1.0.0\n"),
            success("head\n"),
            success("head\n"),
        ]);
        let error = prepare(&schedule_runner, &config, "schedule", None, false)
            .expect_err("scheduled prepare must not resume a draft");

        assert!(
            error.to_string().contains("do not resume drafts automatically"),
            "{error:#}"
        );
    }

    #[test]
    fn publish_verifies_the_downloaded_required_asset_before_editing_the_release() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner =
            FixtureRunner::new([success("demo-linux\ndemo-linux.sha256\n"), success("")]).with_matching_download();

        publish(&runner, &config, "demo-v1.0.0").expect("publish verified release");

        let calls = runner.calls.into_inner();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].args[0..2], ["release", "view"]);
        assert_eq!(calls[1].args[0..2], ["release", "download"]);
        assert_eq!(
            calls[2].args,
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
        let runner = FixtureRunner::new([success("demo-linux\n")]);

        let error = publish(&runner, &config, "demo-v1.0.0").expect_err("missing sidecar blocks publish");

        assert!(error.to_string().contains("demo-linux.sha256"), "{error:#}");
        assert_eq!(runner.calls.borrow().len(), 1);
        assert!(!published_draft(&runner));
    }

    #[test]
    fn publish_leaves_the_release_draft_on_checksum_mismatch() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([success("demo-linux\ndemo-linux.sha256\n")]).with_mismatched_download();

        let error = publish(&runner, &config, "demo-v1.0.0").expect_err("checksum mismatch blocks publish");

        assert!(error.to_string().contains("checksum mismatch"), "{error:#}");
        assert!(!published_draft(&runner));
    }

    #[test]
    fn publish_leaves_the_release_draft_when_optional_asset_lacks_sidecar() {
        let config = ReleaseConfig::parse(CONFIG).expect("config");
        let runner = FixtureRunner::new([success("demo-linux\ndemo-linux.sha256\ndemo-darwin\n")]);

        let error =
            publish(&runner, &config, "demo-v1.0.0").expect_err("optional asset without sidecar blocks publish");

        assert!(error.to_string().contains("without demo-darwin.sha256"), "{error:#}");
        assert_eq!(runner.calls.borrow().len(), 1);
        assert!(!published_draft(&runner));
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
}
