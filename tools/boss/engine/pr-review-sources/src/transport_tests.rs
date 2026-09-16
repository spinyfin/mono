use super::*;

fn rest_metadata(base: &str, head: &str, changed_files: u64) -> boss_github::pr_files::PrComparisonMetadata {
    boss_github::pr_files::PrComparisonMetadata {
        number: 4,
        title: "Capture source".to_owned(),
        body: None,
        base_repository: "acme/widget".to_owned(),
        head_repository: "acme/widget".to_owned(),
        head_ref_name: "feature".to_owned(),
        base_sha: base.to_owned(),
        head_sha: head.to_owned(),
        changed_files,
    }
}

fn tree_blob(path: &str, size: Option<u64>) -> boss_github::trees::PinnedTreeEntry {
    boss_github::trees::PinnedTreeEntry {
        path: path.to_owned(),
        object_sha: format!("obj-{path}"),
        mode: "100644".to_owned(),
        object_type: "blob".to_owned(),
        size,
    }
}

fn inventory_entry(
    filename: &str,
    previous: Option<&str>,
    status: &str,
    patch: Option<&str>,
) -> boss_github::pr_files::PrFileInventoryEntry {
    boss_github::pr_files::PrFileInventoryEntry {
        filename: filename.to_owned(),
        previous_filename: previous.map(str::to_owned),
        status: status.to_owned(),
        additions: 1,
        deletions: 1,
        patch: patch.map(str::to_owned),
    }
}

#[derive(Clone)]
struct FixtureTransport {
    latest: boss_github::pr_files::PrComparisonMetadata,
    merge_base: String,
    inventory: Vec<boss_github::pr_files::PrFileInventoryEntry>,
    trees: HashMap<
        (String, String),
        std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError>,
    >,
    blobs: HashMap<(String, String), Vec<u8>>,
}

impl SourceTransport for FixtureTransport {
    async fn fetch_pr_comparison_metadata(&self, _pr_url: &str) -> Result<boss_github::pr_files::PrComparisonMetadata> {
        Ok(self.latest.clone())
    }

    async fn fetch_merge_base(&self, _repository: &str, _base_sha: &str, _head_sha: &str) -> Result<String> {
        Ok(self.merge_base.clone())
    }

    async fn fetch_complete_pr_file_inventory(
        &self,
        _repository: &str,
        _number: u64,
        expected_changed_files: u64,
    ) -> Result<Vec<boss_github::pr_files::PrFileInventoryEntry>> {
        anyhow::ensure!(
            self.inventory.len() as u64 == expected_changed_files,
            "fixture inventory count mismatch"
        );
        Ok(self.inventory.clone())
    }

    async fn fetch_pinned_tree_directory(
        &self,
        _owner: &str,
        _repo: &str,
        commit_sha: &str,
        directory: &str,
        names: &HashSet<String>,
    ) -> std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError> {
        match self.trees.get(&(commit_sha.to_owned(), directory.to_owned())) {
            Some(Ok(tree)) => {
                let mut tree = tree.clone();
                tree.entries.retain(|entry| names.contains(&entry.path));
                Ok(tree)
            }
            Some(Err(error)) => Err(error.clone()),
            None => Ok(boss_github::trees::PinnedTree {
                sha: commit_sha.to_owned(),
                entries: Vec::new(),
                truncated: false,
            }),
        }
    }

    async fn fetch_repo_file_bytes(&self, _owner: &str, _repo: &str, path: &str, sha: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.blobs.get(&(sha.to_owned(), path.to_owned())).cloned())
    }
}

fn tree(
    sha: &str,
    entries: Vec<boss_github::trees::PinnedTreeEntry>,
) -> std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError> {
    Ok(boss_github::trees::PinnedTree {
        sha: sha.to_owned(),
        entries,
        truncated: false,
    })
}

