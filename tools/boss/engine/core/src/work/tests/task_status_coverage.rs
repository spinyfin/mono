//! The manifest is independent of production basis expressions. Each fixture
//! must perform its assigned real writes and expose ledger rows with these bases.
use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[path = "task_status_expectations.rs"]
mod expectations;
pub(crate) use expectations::EXPECTED;

pub(crate) struct Expectation {
    pub site: &'static str,
    pub expression: &'static str,
    pub bases: &'static [&'static str],
    pub fixture: &'static str,
}

pub(crate) struct Site {
    pub key: String,
    pub file: String,
    pub line: u32,
    pub expression: String,
}

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

pub(crate) fn root() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("TEST_SRCDIR").unwrap())
        .join(std::env::var("TEST_WORKSPACE").unwrap())
        .join("tools/boss/engine/core/src")
}

pub(crate) fn inventory() -> &'static [Site] {
    static SITES: OnceLock<Vec<Site>> = OnceLock::new();
    SITES.get_or_init(|| {
        let root = root();
        let mut files = Vec::new();
        sources(&root, &mut files);
        let sql =
            regex::Regex::new(r#"(?s)"((?:UPDATE\s+tasks\b|INSERT(?:\s+OR\s+\w+)?\s+INTO\s+tasks\b)[^"]*)""#).unwrap();
        let property = regex::Regex::new(r"\b(status|blocked_reason|dispatch_failed_reason|autostart)\s*=").unwrap();
        let helper = regex::Regex::new(
            r#"(?s)task_status_audit::execute\(\s*&?\w+,\s*(.*?),\s*(?:"[^"]+"|actor),\s*(?:&format!\(\s*)?$"#,
        )
        .unwrap();
        let calls = regex::Regex::new(r"task_status_audit::execute\(\s*&?\w+,\s*(.*?),\s*").unwrap();
        let functions = regex::Regex::new(r"\bfn (\w+)").unwrap();
        let inline_tests = regex::Regex::new(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]+\]\s*)*mod\s+\w+\s*\{").unwrap();
        let mut sites = Vec::new();
        for path in files {
            if path.file_stem().unwrap().to_string_lossy().starts_with("schema")
                || path.ends_with("execution_recovery_rearm.rs")
            {
                // Historical migrations precede the ledger; backfill has its own tests.
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let production = inline_tests.split(&source).next().unwrap();
            let file = path.strip_prefix(&root).unwrap().to_str().unwrap().to_owned();
            for matched in sql.captures_iter(production) {
                let statement = &matched[1];
                if !statement.starts_with("INSERT") && !property.is_match(statement.split("WHERE").next().unwrap()) {
                    continue;
                }
                let start = matched.get(0).unwrap().start();
                let prefix = production[..start].rsplit(';').next().unwrap();
                assert!(
                    helper.is_match(prefix),
                    "{file}:{}: task-state SQL must use the ledger writer",
                    production[..start].lines().count()
                );
            }
            let mut ordinals = BTreeMap::new();
            for call in calls.captures_iter(production) {
                let start = call.get(0).unwrap().start();
                let function = functions.captures_iter(&production[..start]).last().unwrap()[1].to_owned();
                let ordinal = ordinals.entry(function.clone()).or_insert(0);
                *ordinal += 1;
                sites.push(Site {
                    key: format!("{file}:{function}:{ordinal}"),
                    file: file.clone(),
                    line: production[..start].bytes().filter(|b| *b == b'\n').count() as u32 + 1,
                    expression: call[1].to_owned(),
                });
            }
        }
        sites
    })
}

struct Active {
    fixture: &'static str,
    seen: BTreeSet<(&'static str, String)>,
}
thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// Scoped to one producer fixture; unrelated tests and threads cannot satisfy it.
pub(crate) struct Fixture;

impl Fixture {
    pub(crate) fn new(fixture: &'static str) -> Self {
        assert!(
            EXPECTED.iter().any(|e| e.fixture == fixture),
            "unregistered fixture {fixture}"
        );
        ACTIVE.with(|active| {
            assert!(active.borrow().is_none(), "nested audit fixture");
            *active.borrow_mut() = Some(Active {
                fixture,
                seen: BTreeSet::new(),
            });
        });
        Self
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let active = ACTIVE.with(|active| active.borrow_mut().take()).unwrap();
        if std::thread::panicking() {
            return;
        }
        for expected in EXPECTED.iter().filter(|e| e.fixture == active.fixture) {
            for basis in expected.bases {
                assert!(
                    active.seen.contains(&(expected.site, (*basis).to_owned())),
                    "{} did not produce a ledger row for {} with basis {basis}",
                    active.fixture,
                    expected.site
                );
            }
        }
    }
}

pub(crate) fn before(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    if !ACTIVE.with(|a| a.borrow().is_some()) {
        return Ok(None);
    }
    conn.query_row("SELECT COALESCE(MAX(id), 0) FROM task_status_audit", [], |r| r.get(0))
        .map(Some)
}

pub(crate) fn observe(conn: &Connection, before: i64, caller: &std::panic::Location<'_>) -> rusqlite::Result<()> {
    ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let Some(active) = active.as_mut() else {
            return Ok(());
        };
        let Some(site) = inventory()
            .iter()
            .find(|s| caller.file().ends_with(&s.file) && caller.line() == s.line)
        else {
            // Fixture setup may deliberately use the helper outside production.
            return Ok(());
        };
        let Some(expected) = EXPECTED
            .iter()
            .find(|e| e.site == site.key && e.fixture == active.fixture)
        else {
            return Ok(());
        };
        let mut stmt = conn.prepare("SELECT basis FROM task_status_audit WHERE id > ?1")?;
        for basis in stmt.query_map([before], |r| r.get::<_, String>(0))? {
            let basis = basis?;
            assert!(
                expected.bases.contains(&basis.as_str()),
                "{}: unexpected ledger basis {basis}",
                expected.site
            );
            active.seen.insert((expected.site, basis));
        }
        Ok(())
    })
}
