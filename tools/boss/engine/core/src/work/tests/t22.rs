use super::*;

/// Stand up a fresh in-memory `WorkDb` plus a product-with-repo and a
/// chore under it. Returns `(db, product_id, chore_id)`.
fn seed_product_and_chore(label: &str) -> (WorkDb, String, String) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_with_repo(&db, label, Some("git@example.invalid:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), format!("Chore {label}"));
    (db, product.id, chore.id)
}

/// Insert a pending conflict-resolution attempt for `work_item_id` with a
/// distinct `base_sha` (the `UNIQUE (work_item_id, base_sha_at_trigger)`
/// key, so each call lands its own row). Returns the inserted attempt.
fn insert_attempt(db: &WorkDb, product_id: &str, work_item_id: &str, base_sha: &str) -> ConflictResolution {
    db.insert_conflict_resolution(
        ConflictResolutionInsertInput::builder()
            .product_id(product_id)
            .work_item_id(work_item_id)
            .pr_url(format!("https://github.com/foo/bar/pull/{base_sha}"))
            .pr_number(1)
            .head_branch("feature")
            .base_branch("main")
            .base_sha_at_trigger(base_sha)
            .head_sha_before("head-before")
            .build(),
    )
    .unwrap()
    .expect("insert must produce a pending attempt")
}

// Behavioural coverage for `WorkDb::conflict_hotspots` (Layer 0 / T5),
// the aggregation query behind `boss engine conflicts hotspots`:
//
// * per-file conflict frequency, deduped within a single event so a
//   file repeated in one diagnosis doesn't inflate its own count;
// * per-file-pair co-conflict frequency, one entry per unordered pair;
// * per-class counts, sourced from the `conflict_class` column
//   (falling back to `"unknown"` when unset) rather than reparsing
//   `conflict_diagnosis`;
// * `top_n` truncation of the two ranked frequency lists, applied
//   after sorting by count descending;
// * strict per-product isolation — a second product's rows never leak
//   into the first's report;
// * rows with no `conflict_diagnosis` still count toward
//   `total_events` / `class_counts` but contribute nothing to file or
//   pair frequency.

fn diagnosis_json(paths: &[&str]) -> String {
    let diagnosis = crate::conflict_diagnosis::ConflictDiagnosis {
        schema_version: 1,
        base_sha: "aaa".into(),
        head_sha: "bbb".into(),
        files: paths
            .iter()
            .map(|p| crate::conflict_diagnosis::ConflictedFile {
                path: (*p).to_owned(),
                marker_count: None,
                shape: "content".into(),
            })
            .collect(),
        error: None,
    };
    serde_json::to_string(&diagnosis).unwrap()
}

#[test]
fn empty_product_returns_empty_report() {
    let (db, product, _chore) = seed_product_and_chore("hotspots-empty");
    let report = db.conflict_hotspots(&product, 20).unwrap();
    assert_eq!(report.product_id, product);
    assert_eq!(report.total_events, 0);
    assert!(report.file_frequency.is_empty());
    assert!(report.file_pair_frequency.is_empty());
    assert!(report.class_counts.is_empty());
}

#[test]
fn aggregates_file_and_pair_frequency_across_events() {
    let (db, product, chore) = seed_product_and_chore("hotspots-freq");

    let a = insert_attempt(&db, &product, &chore, "sha-a");
    db.set_conflict_resolution_diagnosis(&a.id, &diagnosis_json(&["Cargo.lock", "src/completion.rs"]))
        .unwrap();
    let b = insert_attempt(&db, &product, &chore, "sha-b");
    db.set_conflict_resolution_diagnosis(&b.id, &diagnosis_json(&["Cargo.lock"]))
        .unwrap();
    let c = insert_attempt(&db, &product, &chore, "sha-c");
    db.set_conflict_resolution_diagnosis(&c.id, &diagnosis_json(&["Cargo.lock", "src/completion.rs"]))
        .unwrap();

    let report = db.conflict_hotspots(&product, 20).unwrap();
    assert_eq!(report.total_events, 3);

    assert_eq!(report.file_frequency.len(), 2);
    assert_eq!(report.file_frequency[0].path, "Cargo.lock");
    assert_eq!(report.file_frequency[0].count, 3);
    assert_eq!(report.file_frequency[1].path, "src/completion.rs");
    assert_eq!(report.file_frequency[1].count, 2);

    assert_eq!(report.file_pair_frequency.len(), 1);
    assert_eq!(report.file_pair_frequency[0].path_a, "Cargo.lock");
    assert_eq!(report.file_pair_frequency[0].path_b, "src/completion.rs");
    assert_eq!(report.file_pair_frequency[0].count, 2);
}

#[test]
fn a_file_repeated_within_one_event_counts_once() {
    let (db, product, chore) = seed_product_and_chore("hotspots-dedup");
    let attempt = insert_attempt(&db, &product, &chore, "sha-dup");
    // Same path twice in one diagnosis (shouldn't happen from a real
    // probe, but the aggregation must not double-count within an event).
    let diagnosis = crate::conflict_diagnosis::ConflictDiagnosis {
        schema_version: 1,
        base_sha: "aaa".into(),
        head_sha: "bbb".into(),
        files: vec![
            crate::conflict_diagnosis::ConflictedFile {
                path: "Cargo.lock".into(),
                marker_count: None,
                shape: "content".into(),
            },
            crate::conflict_diagnosis::ConflictedFile {
                path: "Cargo.lock".into(),
                marker_count: None,
                shape: "content".into(),
            },
        ],
        error: None,
    };
    db.set_conflict_resolution_diagnosis(&attempt.id, &serde_json::to_string(&diagnosis).unwrap())
        .unwrap();

    let report = db.conflict_hotspots(&product, 20).unwrap();
    assert_eq!(report.file_frequency.len(), 1);
    assert_eq!(report.file_frequency[0].count, 1);
    assert!(
        report.file_pair_frequency.is_empty(),
        "a single distinct path has no pair"
    );
}

#[test]
fn class_counts_source_from_conflict_class_column() {
    let (db, product, chore) = seed_product_and_chore("hotspots-class");

    let lockfile = insert_attempt(&db, &product, &chore, "sha-class-1");
    db.set_conflict_resolution_diagnosis(&lockfile.id, &diagnosis_json(&["Cargo.lock"]))
        .unwrap();

    let semantic = insert_attempt(&db, &product, &chore, "sha-class-2");
    db.set_conflict_resolution_diagnosis(&semantic.id, &diagnosis_json(&["src/completion.rs"]))
        .unwrap();

    // No diagnosis at all: still counted, classified "unknown".
    insert_attempt(&db, &product, &chore, "sha-class-3");

    let report = db.conflict_hotspots(&product, 20).unwrap();
    assert_eq!(report.total_events, 3);
    let class_map: std::collections::HashMap<String, u64> =
        report.class_counts.iter().map(|c| (c.class.clone(), c.count)).collect();
    assert_eq!(class_map.get("lockfile"), Some(&1));
    assert_eq!(class_map.get("semantic"), Some(&1));
    assert_eq!(class_map.get("unknown"), Some(&1));

    // The no-diagnosis row must not contribute to file frequency.
    assert_eq!(report.file_frequency.len(), 2);
}

#[test]
fn top_n_truncates_ranked_lists_but_not_class_counts() {
    let (db, product, chore) = seed_product_and_chore("hotspots-topn");
    for (idx, path) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
        let attempt = insert_attempt(&db, &product, &chore, &format!("sha-top-{idx}"));
        db.set_conflict_resolution_diagnosis(&attempt.id, &diagnosis_json(&[path]))
            .unwrap();
    }

    let report = db.conflict_hotspots(&product, 2).unwrap();
    assert_eq!(report.total_events, 3);
    assert_eq!(report.file_frequency.len(), 2, "top_n caps the ranked file list");
    assert_eq!(report.class_counts.len(), 1, "class counts are never truncated");
}

