use serde::Deserialize;

use crate::version::highest_matching_tag;
use crate::{ReleaseConfig, ReleaseError, Result};

const RELEASE_LIST_ATTEMPTS: usize = 3;

/// One subprocess invocation, represented as data so tests can inject a
/// captured runner instead of executing programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Output captured from a subprocess invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Failure to start or communicate with a command runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerError {
    message: String,
}

impl RunnerError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RunnerError {}

/// The subprocess seam for release-state queries.
pub trait CommandRunner {
    fn run(&self, command: &Command) -> std::result::Result<CommandOutput, RunnerError>;
}

/// The release fields needed by resolution and skip decisions. GitHub's API
/// supplies more fields, which Serde intentionally ignores.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub draft: bool,
}

/// Most-recent published and draft releases, held separately so a draft never
/// narrows the next published changelog or change-detection range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastReleases {
    pub published: Option<GitHubRelease>,
    pub draft: Option<GitHubRelease>,
}

/// A release API snapshot together with its independently enumerated remote
/// tags. The constructor verifies that the snapshot cannot under-report a
/// newer matching tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseState {
    pub releases: Vec<GitHubRelease>,
    pub last: LastReleases,
    pub remote_tags: Vec<String>,
}

/// Reads the GitHub REST releases list and remote tag list through the injected
/// runner. The REST list intentionally avoids `gh release list`, whose
/// GraphQL transport shares a rate-limit budget with unrelated status polling.
pub fn query_release_state(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    manifest_contents: Option<&str>,
) -> Result<ReleaseState> {
    let releases = list_releases(runner, &config.repo)?;
    let remote_tags = list_remote_tags(runner, &config.tag_prefix)?;
    verify_remote_tags(runner, config, manifest_contents, &releases, &remote_tags)?;
    let last = resolve_last_release(&releases, &config.tag_prefix);

    Ok(ReleaseState {
        releases,
        last,
        remote_tags,
    })
}

/// Splits the API's newest-first release list into independent latest
/// published and latest draft values.
#[must_use]
pub fn resolve_last_release(releases: &[GitHubRelease], tag_prefix: &str) -> LastReleases {
    let mut matching = releases
        .iter()
        .filter(|release| release.tag_name.starts_with(tag_prefix));
    let published = matching.clone().find(|release| !release.draft).cloned();
    let draft = matching.find(|release| release.draft).cloned();
    LastReleases { published, draft }
}

