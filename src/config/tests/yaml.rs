use std::fs;
use std::path::Path;

use tempfile::tempdir;

use crate::config::ConfigResolver;

#[test]
fn resolves_yaml_config_file() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checks:
  - id: file-size
    config:
      max_lines: 321
"#,
    )
    .expect("write config file");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    assert_eq!(
        checks
            .get("file-size")
            .expect("file-size present")
            .config
            .as_table()
            .expect("file-size config table")
            .get("max_lines")
            .expect("max_lines")
            .as_integer(),
        Some(321)
    );
}
