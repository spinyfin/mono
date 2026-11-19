//! What a release run does about draft releases an earlier run left unpublished.
//!
//! A run that fails after `prepare` leaves its tag and draft behind. Two cases
//! follow, and each has one automatic answer:
//!
//! - The draft is for the commit being released. A manual run resumes it. A
//!   scheduled run resumes it too, but only a configured number of times, and
//!   never while another run may still be building it. Once the attempts are
//!   used up scheduled runs fail until a person steps in, so a release that
//!   cannot be published is not retried forever.
//! - The draft is for a commit the branch has since moved past. No run will
//!   ever resume it, so a run that releases a newer commit deletes the draft
//!   and its tag.
//!
//! Whether another run may still be building a draft is read from the claim in
//! its notes (see [`crate::draft_claim`]). `prepare`, `upload` and `publish`
//! each date the claim when they start, so it shows when a run last worked on
//! the draft. A draft with no claim at all has an unknown age: the first run
//! to see it records the time and treats it as possibly in progress.
//!
//! Every one of these outcomes is reported on the build page as well as in the
//! job log.

use std::io::Write;

use anyhow::{Context, Result, bail};
use tempfile::NamedTempFile;

use crate::commands::{ensure_full_history, nonempty, resolve_tag_sha, run_checked, view_release};
use crate::draft_claim::DraftClaim;
use crate::release_state::family_drafts;
use crate::{Command, CommandRunner, GitHubRelease, RecoveryConfig, ReleaseConfig, Trigger};

/// Environment override that lets a scheduled build resume a draft regardless
/// of its claim and of the attempts already used.
pub(crate) const FORCE_RESUME_VARIABLE: &str = "RELEASE_RESUME_DRAFT";

/// A draft in the configured version family and what is known about it.
#[derive(Debug, Clone)]
pub(crate) struct FamilyDraft {
    pub counter: u64,
    pub release: GitHubRelease,
    /// The commit the draft's tag points at, when the tag still resolves.
    pub commit: Option<String>,
    pub claim: Option<DraftClaim>,
}

/// The facts about the current run that recovery decisions and reports use.
pub(crate) struct RecoveryContext<'a, R> {
    pub runner: &'a R,
    pub config: &'a ReleaseConfig,
    pub buildkite_job_id: Option<&'a str>,
    pub build_url: Option<&'a str>,
    pub head_sha: &'a str,
    /// The current time in seconds since the Unix epoch.
    pub now: u64,
}

/// How prominently a report is shown on the build page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    fn style(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Whether other runs must keep their hands off a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protection {
    /// A run worked on the draft recently enough that it may still be at it.
    InFlight { seconds_left: u64 },
    /// The draft has no claim, so nothing says how long ago a run worked on
    /// it. It might have been created a minute ago by a run that does not
    /// write claims.
    UnknownAge,
    /// Enough time has passed, or the pipeline cannot have two runs at once.
    None,
}

/// What to do with an unpublished draft for the commit being released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeadDraftAction {
    /// Continue the draft. `scheduled_attempt` numbers the attempt when a
    /// scheduled run resumes on its own; a resume a person asked for is not
    /// counted against the limit.
    Resume { scheduled_attempt: Option<u32> },
    /// Another run may still be building the draft, so leave it alone.
    Wait {
        minutes_left: u64,
        /// Scheduled resumes still allowed once the wait is over.
        attempts_left: u32,
        /// The draft had no claim. This run records one so that the wait has a
        /// start.
        first_sighting: bool,
    },
    /// Scheduled runs have used every attempt they are allowed.
    Stranded { attempts_used: u32 },
}

/// What `prepare` did with the draft for the commit being released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadDraftOutcome {
    Resumed,
    LeftInProgress,
}

/// What became of an older draft that a new release replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Abandonment {
    Deleted,
    /// It had no claim. The time was recorded and it was left for a later run.
    FirstSeen,
    Kept,
}

/// Decides how a run treats an unpublished draft for the commit it releases.
pub(crate) fn head_draft_action(
    trigger: Trigger,
    forced: bool,
    claim: Option<&DraftClaim>,
    now: u64,
    recovery: &RecoveryConfig,
) -> HeadDraftAction {
    if trigger != Trigger::Schedule || forced {
        return HeadDraftAction::Resume {
            scheduled_attempt: None,
        };
    }
    let attempts_used = claim.map_or(0, |claim| claim.scheduled_resumes);
    let attempts_left = recovery.scheduled_resume_attempts.saturating_sub(attempts_used);
    match protection(claim, now, recovery) {
        Protection::InFlight { seconds_left } => HeadDraftAction::Wait {
            minutes_left: seconds_left.div_ceil(60),
            attempts_left,
            first_sighting: false,
        },
        Protection::UnknownAge => HeadDraftAction::Wait {
            minutes_left: recovery.in_flight_minutes,
            attempts_left,
            first_sighting: true,
        },
        Protection::None if attempts_left == 0 => HeadDraftAction::Stranded { attempts_used },
        Protection::None => HeadDraftAction::Resume {
            scheduled_attempt: Some(attempts_used + 1),
        },
    }
}

/// Says whether a draft with this claim may still be in another run's hands.
///
/// A claim dated further ahead than the window is as stale as one that far
/// behind, so a wrong clock cannot protect a draft indefinitely. A window of
/// zero protects nothing: it declares that two runs never overlap.
fn protection(claim: Option<&DraftClaim>, now: u64, recovery: &RecoveryConfig) -> Protection {
    let window = recovery.in_flight_minutes.saturating_mul(60);
    if window == 0 {
        return Protection::None;
    }
    match claim {
        None => Protection::UnknownAge,
        Some(claim) => {
            let age = now.abs_diff(claim.claimed_at);
            if age < window {
                Protection::InFlight {
                    seconds_left: window - age,
                }
            } else {
                Protection::None
            }
        }
    }
}

/// Pairs every family draft with the commit its tag points at and the claim in
/// its notes, ordered from the highest counter down.
pub(crate) fn resolve_family_drafts(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    manifest_contents: Option<&str>,
    releases: &[GitHubRelease],
) -> Result<Vec<FamilyDraft>> {
    let mut drafts = Vec::new();
    for (counter, release) in family_drafts(releases, config, manifest_contents)? {
        let commit = resolve_tag_sha(runner, config, Some(&release))?;
        let claim = release.body.as_deref().and_then(DraftClaim::from_notes);
        drafts.push(FamilyDraft {
            counter,
            release,
            commit,
            claim,
        });
    }
    Ok(drafts)
}

/// The claim a run writes when it starts a new draft.
pub(crate) fn fresh_claim<R>(context: &RecoveryContext<'_, R>) -> DraftClaim {
    DraftClaim {
        claimed_at: context.now,
        scheduled_resumes: 0,
        build_url: context.build_url.map(str::to_owned),
    }
}

