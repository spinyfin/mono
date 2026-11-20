use std::path::PathBuf;

use serde::Deserialize;

use crate::{ReleaseError, Result};

/// The complete, intentionally small release configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseConfig {
    pub repo: String,
    pub tag_prefix: String,
    pub title_prefix: String,
    pub version: VersionConfig,
    pub notes: NotesConfig,
    pub change_paths: Vec<String>,
    /// Required release assets. `{version}` expands to the version encoded in
    /// the release tag (the tag with `tag_prefix` stripped).
    pub required_assets: Vec<String>,
    /// Optional release assets. Uses the same `{version}` expansion as
    /// `required_assets`.
    pub optional_assets: Vec<String>,
    /// How later runs treat a draft release that an earlier run left
    /// unpublished. Optional; every value has a default.
    #[serde(default)]
    pub recovery: RecoveryConfig,
}

/// Limits on recovering a draft release that an earlier run did not publish.
///
/// Each phase of a run dates a claim on the draft it is working on. These two
/// values say how long that claim keeps other runs away from the draft, and
/// how many times scheduled runs may pick the draft up again once it lapses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryConfig {
    /// How many times scheduled runs may resume one unpublished draft for the
    /// commit being released. Once they are used up, scheduled runs stop
    /// retrying and fail until a person intervenes or a newer commit replaces
    /// the draft. `0` never resumes from a schedule.
    #[serde(default = "default_scheduled_resume_attempts")]
    pub scheduled_resume_attempts: u32,
    /// Minutes after a run last worked on a draft during which other runs
    /// assume it is still being built and leave it alone. `prepare`, `upload`
    /// and `publish` each count as working on it, so the value must be longer
    /// than the longest gap between two of them: the slowest build, including
    /// any time its job spends waiting for an agent. Set it to `0` only when a
    /// second release run cannot start before the first has finished, for
    /// example when all three phases run in one job under a concurrency limit
    /// of one.
    #[serde(default = "default_in_flight_minutes")]
    pub in_flight_minutes: u64,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            scheduled_resume_attempts: default_scheduled_resume_attempts(),
            in_flight_minutes: default_in_flight_minutes(),
        }
    }
}

fn default_scheduled_resume_attempts() -> u32 {
    3
}

fn default_in_flight_minutes() -> u64 {
    60
}

/// Version-related values. A patch counter obtains its major/minor pair from
/// exactly one source: a package manifest or a literal pair.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionConfig {
    pub scheme: VersionScheme,
    #[serde(default)]
    pub manifest: Option<PathBuf>,
    #[serde(default)]
    pub major_minor: Option<String>,
}

/// The two version shapes supported by release workflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VersionScheme {
    AlphaCounter,
    PatchCounter,
}

/// Configuration for creating release notes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotesConfig {
    pub source: NotesSource,
    #[serde(default)]
    pub project: Option<PathBuf>,
}

/// The only supported release-note sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotesSource {
    Changelog,
    GithubGenerated,
}

