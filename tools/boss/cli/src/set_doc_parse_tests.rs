//! Parse tests for `boss task set-doc`. Kept out of `tests.rs` so that
//! file stays under the monorepo `max_lines` budget.

use clap::Parser;

use super::{Cli, Commands, TaskCommand};

#[test]
fn parses_task_set_doc_with_path() {
    let cli = Cli::parse_from(["boss", "task", "set-doc", "42", "--path", "docs/investigations/foo.md"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::SetDoc(args),
        } => {
            assert_eq!(args.id, "42");
            assert_eq!(args.path.as_deref(), Some("docs/investigations/foo.md"));
            assert!(!args.unset);
            assert!(args.repo.is_none());
            assert!(args.branch.is_none());
            assert!(args.product.is_none());
        }
        _ => panic!("expected task set-doc command"),
    }
}

#[test]
fn parses_task_set_doc_with_repo_and_branch() {
    let cli = Cli::parse_from([
        "boss",
        "task",
        "set-doc",
        "task_abc",
        "--path",
        "docs/investigations/foo.md",
        "--repo",
        "https://github.com/myorg/wiki.git",
        "--branch",
        "trunk",
        "--product",
        "boss",
    ]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::SetDoc(args),
        } => {
            assert_eq!(args.id, "task_abc");
            assert_eq!(args.repo.as_deref(), Some("https://github.com/myorg/wiki.git"));
            assert_eq!(args.branch.as_deref(), Some("trunk"));
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected task set-doc command"),
    }
}

#[test]
fn parses_task_set_doc_with_unset() {
    let cli = Cli::parse_from(["boss", "task", "set-doc", "99", "--unset"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::SetDoc(args),
        } => {
            assert!(args.unset);
            assert!(args.path.is_none());
            assert_eq!(args.id, "99");
        }
        _ => panic!("expected task set-doc command"),
    }
}

#[test]
fn rejects_task_set_doc_unset_combined_with_path() {
    let err = Cli::try_parse_from([
        "boss",
        "task",
        "set-doc",
        "1",
        "--unset",
        "--path",
        "docs/investigations/foo.md",
    ])
    .expect_err("unset + path must conflict");
    let rendered = err.to_string();
    assert!(
        rendered.contains("--unset") || rendered.contains("--path"),
        "{rendered}"
    );
}

#[test]
fn rejects_task_set_doc_repo_without_path() {
    let err = Cli::try_parse_from(["boss", "task", "set-doc", "1", "--repo", "https://github.com/x/y.git"])
        .expect_err("repo without path must error");
    assert!(err.to_string().contains("--path"), "{err}");
}