/// Resumes, waits on, or refuses the unpublished draft for the commit being
/// released, and reports which.
///
/// A resume is recorded on the draft before anything else happens, and is
/// refused if that record cannot be written. For a scheduled run the record is
/// what limits the attempts. For every run it is what tells other runs that
/// the draft is being worked on.
pub(crate) fn handle_head_draft<R: CommandRunner>(
    context: &RecoveryContext<'_, R>,
    draft: &FamilyDraft,
    trigger: Trigger,
    forced: bool,
) -> Result<HeadDraftOutcome> {
    let tag = draft.release.tag_name.as_str();
    let repo = context.config.repo.as_str();
    let recovery = &context.config.recovery;
    let last_run = last_run_phrase(draft.claim.as_ref());
    match head_draft_action(trigger, forced, draft.claim.as_ref(), context.now, recovery) {
        HeadDraftAction::Wait {
            minutes_left,
            attempts_left,
            first_sighting,
        } => {
            let seen = if first_sighting {
                record_claim(context.runner, repo, &draft.release, &first_sighting_claim(context)).with_context(
                    || {
                        format!(
                            "release {tag} is an unpublished draft for this commit with no record of when a run last worked on it, and that record could not be started"
                        )
                    },
                )?;
                "with no record of when a run last worked on it. This scheduled run recorded the time and leaves it alone, because it may still be building.".to_owned()
            } else {
                format!(
                    "that {last_run} was working on less than {} minutes ago, so it may still be building. This scheduled run leaves it alone.",
                    recovery.in_flight_minutes
                )
            };
            let next = if attempts_left == 0 {
                format!(
                    "Scheduled runs have no resume attempts left for it (the limit is {}), so if it is still unpublished in about {minutes_left} minutes they will report it as stranded and a person will have to retry or abandon it.",
                    recovery.scheduled_resume_attempts
                )
            } else {
                format!("If it is still unpublished in about {minutes_left} minutes, a scheduled run resumes it.")
            };
            report(
                context,
                Severity::Info,
                tag,
                &format!("Release `{tag}` for this commit is an unpublished draft {seen} {next}"),
            );
            Ok(HeadDraftOutcome::LeftInProgress)
        }
        HeadDraftAction::Stranded { attempts_used } => {
            let message = format!(
                "Release `{tag}` is stranded as an unpublished draft for this commit: scheduled runs resumed it {attempts_used} time(s), the limit is {}, and it is still not published. {} Scheduled runs will not retry it again. To retry it, start this pipeline by hand on this commit, or set `{FORCE_RESUME_VARIABLE}=1` on a scheduled build. To abandon it, run `gh release delete {tag} --repo {repo} --yes` and `git push origin :refs/tags/{tag}`. A release of a newer commit abandons it automatically.",
                recovery.scheduled_resume_attempts,
                sentence(&last_run, "last worked on it."),
            );
            annotate(context, Severity::Error, tag, &message);
            bail!(message)
        }
        HeadDraftAction::Resume { scheduled_attempt } => {
            let claim = DraftClaim {
                claimed_at: context.now,
                scheduled_resumes: scheduled_attempt
                    .unwrap_or_else(|| draft.claim.as_ref().map_or(0, |claim| claim.scheduled_resumes)),
                build_url: context.build_url.map(str::to_owned),
            };
            let recorded = record_claim(context.runner, repo, &draft.release, &claim);
            match (scheduled_attempt, recorded) {
                (Some(attempt), Err(error)) => bail!(
                    "release {tag} is an unpublished draft for this commit, but scheduled resume attempt {attempt} could not be recorded on it, and an attempt that is not recorded cannot be limited; not resuming: {error:#}"
                ),
                (None, Err(error)) => bail!(
                    "release {tag} is an unpublished draft for this commit, but this run's claim could not be recorded on it, and without it other runs cannot tell the draft is being worked on; not resuming: {error:#}"
                ),
                (Some(attempt), Ok(())) => report(
                    context,
                    Severity::Warning,
                    tag,
                    &format!(
                        "Release `{tag}` was left as an unpublished draft for this commit. {} This scheduled run is resuming it: attempt {attempt} of {}. It rebuilds, re-uploads and re-verifies every asset before publishing.",
                        sentence(&last_run, "did not publish it."),
                        recovery.scheduled_resume_attempts
                    ),
                ),
                (None, Ok(())) => eprintln!("resuming unpublished draft {tag} for this commit"),
            }
            Ok(HeadDraftOutcome::Resumed)
        }
    }
}

/// Deletes the drafts, and their tags, that `superseding_tag` replaces: those
/// with a lower counter whose commit is behind the one being released.
///
/// Nothing here can fail the release. A draft that cannot be judged or deleted
/// is reported and left for a later run. A draft is left untouched when a run
/// may still be building it, when its commit is not an ancestor of the commit
/// being released, when a published release uses the same tag, or when its tag
/// is gone and it can no longer be matched to a commit.
///
/// Call it only once `superseding_tag` exists on the remote. The version
/// counter is derived from the highest remote tag, so deleting the highest tag
/// first would hand its number to a different commit.
pub(crate) fn abandon_stale_drafts<R: CommandRunner>(
    context: &RecoveryContext<'_, R>,
    drafts: &[FamilyDraft],
    releases: &[GitHubRelease],
    superseding_counter: u64,
    superseding_tag: &str,
) {
    let mut abandoned = Vec::new();
    let mut first_seen = Vec::new();
    let mut unresolved = Vec::new();
    // Read on first use: most runs have no older draft to compare.
    let mut history = None;
    for draft in drafts.iter().filter(|draft| draft.counter < superseding_counter) {
        let tag = draft.release.tag_name.as_str();
        let Some(commit) = draft.commit.as_deref() else {
            unresolved.push(format!(
                "`{tag}`: its tag no longer exists, so it cannot be matched to a commit; if it is not wanted, delete it with `gh release delete {tag} --repo {} --yes`",
                context.config.repo
            ));
            continue;
        };
        if commit == context.head_sha {
            continue;
        }
        if let Protection::InFlight { .. } = protection(draft.claim.as_ref(), context.now, &context.config.recovery) {
            eprintln!(
                "draft release {tag} is for an older commit, but {} was working on it recently and may still be building it; leaving it alone",
                last_run_phrase(draft.claim.as_ref())
            );
            continue;
        }
        let history = history.get_or_insert_with(|| {
            ensure_full_history(
                context.runner,
                "comparing draft releases with the commit being released",
            )
        });
        let outcome = match history {
            Ok(()) => abandon_if_behind(context, draft, commit, releases),
            Err(error) => Err(anyhow::anyhow!("{error:#}")),
        };
        let label = format!(
            "`{tag}` at `{}`, last worked on by {}",
            short_sha(commit),
            last_run_phrase(draft.claim.as_ref())
        );
        match outcome {
            Ok(Abandonment::Deleted) => abandoned.push(label),
            Ok(Abandonment::FirstSeen) => first_seen.push(format!("`{tag}` at `{}`", short_sha(commit))),
            Ok(Abandonment::Kept) => {}
            Err(error) => unresolved.push(format!("{label}: {error:#}")),
        }
    }

    let mut sections = Vec::new();
    if !abandoned.is_empty() {
        sections.push(format!(
            "Abandoned {} draft release(s) that were never published and are behind the commit being released (`{}`), which ships as `{superseding_tag}`. Each draft and its tag were deleted:\n\n{}",
            abandoned.len(),
            short_sha(context.head_sha),
            bullets(&abandoned)
        ));
    }
    if !first_seen.is_empty() {
        sections.push(format!(
            "Found {} older unpublished draft release(s) with no record of when a run last worked on them, so they may still be building. Recorded the time on each. A release cut more than {} minutes from now abandons those that are still unpublished:\n\n{}",
            first_seen.len(),
            context.config.recovery.in_flight_minutes,
            bullets(&first_seen)
        ));
    }
    if !unresolved.is_empty() {
        sections.push(format!(
            "Could not clean up {} older unpublished draft release(s):\n\n{}",
            unresolved.len(),
            bullets(&unresolved)
        ));
    }
    if !sections.is_empty() {
        report(context, Severity::Warning, "abandoned-drafts", &sections.join("\n"));
    }
}

