use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;

use crate::bypass::{bypass_failure_guidance, bypass_name_for_check_id, maybe_bypass_findings};
use crate::check::Check;
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct TypoCheck;

#[async_trait]
impl Check for TypoCheck {
    fn id(&self) -> &str {
        "typo"
    }

    fn description(&self) -> &str {
        "flags configured terminology typos in changed files"
    }

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let parsed = parse_typo_config(config)?;
        run_typo_check(
            self.id(),
            changeset,
            tree,
            &parsed.rules,
            parsed.allow_bypass,
            &parsed.bypass_name,
        )
    }
}

#[derive(Debug, Clone)]
struct TypoRule {
    typo: String,
    canonical: String,
    guidance: String,
    kind: MatchKind,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MatchKind {
    Substring,
    Word,
}

impl Default for MatchKind {
    fn default() -> Self {
        Self::Word
    }
}

#[derive(Debug, Deserialize)]
struct TypoConfig {
    #[serde(default)]
    rules: Vec<TypoRuleConfig>,
    #[serde(default)]
    allow_bypass: Option<bool>,
    #[serde(default)]
    bypass_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TypoRuleConfig {
    typo: String,
    canonical: String,
    #[serde(default)]
    guidance: Option<String>,
    #[serde(default)]
    kind: MatchKind,
}

#[derive(Debug)]
struct CompiledTypoRule {
    rule: TypoRule,
    matcher: Matcher,
}

#[derive(Debug)]
enum Matcher {
    Substring(String),
    Word(Regex),
}

struct ParsedTypoConfig {
    rules: Vec<TypoRule>,
    allow_bypass: bool,
    bypass_name: String,
}

fn run_typo_check(
    check_id: &str,
    changeset: &ChangeSet,
    tree: &dyn SourceTree,
    rules: &[TypoRule],
    allow_bypass: bool,
    bypass_name: &str,
) -> Result<CheckResult> {
    let compiled_rules = compile_rules(rules)?;
    let mut findings = Vec::new();

    for changed_file in &changeset.changed_files {
        if matches!(changed_file.kind, ChangeKind::Deleted) {
            continue;
        }

        let Ok(contents) = tree.read_file(&changed_file.path) else {
            continue;
        };
        let Ok(contents) = String::from_utf8(contents) else {
            continue;
        };

        for (index, line) in contents.lines().enumerate() {
            for compiled_rule in &compiled_rules {
                let Some(column) = find_column(line, compiled_rule) else {
                    continue;
                };

                findings.push(Finding {
                    severity: Severity::Error,
                    message: format!(
                        "Found typo `{}`; use `{}` instead.",
                        compiled_rule.rule.typo, compiled_rule.rule.canonical
                    ),
                    location: Some(Location {
                        path: changed_file.path.clone(),
                        line: Some((index + 1) as u32),
                        column: Some(column as u32),
                    }),
                    remediation: Some(format!(
                        "Replace `{}` with `{}`. {}",
                        compiled_rule.rule.typo,
                        compiled_rule.rule.canonical,
                        compiled_rule.rule.guidance
                    )),
                    suggested_fix: None,
                });

                // Avoid duplicate findings on the same line for overlapping rules.
                break;
            }
        }
    }

    let trigger_files = finding_paths(&findings);
    if let Some(findings) =
        maybe_bypass_findings(changeset, allow_bypass, bypass_name, &trigger_files)
    {
        return Ok(CheckResult {
            check_id: check_id.to_owned(),
            findings,
        });
    }

    if allow_bypass && !findings.is_empty() {
        let guidance = bypass_failure_guidance(bypass_name);
        for finding in &mut findings {
            finding.remediation = Some(match finding.remediation.take() {
                Some(remediation) => format!("{remediation} {guidance}"),
                None => guidance.clone(),
            });
        }
    }

    Ok(CheckResult {
        check_id: check_id.to_owned(),
        findings,
    })
}

fn parse_typo_config(config: &toml::Value) -> Result<ParsedTypoConfig> {
    let parsed: TypoConfig = config
        .clone()
        .try_into()
        .context("invalid typo check config")?;

    if parsed.rules.is_empty() {
        bail!("typo check config must contain at least one rule");
    }

    Ok(ParsedTypoConfig {
        rules: parsed
            .rules
            .into_iter()
            .map(typo_rule_from_config)
            .collect(),
        allow_bypass: parsed.allow_bypass.unwrap_or(false),
        bypass_name: resolve_bypass_name(parsed.bypass_name),
    })
}

fn typo_rule_from_config(config: TypoRuleConfig) -> TypoRule {
    let guidance = config
        .guidance
        .unwrap_or_else(|| "Use the canonical terminology.".to_owned());

    TypoRule {
        typo: config.typo,
        canonical: config.canonical,
        guidance,
        kind: config.kind,
    }
}

fn compile_rules(rules: &[TypoRule]) -> Result<Vec<CompiledTypoRule>> {
    let mut compiled = Vec::with_capacity(rules.len());

    for rule in rules {
        let matcher = match rule.kind {
            MatchKind::Substring => Matcher::Substring(rule.typo.to_ascii_lowercase()),
            MatchKind::Word => {
                let pattern = format!(r"(?i)\b{}\b", regex::escape(&rule.typo));
                let regex = Regex::new(&pattern)
                    .with_context(|| format!("invalid typo rule regex: {pattern}"))?;
                Matcher::Word(regex)
            }
        };

        compiled.push(CompiledTypoRule {
            rule: rule.clone(),
            matcher,
        });
    }

    Ok(compiled)
}

fn find_column(line: &str, rule: &CompiledTypoRule) -> Option<usize> {
    match &rule.matcher {
        Matcher::Substring(needle_lower) => {
            let line_lower = line.to_ascii_lowercase();
            line_lower.find(needle_lower).map(|idx| idx + 1)
        }
        Matcher::Word(regex) => regex
            .find_iter(line)
            .find(|found| !is_hyphen_or_underscore_adjacent(line, found.start(), found.end()))
            .map(|found| found.start() + 1),
    }
}

fn is_hyphen_or_underscore_adjacent(line: &str, start: usize, end: usize) -> bool {
    let previous = line[..start].chars().next_back();
    let next = line[end..].chars().next();

    matches!(previous, Some('-' | '_')) || matches!(next, Some('-' | '_'))
}

fn finding_paths(findings: &[Finding]) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for finding in findings {
        if let Some(location) = finding.location.as_ref() {
            paths.insert(location.path.clone());
        }
    }

