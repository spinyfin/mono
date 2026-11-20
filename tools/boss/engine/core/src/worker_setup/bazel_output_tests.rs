use super::*;

fn with_user(workspace: &Path) -> Option<PathBuf> {
    bazel_output_directory_with(workspace, Some("/home/u"), Some("u"), None, None)
}

#[test]
fn non_bazel_workspace_gets_no_grant() {
    let ws = tempfile::tempdir().unwrap();
    assert!(with_user(ws.path()).is_none());
}

#[test]
fn default_root_is_per_user_and_platform_specific() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
    let home = tempfile::tempdir().unwrap();
    let expected = std::fs::canonicalize(home.path())
        .unwrap()
        .join(if cfg!(target_os = "macos") {
            "Library/Caches/bazel/_bazel_u"
        } else {
            ".cache/bazel/_bazel_u"
        });
    assert_eq!(
        bazel_output_directory_with(ws.path(), home.path().to_str(), Some("u"), None, None),
        Some(expected)
    );
}

#[test]
fn bazelrc_output_user_root_and_output_base_are_honoured() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
    std::fs::write(
        ws.path().join(".bazelrc"),
        "build --foo\nstartup --output_user_root=/data/bz # c\n",
    )
    .unwrap();
    assert_eq!(with_user(ws.path()), Some(PathBuf::from("/data/bz")));
    std::fs::write(
        ws.path().join(".bazelrc"),
        "startup --output_user_root=/data/bz\nstartup --output_base=/data/base\n",
    )
    .unwrap();
    assert_eq!(with_user(ws.path()), Some(PathBuf::from("/data/base")));
}

#[test]
fn imported_bazelrc_is_followed() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join(".bazelrc"), "try-import %workspace%/ci.bazelrc\n").unwrap();
    std::fs::write(ws.path().join("ci.bazelrc"), "startup --output_user_root=/ci/root\n").unwrap();
    assert_eq!(with_user(ws.path()), Some(PathBuf::from("/ci/root")));
}

#[test]
fn existing_bazel_out_symlink_names_the_root() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let out = root_path.join("hash/execroot/_main/bazel-out");
    std::fs::create_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(&out, ws.path().join("bazel-out")).unwrap();
    assert_eq!(with_user(ws.path()), Some(root_path.join("hash")));
}

#[test]
fn custom_symlink_layout_keeps_output_base() {
    // These layouts previously yielded the home directory or filesystem root.
    for (target, expected) in [
        ("/home/u/bazel-base/execroot/_main/bazel-out", "/home/u/bazel-base"),
        ("/bazel-base/execroot/_main/bazel-out", "/bazel-base"),
    ] {
        assert_eq!(
            output_base_from_target(Path::new(target)),
            Some(PathBuf::from(expected))
        );
    }
}

#[test]
fn broad_roots_are_rejected_after_normalization() {
    let ws = tempfile::tempdir().unwrap();
    for root in ["/", "/home/u", "/home/u/sub/..", "/nonexistent-bazel-root/../"] {
        std::fs::write(ws.path().join(".bazelrc"), format!("startup --output_base={root}")).unwrap();
        assert_eq!(with_user(ws.path()), None, "{root}");
    }
}

#[test]
fn quoted_imports_flags_comments_and_continuations() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join(".bazelrc"),
        "import \"%workspace%/with space.bazelrc\"\n",
    )
    .unwrap();
    for flag in ["--output_base", "--output_user_root"] {
        for separator in ["=", " "] {
            let rc = format!("startup \\\n {flag}{separator}\"/output/with space#hash\" # comment\n");
            std::fs::write(ws.path().join("with space.bazelrc"), rc).unwrap();
            assert_eq!(with_user(ws.path()), Some(PathBuf::from("/output/with space#hash")));
        }
    }
}

#[test]
fn rc_root_and_existing_default_are_canonicalized() {
    let ws = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let alias = ws.path().join("alias");
    std::os::unix::fs::symlink(storage.path(), &alias).unwrap();
    std::fs::write(
        ws.path().join(".bazelrc"),
        format!("startup --output_base={}", alias.display()),
    )
    .unwrap();
    assert_eq!(
        with_user(ws.path()),
        Some(std::fs::canonicalize(storage.path()).unwrap())
    );
    // A symlink to home must not evade the broad-grant guard.
    assert_eq!(
        bazel_output_directory_with(ws.path(), storage.path().to_str(), Some("u"), None, None),
        None
    );
    std::fs::remove_file(ws.path().join(".bazelrc")).unwrap();
    std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
    let user_root = alias.join("_bazel_u");
    std::fs::create_dir(&user_root).unwrap();
    assert_eq!(
        bazel_output_directory_with(ws.path(), Some("/home/u"), Some("u"), alias.to_str(), None),
        Some(std::fs::canonicalize(user_root).unwrap())
    );
}

#[test]
fn default_honours_environment_overrides() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
    assert_eq!(
        bazel_output_directory_with(
            ws.path(),
            Some("/home/u"),
            Some("u"),
            Some("/scratch/tests"),
            Some("/custom/cache")
        ),
        Some(PathBuf::from("/scratch/tests/_bazel_u"))
    );
    if !cfg!(target_os = "macos") {
        assert_eq!(
            bazel_output_directory_with(ws.path(), Some("/home/u"), Some("u"), None, Some("/custom/cache")),
            Some(PathBuf::from("/custom/cache/bazel/_bazel_u"))
        );
    }
}

#[test]
fn custom_base_under_home_does_not_grant_home() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let base = home.path().join("bazel-base");
    let out = base.join("execroot/_main/bazel-out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
    std::os::unix::fs::symlink(&out, ws.path().join("bazel-out")).unwrap();
    assert_eq!(
        bazel_output_directory_with(ws.path(), home.path().to_str(), Some("u"), None, None),
        Some(std::fs::canonicalize(&base).unwrap())
    );
}