/// Reports on the build page that `phase` failed for `tag`, and what state
/// that leaves the release in. The state and the attempts left are read from
/// the release at that moment, so the report does not promise a resume that
/// will not happen.
pub(crate) fn report_unpublished<R: CommandRunner>(
    runner: &R,
    config: &ReleaseConfig,
    buildkite_job_id: Option<&str>,
    tag: &str,
    phase: &str,
    error: &anyhow::Error,
) {
    if nonempty(buildkite_job_id).is_none() {
        return;
    }
    let recovery = &config.recovery;
    let outlook = match view_release(runner, config, tag) {
        Err(_) => "The release could not be read afterwards. If a run for a newer commit abandoned it, that run's release replaces it.".to_owned(),
        Ok(view) if !view.is_draft => "The release itself is published.".to_owned(),
        Ok(view) => {
            let used = view
                .body
                .as_deref()
                .and_then(DraftClaim::from_notes)
                .map_or(0, |claim| claim.scheduled_resumes);
            let limit = recovery.scheduled_resume_attempts;
            let left = limit.saturating_sub(used);
            if left == 0 {
                format!(
                    "It is still an unpublished draft. Scheduled runs have no resume attempts left for it ({used} of {limit} used), so it stays a draft until someone starts this pipeline by hand on this commit, or until a release of a newer commit abandons it and deletes its tag."
                )
            } else {
                let wait = if recovery.in_flight_minutes == 0 {
                    String::new()
                } else {
                    format!(
                        ", once {} minutes have passed since a run last worked on it",
                        recovery.in_flight_minutes
                    )
                };
                format!(
                    "It is still an unpublished draft. A scheduled run on this commit resumes it automatically ({left} of {limit} attempts left){wait}. A release of a newer commit abandons it and deletes its tag."
                )
            }
        }
    };
    send_annotation(
        runner,
        Severity::Error,
        tag,
        &format!("Release `{tag}`: {phase} failed.\n\n```\n{error:#}\n```\n\n{outlook}"),
    );
}

/// Dates the claim on a draft to now, keeping the rest of it, so other runs
/// can see the draft is being worked on. `notes` must be the draft's notes as
/// just read. A draft without a claim gets one.
pub(crate) fn refresh_claim(
    runner: &impl CommandRunner,
    repo: &str,
    release_id: u64,
    tag: &str,
    notes: &str,
    now: u64,
) -> Result<()> {
    let previous = DraftClaim::from_notes(notes);
    let claim = DraftClaim {
        claimed_at: now,
        scheduled_resumes: previous.as_ref().map_or(0, |claim| claim.scheduled_resumes),
        build_url: previous.and_then(|claim| claim.build_url),
    };
    write_notes(runner, repo, release_id, tag, &claim.write_into(notes))
}

/// The claim recorded on a draft that had none: the time it was first seen,
/// and no run to credit it to.
fn first_sighting_claim<R>(context: &RecoveryContext<'_, R>) -> DraftClaim {
    DraftClaim {
        claimed_at: context.now,
        scheduled_resumes: 0,
        build_url: None,
    }
}

/// Reads one release from GitHub by id.
fn read_release(runner: &impl CommandRunner, repo: &str, release_id: u64) -> Result<GitHubRelease> {
    let output = run_checked(
        runner,
        Command::new("gh", ["api".to_owned(), format!("repos/{repo}/releases/{release_id}")]),
    )?;
    serde_json::from_str(&output.stdout).context("GitHub's description of the release could not be read")
}

/// Writes `claim` into the notes of the draft `release`.
///
/// The notes are read again first, so the write starts from what the draft
/// holds now and not from the release list read earlier in the run. It is
/// refused if the release has been published in the meantime.
fn record_claim(runner: &impl CommandRunner, repo: &str, release: &GitHubRelease, claim: &DraftClaim) -> Result<()> {
    let tag = release.tag_name.as_str();
    if release.id == 0 {
        bail!("GitHub did not report an id for draft {tag}; cannot edit it");
    }
    let current = read_release(runner, repo, release.id).with_context(|| format!("could not read draft {tag}"))?;
    if !current.draft {
        bail!("release {tag} is no longer a draft");
    }
    write_notes(
        runner,
        repo,
        release.id,
        tag,
        &claim.write_into(current.body.as_deref().unwrap_or_default()),
    )
}

/// Replaces the notes of one release. Only the notes are sent, so the edit
/// cannot publish the release or change anything else about it.
fn write_notes(runner: &impl CommandRunner, repo: &str, release_id: u64, tag: &str, notes: &str) -> Result<()> {
    if release_id == 0 {
        bail!("GitHub did not report an id for release {tag}; cannot edit it");
    }
    let mut payload = NamedTempFile::new().context("could not create a file for the release notes")?;
    serde_json::to_writer(&mut payload, &serde_json::json!({ "body": notes }))
        .context("could not write the release notes")?;
    payload.flush().context("could not write the release notes")?;
    run_checked(
        runner,
        Command::new(
            "gh",
            [
                "api".to_owned(),
                "-X".to_owned(),
                "PATCH".to_owned(),
                format!("repos/{repo}/releases/{release_id}"),
                "--input".to_owned(),
                payload.path().display().to_string(),
            ],
        ),
    )
    .with_context(|| format!("could not update the notes of release {tag}"))?;
    Ok(())
}

/// Deletes `draft` and its tag when `commit` is behind the commit being
/// released and no run can still be working on it.
///
/// The draft is read again immediately before it is deleted. GitHub offers no
/// way to delete a release only if it is still a draft, so this cannot rule
/// out a publish landing between the read and the delete; it narrows that to
/// the moment between two calls. `publish` dates the claim before it starts
/// verifying, so a publish that is under way is seen here as in progress.
fn abandon_if_behind<R: CommandRunner>(
    context: &RecoveryContext<'_, R>,
    draft: &FamilyDraft,
    commit: &str,
    releases: &[GitHubRelease],
) -> Result<Abandonment> {
    let runner = context.runner;
    let repo = context.config.repo.as_str();
    let recovery = &context.config.recovery;
    let tag = draft.release.tag_name.as_str();
    if releases.iter().any(|release| !release.draft && release.tag_name == tag) {
        bail!("a published release also uses this tag, so the tag must stay; delete the draft by hand");
    }
    if draft.release.id == 0 {
        bail!("GitHub did not report an id for the draft; delete it by hand");
    }

    let merge_base = run_checked(runner, Command::new("git", ["merge-base", commit, context.head_sha]))
        .context("could not tell whether its commit is behind the commit being released")?;
    if merge_base.stdout.trim() != commit {
        eprintln!("draft release {tag} is for a commit that is not behind the one being released; leaving it alone");
        return Ok(Abandonment::Kept);
    }

    if protection(draft.claim.as_ref(), context.now, recovery) == Protection::UnknownAge {
        record_claim(runner, repo, &draft.release, &first_sighting_claim(context))
            .context("it has no record of when a run last worked on it, and that record could not be started")?;
        return Ok(Abandonment::FirstSeen);
    }

    let current = read_release(runner, repo, draft.release.id).context("could not confirm it is still a draft")?;
    if !current.draft {
        eprintln!("release {tag} was published while this run was preparing; leaving it alone");
        return Ok(Abandonment::Kept);
    }
    let current_claim = current.body.as_deref().and_then(DraftClaim::from_notes);
    if protection(current_claim.as_ref(), context.now, recovery) != Protection::None {
        eprintln!("a run started working on draft release {tag} while this run was preparing; leaving it alone");
        return Ok(Abandonment::Kept);
    }

    let release_path = format!("repos/{repo}/releases/{}", draft.release.id);
    run_checked(
        runner,
        Command::new("gh", ["api", "-X", "DELETE", release_path.as_str()]),
    )
    .context("could not delete the draft release")?;
    let tag_ref = format!(":refs/tags/{tag}");
    run_checked(runner, Command::new("git", ["push", "origin", tag_ref.as_str()])).with_context(|| {
        format!(
            "deleted the draft release, but its tag is still on the remote; delete it with `git push origin {tag_ref}`"
        )
    })?;
    let _ = runner.run(&Command::new("git", ["tag", "-d", tag]));
    Ok(Abandonment::Deleted)
}