#[tokio::test]
async fn collector_accepts_a_probe_shaped_base_that_differs_from_rest() {
    let rest_base = "rest-base";
    let graphql_base = "graphql-live-tip";
    let head = "head";
    let merge = "merge-base";
    let metadata = rest_metadata(rest_base, head, 1);
    let mut trees = HashMap::new();
    trees.insert(
        (merge.to_owned(), String::new()),
        tree(merge, vec![tree_blob("lib.rs", Some(4))]),
    );
    trees.insert(
        (head.to_owned(), String::new()),
        tree(head, vec![tree_blob("lib.rs", Some(6))]),
    );
    let mut blobs = HashMap::new();
    blobs.insert((merge.to_owned(), "lib.rs".to_owned()), b"old\n".to_vec());
    blobs.insert((head.to_owned(), "lib.rs".to_owned()), b"after\n".to_vec());
    let transport = FixtureTransport {
        latest: metadata.clone(),
        merge_base: merge.to_owned(),
        inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
        trees,
        blobs,
    };
    let observed = PinnedComparison {
        base_sha: graphql_base.to_owned(),
        head_sha: head.to_owned(),
    };
    let packet = collect_pinned_source_packet_with_transport(
        "https://github.com/acme/widget/pull/4",
        &observed,
        Some("feature"),
        metadata,
        true,
        &transport,
    )
    .await
    .unwrap();
    assert_eq!(packet.observed_base_sha, rest_base);
    assert_eq!(packet.probe_base_sha.as_deref(), Some(graphql_base));
    assert_eq!(packet.merge_base_sha, merge);
    assert_eq!(packet.head_sha, head);
    assert!(packet.is_complete());
    assert_eq!(
        packet.files[0].before.as_ref().unwrap().content.as_deref(),
        Some("old\n")
    );
    assert_eq!(
        packet.files[0].after.as_ref().unwrap().content.as_deref(),
        Some("after\n")
    );
}

#[tokio::test]
async fn collector_covers_add_delete_rename_tree_failure_and_oversize() {
    let rest_base = "rest-base";
    let head = "head";
    let merge = "merge-base";
    let metadata = rest_metadata(rest_base, head, 5);
    let mut trees = HashMap::new();
    trees.insert(
        (merge.to_owned(), String::new()),
        tree(merge, vec![tree_blob("gone.rs", Some(3)), tree_blob("old.rs", Some(3))]),
    );
    trees.insert(
        (head.to_owned(), String::new()),
        tree(
            head,
            vec![
                tree_blob("added.rs", Some(4)),
                tree_blob("new.rs", Some(3)),
                boss_github::trees::PinnedTreeEntry {
                    path: "huge.bin".to_owned(),
                    object_sha: "huge".to_owned(),
                    mode: "100644".to_owned(),
                    object_type: "blob".to_owned(),
                    size: Some(MAX_PINNED_SOURCE_BYTES + 1),
                },
            ],
        ),
    );
    trees.insert(
        (head.to_owned(), "broken".to_owned()),
        Err(boss_github::trees::TreeApiError {
            kind: boss_github::trees::TreeApiErrorKind::Unreachable,
            message: "rate limited".to_owned(),
        }),
    );
    let mut blobs = HashMap::new();
    blobs.insert((merge.to_owned(), "gone.rs".to_owned()), b"old\n".to_vec());
    blobs.insert((merge.to_owned(), "old.rs".to_owned()), b"was\n".to_vec());
    blobs.insert((head.to_owned(), "added.rs".to_owned()), b"new\n".to_vec());
    blobs.insert((head.to_owned(), "new.rs".to_owned()), b"now\n".to_vec());
    let transport = FixtureTransport {
        latest: metadata.clone(),
        merge_base: merge.to_owned(),
        inventory: vec![
            inventory_entry("added.rs", None, "added", Some("@@")),
            inventory_entry("gone.rs", None, "deleted", Some("@@")),
            inventory_entry("new.rs", Some("old.rs"), "renamed", None),
            inventory_entry("broken/a.rs", None, "modified", Some("@@")),
            inventory_entry("huge.bin", None, "added", None),
        ],
        trees,
        blobs,
    };
    let observed = PinnedComparison {
        base_sha: rest_base.to_owned(),
        head_sha: head.to_owned(),
    };
    let packet = collect_pinned_source_packet_with_transport(
        "https://github.com/acme/widget/pull/4",
        &observed,
        None,
        metadata,
        false,
        &transport,
    )
    .await
    .unwrap();
    assert_eq!(packet.files.len(), 5);
    assert!(packet.files[0].before.is_none());
    assert_eq!(
        packet.files[0].after.as_ref().unwrap().content.as_deref(),
        Some("new\n")
    );
    assert_eq!(
        packet.files[1].before.as_ref().unwrap().content.as_deref(),
        Some("old\n")
    );
    assert!(packet.files[1].after.is_none());
    assert_eq!(packet.files[2].previous_path.as_deref(), Some("old.rs"));
    assert_eq!(packet.files[2].before.as_ref().unwrap().path, "old.rs");
    assert_eq!(packet.files[2].after.as_ref().unwrap().path, "new.rs");
    assert!(
        packet.files[3]
            .after
            .as_ref()
            .unwrap()
            .omission
            .as_deref()
            .unwrap()
            .contains("rate limited")
    );
    assert_eq!(packet.files[3].after.as_ref().unwrap().terminal, Some(false));
    assert_eq!(packet.files[4].after.as_ref().unwrap().terminal, Some(true));
    assert!(
        packet.files[4]
            .after
            .as_ref()
            .unwrap()
            .omission
            .as_deref()
            .unwrap()
            .contains("per-file capture budget")
    );
    assert!(
        !packet.is_complete(),
        "a retryable tree failure must keep the packet incomplete"
    );
    let reasons: Vec<_> = packet
        .omissions
        .iter()
        .map(|o| (o.path.clone(), o.side, o.reason.clone()))
        .collect();
    assert!(
        reasons.iter().any(|(path, side, reason)| {
            path.as_deref() == Some("broken/a.rs")
                && *side == Some(SourceSide::After)
                && reason.contains("rate limited")
        }),
        "omission order must record the tree failure during assembly: {reasons:?}"
    );
}

