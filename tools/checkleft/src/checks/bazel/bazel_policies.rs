use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tree_sitter::Node;

use crate::check::{Check, ConfiguredCheck, count_applicable, run_per_text_file};
use crate::input::{ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

use super::starlark::{
    ParsedStarlarkFile, SourceLocation, StarlarkFileKind, call_function_name, find_matching_string_literal,
    normalize_callee, parse_starlark_file, source_location, starlark_file_kind,
};

#[derive(Debug, Default)]
pub(crate) struct BazelPoliciesCheck;

#[async_trait]
impl Check for BazelPoliciesCheck {
    fn id(&self) -> &str {
        "bazel-policies"
    }

    fn description(&self) -> &str {
        "flags configured Bazel Starlark policy violations in changed files"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledBazelPoliciesConfig {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        count_applicable(changeset, is_starlark_file)
    }

    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let findings = run_per_text_file(
            changeset,
            tree,
            is_starlark_file,
            &*on_file_processed,
            |changed_file, contents, findings| {
                let Some(file_kind) = starlark_file_kind(&changed_file.path) else {
                    return;
                };
                let Some(parsed) = parse_starlark_file(contents) else {
                    return;
                };

                for rule in &self.rules {
                    findings.extend(rule.evaluate(&changed_file.path, file_kind, &parsed));
                }
            },
        );

        Ok(CheckResult {
            check_id: "bazel-policies".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BazelPoliciesConfig {
    #[serde(default)]
    rules: Vec<BazelPolicyRuleConfig>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BazelPolicyRuleConfig {
    ForbiddenRuleCall {
        symbols: Vec<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        remediation: Option<String>,
        #[serde(default)]
        severity: Option<String>,
    },
    ForbiddenPackageDefaultVisibility {
        values: Vec<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        remediation: Option<String>,
        #[serde(default)]
        severity: Option<String>,
    },
    RequiredPackageFeature {
        when_calling: Vec<String>,
        feature: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        remediation: Option<String>,
        #[serde(default)]
        severity: Option<String>,
    },
}

#[derive(Debug)]
struct CompiledBazelPoliciesConfig {
    rules: Vec<CompiledRule>,
}

#[derive(Debug)]
enum CompiledRule {
    ForbiddenRuleCall(CompiledForbiddenRuleCallRule),
    ForbiddenPackageDefaultVisibility(CompiledForbiddenPackageDefaultVisibilityRule),
    RequiredPackageFeature(CompiledRequiredPackageFeatureRule),
}

#[derive(Debug)]
struct CompiledForbiddenRuleCallRule {
    symbols: Vec<String>,
    message: Option<String>,
    remediation: Option<String>,
    severity: Severity,
}

#[derive(Debug)]
struct CompiledForbiddenPackageDefaultVisibilityRule {
    values: Vec<String>,
    message: Option<String>,
    remediation: Option<String>,
    severity: Severity,
}

#[derive(Debug)]
struct CompiledRequiredPackageFeatureRule {
    when_calling: Vec<String>,
    feature: String,
    message: Option<String>,
    remediation: Option<String>,
    severity: Severity,
}

impl CompiledRule {
    fn evaluate(&self, path: &Path, file_kind: StarlarkFileKind, parsed: &ParsedStarlarkFile<'_>) -> Vec<Finding> {
        match self {
            Self::ForbiddenRuleCall(rule) => rule.evaluate(path, parsed),
            Self::ForbiddenPackageDefaultVisibility(rule) => rule.evaluate(path, file_kind, parsed),
            Self::RequiredPackageFeature(rule) => rule.evaluate(path, file_kind, parsed),
        }
    }
}

impl CompiledForbiddenRuleCallRule {
    fn evaluate(&self, path: &Path, parsed: &ParsedStarlarkFile<'_>) -> Vec<Finding> {
        let mut findings = Vec::new();
        collect_findings_forbidden_rule_calls(parsed.root(), parsed.source, self, path, &mut findings);
        findings
    }
}

impl CompiledForbiddenPackageDefaultVisibilityRule {
    fn evaluate(&self, path: &Path, file_kind: StarlarkFileKind, parsed: &ParsedStarlarkFile<'_>) -> Vec<Finding> {
        if file_kind != StarlarkFileKind::Build {
            return Vec::new();
        }

        let mut findings = Vec::new();
        collect_findings_forbidden_default_visibility(parsed.root(), parsed.source, self, path, &mut findings);
        findings
    }
}

impl CompiledRequiredPackageFeatureRule {
    fn evaluate(&self, path: &Path, file_kind: StarlarkFileKind, parsed: &ParsedStarlarkFile<'_>) -> Vec<Finding> {
        if file_kind != StarlarkFileKind::Build {
            return Vec::new();
        }

        let Some((matched_symbol, trigger_location)) =
            find_first_rule_call(parsed.root(), parsed.source, &self.when_calling)
        else {
            return Vec::new();
        };

        let required = [self.feature.clone()];
        let mut package_location = None;
        if package_declares_feature(parsed.root(), parsed.source, &required, &mut package_location) {
            return Vec::new();
        }

        let location = package_location.unwrap_or(trigger_location);
        vec![Finding {
            fixable: false,
            severity: self.severity,
            message: self.message.clone().unwrap_or_else(|| {
                format!(
                    "BUILD file calling `{matched_symbol}` must enable package feature `{}`.",
                    self.feature
                )
            }),
            location: Some(Location {
                path: path.to_path_buf(),
                line: Some(location.line),
                column: Some(location.column),
            }),
            surface: None,
            remediations: vec![self.remediation.clone().unwrap_or_else(|| {
                format!(
                    "Add `\"{}\"` to `package(features = [...])` in this BUILD file.",
                    self.feature
                )
            })],
            suggested_fix: None,
        }]
    }
}

fn is_starlark_file(path: &Path) -> bool {
    starlark_file_kind(path).is_some()
}

fn parse_config(config: &toml::Value) -> Result<CompiledBazelPoliciesConfig> {
    let parsed: BazelPoliciesConfig = config
        .clone()
        .try_into()
        .context("invalid bazel-policies check config")?;
    if parsed.rules.is_empty() {
        bail!("bazel-policies check config must contain at least one `rules` entry");
    }

    let default_severity = Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error);
    let default_remediation = normalize_optional_string(parsed.remediation, "remediation")?;

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for (index, rule) in parsed.rules.into_iter().enumerate() {
        let field_prefix = format!("rules[{index}]");
        rules.push(match rule {
            BazelPolicyRuleConfig::ForbiddenRuleCall {
                symbols,
                message,
                remediation,
                severity,
            } => CompiledRule::ForbiddenRuleCall(CompiledForbiddenRuleCallRule {
                symbols: normalize_non_empty_unique_strings(symbols, &format!("{field_prefix}.symbols"))?,
                message: normalize_optional_string(message, &format!("{field_prefix}.message"))?,
                remediation: normalize_optional_string(remediation, &format!("{field_prefix}.remediation"))?
                    .or_else(|| default_remediation.clone()),
                severity: Severity::parse_with_default(severity.as_deref(), default_severity),
            }),
            BazelPolicyRuleConfig::ForbiddenPackageDefaultVisibility {
                values,
                message,
                remediation,
                severity,
            } => CompiledRule::ForbiddenPackageDefaultVisibility(CompiledForbiddenPackageDefaultVisibilityRule {
                values: normalize_non_empty_unique_strings(values, &format!("{field_prefix}.values"))?,
                message: normalize_optional_string(message, &format!("{field_prefix}.message"))?,
                remediation: normalize_optional_string(remediation, &format!("{field_prefix}.remediation"))?
                    .or_else(|| default_remediation.clone()),
                severity: Severity::parse_with_default(severity.as_deref(), default_severity),
            }),
            BazelPolicyRuleConfig::RequiredPackageFeature {
                when_calling,
                feature,
                message,
                remediation,
                severity,
            } => {
                let feature = feature.trim();
                if feature.is_empty() {
                    bail!("bazel-policies check config `{field_prefix}.feature` must not be empty");
                }
                CompiledRule::RequiredPackageFeature(CompiledRequiredPackageFeatureRule {
                    when_calling: normalize_non_empty_unique_strings(
                        when_calling,
                        &format!("{field_prefix}.when_calling"),
                    )?,
                    feature: feature.to_owned(),
                    message: normalize_optional_string(message, &format!("{field_prefix}.message"))?,
                    remediation: normalize_optional_string(remediation, &format!("{field_prefix}.remediation"))?
                        .or_else(|| default_remediation.clone()),
                    severity: Severity::parse_with_default(severity.as_deref(), default_severity),
                })
            }
        });
    }

    Ok(CompiledBazelPoliciesConfig { rules })
}

fn normalize_optional_string(value: Option<String>, field_name: &str) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("bazel-policies check config `{field_name}` must not be empty when present");
    }
    Ok(Some(trimmed.to_owned()))
}

fn normalize_non_empty_unique_strings(values: Vec<String>, field_name: &str) -> Result<Vec<String>> {
    if values.is_empty() {
        bail!("bazel-policies check config `{field_name}` must contain at least one value");
    }

    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("bazel-policies check config `{field_name}` must not contain empty values");
        }
        if seen.insert(trimmed.to_owned()) {
            output.push(trimmed.to_owned());
        }
    }
    Ok(output)
}

fn collect_findings_forbidden_rule_calls(
    node: Node<'_>,
    source: &[u8],
    rule: &CompiledForbiddenRuleCallRule,
    path: &Path,
    findings: &mut Vec<Finding>,
) {
    if let Some((matched_symbol, location)) = forbidden_rule_call_match(node, source, &rule.symbols) {
        findings.push(Finding {
            fixable: false,
            severity: rule.severity,
            message: rule
                .message
                .clone()
                .unwrap_or_else(|| format!("Disallowed Bazel call to `{matched_symbol}`.")),
            location: Some(Location {
                path: path.to_path_buf(),
                line: Some(location.line),
                column: Some(location.column),
            }),
            surface: None,
            remediations: vec![rule.remediation.clone().unwrap_or_else(|| {
                "Replace the forbidden Bazel rule or macro call with an approved alternative.".to_owned()
            })],
            suggested_fix: None,
        });
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_findings_forbidden_rule_calls(child, source, rule, path, findings);
    }
}

fn forbidden_rule_call_match<'a>(
    node: Node<'_>,
    source: &'a [u8],
    symbols: &'a [String],
) -> Option<(&'a str, SourceLocation)> {
    if node.kind() != "call" {
        return None;
    }

    let function = node.child_by_field_name("function")?;
    let callee = normalize_callee(function, source)?;
    let matched_symbol = symbols.iter().find(|symbol| symbol.as_str() == callee)?;
    Some((matched_symbol.as_str(), source_location(function)))
}