/// Prints `message` to the job log and, inside Buildkite, to the build page.
fn report<R: CommandRunner>(context: &RecoveryContext<'_, R>, severity: Severity, subject: &str, message: &str) {
    eprintln!("{message}");
    annotate(context, severity, subject, message);
}

fn annotate<R: CommandRunner>(context: &RecoveryContext<'_, R>, severity: Severity, subject: &str, message: &str) {
    if nonempty(context.buildkite_job_id).is_some() {
        send_annotation(context.runner, severity, subject, message);
    }
}

/// Adds an annotation to the Buildkite build. Annotations sharing a subject
/// replace one another, so a build shows the latest word on each release. A
/// failure to annotate is logged and otherwise ignored: the report is an aid,
/// and the release must not depend on it.
fn send_annotation(runner: &impl CommandRunner, severity: Severity, subject: &str, message: &str) {
    let command = Command::new(
        "buildkite-agent",
        [
            "annotate",
            message,
            "--style",
            severity.style(),
            "--context",
            &format!("release-{subject}"),
        ],
    );
    match runner.run(&command) {
        Ok(output) if output.success => {}
        Ok(output) => eprintln!("warning: could not annotate the build: {}", output.stderr.trim()),
        Err(error) => eprintln!("warning: could not annotate the build: {error}"),
    }
}

/// Names the run that created or last resumed a draft, for use as the subject
/// of a sentence.
fn last_run_phrase(claim: Option<&DraftClaim>) -> String {
    match claim.and_then(|claim| claim.build_url.as_deref()) {
        Some(url) => format!("the run at {url}"),
        None if claim.is_some() => "another run".to_owned(),
        None => "an earlier run".to_owned(),
    }
}

/// Builds a sentence from a lower-case subject and the rest of the sentence.
fn sentence(subject: &str, rest: &str) -> String {
    let mut characters = subject.chars();
    let subject = match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect::<String>(),
        None => String::new(),
    };
    format!("{subject} {rest}")
}