fn list_releases(runner: &impl CommandRunner, repo: &str) -> Result<Vec<GitHubRelease>> {
    let command = Command::new(
        "gh",
        [
            "api".to_owned(),
            format!("repos/{repo}/releases"),
            "--paginate".to_owned(),
            "-X".to_owned(),
            "GET".to_owned(),
            "-F".to_owned(),
            "per_page=100".to_owned(),
        ],
    );
    let mut last_error = String::new();
    for _ in 0..RELEASE_LIST_ATTEMPTS {
        match runner.run(&command) {
            Ok(output) if output.success => return Ok(serde_json::from_str(&output.stdout)?),
            Ok(output) => last_error = output.stderr,
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(ReleaseError::ReleaseListUnavailable {
        attempts: RELEASE_LIST_ATTEMPTS,
        last_error,
    })
}

fn list_remote_tags(runner: &impl CommandRunner, tag_prefix: &str) -> Result<Vec<String>> {
    let command = Command::new(
        "git",
        [
            "ls-remote".to_owned(),
            "--tags".to_owned(),
            "origin".to_owned(),
            format!("refs/tags/{tag_prefix}*"),
        ],
    );
    let output = runner
        .run(&command)
        .map_err(|error| ReleaseError::RemoteTagsUnavailable(error.to_string()))?;
    if !output.success {
        return Err(ReleaseError::RemoteTagsUnavailable(output.stderr));
    }
    Ok(parse_remote_tags(&output.stdout))
}

fn verify_remote_tags(
    runner: &impl CommandRunner,
    config: &ReleaseConfig,
    manifest_contents: Option<&str>,
    releases: &[GitHubRelease],
    remote_tags: &[String],
) -> Result<()> {
    let release_tags = releases
        .iter()
        .map(|release| release.tag_name.clone())
        .collect::<Vec<_>>();
    let highest_api = highest_matching_tag(&config.version, &config.tag_prefix, manifest_contents, &release_tags)?;
    let highest_remote = highest_matching_tag(&config.version, &config.tag_prefix, manifest_contents, remote_tags)?;

    let Some((remote_counter, remote_tag)) = highest_remote else {
        return Ok(());
    };
    if highest_api
        .as_ref()
        .is_some_and(|(api_counter, _)| *api_counter >= remote_counter)
    {
        return Ok(());
    }

    let command = Command::new(
        "gh",
        [
            "api".to_owned(),
            format!("repos/{}/releases/tags/{remote_tag}", config.repo),
        ],
    );
    match runner.run(&command) {
        Ok(output) if output.success => Err(ReleaseError::ReleaseListUnderreported {
            api_tag: highest_api.map(|(_, tag)| tag),
            remote_tag,
        }),
        Ok(_) => Err(ReleaseError::LeakedRemoteTag { tag: remote_tag }),
        Err(error) => Err(ReleaseError::ReleaseTagLookupUnavailable {
            tag: remote_tag,
            detail: error.to_string(),
        }),
    }
}

fn parse_remote_tags(stdout: &str) -> Vec<String> {
    let mut tags = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter_map(|reference| reference.strip_prefix("refs/tags/"))
        .map(|tag| tag.strip_suffix("^{}").unwrap_or(tag).to_owned())
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    tags
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    const CONFIG: &str = include_str!("testdata/alpha-release.toml");
    const MANIFEST: &str = include_str!("testdata/alpha-package.toml");
    const RELEASES: &str = include_str!("testdata/releases.json");
    const REMOTE_TAGS: &str = include_str!("testdata/remote-tags.txt");

    struct FixtureRunner {
        responses: RefCell<VecDeque<std::result::Result<CommandOutput, RunnerError>>>,
        calls: RefCell<Vec<Command>>,
    }

    impl FixtureRunner {
        fn new(responses: impl IntoIterator<Item = std::result::Result<CommandOutput, RunnerError>>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FixtureRunner {
        fn run(&self, command: &Command) -> std::result::Result<CommandOutput, RunnerError> {
            self.calls.borrow_mut().push(command.clone());
            self.responses
                .borrow_mut()
                .pop_front()
                .expect("fixture response for command")
        }
    }

    fn success(stdout: &str) -> std::result::Result<CommandOutput, RunnerError> {
        Ok(CommandOutput {
            success: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    #[test]
    fn splits_published_and_draft_releases_from_captured_json() {
        let releases: Vec<GitHubRelease> = serde_json::from_str(RELEASES).expect("parse captured JSON");
        let last = resolve_last_release(&releases, "demo-v");

        assert_eq!(last.published.expect("published").tag_name, "demo-v0.4.1-alpha.8");
        assert_eq!(last.draft.expect("draft").tag_name, "demo-v0.4.1-alpha.9");
    }

    #[test]
    fn queries_rest_releases_and_remote_tags_through_the_runner() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let runner = FixtureRunner::new([success(RELEASES), success(REMOTE_TAGS)]);

        let state = query_release_state(&runner, &config, Some(MANIFEST)).expect("query release state");

        assert_eq!(state.remote_tags, vec!["demo-v0.4.1-alpha.8", "demo-v0.4.1-alpha.9"]);
        assert_eq!(state.last.published.expect("published").tag_name, "demo-v0.4.1-alpha.8");
        assert_eq!(state.last.draft.expect("draft").tag_name, "demo-v0.4.1-alpha.9");
        assert_eq!(
            runner.calls.into_inner(),
            vec![
                Command::new(
                    "gh",
                    [
                        "api",
                        "repos/example/project/releases",
                        "--paginate",
                        "-X",
                        "GET",
                        "-F",
                        "per_page=100",
                    ],
                ),
                Command::new("git", ["ls-remote", "--tags", "origin", "refs/tags/demo-v*"]),
            ]
        );
    }

    #[test]
    fn fails_closed_when_remote_tags_exceed_the_api_snapshot() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let remote_tags = "abc\trefs/tags/demo-v0.4.1-alpha.10\n";
        let runner = FixtureRunner::new([
            success(RELEASES),
            success(remote_tags),
            success(r#"{"tag_name":"demo-v0.4.1-alpha.10"}"#),
        ]);

        let error = query_release_state(&runner, &config, Some(MANIFEST)).expect_err("incomplete API list must fail");

        assert!(
            matches!(error, ReleaseError::ReleaseListUnderreported { .. }),
            "{error}"
        );
    }

    #[test]
    fn retries_a_failed_release_list_before_accepting_a_snapshot() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let runner = FixtureRunner::new([
            Ok(CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "temporary API failure".to_owned(),
            }),
            success(RELEASES),
            success(REMOTE_TAGS),
        ]);

        query_release_state(&runner, &config, Some(MANIFEST)).expect("retry should succeed");

        assert_eq!(runner.calls.borrow().len(), 3);
    }
}
