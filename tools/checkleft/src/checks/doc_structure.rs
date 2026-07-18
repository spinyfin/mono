use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;

use crate::check::{Check, ConfiguredCheck, count_applicable, run_per_text_file};
use crate::input::{ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

/// Flags two ways generated/hand-written docs commonly render as a single
/// wall-of-text paragraph, because Markdown folds consecutive non-blank
/// lines into one paragraph:
///
/// 1. A metadata/framing line (e.g. `Date:`, `Task:`, `Verdict:`) glued to
///    neighboring prose instead of standing alone (blank-line-separated or
///    a list item).
/// 2. An opening paragraph after the H1 that runs on far longer than a
///    short lead-in should.
#[derive(Debug, Default)]
pub struct DocStructureCheck;

#[async_trait]
impl Check for DocStructureCheck {
    fn id(&self) -> &str {
        "md/doc-structure"
    }

    fn description(&self) -> &str {
        "flags metadata lines smooshed into prose paragraphs and overlong opening paragraphs in Markdown docs"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledDocStructureConfig {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        count_applicable(changeset, |path| self.applies_to(path))
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
            |path| self.applies_to(path),
            &*on_file_processed,
            |changed_file, contents, findings| {
                self.check_smooshed_metadata(changed_file, contents, findings);
                self.check_first_paragraph_length(changed_file, contents, findings);
            },
        );

        Ok(CheckResult {
            check_id: "md/doc-structure".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct DocStructureConfig {
    include_globs: Vec<String>,
    #[serde(default)]
    exclude_globs: Vec<String>,
    #[serde(default)]
    metadata_prefixes: Option<Vec<String>>,
    #[serde(default)]
    max_first_paragraph_chars: Option<usize>,
    #[serde(default)]
    severity: Option<String>,
}

struct CompiledDocStructureConfig {
    include_globs: GlobSet,
    exclude_globs: Option<GlobSet>,
    metadata_line: Regex,
    max_first_paragraph_chars: usize,
    severity: Severity,
}

const DEFAULT_METADATA_PREFIXES: &[&str] = &["Date", "Task", "Verdict"];
const DEFAULT_MAX_FIRST_PARAGRAPH_CHARS: usize = 600;

static LIST_ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:[-*+]\s|\d+[.)]\s)").expect("valid regex"));
static NON_PROSE_BLOCK_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:#|>|\||```|[-*+]\s|\d+[.)]\s)").expect("valid regex"));
static H1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#\s+\S").expect("valid regex"));

impl CompiledDocStructureConfig {
    fn applies_to(&self, path: &Path) -> bool {
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            return false;
        }
        if let Some(exclude_globs) = &self.exclude_globs
            && exclude_globs.is_match(path)
        {
            return false;
        }
        self.include_globs.is_match(path)
    }

    fn check_smooshed_metadata(
        &self,
        changed_file: &crate::input::ChangedFile,
        contents: &str,
        findings: &mut Vec<Finding>,
    ) {
        for block in paragraph_blocks(contents) {
            if block.lines.len() < 2 {
                continue;
            }
            for (offset, line) in block.lines.iter().enumerate() {
                if LIST_ITEM.is_match(line) || !self.metadata_line.is_match(line) {
                    continue;
                }

                findings.push(Finding {
                    fixable: false,
                    severity: self.severity,
                    message: format!(
                        "metadata/framing line `{}` is smooshed into a multi-line paragraph",
                        line.trim()
                    ),
                    location: Some(Location {
                        path: changed_file.path.clone(),
                        line: Some((block.start_line + offset + 1) as u32),
                        column: Some(1),
                    }),
                    remediations: vec![
                        "Markdown folds consecutive single-newline lines into one paragraph. Put metadata \
                         (Date, Task/provenance, related work items) in a bullet list or table immediately \
                         after the H1, or give the line its own paragraph separated by blank lines from \
                         surrounding prose."
                            .to_owned(),
                    ],
                    suggested_fix: None,
                });
            }
        }
    }

    fn check_first_paragraph_length(
        &self,
        changed_file: &crate::input::ChangedFile,
        contents: &str,
        findings: &mut Vec<Finding>,
    ) {
        let Some(first_paragraph) = first_paragraph_after_h1(contents) else {
            return;
        };
        let length: usize = first_paragraph
            .lines
            .iter()
            .map(|line| line.trim().len())
            .sum::<usize>()
            + first_paragraph.lines.len().saturating_sub(1);
        if length <= self.max_first_paragraph_chars {
            return;
        }

        findings.push(Finding {
            fixable: false,
            severity: self.severity,
            message: format!(
                "first paragraph after the title is {length} chars, over the {}-char limit",
                self.max_first_paragraph_chars
            ),
            location: Some(Location {
                path: changed_file.path.clone(),
                line: Some((first_paragraph.start_line + 1) as u32),
                column: Some(1),
            }),
            remediations: vec![
                "Keep the opening paragraph after the title to 2-3 short sentences. Move framing, method, \
                 and findings into their own sections below, separated by blank lines."
                    .to_owned(),
            ],
            suggested_fix: None,
        });
    }
}

struct ParagraphBlock<'a> {
    start_line: usize,
    lines: Vec<&'a str>,
}

fn paragraph_blocks(contents: &str) -> Vec<ParagraphBlock<'_>> {
    let mut blocks = Vec::new();
    let mut current: Option<ParagraphBlock<'_>> = None;

    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            continue;
        }

        match &mut current {
            Some(block) => block.lines.push(line),
            None => {
                current = Some(ParagraphBlock {
                    start_line: index,
                    lines: vec![line],
                });
            }
        }
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }

    blocks
}