fn bullets(items: &[String]) -> String {
    items.iter().map(|item| format!("- {item}\n")).collect()
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::commands::{BUILDKITE_TAG_KEY, PrepareOutcome, PrepareRequest, prepare};
    use crate::{CommandOutput, RunnerError};

    fn recovery(scheduled_resume_attempts: u32, in_flight_minutes: u64) -> RecoveryConfig {
        RecoveryConfig {
            scheduled_resume_attempts,
            in_flight_minutes,
        }
    }

    fn claim(claimed_at: u64, scheduled_resumes: u32) -> DraftClaim {
        DraftClaim {
            claimed_at,
            scheduled_resumes,
            build_url: None,
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn a_manual_run_resumes_whatever_the_claim_says() {
        let fresh = claim(NOW - 10, 99);

        for trigger in [Trigger::Ui, Trigger::Api] {
            assert_eq!(
                head_draft_action(trigger, false, Some(&fresh), NOW, &recovery(3, 60)),
                HeadDraftAction::Resume {
                    scheduled_attempt: None
                }
            );
        }
    }

    #[test]
    fn a_forced_scheduled_run_resumes_without_using_an_attempt() {
        assert_eq!(
            head_draft_action(
                Trigger::Schedule,
                true,
                Some(&claim(NOW - 10, 3)),
                NOW,
                &recovery(3, 60)
            ),
            HeadDraftAction::Resume {
                scheduled_attempt: None
            }
        );
    }

    #[test]
    fn a_scheduled_run_waits_while_a_run_worked_on_the_draft_recently() {
        assert_eq!(
            head_draft_action(
                Trigger::Schedule,
                false,
                Some(&claim(NOW - 10 * 60, 0)),
                NOW,
                &recovery(3, 60)
            ),
            HeadDraftAction::Wait {
                minutes_left: 50,
                attempts_left: 3,
                first_sighting: false
            }
        );
        assert_eq!(
            head_draft_action(
                Trigger::Schedule,
                false,
                Some(&claim(NOW - 59 * 60 - 1, 1)),
                NOW,
                &recovery(3, 60)
            ),
            HeadDraftAction::Wait {
                minutes_left: 1,
                attempts_left: 2,
                first_sighting: false
            }
        );
    }

    #[test]
    fn a_wait_says_when_no_attempts_will_be_left_after_it() {
        assert_eq!(
            head_draft_action(
                Trigger::Schedule,
                false,
                Some(&claim(NOW - 10 * 60, 3)),
                NOW,
                &recovery(3, 60)
            ),
            HeadDraftAction::Wait {
                minutes_left: 50,
                attempts_left: 0,
                first_sighting: false
            }
        );
    }

    #[test]
    fn a_scheduled_run_resumes_once_the_claim_has_lapsed_and_counts_the_attempt() {
        assert_eq!(
            head_draft_action(
                Trigger::Schedule,
                false,
                Some(&claim(NOW - 60 * 60, 0)),
                NOW,
                &recovery(3, 60)
            ),
            HeadDraftAction::Resume {
                scheduled_attempt: Some(1)
            }
        );
        assert_eq!(
            head_draft_action(
                Trigger::Schedule,
                false,
                Some(&claim(NOW - 2 * 60 * 60, 2)),
                NOW,
                &recovery(3, 60)
            ),
            HeadDraftAction::Resume {
                scheduled_attempt: Some(3)
            }
        );
    }

    #[test]
    fn a_scheduled_run_stops_when_the_attempts_are_used_up() {
        assert_eq!(
            head_draft_action(
                Trigger::Schedule,
                false,
                Some(&claim(NOW - 2 * 60 * 60, 3)),
                NOW,
                &recovery(3, 60)
            ),
            HeadDraftAction::Stranded { attempts_used: 3 }
        );
    }

    #[test]
    fn a_limit_of_zero_never_resumes_from_a_schedule() {
        assert_eq!(
            head_draft_action(Trigger::Schedule, false, None, NOW, &recovery(0, 0)),
            HeadDraftAction::Stranded { attempts_used: 0 }
        );
    }

    #[test]
    fn a_draft_of_unknown_age_is_given_the_whole_window_first() {
        assert_eq!(
            head_draft_action(Trigger::Schedule, false, None, NOW, &recovery(3, 60)),
            HeadDraftAction::Wait {
                minutes_left: 60,
                attempts_left: 3,
                first_sighting: true
            }
        );
    }

    #[test]
    fn a_zero_window_never_waits() {
        assert_eq!(
            head_draft_action(Trigger::Schedule, false, Some(&claim(NOW, 1)), NOW, &recovery(3, 0)),
            HeadDraftAction::Resume {
                scheduled_attempt: Some(2)
            }
        );
        assert_eq!(
            head_draft_action(Trigger::Schedule, false, None, NOW, &recovery(3, 0)),
            HeadDraftAction::Resume {
                scheduled_attempt: Some(1)
            }
        );
    }

    #[test]
    fn a_claim_dated_far_ahead_does_not_protect_its_draft() {
        let ahead = claim(NOW + 30, 0);
        let far_ahead = claim(NOW + 10 * 60 * 60, 0);

        assert_eq!(
            head_draft_action(Trigger::Schedule, false, Some(&ahead), NOW, &recovery(3, 60)),
            HeadDraftAction::Wait {
                minutes_left: 60,
                attempts_left: 3,
                first_sighting: false
            }
        );
        assert_eq!(
            head_draft_action(Trigger::Schedule, false, Some(&far_ahead), NOW, &recovery(3, 60)),
            HeadDraftAction::Resume {
                scheduled_attempt: Some(1)
            }
        );
    }

    #[test]
    fn the_last_run_is_named_by_its_build_when_known() {
        let with_build = DraftClaim {
            claimed_at: NOW,
            scheduled_resumes: 0,
            build_url: Some("https://ci.example/builds/7".to_owned()),
        };

        assert_eq!(
            sentence(&last_run_phrase(Some(&with_build)), "did not publish it."),
            "The run at https://ci.example/builds/7 did not publish it."
        );
        assert_eq!(
            sentence(&last_run_phrase(Some(&claim(NOW, 0))), "did not publish it."),
            "Another run did not publish it."
        );
        assert_eq!(
            sentence(&last_run_phrase(None), "did not publish it."),
            "An earlier run did not publish it."
        );
    }

    #[test]
    fn refreshing_a_claim_keeps_its_count_and_its_run() {
        let forge = FakeForge::new();
        let notes = DraftClaim {
            claimed_at: NOW - 3 * 60 * 60,
            scheduled_resumes: 2,
            build_url: Some(BUILD.to_owned()),
        }
        .write_into("## Changes\n");

        refresh_claim(&forge, "example/project", 9, "demo-v1.0.9", &notes, NOW).expect("refresh");

        assert_eq!(
            forge.notes_written_to(9).as_deref(),
            Some(
                "## Changes\n\n<!-- release-tool: claimed-at=1800000000 scheduled-resumes=2 build=https://ci.example/builds/41 -->\n"
            )
        );
    }

    #[test]
    fn refreshing_gives_a_draft_without_a_claim_one() {
        let forge = FakeForge::new();

        refresh_claim(&forge, "example/project", 9, "demo-v1.0.9", "## Changes\n", NOW).expect("refresh");

        assert_eq!(
            forge.notes_written_to(9).as_deref(),
            Some("## Changes\n\n<!-- release-tool: claimed-at=1800000000 scheduled-resumes=0 -->\n")
        );
    }

    const CONFIG: &str = r#"
repo = "example/project"
tag_prefix = "demo-v"
title_prefix = "Demo"
change_paths = ["tool/"]
required_assets = ["demo-linux"]
optional_assets = []

[version]
scheme = "patch-counter"
major_minor = "1.0"

[notes]
source = "github-generated"
"#;

    const HEAD: &str = "c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7";
    const BUILD: &str = "https://ci.example/builds/41";

    /// A stand-in for git, gh and buildkite-agent that answers by what it is
    /// asked, not by the order of the calls.
    struct FakeForge {
        root: TempDir,
        releases: Vec<serde_json::Value>,
        /// Tag name and the commit it points at.
        tags: Vec<(String, String)>,
        /// Commits that are ancestors of `HEAD`.
        behind_head: Vec<String>,
        /// Commands, by the start of their command line, that fail.
        failing: Vec<&'static str>,
        /// Releases, by id, that turn out to be published when read again.
        published_meanwhile: Vec<u64>,
        /// Releases, by id, that a run turns out to have started working on
        /// when they are read again.
        claimed_meanwhile: Vec<u64>,
        calls: RefCell<Vec<String>>,
        /// Notes written to a release, by the release's API path.
        written_notes: RefCell<Vec<(String, String)>>,
    }

    impl FakeForge {
        fn new() -> Self {
            Self {
                root: tempdir().expect("repo root"),
                releases: Vec::new(),
                tags: Vec::new(),
                behind_head: Vec::new(),
                failing: Vec::new(),
                published_meanwhile: Vec::new(),
                claimed_meanwhile: Vec::new(),
                calls: RefCell::new(Vec::new()),
                written_notes: RefCell::new(Vec::new()),
            }
        }

        fn published(mut self, counter: u64, commit: &str) -> Self {
            self.add_release(counter, commit, false, "notes\n".to_owned());
            self.behind_head.push(commit.to_owned());
            self
        }

        /// A draft with the given notes, tagged at `commit`.
        fn draft(mut self, counter: u64, commit: &str, notes: &str) -> Self {
            self.add_release(counter, commit, true, notes.to_owned());
            self
        }

        /// A draft whose tag has been deleted from the repository.
        fn untagged_draft(mut self, counter: u64) -> Self {
            self.releases.push(serde_json::json!({
                "id": counter,
                "tag_name": format!("demo-v1.0.{counter}"),
                "draft": true,
                "body": "## Changes\n",
            }));
            self
        }

        fn behind_head(mut self, commit: &str) -> Self {
            self.behind_head.push(commit.to_owned());
            self
        }

        fn failing(mut self, command_line: &'static str) -> Self {
            self.failing.push(command_line);
            self
        }

        fn published_meanwhile(mut self, counter: u64) -> Self {
            self.published_meanwhile.push(counter);
            self
        }

        fn claimed_meanwhile(mut self, counter: u64) -> Self {
            self.claimed_meanwhile.push(counter);
            self
        }

        /// Release ids equal their counter, which keeps scenarios readable.
        fn add_release(&mut self, counter: u64, commit: &str, draft: bool, notes: String) {
            let tag = format!("demo-v1.0.{counter}");
            self.releases
                .push(serde_json::json!({ "id": counter, "tag_name": tag, "draft": draft, "body": notes }));
            if !self.tags.iter().any(|(existing, _)| *existing == tag) {
                self.tags.push((tag, commit.to_owned()));
            }
        }

        /// The release as GitHub would describe it now, which is how the
        /// tool sees what changed since it read the release list.
        fn release_now(&self, id: u64) -> Option<serde_json::Value> {
            let mut release = self
                .releases
                .iter()
                .find(|release| release["id"] == id && release["draft"] == true)?
                .clone();
            if self.published_meanwhile.contains(&id) {
                release["draft"] = serde_json::json!(false);
            }
            if self.claimed_meanwhile.contains(&id) {
                release["body"] = serde_json::json!(claimed(0, 0));
            }
            Some(release)
        }

        fn ran(&self, command_line: &str) -> bool {
            self.calls.borrow().iter().any(|call| call.starts_with(command_line))
        }

        fn position(&self, command_line: &str) -> Option<usize> {
            self.calls
                .borrow()
                .iter()
                .position(|call| call.starts_with(command_line))
        }

        /// The message and style of the first annotation this run added.
        fn annotation(&self) -> Option<(String, String)> {
            let calls = self.calls.borrow();
            let call = calls
                .iter()
                .find(|call| call.starts_with("buildkite-agent annotate "))?;
            let (message, rest) = call
                .trim_start_matches("buildkite-agent annotate ")
                .rsplit_once(" --style ")?;
            let style = rest.split(' ').next()?;
            Some((message.to_owned(), style.to_owned()))
        }

        fn notes_written_to(&self, counter: u64) -> Option<String> {
            let path = format!("repos/example/project/releases/{counter}");
            self.written_notes
                .borrow()
                .iter()
                .rev()
                .find(|(written, _)| *written == path)
                .map(|(_, notes)| notes.clone())
        }
    }

    fn ok(stdout: &str) -> std::result::Result<CommandOutput, RunnerError> {
        Ok(CommandOutput {
            success: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    fn fails(stderr: &str) -> std::result::Result<CommandOutput, RunnerError> {
        Ok(CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        })
    }

    impl CommandRunner for FakeForge {
        fn run(&self, command: &Command) -> std::result::Result<CommandOutput, RunnerError> {
            let line = std::iter::once(command.program.as_str())
                .chain(command.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.borrow_mut().push(line.clone());
            if self.failing.iter().any(|failing| line.starts_with(failing)) {
                return fails("scripted failure");
            }
            let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
            match (command.program.as_str(), args.as_slice()) {
                ("git", ["fetch", ..]) | ("git", ["push", ..]) | ("git", ["tag", ..]) => ok(""),
                ("git", ["rev-parse", "--show-toplevel"]) => ok(&format!("{}\n", self.root.path().display())),
                ("git", ["rev-parse", "HEAD"]) => ok(&format!("{HEAD}\n")),
                ("git", ["rev-parse", "--is-shallow-repository"]) => ok("false\n"),
                // No tag the tool is about to create exists yet.
                ("git", ["rev-parse", "--verify", _]) => fails("not a valid ref"),
                ("git", ["ls-remote", ..]) => ok(&self
                    .tags
                    .iter()
                    .map(|(tag, commit)| format!("{commit}\trefs/tags/{tag}\n"))
                    .collect::<String>()),
                ("git", ["rev-list", "-n", "1", tag]) => match self.tags.iter().find(|(existing, _)| existing == tag) {
                    Some((_, commit)) => ok(&format!("{commit}\n")),
                    None => fails("unknown revision"),
                },
                ("git", ["diff", "--name-only", _]) => ok("tool/src/main.rs\n"),
                ("git", ["merge-base", commit, _]) => {
                    if self.behind_head.iter().any(|behind| behind == commit) {
                        ok(&format!("{commit}\n"))
                    } else {
                        ok("0000000000000000000000000000000000000000\n")
                    }
                }
                ("gh", ["api", "repos/example/project/releases", ..]) => {
                    ok(&serde_json::Value::Array(self.releases.clone()).to_string())
                }
                // One release, read again by id.
                ("gh", ["api", path]) => {
                    let id = path.rsplit('/').next().and_then(|id| id.parse().ok());
                    match id.and_then(|id| self.release_now(id)) {
                        Some(release) => ok(&release.to_string()),
                        None => fails("Not Found (HTTP 404)"),
                    }
                }
                // A tag that is not in the repository has no commit on GitHub either.
                ("gh", ["api", _, "--jq", ".sha"]) => fails("Not Found (HTTP 404)"),
                ("gh", ["api", "-X", "DELETE", _]) => ok(""),
                ("gh", ["api", "-X", "PATCH", path, "--input", file]) => {
                    let payload: serde_json::Value =
                        serde_json::from_str(&std::fs::read_to_string(file).expect("read payload")).expect("payload");
                    self.written_notes
                        .borrow_mut()
                        .push(((*path).to_owned(), payload["body"].as_str().expect("body").to_owned()));
                    ok("")
                }
                ("gh", ["release", "view", tag, ..]) => {
                    match self.releases.iter().find(|release| release["tag_name"] == *tag) {
                        Some(release) => ok(&serde_json::json!({
                            "apiUrl": format!("https://api.github.com/repos/example/project/releases/{}", release["id"]),
                            "assets": [],
                            "body": release["body"],
                            "isDraft": release["draft"],
                        })
                        .to_string()),
                        None => fails("release not found"),
                    }
                }
                ("gh", ["release", "create", ..]) | ("buildkite-agent", _) => ok(""),
                _ => panic!("the fake forge was asked something it has no answer for: {line}"),
            }
        }

        fn sleep(&self, _duration: Duration) {}

        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(NOW)
        }
    }

    fn config(recovery: &str) -> ReleaseConfig {
        ReleaseConfig::parse(&format!("{CONFIG}\n[recovery]\n{recovery}\n")).expect("config")
    }

    /// A pipeline whose phases share one serialized job, as Boss's do.
    fn one_job_config() -> ReleaseConfig {
        config("in_flight_minutes = 0")
    }

    fn scheduled() -> PrepareRequest<'static> {
        PrepareRequest {
            trigger_source: "schedule",
            buildkite_job_id: Some("job-1"),
            build_url: Some("https://ci.example/builds/42"),
            resume_draft: false,
        }
    }

    /// Notes carrying a claim dated `minutes_ago` by the build at `BUILD`.
    fn claimed(minutes_ago: u64, scheduled_resumes: u32) -> String {
        DraftClaim {
            claimed_at: NOW - minutes_ago * 60,
            scheduled_resumes,
            build_url: Some(BUILD.to_owned()),
        }
        .write_into("## Changes\n")
    }

    #[test]
    fn a_scheduled_run_resumes_a_stranded_draft_and_counts_the_attempt() {
        let forge = FakeForge::new().published(4, "c4").draft(5, HEAD, &claimed(120, 1));

        let outcome = prepare(&forge, &config(""), scheduled()).expect("scheduled resume");

        assert_eq!(
            outcome,
            PrepareOutcome::Resumed {
                tag: "demo-v1.0.5".to_owned()
            }
        );
        assert_eq!(
            forge.notes_written_to(5).as_deref(),
            Some(
                "## Changes\n\n<!-- release-tool: claimed-at=1800000000 scheduled-resumes=2 build=https://ci.example/builds/42 -->\n"
            ),
            "the attempt is counted on the draft and its notes are kept"
        );
        let (message, style) = forge.annotation().expect("the resume is reported on the build page");
        assert_eq!(style, "warning");
        assert!(message.contains("`demo-v1.0.5`"), "{message}");
        assert!(message.contains("attempt 2 of 3"), "{message}");
        assert!(
            message.contains(BUILD),
            "the report names the run that left it: {message}"
        );
        assert!(forge.ran(&format!(
            "buildkite-agent meta-data set {BUILDKITE_TAG_KEY} demo-v1.0.5"
        )));
        assert!(!forge.ran("gh release create"), "a resume cuts no new version");
        assert!(
            forge.position("gh api -X PATCH").expect("claim recorded")
                < forge.position("buildkite-agent meta-data set").expect("tag recorded"),
            "the attempt is recorded before later phases are told to build"
        );
    }

    #[test]
    fn a_claim_is_written_onto_the_notes_as_they_are_now() {
        // The release list is read at the start of prepare. A claim written
        // from that copy would undo whatever changed on the draft since.
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, HEAD, &claimed(120, 1))
            .claimed_meanwhile(5);

        prepare(&forge, &config(""), scheduled()).expect("scheduled resume");

        assert!(
            forge
                .position("gh api repos/example/project/releases/5")
                .expect("read again")
                < forge.position("gh api -X PATCH").expect("claim recorded")
        );
    }

    #[test]
    fn a_draft_without_a_claim_is_given_time_before_a_scheduled_run_resumes_it() {
        let forge = FakeForge::new().published(4, "c4").draft(5, HEAD, "## Changes\n");

        let outcome = prepare(&forge, &config(""), scheduled()).expect("nothing to do yet");

        assert_eq!(
            outcome,
            PrepareOutcome::InProgress {
                tag: "demo-v1.0.5".to_owned()
            },
            "a draft of unknown age may have been created a minute ago"
        );
        assert_eq!(
            forge.notes_written_to(5).as_deref(),
            Some("## Changes\n\n<!-- release-tool: claimed-at=1800000000 scheduled-resumes=0 -->\n"),
            "the time is recorded so the wait has an end"
        );
        let (message, style) = forge.annotation().expect("the wait is reported");
        assert_eq!(style, "info");
        assert!(
            message.contains("no record of when a run last worked on it"),
            "{message}"
        );
        assert!(message.contains("about 60 minutes"), "{message}");
        assert!(!forge.ran("buildkite-agent meta-data set"));
    }

    #[test]
    fn a_draft_without_a_claim_is_resumed_at_once_when_runs_cannot_overlap() {
        let forge = FakeForge::new().published(4, "c4").draft(5, HEAD, "## Changes\n");

        let outcome = prepare(&forge, &one_job_config(), scheduled()).expect("scheduled resume");

        assert_eq!(
            outcome,
            PrepareOutcome::Resumed {
                tag: "demo-v1.0.5".to_owned()
            }
        );
        let notes = forge.notes_written_to(5).expect("claim recorded");
        assert!(notes.contains("scheduled-resumes=1"), "{notes}");
    }

    #[test]
    fn a_scheduled_run_leaves_a_draft_alone_while_a_run_may_be_building_it() {
        let forge = FakeForge::new().published(4, "c4").draft(5, HEAD, &claimed(5, 0));

        let outcome = prepare(&forge, &config(""), scheduled()).expect("nothing to do");

        assert_eq!(
            outcome,
            PrepareOutcome::InProgress {
                tag: "demo-v1.0.5".to_owned()
            }
        );
        let (message, style) = forge.annotation().expect("the wait is reported, not silent");
        assert_eq!(style, "info");
        assert!(message.contains("may still be building"), "{message}");
        assert!(
            message.contains("in about 55 minutes, a scheduled run resumes it"),
            "{message}"
        );
        assert!(!forge.ran("gh api -X PATCH"), "another run's claim is not overwritten");
        assert!(
            !forge.ran("buildkite-agent meta-data set"),
            "no later phase is told to build"
        );
        assert!(!forge.ran("gh release create"));
    }

    #[test]
    fn a_wait_does_not_promise_a_resume_when_the_attempts_are_used_up() {
        let forge = FakeForge::new().published(4, "c4").draft(5, HEAD, &claimed(5, 3));

        let outcome = prepare(&forge, &config(""), scheduled()).expect("nothing to do");

        assert_eq!(
            outcome,
            PrepareOutcome::InProgress {
                tag: "demo-v1.0.5".to_owned()
            }
        );
        let (message, _) = forge.annotation().expect("the wait is reported");
        assert!(message.contains("no resume attempts left"), "{message}");
        assert!(message.contains("report it as stranded"), "{message}");
        assert!(!message.contains("a scheduled run resumes it"), "{message}");
    }

    #[test]
    fn a_scheduled_run_refuses_a_draft_whose_attempts_are_used_up() {
        let forge = FakeForge::new().published(4, "c4").draft(5, HEAD, &claimed(120, 3));

        let error = prepare(&forge, &config(""), scheduled()).expect_err("a stuck release is not retried forever");

        let rendered = format!("{error:#}");
        assert!(rendered.contains("`demo-v1.0.5` is stranded"), "{rendered}");
        assert!(rendered.contains("resumed it 3 time(s), the limit is 3"), "{rendered}");
        assert!(rendered.contains("RELEASE_RESUME_DRAFT=1"), "{rendered}");
        assert!(
            rendered.contains("gh release delete demo-v1.0.5 --repo example/project --yes"),
            "{rendered}"
        );
        assert!(rendered.contains(BUILD), "{rendered}");
        let (message, style) = forge.annotation().expect("the refusal is reported on the build page");
        assert_eq!(style, "error");
        assert_eq!(message, rendered);
        assert!(!forge.ran("gh api -X PATCH"));
        assert!(!forge.ran("buildkite-agent meta-data set"));
    }

    #[test]
    fn no_run_resumes_a_draft_it_cannot_put_its_claim_on() {
        for (trigger_source, reason) in [
            (
                "schedule",
                "could not be recorded on it, and an attempt that is not recorded",
            ),
            ("ui", "this run's claim could not be recorded"),
        ] {
            let forge = FakeForge::new()
                .published(4, "c4")
                .draft(5, HEAD, &claimed(120, 0))
                .failing("gh api -X PATCH");
            let request = PrepareRequest {
                trigger_source,
                ..scheduled()
            };

            let error = prepare(&forge, &config(""), request).expect_err("an unrecorded resume is refused");

            assert!(format!("{error:#}").contains(reason), "{trigger_source}: {error:#}");
            assert!(!forge.ran("buildkite-agent meta-data set"), "{trigger_source}");
        }
    }

    #[test]
    fn a_draft_published_since_the_list_was_read_is_not_resumed() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, HEAD, &claimed(120, 0))
            .published_meanwhile(5);
        let request = PrepareRequest {
            trigger_source: "ui",
            ..scheduled()
        };

        let error = prepare(&forge, &config(""), request).expect_err("there is no draft left to resume");

        assert!(format!("{error:#}").contains("no longer a draft"), "{error:#}");
        assert!(!forge.ran("gh api -X PATCH"), "published notes are not written to");
    }

    #[test]
    fn the_operator_override_resumes_a_draft_whose_attempts_are_used_up() {
        let forge = FakeForge::new().published(4, "c4").draft(5, HEAD, &claimed(1, 3));
        let request = PrepareRequest {
            resume_draft: true,
            ..scheduled()
        };

        let outcome = prepare(&forge, &config(""), request).expect("forced resume");

        assert_eq!(
            outcome,
            PrepareOutcome::Resumed {
                tag: "demo-v1.0.5".to_owned()
            }
        );
        let notes = forge.notes_written_to(5).expect("claim recorded");
        assert!(
            notes.contains("scheduled-resumes=3"),
            "an override neither uses nor resets the attempts: {notes}"
        );
    }

    #[test]
    fn a_new_release_abandons_the_drafts_the_branch_has_moved_past() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "c5c5c5c5c5", "## Changes\n")
            .behind_head("c5c5c5c5c5")
            .draft(6, "c6c6c6c6c6", &claimed(120, 2))
            .behind_head("c6c6c6c6c6");

        let outcome = prepare(&forge, &one_job_config(), scheduled()).expect("fresh release");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.7".to_owned()
            },
            "the abandoned numbers are not handed out again"
        );
        for counter in [5, 6] {
            assert!(forge.ran(&format!("gh api -X DELETE repos/example/project/releases/{counter}")));
            assert!(forge.ran(&format!("git push origin :refs/tags/demo-v1.0.{counter}")));
            assert!(forge.ran(&format!("git tag -d demo-v1.0.{counter}")));
        }
        assert!(
            forge.position("gh release create demo-v1.0.7").expect("new draft")
                < forge.position("gh api -X DELETE").expect("old draft deleted"),
            "the new tag must exist before an older one is deleted"
        );
        let (message, style) = forge
            .annotation()
            .expect("abandoned drafts are reported on the build page");
        assert_eq!(style, "warning");
        assert!(message.contains("Abandoned 2 draft release(s)"), "{message}");
        assert!(message.contains("ships as `demo-v1.0.7`"), "{message}");
        assert!(
            message.contains("`demo-v1.0.5` at `c5c5c5c`, last worked on by an earlier run"),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "`demo-v1.0.6` at `c6c6c6c`, last worked on by the run at {BUILD}"
            )),
            "{message}"
        );
    }

    #[test]
    fn an_older_draft_without_a_claim_is_dated_first_and_abandoned_later() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "c5c5c5c5c5", "## Changes\n")
            .behind_head("c5c5c5c5c5");

        let outcome = prepare(&forge, &config(""), scheduled()).expect("fresh release");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.6".to_owned()
            }
        );
        assert!(
            !forge.ran("gh api -X DELETE"),
            "a draft of unknown age may be in another run's hands"
        );
        assert!(!forge.ran("git push origin :refs/tags/"));
        assert_eq!(
            forge.notes_written_to(5).as_deref(),
            Some("## Changes\n\n<!-- release-tool: claimed-at=1800000000 scheduled-resumes=0 -->\n")
        );
        let (message, style) = forge.annotation().expect("the draft is reported, not passed over");
        assert_eq!(style, "warning");
        assert!(
            message.contains("Found 1 older unpublished draft release(s) with no record"),
            "{message}"
        );
        assert!(message.contains("more than 60 minutes from now"), "{message}");
        assert!(message.contains("- `demo-v1.0.5` at `c5c5c5c`"), "{message}");
    }

    #[test]
    fn resuming_a_draft_also_abandons_the_older_drafts_behind_it() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "c5c5c5c5c5", &claimed(180, 0))
            .behind_head("c5c5c5c5c5")
            .draft(6, HEAD, &claimed(120, 0));

        let outcome = prepare(&forge, &config(""), scheduled()).expect("scheduled resume");

        assert_eq!(
            outcome,
            PrepareOutcome::Resumed {
                tag: "demo-v1.0.6".to_owned()
            }
        );
        assert!(forge.ran("gh api -X DELETE repos/example/project/releases/5"));
        assert!(!forge.ran("gh api -X DELETE repos/example/project/releases/6"));
    }

    #[test]
    fn a_draft_that_may_still_be_building_is_not_abandoned() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "c5c5c5c5c5", &claimed(5, 0))
            .behind_head("c5c5c5c5c5");

        let outcome = prepare(&forge, &config(""), scheduled()).expect("fresh release");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.6".to_owned()
            }
        );
        assert!(!forge.ran("gh api -X DELETE"));
        assert!(!forge.ran("git push origin :refs/tags/"));
        assert_eq!(forge.annotation(), None);
    }

    #[test]
    fn a_draft_a_run_started_on_while_this_one_prepared_is_not_deleted() {
        // The release list said the draft was idle. By the time it is about
        // to be deleted, another run has dated its claim.
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "c5c5c5c5c5", &claimed(180, 0))
            .behind_head("c5c5c5c5c5")
            .claimed_meanwhile(5);

        prepare(&forge, &config(""), scheduled()).expect("fresh release");

        assert!(forge.ran("gh api repos/example/project/releases/5"));
        assert!(!forge.ran("gh api -X DELETE"));
        assert!(!forge.ran("git push origin :refs/tags/"));
    }

    #[test]
    fn a_draft_for_a_commit_that_is_not_behind_the_release_is_kept() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "elsewhere", &claimed(180, 0));

        prepare(&forge, &config(""), scheduled()).expect("fresh release");

        assert!(forge.ran("git merge-base elsewhere"));
        assert!(!forge.ran("gh api -X DELETE"));
        assert!(!forge.ran("git push origin :refs/tags/"));
    }

    #[test]
    fn a_draft_published_while_the_run_prepared_is_not_deleted() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "c5c5c5c5c5", &claimed(180, 0))
            .behind_head("c5c5c5c5c5")
            .published_meanwhile(5);

        prepare(&forge, &config(""), scheduled()).expect("fresh release");

        assert!(forge.ran("gh api repos/example/project/releases/5"));
        assert!(!forge.ran("gh api -X DELETE"));
        assert!(!forge.ran("git push origin :refs/tags/"));
    }

    #[test]
    fn a_draft_sharing_its_tag_with_a_published_release_keeps_the_tag() {
        let forge =
            FakeForge::new()
                .published(4, "c4")
                .published(5, "c5c5c5c5c5")
                .draft(5, "c5c5c5c5c5", &claimed(180, 0));

        let outcome = prepare(&forge, &config(""), scheduled()).expect("fresh release");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.6".to_owned()
            }
        );
        assert!(!forge.ran("gh api -X DELETE"));
        assert!(!forge.ran("git push origin :refs/tags/"));
        let (message, _) = forge.annotation().expect("the draft that needs a person is reported");
        assert!(
            message.contains("Could not clean up 1 older unpublished draft"),
            "{message}"
        );
        assert!(message.contains("a published release also uses this tag"), "{message}");
    }

    #[test]
    fn an_older_draft_whose_tag_is_gone_is_reported_and_kept() {
        let forge = FakeForge::new().untagged_draft(3).published(4, "c4");

        let outcome = prepare(&forge, &config(""), scheduled()).expect("fresh release");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.5".to_owned()
            }
        );
        assert!(!forge.ran("gh api -X DELETE"));
        assert!(!forge.ran("git merge-base"), "there is no commit to compare");
        let (message, style) = forge.annotation().expect("a draft nobody can resume is not ignored");
        assert_eq!(style, "warning");
        assert!(
            message.contains("Could not clean up 1 older unpublished draft"),
            "{message}"
        );
        assert!(message.contains("`demo-v1.0.3`: its tag no longer exists"), "{message}");
        assert!(
            message.contains("gh release delete demo-v1.0.3 --repo example/project --yes"),
            "{message}"
        );
    }

    #[test]
    fn a_failed_cleanup_is_reported_and_does_not_fail_the_release() {
        let forge = FakeForge::new()
            .published(4, "c4")
            .draft(5, "c5c5c5c5c5", &claimed(180, 0))
            .behind_head("c5c5c5c5c5")
            .failing("gh api -X DELETE");

        let outcome = prepare(&forge, &config(""), scheduled()).expect("the release itself still proceeds");

        assert_eq!(
            outcome,
            PrepareOutcome::Created {
                tag: "demo-v1.0.6".to_owned()
            }
        );
        assert!(
            !forge.ran("git push origin :refs/tags/demo-v1.0.5"),
            "a tag is only deleted after its draft"
        );
        let (message, style) = forge.annotation().expect("the failure is reported on the build page");
        assert_eq!(style, "warning");
        assert!(
            message.contains("Could not clean up 1 older unpublished draft"),
            "{message}"
        );
        assert!(message.contains("could not delete the draft release"), "{message}");
    }

    fn failed_phase_report(forge: &FakeForge, config: &ReleaseConfig) -> String {
        let error = anyhow::anyhow!("checksum mismatch for demo-linux");
        report_unpublished(
            forge,
            config,
            Some("job-1"),
            "demo-v1.0.5",
            "verification or publishing",
            &error,
        );
        let (message, style) = forge.annotation().expect("annotation");
        assert_eq!(style, "error");
        assert!(
            message.contains("Release `demo-v1.0.5`: verification or publishing failed."),
            "{message}"
        );
        assert!(message.contains("checksum mismatch for demo-linux"), "{message}");
        message
    }

    #[test]
    fn a_failed_phase_is_reported_with_the_attempts_the_draft_has_left() {
        let forge = FakeForge::new().draft(5, HEAD, &claimed(1, 1));

        let message = failed_phase_report(&forge, &config(""));

        assert!(
            message.contains(
                "It is still an unpublished draft. A scheduled run on this commit resumes it automatically (2 of 3 attempts left), once 60 minutes have passed since a run last worked on it."
            ),
            "{message}"
        );
        assert!(message.contains("A release of a newer commit abandons it"), "{message}");

        let one_job = FakeForge::new().draft(5, HEAD, &claimed(1, 1));
        let message = failed_phase_report(&one_job, &one_job_config());
        assert!(
            message.contains("resumes it automatically (2 of 3 attempts left). A release"),
            "{message}"
        );
    }

    #[test]
    fn a_failed_phase_report_does_not_promise_a_resume_that_cannot_happen() {
        let forge = FakeForge::new().draft(5, HEAD, &claimed(1, 3));

        let message = failed_phase_report(&forge, &config(""));

        assert!(
            message.contains("no resume attempts left for it (3 of 3 used)"),
            "{message}"
        );
        assert!(!message.contains("resumes it automatically"), "{message}");
    }

    #[test]
    fn a_failed_phase_report_says_when_the_release_is_not_a_draft_any_more() {
        let published = FakeForge::new().published(5, HEAD);
        let message = failed_phase_report(&published, &config(""));
        assert!(message.contains("The release itself is published."), "{message}");
        assert!(!message.contains("unpublished draft"), "{message}");

        let gone = FakeForge::new();
        let message = failed_phase_report(&gone, &config(""));
        assert!(message.contains("could not be read afterwards"), "{message}");
        assert!(!message.contains("still an unpublished draft"), "{message}");
    }

    #[test]
    fn nothing_is_reported_to_a_build_page_outside_ci() {
        let forge = FakeForge::new().draft(5, HEAD, &claimed(1, 1));

        report_unpublished(
            &forge,
            &config(""),
            None,
            "demo-v1.0.5",
            "the asset upload",
            &anyhow::anyhow!("boom"),
        );

        assert!(forge.calls.borrow().is_empty());
    }
}
