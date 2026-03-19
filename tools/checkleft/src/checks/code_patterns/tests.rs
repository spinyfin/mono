use std::fs;
use std::path::Path;

use tempfile::tempdir;

use super::CodePatternsCheck;
use crate::check::Check;
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::source_tree::LocalSourceTree;

#[tokio::test]
async fn flags_future_get_on_completable_future_variable() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.CompletableFuture;

class Foo {
    String load() throws Exception {
        CompletableFuture<String> future = someAsyncMethod();
        return future.get();
    }

    CompletableFuture<String> someAsyncMethod() {
        return new CompletableFuture<>();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert_eq!(result.findings.len(), 1);
    assert_eq!(
        result.findings[0]
            .location
            .as_ref()
            .and_then(|location| location.line),
        Some(9)
    );
}

#[tokio::test]
async fn flags_var_inferred_from_local_method_return_type() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.CompletableFuture;

class Foo {
    String load() throws Exception {
        var future = someAsyncMethod();
        return future.get();
    }

    CompletableFuture<String> someAsyncMethod() {
        return new CompletableFuture<>();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert_eq!(result.findings.len(), 1);
}

#[tokio::test]
async fn flags_same_file_subtype_for_future_pattern() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.Future;

class MyFuture implements Future<String> {}

class Foo {
    String load(MyFuture future) throws Exception {
        return future.get();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert_eq!(result.findings.len(), 1);
}

#[tokio::test]
async fn ignores_timeout_overload_and_unrelated_get() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

class Other {
    String get() { return "ok"; }
}

class Foo {
    String load(Future<String> future, Other other) throws Exception {
        future.get(1L, TimeUnit.SECONDS);
        return other.get();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert!(result.findings.is_empty());
}

#[tokio::test]
async fn rejects_unsupported_language() {
    let temp = tempdir().expect("create temp dir");
    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let error = check
        .run(
            &ChangeSet::default(),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "kotlin"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect_err("must fail");

    assert!(
        error
            .to_string()
            .contains("unsupported code-patterns `lang`")
    );
}

#[tokio::test]
async fn rejects_non_zero_arg_nocall_pattern() {
    let temp = tempdir().expect("create temp dir");
    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let error = check
        .run(
            &ChangeSet::default(),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get(..)" }]
            }),
        )
        .await
        .expect_err("must fail");

    assert!(
        error
            .to_string()
            .contains("currently supports only zero-argument signatures")
    );
}