#[tokio::test]
async fn collector_bails_when_rest_head_moves_mid_collection() {
    let metadata = rest_metadata("base", "head-one", 1);
    let mut latest = metadata.clone();
    latest.head_sha = "head-two".to_owned();
    let transport = FixtureTransport {
        latest,
        merge_base: "merge-base".to_owned(),
        inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
        trees: HashMap::new(),
        blobs: HashMap::new(),
    };
    let observed = PinnedComparison {
        base_sha: "base".to_owned(),
        head_sha: "head-one".to_owned(),
    };
    let err = collect_pinned_source_packet_with_transport(
        "https://github.com/acme/widget/pull/4",
        &observed,
        None,
        metadata,
        true,
        &transport,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("head-one"));
    assert!(err.contains("head-two"));
}

#[test]
fn poller_seam_head_mismatch_does_not_print_base_shas() {
    let observed = PinnedComparison {
        base_sha: "graphql-base".to_owned(),
        head_sha: "head-one".to_owned(),
    };
    let moved = rest_metadata("rest-base", "head-two", 1);
    let err = require_stable_head(&observed, &moved).unwrap_err().to_string();
    assert!(err.contains("PR head changed while collecting sources"));
    assert!(err.contains("head-one"));
    assert!(err.contains("head-two"));
    assert!(
        !err.contains("graphql-base") && !err.contains("rest-base"),
        "poller-seam head mismatch must not mention REST/GraphQL base SHAs: {err}"
    );
}

#[tokio::test]
async fn collector_settles_a_not_found_directory_read() {
    let rest_base = "rest-base";
    let head = "head";
    let merge = "merge-base";
    let metadata = rest_metadata(rest_base, head, 1);
    let mut trees = HashMap::new();
    trees.insert(
        (merge.to_owned(), String::new()),
        tree(merge, vec![tree_blob("lib.rs", Some(4))]),
    );
    trees.insert(
        (head.to_owned(), String::new()),
        Err(boss_github::trees::TreeApiError {
            kind: boss_github::trees::TreeApiErrorKind::NotFound,
            message: "Not Found".to_owned(),
        }),
    );
    let mut blobs = HashMap::new();
    blobs.insert((merge.to_owned(), "lib.rs".to_owned()), b"old\n".to_vec());
    let transport = FixtureTransport {
        latest: metadata.clone(),
        merge_base: merge.to_owned(),
        inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
        trees,
        blobs,
    };
    let packet = collect_pinned_source_packet_with_transport(
        "https://github.com/acme/widget/pull/4",
        &PinnedComparison {
            base_sha: rest_base.to_owned(),
            head_sha: head.to_owned(),
        },
        None,
        metadata,
        false,
        &transport,
    )
    .await
    .unwrap();
    assert!(
        packet.is_complete(),
        "a NotFound tree read must settle rather than retry forever"
    );
    assert_eq!(packet.files[0].after.as_ref().unwrap().terminal, Some(true));
}