    paths.into_iter().collect()
}

fn resolve_bypass_name(config_bypass_name: Option<String>) -> String {
    let Some(name) = config_bypass_name else {
        return bypass_name_for_check_id("typo");
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return bypass_name_for_check_id("typo");
    }
    if trimmed.to_ascii_uppercase().starts_with("BYPASS_") {
        return trimmed.to_ascii_uppercase();
    }

    bypass_name_for_check_id(trimmed)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    use super::TypoCheck;

    #[tokio::test]
    async fn flags_configured_typos() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("typo.txt"),
            "let teh_value = \"x\";\nprintln!(\"recieve\");\n",
        )
        .expect("write file");

        let check = TypoCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("typo.txt").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [
                        { typo = "teh", canonical = "the", kind = "substring", guidance = "Use canonical spelling." },
                        { typo = "recieve", canonical = "receive", kind = "word", guidance = "Use canonical spelling." },
                    ]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 2);
        assert!(result.findings[0].message.contains("`teh`"));
        assert!(result.findings[1].message.contains("`recieve`"));
    }

    #[tokio::test]
    async fn ignores_hyphenated_word_matches() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("config.txt"),
            "id = \"custom-typo-check\"\n",
        )
        .expect("write file");

        let check = TypoCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("config.txt").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [
                        { typo = "typo", canonical = "Typo", kind = "word" },
                    ]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn requires_at_least_one_rule() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("clean.txt"), "all good\n").expect("write file");

        let check = TypoCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("clean.txt").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn emits_warning_when_bypass_directive_exists_and_bypass_is_enabled() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("typo.txt"), "let usfa_id = \"123\";\n").expect("write file");

        let check = TypoCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("typo.txt").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_NO_USFA_TYPO=Legacy upstream field name is intentionally retained."
                .to_owned(),
        ));
        let result = check
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [
                        { typo = "usfa_id", canonical = "usaf_id", kind = "substring", guidance = "USAF stands for USA Fencing." },
                    ]
                    allow_bypass = true
                    bypass_name = "BYPASS_NO_USFA_TYPO"
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Warning);
        assert!(result.findings[0].message.contains("BYPASS_NO_USFA_TYPO"));
        assert!(
            result.findings[0]
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("Legacy upstream field name is intentionally retained.")
        );
    }

    #[tokio::test]
    async fn includes_bypass_guidance_when_bypass_is_enabled_and_missing() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("typo.txt"), "let usfa_id = \"123\";\n").expect("write file");

        let check = TypoCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("typo.txt").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [
                        { typo = "usfa_id", canonical = "usaf_id", kind = "substring", guidance = "USAF stands for USA Fencing." },
                    ]
                    allow_bypass = true
                    bypass_name = "BYPASS_NO_USFA_TYPO"
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Error);
        let remediation = result.findings[0]
            .remediation
            .as_deref()
            .unwrap_or_default()
            .to_owned();
        assert!(remediation.contains("BYPASS_NO_USFA_TYPO"));
        assert!(remediation.contains("Never use bypasses for convenience"));
    }

    #[tokio::test]
    async fn does_not_bypass_when_bypass_is_disabled() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("typo.txt"), "let usfa_id = \"123\";\n").expect("write file");

        let check = TypoCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("typo.txt").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_NO_USFA_TYPO=Legacy upstream field name is intentionally retained."
                .to_owned(),
        ));
        let result = check
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [
                        { typo = "usfa_id", canonical = "usaf_id", kind = "substring", guidance = "USAF stands for USA Fencing." },
                    ]
                    allow_bypass = false
                    bypass_name = "BYPASS_NO_USFA_TYPO"
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Error);
        assert!(
            !result.findings[0].message.contains("check was bypassed"),
            "expected normal typo finding when bypass is disabled"
        );
    }
}