impl ReleaseConfig {
    /// Parses a complete release manifest and rejects unsupported fields.
    pub fn parse(contents: &str) -> Result<Self> {
        let config: Self = toml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.repo.trim().is_empty() || !self.repo.contains('/') {
            return Err(ReleaseError::InvalidConfig(
                "repo must be an owner/repository pair".to_owned(),
            ));
        }
        if self.tag_prefix.is_empty() {
            return Err(ReleaseError::InvalidConfig("tag_prefix must not be empty".to_owned()));
        }
        if self.title_prefix.is_empty() {
            return Err(ReleaseError::InvalidConfig("title_prefix must not be empty".to_owned()));
        }
        if self.change_paths.iter().any(|path| path.is_empty()) {
            return Err(ReleaseError::InvalidConfig(
                "change_paths must not contain empty paths".to_owned(),
            ));
        }
        if self.required_assets.iter().any(|asset| asset.is_empty())
            || self.optional_assets.iter().any(|asset| asset.is_empty())
        {
            return Err(ReleaseError::InvalidConfig("asset names must not be empty".to_owned()));
        }
        if self
            .required_assets
            .iter()
            .any(|asset| self.optional_assets.contains(asset))
        {
            return Err(ReleaseError::InvalidConfig(
                "an asset cannot be both required and optional".to_owned(),
            ));
        }

        match self.version.scheme {
            VersionScheme::AlphaCounter => {
                if self.version.manifest.is_none() || self.version.major_minor.is_some() {
                    return Err(ReleaseError::InvalidConfig(
                        "alpha-counter requires manifest and does not accept major_minor".to_owned(),
                    ));
                }
            }
            VersionScheme::PatchCounter => {
                if self.version.manifest.is_some() == self.version.major_minor.is_some() {
                    return Err(ReleaseError::InvalidConfig(
                        "patch-counter requires exactly one of manifest or major_minor".to_owned(),
                    ));
                }
            }
        }

        if self.notes.source == NotesSource::Changelog && self.notes.project.is_none() {
            return Err(ReleaseError::InvalidConfig(
                "changelog notes require a project path".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_CONFIG: &str = include_str!("testdata/alpha-release.toml");

    #[test]
    fn parses_a_closed_alpha_counter_manifest() {
        let config = ReleaseConfig::parse(RELEASE_CONFIG).expect("parse fixture config");

        assert_eq!(config.repo, "example/project");
        assert_eq!(config.version.scheme, VersionScheme::AlphaCounter);
        assert_eq!(config.version.manifest, Some(PathBuf::from("tool/Cargo.toml")));
        assert_eq!(config.notes.source, NotesSource::Changelog);
    }

    #[test]
    fn rejects_unknown_fields() {
        let invalid = RELEASE_CONFIG.replace("title_prefix", "unexpected_prefix");
        let error = ReleaseConfig::parse(&invalid).expect_err("unknown key should fail");

        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn rejects_invalid_configuration_combinations() {
        let cases = [
            (
                "repo must be owner/repository",
                RELEASE_CONFIG.replace("repo = \"example/project\"", "repo = \"example\""),
                "repo must be an owner/repository pair",
            ),
            (
                "empty tag prefix",
                RELEASE_CONFIG.replace("tag_prefix = \"demo-v\"", "tag_prefix = \"\""),
                "tag_prefix must not be empty",
            ),
            (
                "empty title prefix",
                RELEASE_CONFIG.replace("title_prefix = \"Demo\"", "title_prefix = \"\""),
                "title_prefix must not be empty",
            ),
            (
                "empty change path",
                RELEASE_CONFIG.replace(
                    "change_paths = [\"tool/\", \".buildkite/pipeline-release.yml\"]",
                    "change_paths = [\"\"]",
                ),
                "change_paths must not contain empty paths",
            ),
            (
                "empty asset name",
                RELEASE_CONFIG.replace("required_assets = [\"demo-linux\"]", "required_assets = [\"\"]"),
                "asset names must not be empty",
            ),
            (
                "duplicate asset role",
                RELEASE_CONFIG.replace(
                    "optional_assets = [\"demo-darwin\"]",
                    "optional_assets = [\"demo-linux\"]",
                ),
                "an asset cannot be both required and optional",
            ),
            (
                "alpha without manifest",
                RELEASE_CONFIG.replace("manifest = \"tool/Cargo.toml\"\n", ""),
                "alpha-counter requires manifest",
            ),
            (
                "alpha with literal major minor",
                RELEASE_CONFIG.replace(
                    "manifest = \"tool/Cargo.toml\"",
                    "manifest = \"tool/Cargo.toml\"\nmajor_minor = \"0.4\"",
                ),
                "alpha-counter requires manifest",
            ),
            (
                "patch with neither source",
                RELEASE_CONFIG.replace(
                    "scheme = \"alpha-counter\"\nmanifest = \"tool/Cargo.toml\"",
                    "scheme = \"patch-counter\"",
                ),
                "patch-counter requires exactly one",
            ),
            (
                "patch with both sources",
                RELEASE_CONFIG
                    .replace("scheme = \"alpha-counter\"", "scheme = \"patch-counter\"")
                    .replace(
                        "manifest = \"tool/Cargo.toml\"",
                        "manifest = \"tool/Cargo.toml\"\nmajor_minor = \"0.4\"",
                    ),
                "patch-counter requires exactly one",
            ),
            (
                "changelog without project",
                RELEASE_CONFIG.replace("project = \"tool/PROJECT.yaml\"\n", ""),
                "changelog notes require a project path",
            ),
        ];

        for (name, contents, expected) in cases {
            let error = ReleaseConfig::parse(&contents).expect_err(name);
            assert!(error.to_string().contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn recovery_limits_default_when_the_table_is_absent() {
        let config = ReleaseConfig::parse(RELEASE_CONFIG).expect("parse fixture config");

        assert_eq!(
            config.recovery,
            RecoveryConfig {
                scheduled_resume_attempts: 3,
                in_flight_minutes: 60,
            }
        );
    }

    #[test]
    fn recovery_limits_can_be_set_independently() {
        let config = ReleaseConfig::parse(&format!("{RELEASE_CONFIG}\n[recovery]\nin_flight_minutes = 0\n"))
            .expect("parse recovery table");

        assert_eq!(config.recovery.in_flight_minutes, 0);
        assert_eq!(config.recovery.scheduled_resume_attempts, 3);

        let config = ReleaseConfig::parse(&format!(
            "{RELEASE_CONFIG}\n[recovery]\nscheduled_resume_attempts = 1\n"
        ))
        .expect("parse recovery table");

        assert_eq!(config.recovery.scheduled_resume_attempts, 1);
        assert_eq!(config.recovery.in_flight_minutes, 60);
    }

    #[test]
    fn recovery_table_rejects_unknown_keys() {
        let error = ReleaseConfig::parse(&format!("{RELEASE_CONFIG}\n[recovery]\nretries = 2\n"))
            .expect_err("unknown recovery key should fail");

        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn patch_counter_accepts_a_literal_major_minor_pair() {
        let config = ReleaseConfig::parse(
            r#"
repo = "example/project"
tag_prefix = "boss-v"
title_prefix = "Boss"
change_paths = ["tools/boss/"]
required_assets = ["Boss.zip"]
optional_assets = []

[version]
scheme = "patch-counter"
major_minor = "1.0"

[notes]
source = "github-generated"
"#,
        )
        .expect("literal major/minor config");

        assert_eq!(config.version.major_minor.as_deref(), Some("1.0"));
    }
}