fn collect_findings_forbidden_default_visibility(
    node: Node<'_>,
    source: &[u8],
    rule: &CompiledForbiddenPackageDefaultVisibilityRule,
    path: &Path,
    findings: &mut Vec<Finding>,
) {
    if let Some((matched_value, location)) = forbidden_default_visibility_match(node, source, &rule.values) {
        findings.push(Finding {
            fixable: false,
            severity: rule.severity,
            message: rule
                .message
                .clone()
                .unwrap_or_else(|| format!("package default_visibility must not include `{matched_value}`")),
            location: Some(Location {
                path: path.to_path_buf(),
                line: Some(location.line),
                column: Some(location.column),
            }),
            surface: None,
            remediations: vec![rule.remediation.clone().unwrap_or_else(|| {
                "Remove the package default visibility or narrow visibility on individual targets.".to_owned()
            })],
            suggested_fix: None,
        });
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_findings_forbidden_default_visibility(child, source, rule, path, findings);
    }
}

fn forbidden_default_visibility_match<'a>(
    node: Node<'_>,
    source: &[u8],
    values: &'a [String],
) -> Option<(&'a str, SourceLocation)> {
    if node.kind() != "call" || call_function_name(node, source)? != "package" {
        return None;
    }

    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        if argument.kind() != "keyword_argument" {
            continue;
        }
        let Some(name) = argument.child_by_field_name("name") else {
            continue;
        };
        let Ok(name_text) = name.utf8_text(source) else {
            continue;
        };
        if name_text != "default_visibility" {
            continue;
        }

        let value = argument.child_by_field_name("value")?;
        if let Some((matched_value, location)) = find_matching_string_literal(value, source, values) {
            return Some((matched_value, location));
        }
    }

    None
}

