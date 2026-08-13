use crate::GitHubRelease;

/// Build trigger sources the release tool accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Ui,
    Api,
}

/// Converts a Buildkite source into the closed set accepted by a release
/// workflow. Push and pull-request builds are rejected before any release
/// operation can begin.
pub fn assert_allowed_trigger(source: &str) -> crate::Result<Trigger> {
    match source {
        "schedule" => Ok(Trigger::Schedule),
        "ui" => Ok(Trigger::Ui),
        "api" => Ok(Trigger::Api),
        _ => Err(crate::ReleaseError::UnsupportedTrigger {
            trigger_source: source.to_owned(),
        }),
    }
}

/// Inputs needed to decide whether a release run can proceed.
#[derive(Debug, Clone, Copy)]
pub struct SkipInput<'a> {
    pub trigger: Trigger,
    pub head_sha: &'a str,
    pub last_published: Option<&'a GitHubRelease>,
    pub last_published_sha: Option<&'a str>,
    pub change_paths: &'a [String],
    pub changed_paths: &'a [String],
}

/// The outcome of an idempotency/change-detection decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipDecision {
    Proceed,
    Skip(SkipReason),
}

/// The concrete reason a run is a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    AlreadyReleased { tag: String },
    NoRelevantChanges { tag: String },
}

/// Decides whether a release run should stop before allocating a new version.
///
/// Idempotency applies to every accepted trigger. Only scheduled runs use
/// change detection; an explicit UI/API request proceeds unless its commit was
/// already published. An unknown last-tag SHA fails open, matching the current
/// scripts' behaviour rather than silently suppressing a requested release.
pub fn should_skip(input: SkipInput<'_>) -> SkipDecision {
    if let (Some(last_release), Some(last_sha)) = (input.last_published, input.last_published_sha)
        && input.head_sha == last_sha
    {
        return SkipDecision::Skip(SkipReason::AlreadyReleased {
            tag: last_release.tag_name.clone(),
        });
    }

    if input.trigger != Trigger::Schedule || input.last_published.is_none() || input.last_published_sha.is_none() {
        return SkipDecision::Proceed;
    }

    let touched_relevant_path = input.changed_paths.iter().any(|changed| {
        input
            .change_paths
            .iter()
            .any(|release_path| changed.starts_with(release_path))
    });
    if touched_relevant_path {
        SkipDecision::Proceed
    } else {
        SkipDecision::Skip(SkipReason::NoRelevantChanges {
            tag: input
                .last_published
                .expect("last release checked above")
                .tag_name
                .clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.to_owned(),
            draft,
        }
    }

    fn input<'a>(
        trigger: Trigger,
        last_published: Option<&'a GitHubRelease>,
        last_published_sha: Option<&'a str>,
        change_paths: &'a [String],
        changed_paths: &'a [String],
    ) -> SkipInput<'a> {
        SkipInput {
            trigger,
            head_sha: "head",
            last_published,
            last_published_sha,
            change_paths,
            changed_paths,
        }
    }

    #[test]
    fn rejects_non_release_trigger_sources() {
        let error = assert_allowed_trigger("push").expect_err("push trigger must fail");

        assert!(error.to_string().contains("expected schedule, ui, or api"), "{error}");
    }

    #[test]
    fn does_not_rerelease_the_head_commit_for_manual_triggers() {
        let release = release("v1.2.3", false);
        let release_paths = vec!["tools/release/".to_owned(), ".buildkite/release.yml".to_owned()];
        let decision = should_skip(input(Trigger::Ui, Some(&release), Some("head"), &release_paths, &[]));

        assert_eq!(
            decision,
            SkipDecision::Skip(SkipReason::AlreadyReleased {
                tag: "v1.2.3".to_owned()
            })
        );
    }

    #[test]
    fn scheduled_run_skips_when_no_configured_path_changed() {
        let release = release("v1.2.3", false);
        let release_paths = vec!["tools/release/".to_owned(), ".buildkite/release.yml".to_owned()];
        let changed = vec!["docs/readme.md".to_owned()];
        let decision = should_skip(input(
            Trigger::Schedule,
            Some(&release),
            Some("previous"),
            &release_paths,
            &changed,
        ));

        assert_eq!(
            decision,
            SkipDecision::Skip(SkipReason::NoRelevantChanges {
                tag: "v1.2.3".to_owned()
            })
        );
    }

    #[test]
    fn scheduled_run_proceeds_when_a_configured_path_changed() {
        let release = release("v1.2.3", false);
        let release_paths = vec!["tools/release/".to_owned(), ".buildkite/release.yml".to_owned()];
        let changed = vec!["tools/release/src/lib.rs".to_owned()];

        assert_eq!(
            should_skip(input(
                Trigger::Schedule,
                Some(&release),
                Some("previous"),
                &release_paths,
                &changed,
            )),
            SkipDecision::Proceed
        );
    }

    #[test]
    fn manual_trigger_proceeds_for_a_new_commit_without_matching_paths() {
        let release = release("v1.2.3", false);
        let release_paths = vec!["tools/release/".to_owned(), ".buildkite/release.yml".to_owned()];

        assert_eq!(
            should_skip(input(
                Trigger::Api,
                Some(&release),
                Some("previous"),
                &release_paths,
                &[],
            )),
            SkipDecision::Proceed
        );
    }
}
