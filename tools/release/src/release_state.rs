use std::cmp::Ordering;
use std::time::Duration;

use serde::Deserialize;

use crate::version::{highest_matching_tag, matching_tag_counter};
use crate::{Command, CommandRunner, ReleaseConfig, ReleaseError, Result};

const RELEASE_LIST_ATTEMPTS: usize = 3;

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
    let last = resolve_last_release(&releases, config, manifest_contents)?;

    Ok(ReleaseState {
        releases,
        last,
        remote_tags,
    })
}

/// Splits releases into the highest matching published and draft values.
/// Tags outside the configured version family are ignored, and API list order
/// is never used as a proxy for version order.
pub fn resolve_last_release(
    releases: &[GitHubRelease],
    config: &ReleaseConfig,
    manifest_contents: Option<&str>,
) -> Result<LastReleases> {
    let mut published = None;
    let mut draft = None;
    for release in releases {
        let Some(counter) = matching_tag_counter(
            &config.version,
            &config.tag_prefix,
            manifest_contents,
            &release.tag_name,
        )?
        else {
            continue;
        };
        let selected = if release.draft { &mut draft } else { &mut published };
        if selected.as_ref().is_none_or(|(highest, _)| counter > *highest) {
            *selected = Some((counter, release.clone()));
        }
    }
    Ok(LastReleases {
        published: published.map(|(_, release)| release),
        draft: draft.map(|(_, release)| release),
    })
}

/// Previous published tag that should bound generated notes for this tool.
///
/// GitHub's `--generate-notes` otherwise picks the newest release in the
/// repository, which in a monorepo is almost never the previous release of the
/// same tool. Drafts and tags that do not start with `tag_prefix` are ignored.
/// `None` means this prefix has no prior published tag.
pub fn previous_notes_tag<'a>(releases: &'a [GitHubRelease], tag_prefix: &str) -> Option<&'a str> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .map(|release| release.tag_name.as_str())
        .filter(|tag| tag.starts_with(tag_prefix) && tag.len() > tag_prefix.len())
        .max_by(|left, right| cmp_prefixed_tags(tag_prefix, left, right))
}

fn cmp_prefixed_tags(tag_prefix: &str, left: &str, right: &str) -> Ordering {
    cmp_version_like(
        left.strip_prefix(tag_prefix).unwrap_or(left),
        right.strip_prefix(tag_prefix).unwrap_or(right),
    )
}