fn first_paragraph_after_h1(contents: &str) -> Option<ParagraphBlock<'_>> {
    let lines: Vec<&str> = contents.lines().collect();
    let h1_index = lines.iter().position(|line| H1.is_match(line))?;

    let mut index = h1_index + 1;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    if index >= lines.len() || NON_PROSE_BLOCK_START.is_match(lines[index]) {
        return None;
    }

    let start_line = index;
    let mut block_lines = Vec::new();
    while index < lines.len() && !lines[index].trim().is_empty() {
        block_lines.push(lines[index]);
        index += 1;
    }

    Some(ParagraphBlock {
        start_line,
        lines: block_lines,
    })
}

fn parse_config(config: &toml::Value) -> Result<CompiledDocStructureConfig> {
    let parsed: DocStructureConfig = config.clone().try_into().context("invalid md/doc-structure config")?;
    if parsed.include_globs.is_empty() {
        bail!("md/doc-structure config must contain at least one `include_globs` entry");
    }

    let metadata_prefixes = parsed
        .metadata_prefixes
        .unwrap_or_else(|| DEFAULT_METADATA_PREFIXES.iter().map(|s| (*s).to_owned()).collect());
    let alternation = metadata_prefixes
        .iter()
        .map(|prefix| regex::escape(prefix))
        .collect::<Vec<_>>()
        .join("|");
    let metadata_line =
        Regex::new(&format!(r"^\s*(?:{alternation}):")).context("invalid metadata_prefixes alternation")?;

    Ok(CompiledDocStructureConfig {
        include_globs: compile_globs("include_globs", &parsed.include_globs)?,
        exclude_globs: if parsed.exclude_globs.is_empty() {
            None
        } else {
            Some(compile_globs("exclude_globs", &parsed.exclude_globs)?)
        },
        metadata_line,
        max_first_paragraph_chars: parsed
            .max_first_paragraph_chars
            .unwrap_or(DEFAULT_MAX_FIRST_PARAGRAPH_CHARS),
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error),
    })
}

fn compile_globs(field_name: &str, patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .with_context(|| format!("failed to compile `{field_name}` globs"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::DocStructureCheck;
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    async fn run_check(contents: &str) -> Vec<crate::output::Finding> {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs/investigations")).expect("create dirs");
        fs::write(temp.path().join("docs/investigations/example.md"), contents).expect("write doc");

        let check = DocStructureCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("docs/investigations/example.md").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    include_globs = ["docs/investigations/**"]
                }),
            )
            .await
            .expect("run check");
        result.findings
    }

    #[tokio::test]
    async fn flags_metadata_line_smooshed_into_paragraph() {
        let findings =
            run_check("# Title\n\nDate: 2026-07-18\nTask: entry 2 of the breakdown.\nVerdict: it works.\n").await;

        assert_eq!(findings.len(), 3, "{findings:?}");
        assert!(findings.iter().any(|f| f.message.contains("Date:")));
        assert!(findings.iter().any(|f| f.message.contains("Task:")));
        assert!(findings.iter().any(|f| f.message.contains("Verdict:")));
    }

    #[tokio::test]
    async fn allows_metadata_as_isolated_paragraph() {
        let findings = run_check("# Title\n\nDate: 2026-07-18\n\nSome short intro.\n").await;
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[tokio::test]
    async fn allows_metadata_as_list_items() {
        let findings = run_check("# Title\n\n- Date: 2026-07-18\n- Task: entry 2\n\nIntro.\n").await;
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[tokio::test]
    async fn flags_overlong_first_paragraph() {
        let long_sentence =
            "This sentence keeps going and going and going and going and going and going and going. ".repeat(8);
        let contents = format!("# Title\n\n{long_sentence}\n\n## Next section\n");

        let findings = run_check(&contents).await;

        assert!(
            findings.iter().any(|f| f.message.contains("first paragraph")),
            "{findings:?}"
        );
    }

    #[tokio::test]
    async fn allows_short_first_paragraph() {
        let findings = run_check("# Title\n\nA short two sentence intro. That is all.\n\n## Details\n").await;
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[tokio::test]
    async fn skips_non_markdown_files() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs/investigations")).expect("create dirs");
        fs::write(
            temp.path().join("docs/investigations/example.txt"),
            "Date: 2026-07-18\nTask: entry 2.\n",
        )
        .expect("write doc");

        let check = DocStructureCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("docs/investigations/example.txt").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    include_globs = ["docs/investigations/**"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
