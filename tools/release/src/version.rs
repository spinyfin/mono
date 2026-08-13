use serde::Deserialize;

use crate::{ReleaseError, Result, VersionConfig, VersionScheme};

/// A release version and its repository tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextVersion {
    pub version: String,
    pub tag: String,
}

#[derive(Debug, Deserialize)]
struct CargoManifest {
    package: CargoPackage,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionBase {
    Alpha {
        base: String,
        manifest_counter: u64,
    },
    Patch {
        major: u64,
        minor: u64,
        manifest_counter: Option<u64>,
    },
}

/// Computes the next version from its configured baseline and every known
/// release tag. Tags in other version families are deliberately ignored.
pub fn compute_next_version(
    config: &VersionConfig,
    tag_prefix: &str,
    manifest_contents: Option<&str>,
    tags: &[String],
) -> Result<NextVersion> {
    let base = version_base(config, manifest_contents)?;
    let highest_tag = highest_counter(&base, tag_prefix, tags);

    match base {
        VersionBase::Alpha { base, manifest_counter } => {
            let next = manifest_counter
                .max(highest_tag.unwrap_or(0))
                .checked_add(1)
                .ok_or_else(|| ReleaseError::InvalidConfig("alpha counter cannot exceed u64::MAX".to_owned()))?;
            let version = format!("{base}-alpha.{next}");
            Ok(NextVersion {
                tag: format!("{tag_prefix}{version}"),
                version,
            })
        }
        VersionBase::Patch {
            major,
            minor,
            manifest_counter,
        } => {
            let next = highest_tag
                .into_iter()
                .chain(manifest_counter)
                .max()
                .map_or(Ok(0), |counter| {
                    counter
                        .checked_add(1)
                        .ok_or_else(|| ReleaseError::InvalidConfig("patch counter cannot exceed u64::MAX".to_owned()))
                })?;
            let version = format!("{major}.{minor}.{next}");
            Ok(NextVersion {
                tag: format!("{tag_prefix}{version}"),
                version,
            })
        }
    }
}

pub(crate) fn highest_matching_tag(
    config: &VersionConfig,
    tag_prefix: &str,
    manifest_contents: Option<&str>,
    tags: &[String],
) -> Result<Option<(u64, String)>> {
    let base = version_base(config, manifest_contents)?;
    Ok(highest_counter(&base, tag_prefix, tags).map(|counter| (counter, tag_for_counter(&base, tag_prefix, counter))))
}

pub(crate) fn matching_tag_counter(
    config: &VersionConfig,
    tag_prefix: &str,
    manifest_contents: Option<&str>,
    tag: &str,
) -> Result<Option<u64>> {
    let base = version_base(config, manifest_contents)?;
    Ok(counter_for_tag(&base, tag_prefix, tag))
}

fn version_base(config: &VersionConfig, manifest_contents: Option<&str>) -> Result<VersionBase> {
    let manifest_version = || -> Result<String> {
        let contents = manifest_contents.ok_or_else(|| {
            ReleaseError::InvalidConfig("version scheme requires the configured manifest contents".to_owned())
        })?;
        let manifest: CargoManifest = toml::from_str(contents)?;
        Ok(manifest.package.version)
    };

    match config.scheme {
        VersionScheme::AlphaCounter => {
            let version = manifest_version()?;
            let (base, counter) = version.split_once("-alpha.").ok_or_else(|| {
                ReleaseError::InvalidConfig(format!("alpha-counter version {version:?} must be X.Y.Z-alpha.N"))
            })?;
            parse_three_part_version(base)?;
            let counter = parse_counter(counter, "alpha counter")?;
            Ok(VersionBase::Alpha {
                base: base.to_owned(),
                manifest_counter: counter,
            })
        }
        VersionScheme::PatchCounter => {
            let (major, minor, manifest_counter) = if let Some(literal) = &config.major_minor {
                let (major, minor) = parse_major_minor(literal)?;
                (major, minor, None)
            } else {
                let version = manifest_version()?;
                let (major, minor, patch) = parse_three_part_version(&version)?;
                (major, minor, Some(patch))
            };
            Ok(VersionBase::Patch {
                major,
                minor,
                manifest_counter,
            })
        }
    }
}

fn highest_counter(base: &VersionBase, tag_prefix: &str, tags: &[String]) -> Option<u64> {
    tags.iter()
        .filter_map(|tag| counter_for_tag(base, tag_prefix, tag))
        .max()
}

fn counter_for_tag(base: &VersionBase, tag_prefix: &str, tag: &str) -> Option<u64> {
    let suffix = tag.strip_prefix(tag_prefix)?;
    match base {
        VersionBase::Alpha { base, .. } => {
            let counter = suffix.strip_prefix(base)?.strip_prefix("-alpha.")?;
            parse_counter(counter, "tag alpha counter").ok()
        }
        VersionBase::Patch { major, minor, .. } => {
            let prefix = format!("{major}.{minor}.");
            let counter = suffix.strip_prefix(&prefix)?;
            parse_counter(counter, "tag patch counter").ok()
        }
    }
}

fn tag_for_counter(base: &VersionBase, tag_prefix: &str, counter: u64) -> String {
    match base {
        VersionBase::Alpha { base, .. } => format!("{tag_prefix}{base}-alpha.{counter}"),
        VersionBase::Patch { major, minor, .. } => format!("{tag_prefix}{major}.{minor}.{counter}"),
    }
}

fn parse_major_minor(value: &str) -> Result<(u64, u64)> {
    let mut parts = value.split('.');
    let major = parts
        .next()
        .ok_or_else(|| ReleaseError::InvalidConfig("major_minor must be X.Y".to_owned()))?;
    let minor = parts
        .next()
        .ok_or_else(|| ReleaseError::InvalidConfig("major_minor must be X.Y".to_owned()))?;
    if parts.next().is_some() {
        return Err(ReleaseError::InvalidConfig("major_minor must be X.Y".to_owned()));
    }
    Ok((parse_counter(major, "major")?, parse_counter(minor, "minor")?))
}

fn parse_three_part_version(value: &str) -> Result<(u64, u64, u64)> {
    let mut parts = value.split('.');
    let major = parts
        .next()
        .ok_or_else(|| ReleaseError::InvalidConfig(format!("version {value:?} must be X.Y.Z")))?;
    let minor = parts
        .next()
        .ok_or_else(|| ReleaseError::InvalidConfig(format!("version {value:?} must be X.Y.Z")))?;
    let patch = parts
        .next()
        .ok_or_else(|| ReleaseError::InvalidConfig(format!("version {value:?} must be X.Y.Z")))?;
    if parts.next().is_some() {
        return Err(ReleaseError::InvalidConfig(format!("version {value:?} must be X.Y.Z")));
    }
    Ok((
        parse_counter(major, "major")?,
        parse_counter(minor, "minor")?,
        parse_counter(patch, "patch")?,
    ))
}

fn parse_counter(value: &str, label: &str) -> Result<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ReleaseError::InvalidConfig(format!(
            "{label} {value:?} must be a non-negative integer"
        )));
    }
    value
        .parse()
        .map_err(|_| ReleaseError::InvalidConfig(format!("{label} {value:?} is too large")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReleaseConfig;

    const ALPHA_CONFIG: &str = include_str!("testdata/alpha-release.toml");
    const ALPHA_MANIFEST: &str = include_str!("testdata/alpha-package.toml");
    const PATCH_MANIFEST: &str = include_str!("testdata/patch-package.toml");
    const ALPHA_TAGS: &str = include_str!("testdata/alpha-tags.txt");
    const PATCH_TAGS: &str = include_str!("testdata/patch-tags.txt");

    fn tags(contents: &str) -> Vec<String> {
        contents.lines().map(str::to_owned).collect()
    }

    #[test]
    fn increments_the_highest_alpha_counter_from_fixture_tags() {
        let config = ReleaseConfig::parse(ALPHA_CONFIG).expect("parse config");

        let next = compute_next_version(
            &config.version,
            &config.tag_prefix,
            Some(ALPHA_MANIFEST),
            &tags(ALPHA_TAGS),
        )
        .expect("compute version");

        assert_eq!(next.version, "0.4.1-alpha.9");
        assert_eq!(next.tag, "demo-v0.4.1-alpha.9");
    }

    #[test]
    fn patch_counter_uses_the_higher_manifest_patch() {
        let config = ReleaseConfig::parse(
            r#"
repo = "example/project"
tag_prefix = "v"
title_prefix = "Example"
change_paths = ["src/"]
required_assets = []
optional_assets = []

[version]
scheme = "patch-counter"
manifest = "Cargo.toml"

[notes]
source = "github-generated"
"#,
        )
        .expect("parse config");

        let next = compute_next_version(&config.version, "v", Some(PATCH_MANIFEST), &tags(PATCH_TAGS))
            .expect("compute version");

        assert_eq!(next.version, "2.7.5");
        assert_eq!(next.tag, "v2.7.5");
    }

    #[test]
    fn patch_counter_supports_a_literal_major_minor_for_boss() {
        let config = VersionConfig {
            scheme: VersionScheme::PatchCounter,
            manifest: None,
            major_minor: Some("1.0".to_owned()),
        };
        let tags = vec!["boss-v1.0.9".to_owned(), "boss-v2.0.3".to_owned()];

        let next = compute_next_version(&config, "boss-v", None, &tags).expect("compute version");

        assert_eq!(next.version, "1.0.10");
        assert_eq!(next.tag, "boss-v1.0.10");
    }

    #[test]
    fn literal_patch_counter_starts_at_zero() {
        let config = VersionConfig {
            scheme: VersionScheme::PatchCounter,
            manifest: None,
            major_minor: Some("1.0".to_owned()),
        };

        let next = compute_next_version(&config, "boss-v", None, &[]).expect("compute first version");

        assert_eq!(next.version, "1.0.0");
    }

    #[test]
    fn patch_counter_rejects_prerelease_manifest_versions() {
        let config = VersionConfig {
            scheme: VersionScheme::PatchCounter,
            manifest: Some("Cargo.toml".into()),
            major_minor: None,
        };

        let error = compute_next_version(&config, "v", Some("[package]\nversion = \"1.2.3-alpha.1\""), &[])
            .expect_err("pre-release version should fail");

        assert!(error.to_string().contains("must be X.Y.Z"), "{error}");
    }
}
