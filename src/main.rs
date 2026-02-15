use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use checkleft::check::CheckRegistry;
use checkleft::checks::register_builtin_checks;
use checkleft::config::ConfigResolver;
use checkleft::input::ChangeSet;
use checkleft::output::{CheckResult, Finding, Location, Severity, SuggestedFix};
use checkleft::runner::Runner;
use checkleft::source_tree::LocalSourceTree;
use checkleft::vcs::{Vcs, github_pull_request_description};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "checkleft")]
#[command(about = "Run repository convention checks")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        base_ref: Option<String>,
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        base_ref: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

const CHECKS_PR_DESCRIPTION_ENV: &str = "CHECKS_PR_DESCRIPTION";
const CHECKS_CHANGE_ID_ENV: &str = "CHECKS_CHANGE_ID";
const CHECKS_PR_NUMBER_ENV: &str = "CHECKS_PR_NUMBER";
const CHECKS_REPOSITORY_ENV: &str = "CHECKS_REPOSITORY";
const CHECKS_GITHUB_TOKEN_ENV: &str = "CHECKS_GITHUB_TOKEN";

#[tokio::main]
async fn main() -> ExitCode {
    match run_cli().await {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_cli() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = std::env::current_dir()?;

    let vcs = Vcs::detect(&root)?;
    let resolver = Arc::new(ConfigResolver::new(&root)?);
    let source_tree = Arc::new(LocalSourceTree::new(&root)?);

    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry)?;

    let runner = Runner::new(Arc::new(registry), resolver, source_tree);

    match cli.command {
        Commands::Run {
            all,
            base_ref,
            format,
        } => {
            let changeset = attach_description_context(
                resolve_changeset(&vcs, all, base_ref.as_deref())?,
                &vcs,
            )
            .await;
            let mut results = runner.run_changeset(&changeset).await?;
            sort_results_for_output(&mut results);

            match format {
                OutputFormat::Human => print_human_results(&results),
                OutputFormat::Json => print_json_results(&results)?,
            }

            let has_error = results.iter().any(|result| {
                result
                    .findings
                    .iter()
                    .any(|finding| finding.severity == Severity::Error)
            });
            Ok(if has_error {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Commands::List { all, base_ref } => {
            let changeset = resolve_changeset(&vcs, all, base_ref.as_deref())?;
            let checks = runner.list_configured_checks(&changeset)?;
            if checks.is_empty() {
                println!("No configured checks found.");
            } else {
                for check in checks {
                    println!("{check}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn resolve_changeset(vcs: &Vcs, all: bool, base_ref: Option<&str>) -> Result<ChangeSet> {
    if all {
        return vcs.all_files_changeset();
    }

    if let Some(base_ref) = base_ref {
        if !base_ref.trim().is_empty() {
            return vcs.changeset_since(base_ref);
        }
        return vcs.current_changeset();
    }

    vcs.current_changeset()
}

async fn attach_description_context(changeset: ChangeSet, vcs: &Vcs) -> ChangeSet {
    let commit_description = normalize_optional_description(vcs.current_commit_description().ok());
    let change_id = resolve_change_id();
    let repository = resolve_repository(vcs);
    let pr_description = normalize_optional_description(
        resolve_pr_description(repository.as_deref(), change_id.as_deref()).await,
    );
    changeset
        .with_commit_description(commit_description)
        .with_change_id(change_id)
        .with_repository(repository)
        .with_pr_description(pr_description)
}

fn resolve_change_id() -> Option<String> {
    [
        std::env::var(CHECKS_CHANGE_ID_ENV),
        std::env::var(CHECKS_PR_NUMBER_ENV),
    ]
    .into_iter()
    .find_map(|value| normalize_optional_description(value.ok()))
}

fn resolve_repository(vcs: &Vcs) -> Option<String> {
    normalize_optional_description(std::env::var(CHECKS_REPOSITORY_ENV).ok())
        .or_else(|| normalize_optional_description(vcs.remote_repo_slug()))
}

async fn resolve_pr_description(
    repository: Option<&str>,
    change_id: Option<&str>,
) -> Option<String> {
    if let Ok(raw) = std::env::var(CHECKS_PR_DESCRIPTION_ENV) {
        if !raw.trim().is_empty() {
            return Some(raw);
        }
    }

    let Some(repository) = repository else {
        return None;
    };
    let Some(change_id) = change_id else {
        return None;
    };

    let github_token = detect_github_token();
    github_pull_request_description(repository, change_id, github_token.as_deref()).await
}

fn detect_github_token() -> Option<String> {
    [
        std::env::var(CHECKS_GITHUB_TOKEN_ENV),
        std::env::var("GH_TOKEN"),
        std::env::var("GITHUB_TOKEN"),
    ]
    .into_iter()
    .find_map(|value| normalize_optional_description(value.ok()))
}

fn normalize_optional_description(value: Option<String>) -> Option<String> {
    value
        .map(|description| description.trim().to_owned())
        .filter(|description| !description.is_empty())
}

fn print_human_results(results: &[CheckResult]) {
    print!(
        "{}",
        render_human_results(results, OutputStyle::detect_for_stdout())
    );
}

fn print_json_results(results: &[CheckResult]) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(results)?);
    Ok(())
}

fn sort_results_for_output(results: &mut [CheckResult]) {
    for result in results.iter_mut() {
        result
            .findings
            .sort_by_key(|finding| severity_sort_key(finding.severity));
    }

    results.sort_by(|left, right| {
        most_severe_finding_sort_key(left)
            .cmp(&most_severe_finding_sort_key(right))
            .then_with(|| left.check_id.cmp(&right.check_id))
    });
}

fn most_severe_finding_sort_key(result: &CheckResult) -> u8 {
    result
        .findings
        .iter()
        .map(|finding| severity_sort_key(finding.severity))
        .min()
        .unwrap_or(u8::MAX)
}

fn severity_sort_key(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    }
}

fn render_human_results(results: &[CheckResult], style: OutputStyle) -> String {
    if results.is_empty() {
        return "No checks ran.\n".to_owned();
    }

    let total_findings: usize = results.iter().map(|result| result.findings.len()).sum();
    if total_findings == 0 {
        return format!(
            "{}: no findings ({} checks run)\n",
            style.paint_info("checks"),
            results.len()
        );
    }

    let mut output = String::new();
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut infos = 0usize;

    for result in results {
        for finding in &result.findings {
            match finding.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => infos += 1,
            }

            output.push_str(&render_finding(result, finding, style));
        }
    }

    output.push_str(&format!(
        "{}: {errors} error(s), {warnings} warning(s), {infos} info finding(s)\n",
        style.paint_bold("summary")
    ));
    output
}

fn render_finding(result: &CheckResult, finding: &Finding, style: OutputStyle) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}[{}]: {}\n",
        style.paint_severity(finding.severity),
        result.check_id,
        finding.message
    ));

    let location = finding
        .location
        .as_ref()
        .map(format_location)
        .unwrap_or_else(|| "<unknown>".to_owned());
    out.push_str(&format!("  --> {location}\n"));

    if let Some(remediation) = &finding.remediation {
        out.push_str(&format!(
            "   = {}: {remediation}\n",
            style.paint_help_label("help")
        ));
    }

    if let Some(suggested_fix) = &finding.suggested_fix {
        out.push_str(&format!(
            "   = {}: {}\n",
            style.paint_help_label("fix"),
            format_fix_summary(suggested_fix)
        ));
    }

    out.push('\n');
    out
}