fn find_first_rule_call<'a>(
    node: Node<'_>,
    source: &'a [u8],
    symbols: &'a [String],
) -> Option<(&'a str, SourceLocation)> {
    if let Some(matched) = forbidden_rule_call_match(node, source, symbols) {
        return Some(matched);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(matched) = find_first_rule_call(child, source, symbols) {
            return Some(matched);
        }
    }
    None
}

fn package_declares_feature(
    node: Node<'_>,
    source: &[u8],
    required: &[String],
    package_location: &mut Option<SourceLocation>,
) -> bool {
    if node.kind() == "call" && call_function_name(node, source) == Some("package") {
        package_location.get_or_insert_with(|| source_location(node));
        if let Some(arguments) = node.child_by_field_name("arguments") {
            let mut cursor = arguments.walk();
            for argument in arguments.named_children(&mut cursor) {
                if argument.kind() != "keyword_argument" {
                    continue;
                }
                let Some(name) = argument.child_by_field_name("name") else {
                    continue;
                };
                let Ok(name_text) = name.utf8_text(source) else {
                    continue;
                };
                if name_text != "features" {
                    continue;
                }
                if let Some(value) = argument.child_by_field_name("value")
                    && find_matching_string_literal(value, source, required).is_some()
                {
                    return true;
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if package_declares_feature(child, source, required, package_location) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::BazelPoliciesCheck;
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    #[tokio::test]
    async fn flags_forbidden_rule_call_in_build_file() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
genrule(
    name = "demo",
    outs = ["demo.txt"],
    cmd = "echo hi > $@",
)
"#,
        )
        .expect("write build file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "forbidden_rule_call", symbols = ["genrule", "native.genrule"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].location.as_ref().and_then(|loc| loc.line), Some(2));
        assert!(result.findings[0].message.contains("genrule"));
    }

    #[tokio::test]
    async fn flags_forbidden_rule_call_in_bzl_file() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("defs")).expect("create defs dir");
        fs::write(
            temp.path().join("defs/rules.bzl"),
            r#"
def make_demo(name):
    native.genrule(
        name = name,
        outs = [name + ".txt"],
        cmd = "echo hi > $@",
    )
"#,
        )
        .expect("write bzl file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("defs/rules.bzl").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "forbidden_rule_call", symbols = ["native.genrule"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].location.as_ref().and_then(|loc| loc.line), Some(3));
        assert_eq!(
            result.findings[0].remediations,
            vec!["Replace the forbidden Bazel rule or macro call with an approved alternative."]
        );
    }

    #[tokio::test]
    async fn flags_forbidden_package_default_visibility() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
package(
    default_visibility = [
        "//visibility:public",
    ],
)
"#,
        )
        .expect("write build file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "forbidden_package_default_visibility", values = ["//visibility:public"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].location.as_ref().and_then(|loc| loc.line), Some(4));
        assert!(result.findings[0].message.contains("//visibility:public"));
    }

    #[tokio::test]
    async fn ignores_default_visibility_rule_for_module_file() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("MODULE.bazel"),
            r#"
bazel_dep(name = "rules_rust", version = "0.62.0")
"#,
        )
        .expect("write module file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("MODULE.bazel").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "forbidden_package_default_visibility", values = ["//visibility:public"] }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn flags_missing_required_package_feature_when_package_call_omits_it() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
load("@build_bazel_rules_swift//swift:swift.bzl", "swift_library")

package(default_visibility = ["//visibility:private"])

swift_library(
    name = "demo",
    srcs = ["Demo.swift"],
)
"#,
        )
        .expect("write build file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        kind = "required_package_feature",
                        when_calling = ["swift_library", "swift_binary"],
                        feature = "swift.treat_warnings_as_errors",
                    }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].location.as_ref().and_then(|loc| loc.line), Some(4));
        assert!(result.findings[0].message.contains("swift.treat_warnings_as_errors"));
    }

    #[tokio::test]
    async fn flags_missing_required_package_feature_when_package_call_absent() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