#[tokio::test]
async fn contents_none_after_a_tree_blob_leaves_the_packet_incomplete() {
    let rest_base = "rest-base";
    let head = "head";
    let merge = "merge-base";
    let metadata = rest_metadata(rest_base, head, 1);
    let mut trees = HashMap::new();
    trees.insert(
        (merge.to_owned(), String::new()),
        tree(merge, vec![tree_blob("lib.rs", Some(4))]),
    );
    trees.insert(
        (head.to_owned(), String::new()),
        tree(head, vec![tree_blob("lib.rs", Some(6))]),
    );
    let mut blobs = HashMap::new();
    blobs.insert((merge.to_owned(), "lib.rs".to_owned()), b"old\n".to_vec());
    let missing = FixtureTransport {
        latest: metadata.clone(),
        merge_base: merge.to_owned(),
        inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
        trees: trees.clone(),
        blobs: blobs.clone(),
    };
    let first = collect_pinned_source_packet_with_transport(
        "https://github.com/acme/widget/pull/4",
        &PinnedComparison {
            base_sha: rest_base.to_owned(),
            head_sha: head.to_owned(),
        },
        None,
        metadata.clone(),
        false,
        &missing,
    )
    .await
    .unwrap();
    assert!(
        !first.is_complete(),
        "Contents Ok(None) after a tree blob must stay retryable"
    );
    assert_eq!(first.files[0].after.as_ref().unwrap().terminal, Some(false));
    blobs.insert((head.to_owned(), "lib.rs".to_owned()), b"after\n".to_vec());
    let recovered = FixtureTransport {
        latest: metadata.clone(),
        merge_base: merge.to_owned(),
        inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
        trees,
        blobs,
    };
    let second = collect_pinned_source_packet_with_transport(
        "https://github.com/acme/widget/pull/4",
        &PinnedComparison {
            base_sha: rest_base.to_owned(),
            head_sha: head.to_owned(),
        },
        None,
        metadata,
        false,
        &recovered,
    )
    .await
    .unwrap();
    assert!(second.is_complete());
    assert_eq!(
        second.files[0].after.as_ref().unwrap().content.as_deref(),
        Some("after\n")
    );
}

#[tokio::test]
async fn truncated_immutable_directory_settles_the_packet() {
    let metadata = rest_metadata("base", "head", 1);
    let transport = FixtureTransport {
        latest: metadata.clone(),
        merge_base: "merge".to_owned(),
        inventory: vec![inventory_entry("a.rs", None, "added", Some("@@"))],
        trees: HashMap::from([(
            ("head".to_owned(), String::new()),
            Ok(boss_github::trees::PinnedTree {
                sha: "head".to_owned(),
                entries: Vec::new(),
                truncated: true,
            }),
        )]),
        blobs: HashMap::new(),
    };
    let packet = collect_pinned_source_packet_with_transport(
        "https://github.com/acme/widget/pull/4",
        &PinnedComparison {
            base_sha: "base".to_owned(),
            head_sha: "head".to_owned(),
        },
        None,
        metadata,
        false,
        &transport,
    )
    .now_or_never()
    .unwrap()
    .unwrap();
    assert!(packet.is_complete());
    assert_eq!(packet.files[0].after.as_ref().unwrap().terminal, Some(true));
    assert!(
        packet
            .omissions
            .iter()
            .any(|o| o.reason.contains("directory response was truncated"))
    );
}