/// Compares version-like suffixes so `0.4.10` ranks above `0.4.9` and
/// `0.4.1-alpha.9` above `0.4.1-alpha.8`. Lexicographic tag order is not used.
fn cmp_version_like(left: &str, right: &str) -> Ordering {
    version_chunks(left).cmp(&version_chunks(right))
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum VersionChunk<'a> {
    Number(u64),
    Text(&'a str),
}

fn version_chunks(value: &str) -> Vec<VersionChunk<'_>> {
    let mut chunks = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        if rest.as_bytes()[0].is_ascii_digit() {
            let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            let (digits, next) = rest.split_at(end);
            chunks.push(VersionChunk::Number(digits.parse().unwrap_or(u64::MAX)));
            rest = next;
        } else {
            let end = rest.find(|c: char| c.is_ascii_digit()).unwrap_or(rest.len());
            let (text, next) = rest.split_at(end);
            chunks.push(VersionChunk::Text(text));
            rest = next;
        }
    }
    chunks
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
    for attempt in 1..=RELEASE_LIST_ATTEMPTS {
        match runner.run(&command) {
            Ok(output) if output.success => return Ok(serde_json::from_str(&output.stdout)?),
            Ok(output) => last_error = output.stderr,
            Err(error) => last_error = error.to_string(),
        }
        if attempt < RELEASE_LIST_ATTEMPTS {
            let delay = Duration::from_secs((attempt * 5) as u64);
            eprintln!(
                "release list attempt {attempt}/{RELEASE_LIST_ATTEMPTS} failed: {last_error}; retrying in {} seconds",
                delay.as_secs()
            );
            runner.sleep(delay);
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
    use std::time::Duration;

    use super::*;
    use crate::{CommandOutput, RunnerError};

    const CONFIG: &str = include_str!("testdata/alpha-release.toml");
    const MANIFEST: &str = include_str!("testdata/alpha-package.toml");
    const RELEASES: &str = include_str!("testdata/releases.json");
    const REMOTE_TAGS: &str = include_str!("testdata/remote-tags.txt");

    struct FixtureRunner {
        responses: RefCell<VecDeque<std::result::Result<CommandOutput, RunnerError>>>,
        calls: RefCell<Vec<Command>>,
        sleeps: RefCell<Vec<Duration>>,
    }

    impl FixtureRunner {
        fn new(responses: impl IntoIterator<Item = std::result::Result<CommandOutput, RunnerError>>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
                sleeps: RefCell::new(Vec::new()),
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

        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
        }
    }

    fn success(stdout: &str) -> std::result::Result<CommandOutput, RunnerError> {
        Ok(CommandOutput {
            success: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    fn release(tag: &str, draft: bool) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.to_owned(),
            draft,
        }
    }

    #[test]
    fn first_release_for_a_prefix_has_no_notes_start_tag() {
        let releases = vec![
            release("boss-v1.0.608", false),
            release("checkleft-v0.1.0-alpha.122", false),
            release("changelog-v0.1.3", false),
        ];

        assert_eq!(previous_notes_tag(&releases, "release-v"), None);
    }

    #[test]
    fn subsequent_release_picks_the_highest_tag_with_the_same_prefix() {
        let releases = vec![
            release("checkleft-v0.1.0-alpha.122", false),
            release("checkleft-v0.1.0-alpha.121", false),
            release("boss-v1.0.608", false),
        ];

        assert_eq!(
            previous_notes_tag(&releases, "checkleft-v"),
            Some("checkleft-v0.1.0-alpha.122")
        );
    }

    #[test]
    fn notes_start_tag_ignores_a_newer_release_from_another_tool() {
        let releases = vec![
            release("boss-v1.0.608", false),
            release("checkleft-v0.1.0-alpha.122", false),
            release("changelog-v0.1.3", false),
            release("release-v0.1.1", false),
        ];

        assert_eq!(
            previous_notes_tag(&releases, "checkleft-v"),
            Some("checkleft-v0.1.0-alpha.122")
        );
        assert_eq!(previous_notes_tag(&releases, "changelog-v"), Some("changelog-v0.1.3"));
        assert_eq!(previous_notes_tag(&releases, "boss-v"), Some("boss-v1.0.608"));
    }

    #[test]
    fn notes_start_tag_ignores_drafts_and_orders_numeric_suffixes() {
        let releases = vec![
            release("demo-v1.0.10", false),
            release("demo-v1.0.9", false),
            release("demo-v1.0.11", true),
            release("other-v9.9.9", false),
        ];

        assert_eq!(previous_notes_tag(&releases, "demo-v"), Some("demo-v1.0.10"));
    }

    #[test]
    fn splits_published_and_draft_releases_from_captured_json() {
        let releases: Vec<GitHubRelease> = serde_json::from_str(RELEASES).expect("parse captured JSON");
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let last = resolve_last_release(&releases, &config, Some(MANIFEST)).expect("resolve last releases");

        assert_eq!(last.published.expect("published").tag_name, "demo-v0.4.1-alpha.8");
        assert_eq!(last.draft.expect("draft").tag_name, "demo-v0.4.1-alpha.9");
    }

    #[test]
    fn selects_the_highest_release_in_the_configured_version_family() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let releases = vec![
            GitHubRelease {
                tag_name: "demo-v0.3.9-alpha.99".to_owned(),
                draft: false,
            },
            GitHubRelease {
                tag_name: "demo-v0.4.1-alpha.8".to_owned(),
                draft: false,
            },
            GitHubRelease {
                tag_name: "demo-v0.4.1-alpha.10".to_owned(),
                draft: false,
            },
            GitHubRelease {
                tag_name: "demo-v0.4.1-alpha.bad".to_owned(),
                draft: true,
            },
            GitHubRelease {
                tag_name: "demo-v0.4.1-alpha.9".to_owned(),
                draft: true,
            },
        ];

        let last = resolve_last_release(&releases, &config, Some(MANIFEST)).expect("resolve last releases");

        assert_eq!(last.published.expect("published").tag_name, "demo-v0.4.1-alpha.10");
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
    fn reports_a_leaked_remote_tag_when_lookup_returns_not_found() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let remote_tags = "abc\trefs/tags/demo-v0.4.1-alpha.10\n";
        let runner = FixtureRunner::new([
            success(RELEASES),
            success(remote_tags),
            Ok(CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "not found".to_owned(),
            }),
        ]);

        let error = query_release_state(&runner, &config, Some(MANIFEST)).expect_err("leaked tag must fail");

        assert!(matches!(error, ReleaseError::LeakedRemoteTag { .. }), "{error}");
    }

    #[test]
    fn reports_an_unavailable_tag_lookup() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let remote_tags = "abc\trefs/tags/demo-v0.4.1-alpha.10\n";
        let runner = FixtureRunner::new([
            success(RELEASES),
            success(remote_tags),
            Err(RunnerError::new("network unavailable")),
        ]);

        let error = query_release_state(&runner, &config, Some(MANIFEST)).expect_err("unknown tag state must fail");

        assert!(
            matches!(error, ReleaseError::ReleaseTagLookupUnavailable { .. }),
            "{error}"
        );
    }

    #[test]
    fn reports_an_unavailable_remote_tag_list() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let runner = FixtureRunner::new([success(RELEASES), Err(RunnerError::new("network unavailable"))]);

        let error = query_release_state(&runner, &config, Some(MANIFEST)).expect_err("remote tags must be available");

        assert!(matches!(error, ReleaseError::RemoteTagsUnavailable(_)), "{error}");
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
        assert_eq!(*runner.sleeps.borrow(), vec![Duration::from_secs(5)]);
    }

    #[test]
    fn reports_the_last_error_after_all_release_list_attempts_fail() {
        let config = ReleaseConfig::parse(CONFIG).expect("parse config");
        let runner = FixtureRunner::new([
            Err(RunnerError::new("first")),
            Err(RunnerError::new("second")),
            Err(RunnerError::new("third")),
        ]);

        let error = query_release_state(&runner, &config, Some(MANIFEST)).expect_err("all attempts must fail");

        assert!(
            matches!(error, ReleaseError::ReleaseListUnavailable { attempts: 3, ref last_error } if last_error == "third"),
            "{error}"
        );
        assert_eq!(
            *runner.sleeps.borrow(),
            vec![Duration::from_secs(5), Duration::from_secs(10)]
        );
    }
}