#[test]
fn report_is_scoped_to_one_product() {
    let (db, product_a, chore_a) = seed_product_and_chore("hotspots-product-a");
    let product_b = create_test_product_with_repo(&db, "hotspots-product-b", Some("git@example.invalid:foo/baz.git"));
    let chore_b = create_test_chore_manual(&db, product_b.id.clone(), "Chore B".to_owned());

    let attempt_a = insert_attempt(&db, &product_a, &chore_a, "sha-scope-a");
    db.set_conflict_resolution_diagnosis(&attempt_a.id, &diagnosis_json(&["only_in_a.rs"]))
        .unwrap();

    let attempt_b = insert_attempt(&db, &product_b.id, &chore_b.id, "sha-scope-b");
    db.set_conflict_resolution_diagnosis(&attempt_b.id, &diagnosis_json(&["only_in_b.rs"]))
        .unwrap();

    let report_a = db.conflict_hotspots(&product_a, 20).unwrap();
    assert_eq!(report_a.total_events, 1);
    assert_eq!(report_a.file_frequency.len(), 1);
    assert_eq!(report_a.file_frequency[0].path, "only_in_a.rs");

    let report_b = db.conflict_hotspots(&product_b.id, 20).unwrap();
    assert_eq!(report_b.total_events, 1);
    assert_eq!(report_b.file_frequency.len(), 1);
    assert_eq!(report_b.file_frequency[0].path, "only_in_b.rs");
}
