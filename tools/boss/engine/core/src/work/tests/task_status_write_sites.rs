//! Static completeness plus an independent producer/basis/fixture contract.
//! Fixture guards check actual ledger rows when the real producer tests run.
use crate::work::task_status_audit::coverage::{EXPECTED, inventory, root};
use std::collections::BTreeSet;

fn fixture_registrations(dir: &std::path::Path, found: &mut BTreeSet<String>) {
    static PATTERNS: std::sync::LazyLock<(regex::Regex, regex::Regex, regex::Regex)> = std::sync::LazyLock::new(|| {
        (
            regex::Regex::new(r#"coverage::Fixture::new\(\s*"([^"]+)""#).unwrap(),
            regex::Regex::new(r"\bfn (\w+)\(\)").unwrap(),
            regex::Regex::new(r"#\[(?:tokio::)?test(?:\([^]]*\))?\]\s*(?:async\s+)?fn\s+(\w+)\(\)").unwrap(),
        )
    });
    let (guard, function, test) = &*PATTERNS;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            fixture_registrations(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            let tests: BTreeSet<_> = test.captures_iter(&source).map(|c| c[1].to_owned()).collect();
            for registration in guard.captures_iter(&source) {
                let name = &registration[1];
                let prefix = &source[..registration.get(0).unwrap().start()];
                let function_name = function.captures_iter(prefix).last().unwrap()[1].to_owned();
                assert_eq!(
                    name.rsplit("::").next().unwrap(),
                    function_name,
                    "guard must belong to its named test"
                );
                assert!(
                    tests.contains(&function_name),
                    "{name}: fixture must be an enabled test"
                );
                found.insert(name.to_owned());
            }
        }
    }
}

#[test]
fn task_status_write_sites_all_supply_transactional_provenance() {
    let sites = inventory();
    assert!(!sites.is_empty(), "source inventory must not be empty");
    let mut fixtures = BTreeSet::new();
    fixture_registrations(&root(), &mut fixtures);
    let expected_keys: BTreeSet<_> = EXPECTED.iter().map(|e| e.site).collect();
    assert_eq!(expected_keys.len(), EXPECTED.len(), "duplicate expectations");
    assert_eq!(
        sites.iter().map(|s| s.key.as_str()).collect::<BTreeSet<_>>(),
        expected_keys,
        "every write site, including dynamic bases, needs an independently specified expectation and producer fixture"
    );
    for site in sites {
        let expected = EXPECTED.iter().find(|e| e.site == site.key).unwrap();
        assert_eq!(
            site.expression, expected.expression,
            "{}: wrong producer basis",
            site.key
        );
        assert!(
            !expected.bases.is_empty(),
            "{}: expected ledger bases required",
            site.key
        );
        assert!(
            fixtures.contains(expected.fixture),
            "{}: missing real producer fixture {}",
            site.key,
            expected.fixture
        );
    }
}