fn format_location(location: &Location) -> String {
    let path = location.path.display();
    match (location.line, location.column) {
        (Some(line), Some(column)) => format!("{path}:{line}:{column}"),
        (Some(line), None) => format!("{path}:{line}"),
        (None, _) => format!("{path}"),
    }
}

fn format_fix_summary(suggested_fix: &SuggestedFix) -> String {
    format!(
        "{} ({} edit{})",
        suggested_fix.description,
        suggested_fix.edits.len(),
        if suggested_fix.edits.len() == 1 {
            ""
        } else {
            "s"
        }
    )
}

#[derive(Clone, Copy)]
struct OutputStyle {
    color: bool,
}

impl OutputStyle {
    fn detect_for_stdout() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            color: std::io::stdout().is_terminal() && !no_color,
        }
    }

    fn paint_bold(self, text: &str) -> String {
        self.paint(text, "1")
    }

    fn paint_error(self, text: &str) -> String {
        self.paint(text, "1;31")
    }

    fn paint_warning(self, text: &str) -> String {
        self.paint(text, "1;33")
    }

    fn paint_info(self, text: &str) -> String {
        self.paint(text, "1;36")
    }

    fn paint_help_label(self, text: &str) -> String {
        self.paint(text, "1;32")
    }

    fn paint_severity(self, severity: Severity) -> String {
        match severity {
            Severity::Error => self.paint_error("error"),
            Severity::Warning => self.paint_warning("warning"),
            Severity::Info => self.paint_info("info"),
        }
    }

    fn paint(self, text: &str, code: &str) -> String {
        if self.color {
            format!("\u{1b}[{code}m{text}\u{1b}[0m")
        } else {
            text.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use checkleft::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};

    use super::{
        OutputStyle, normalize_optional_description, render_human_results, sort_results_for_output,
    };

    #[test]
    fn human_output_includes_line_and_column() {
        let output = render_human_results(
            &[CheckResult {
                check_id: "typo".to_owned(),
                findings: vec![Finding {
                    severity: Severity::Error,
                    message: "Found typo `teh`; use `the` instead.".to_owned(),
                    location: Some(Location {
                        path: PathBuf::from("docs/CHECKS.toml"),
                        line: Some(12),
                        column: Some(5),
                    }),
                    remediation: Some("Replace typo.".to_owned()),
                    suggested_fix: None,
                }],
            }],
            OutputStyle { color: false },
        );

        assert!(output.contains("error[typo]: Found typo `teh`; use `the` instead."));
        assert!(output.contains("  --> docs/CHECKS.toml:12:5"));
        assert!(output.contains("   = help: Replace typo."));
    }

    #[test]
    fn human_output_omits_ansi_when_color_is_disabled() {
        let output = render_human_results(
            &[CheckResult {
                check_id: "file-size".to_owned(),
                findings: vec![Finding {
                    severity: Severity::Warning,
                    message: "File exceeds configured line count.".to_owned(),
                    location: Some(Location {
                        path: PathBuf::from("backend/src/lib.rs"),
                        line: Some(200),
                        column: None,
                    }),
                    remediation: None,
                    suggested_fix: Some(SuggestedFix {
                        description: "Split file by module.".to_owned(),
                        edits: vec![FileEdit {
                            path: PathBuf::from("backend/src/lib.rs"),
                            old_text: "old".to_owned(),
                            new_text: "new".to_owned(),
                        }],
                    }),
                }],
            }],
            OutputStyle { color: false },
        );

        assert!(!output.contains("\u{1b}["));
        assert!(output.contains("  --> backend/src/lib.rs:200"));
        assert!(output.contains("   = fix: Split file by module. (1 edit)"));
    }

    #[test]
    fn output_sorting_prioritizes_error_checks_before_warning_checks() {
        let mut results = vec![
            CheckResult {
                check_id: "alpha-warning".to_owned(),
                findings: vec![Finding {
                    severity: Severity::Warning,
                    message: "warning finding".to_owned(),
                    location: None,
                    remediation: None,
                    suggested_fix: None,
                }],
            },
            CheckResult {
                check_id: "zeta-error".to_owned(),
                findings: vec![Finding {
                    severity: Severity::Error,
                    message: "error finding".to_owned(),
                    location: None,
                    remediation: None,
                    suggested_fix: None,
                }],
            },
        ];

        sort_results_for_output(&mut results);

        assert_eq!(results[0].check_id, "zeta-error");
        assert_eq!(results[1].check_id, "alpha-warning");
    }

    #[test]
    fn output_sorting_orders_findings_within_each_check_by_severity() {
        let mut results = vec![CheckResult {
            check_id: "mixed".to_owned(),
            findings: vec![
                Finding {
                    severity: Severity::Warning,
                    message: "warning finding".to_owned(),
                    location: None,
                    remediation: None,
                    suggested_fix: None,
                },
                Finding {
                    severity: Severity::Info,
                    message: "info finding".to_owned(),
                    location: None,
                    remediation: None,
                    suggested_fix: None,
                },
                Finding {
                    severity: Severity::Error,
                    message: "error finding".to_owned(),
                    location: None,
                    remediation: None,
                    suggested_fix: None,
                },
            ],
        }];

        sort_results_for_output(&mut results);

        let severities: Vec<_> = results[0]
            .findings
            .iter()
            .map(|finding| finding.severity)
            .collect();
        assert_eq!(
            severities,
            vec![Severity::Error, Severity::Warning, Severity::Info]
        );
    }

    #[test]
    fn normalize_optional_description_trims_and_filters_empty_values() {
        assert_eq!(normalize_optional_description(None), None);
        assert_eq!(normalize_optional_description(Some("".to_owned())), None);
        assert_eq!(
            normalize_optional_description(Some("  235  ".to_owned())),
            Some("235".to_owned())
        );
    }
}
