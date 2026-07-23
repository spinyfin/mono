use super::*;

#[tokio::test]
async fn open_document_missing_path_returns_not_found() {
    let (server_state, dir) = test_server_state();
    let path = dir.path().join("does-not-exist.md");
    let err = server_state
        .open_document(path.to_str().unwrap())
        .await
        .expect_err("missing path should fail");
    assert!(matches!(err, OpenDocumentError::NotFound(_)));
}

#[tokio::test]
async fn open_document_directory_returns_not_a_file() {
    let (server_state, dir) = test_server_state();
    let err = server_state
        .open_document(dir.path().to_str().unwrap())
        .await
        .expect_err("directory should fail");
    assert!(matches!(err, OpenDocumentError::NotAFile(_)));
}

#[tokio::test]
async fn open_document_non_markdown_file_returns_not_markdown() {
    let (server_state, dir) = test_server_state();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, b"hello").unwrap();
    let err = server_state
        .open_document(path.to_str().unwrap())
        .await
        .expect_err("non-markdown file should fail");
    assert!(matches!(err, OpenDocumentError::NotMarkdown(_)));
}

#[tokio::test]
async fn open_document_markdown_file_with_no_app_session_returns_no_app_session() {
    let (server_state, dir) = test_server_state();
    let path = dir.path().join("notes.md");
    std::fs::write(&path, b"# hello").unwrap();
    let err = server_state
        .open_document(path.to_str().unwrap())
        .await
        .expect_err("no registered app session should fail");
    assert!(matches!(err, OpenDocumentError::NoAppSession));
}

#[tokio::test]
async fn open_document_accepts_uppercase_md_extension() {
    let (server_state, dir) = test_server_state();
    let path = dir.path().join("NOTES.MD");
    std::fs::write(&path, b"# hello").unwrap();
    // No app session registered — validation should pass and the
    // failure should come from the send step, not `NotMarkdown`.
    let err = server_state
        .open_document(path.to_str().unwrap())
        .await
        .expect_err("no registered app session should fail");
    assert!(matches!(err, OpenDocumentError::NoAppSession));
}
