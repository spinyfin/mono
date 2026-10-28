//! Keep the inventory tied to production SQL, including new files and bulk writes.
use super::*;

fn sources(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap().to_string_lossy().contains("test") {
            continue;
        }
        if path.is_dir() {
            sources(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn task_status_write_sites_all_supply_transactional_provenance() {
    let root = std::path::PathBuf::from(std::env::var("TEST_SRCDIR").unwrap())
        .join(std::env::var("TEST_WORKSPACE").unwrap())
        .join("tools/boss/engine/core/src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    let sql =
        regex::Regex::new(r#"(?s)"((?:UPDATE\s+tasks\b|INSERT(?:\s+OR\s+\w+)?\s+INTO\s+tasks\b)[^"]*)""#).unwrap();
    let property = regex::Regex::new(r"\b(status|blocked_reason|dispatch_failed_reason|autostart)\s*=").unwrap();
    let helper = regex::Regex::new(
        r#"(?s)task_status_audit::execute\(\s*&?\w+,\s*(.*?),\s*(?:"[^"]+"|actor),\s*(?:&format!\(\s*)?$"#,
    )
    .unwrap();
    let inline_tests = regex::Regex::new(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]+\]\s*)*mod\s+\w+\s*\{").unwrap();
    let mut sites = Vec::new();
    for path in files {
        let source = std::fs::read_to_string(&path).unwrap();
        // Historical migrations precede creation of the ledger. The migration
        // backfill, tested separately, captures their resulting state.
        if path.file_stem().unwrap().to_string_lossy().starts_with("schema")
            || path.ends_with("execution_recovery_rearm.rs")
        {
            continue;
        }
        let production = inline_tests.split(&source).next().unwrap();
        for matched in sql.captures_iter(production) {
            let statement = &matched[1];
            if !statement.starts_with("INSERT") && !property.is_match(statement.split("WHERE").next().unwrap()) {
                continue;
            }
            let start = matched.get(0).unwrap().start();
            let prefix = production[..start].rsplit(';').next().unwrap();
            let site = format!(
                "{}:{}",
                path.strip_prefix(&root).unwrap().display(),
                production[..start].lines().count()
            );
            let call = helper
                .captures(prefix)
                .unwrap_or_else(|| panic!("{site}: task-state SQL must use the ledger writer"));
            sites.push((site, call[1].to_owned()));
        }
    }
    assert!(!sites.is_empty(), "source inventory must not be empty");
    // Exercise every inventoried literal basis through the same transaction
    // boundary and triggers. Dynamic failure producers have end-to-end tests.
    let db = WorkDb::open(temp_db_path("audit-write-sites")).unwrap();
    let product = create_test_product_named(&db, "Ledger inventory");
    let task = create_test_chore_manual(&db, &product.id, "Inventory");
    let conn = db.connect().unwrap();
    for (site, expression) in sites {
        if !expression.starts_with('"') {
            continue;
        }
        let basis = expression.trim_matches('"');
        let tx = conn.unchecked_transaction().unwrap();
        crate::work::task_status_audit::execute(&tx, basis, "engine",
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'dependency', dispatch_failed_reason = 'churn_guard', autostart = 1 WHERE id = ?1",
            [&task.id]).unwrap();
        for property in ["status", "blocked_reason", "dispatch_failed_reason", "autostart"] {
            let recorded: String = tx.query_row(
                "SELECT basis FROM task_status_audit WHERE work_item_id = ?1 AND property = ?2 ORDER BY id DESC LIMIT 1",
                params![task.id, property], |r| r.get(0)).unwrap();
            assert_eq!(recorded, basis, "{site}: {property}");
        }
        tx.rollback().unwrap();
    }
}