load("@build_bazel_rules_swift//swift:swift.bzl", "swift_library")

swift_library(
    name = "demo",
    srcs = ["Demo.swift"],
)
"#,
        )
        .expect("write build file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        kind = "required_package_feature",
                        when_calling = ["swift_library"],
                        feature = "swift.treat_warnings_as_errors",
                    }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].location.as_ref().and_then(|loc| loc.line), Some(4));
    }

    #[tokio::test]
    async fn rejects_negated_feature_for_required_package_feature() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
package(features = ["-swift.treat_warnings_as_errors"])

swift_library(name = "demo", srcs = ["Demo.swift"])
"#,
        )
        .expect("write build file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        kind = "required_package_feature",
                        when_calling = ["swift_library"],
                        feature = "swift.treat_warnings_as_errors",
                    }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn allows_build_file_with_required_package_feature() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
package(
    default_visibility = ["//visibility:private"],
    features = ["swift.treat_warnings_as_errors"],
)

swift_library(name = "demo", srcs = ["Demo.swift"])
"#,
        )
        .expect("write build file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        kind = "required_package_feature",
                        when_calling = ["swift_library"],
                        feature = "swift.treat_warnings_as_errors",
                    }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn ignores_required_package_feature_for_non_matching_build_and_bzl_files() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
# Mentions swift_library in a comment only.
package(default_visibility = ["//visibility:private"])
cc_binary(name = "helper", srcs = ["helper.c"])
"#,
        )
        .expect("write build file");
        fs::write(
            temp.path().join("macros.bzl"),
            r#"
def my_swift_macro(name):
    swift_library(name = name, srcs = [name + ".swift"])
"#,
        )
        .expect("write bzl file");

        let check = BazelPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![
                    ChangedFile {
                        path: Path::new("BUILD.bazel").to_path_buf(),
                        kind: ChangeKind::Added,
                        old_path: None,
                    },
                    ChangedFile {
                        path: Path::new("macros.bzl").to_path_buf(),
                        kind: ChangeKind::Added,
                        old_path: None,
                    },
                ]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        kind = "required_package_feature",
                        when_calling = ["swift_library"],
                        feature = "swift.treat_warnings_as_errors",
                    }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
