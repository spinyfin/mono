use super::*;

#[test]
fn directory_grouping_and_rejoining_preserve_repository_paths() {
    let paths: HashSet<String> = ["lib.rs", "src/lib.rs", "src/main.rs", "other/lib.rs"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let grouped = group_paths(&paths);
    assert_eq!(grouped.len(), 3);
    assert_eq!(grouped["src"].len(), 2);
    assert!(grouped[""].contains("lib.rs"));
    let reconstructed: HashSet<String> = grouped
        .into_iter()
        .flat_map(|(directory, names)| names.into_iter().map(move |name| join_repo_path(&directory, &name)))
        .collect();
    assert_eq!(reconstructed, paths);
}

#[tokio::test]
async fn one_directory_failure_preserves_other_sources_and_precise_omissions() {
    let paths = ["lib.rs", "broken/a.rs", "broken/b.rs"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let result = fetch_directory_entries(
        "acme/widget",
        "pinned",
        &paths,
        SourceSide::Before,
        |directory, names| {
            let result = if directory == "broken" {
                Err(boss_github::trees::TreeApiError {
                    kind: boss_github::trees::TreeApiErrorKind::Unreachable,
                    message: "rate limited".to_owned(),
                })
            } else {
                let mut entry = blob_entry();
                entry.path = "lib.rs".to_owned();
                assert!(names.contains(&entry.path));
                Ok(boss_github::trees::PinnedTree {
                    sha: "pinned".to_owned(),
                    truncated: false,
                    entries: vec![entry],
                })
            };
            std::future::ready(result)
        },
    )
    .await
    .unwrap();
    assert!(result.0.contains_key("lib.rs"));
    assert_eq!(result.1.len(), 2);
    let source = fetch_source(
        &LiveSourceTransport,
        "acme/widget",
        "pinned",
        "broken/a.rs",
        None,
        Some(&TreeSideError {
            reason: result.1[0].reason.clone(),
            terminal: result.1[0].terminal,
        }),
    )
    .await;
    assert_eq!(source.omission.as_deref(), Some(result.1[0].reason.as_str()));
    assert_eq!(source.terminal, Some(false));
    assert_eq!(result.1[0].path.as_deref(), Some("broken/a.rs"));
    assert_eq!(result.1[1].path.as_deref(), Some("broken/b.rs"));
    assert!(
        result
            .1
            .iter()
            .all(|omission| omission.side == Some(SourceSide::Before) && omission.reason.contains("rate limited"))
    );
}

fn blob_entry() -> boss_github::trees::PinnedTreeEntry {
    boss_github::trees::PinnedTreeEntry {
        path: "src/with space.rs".to_owned(),
        object_sha: "d".repeat(40),
        mode: "100644".to_owned(),
        object_type: "blob".to_owned(),
        size: None,
    }
}

fn packet() -> SourcePacket {
    SourcePacket {
        schema_version: PACKET_SCHEMA_VERSION,
        canonical_pr_url: "https://github.com/acme/widget/pull/4".to_owned(),
        pr_number: 4,
        title: "Capture source".to_owned(),
        body: None,
        base_repository: "acme/widget".to_owned(),
        head_repository: "acme/widget".to_owned(),
        observed_base_sha: "a".repeat(40),
        probe_base_sha: None,
        merge_base_sha: "b".repeat(40),
        head_sha: "c".repeat(40),
        files: vec![SourceFile {
            path: "src/with space.rs".to_owned(),
            previous_path: None,
            change_kind: ChangeKind::Modified,
            additions: 1,
            deletions: 1,
            patch: Some("@@ -1 +1 @@".to_owned()),
            before: Some(PinnedSource::captured(
                "acme/widget",
                &"b".repeat(40),
                "src/with space.rs",
                "old\n".to_owned(),
                &blob_entry(),
            )),
            after: Some(PinnedSource::captured(
                "acme/widget",
                &"c".repeat(40),
                "src/with space.rs",
                "first\nsecond\n".to_owned(),
                &blob_entry(),
            )),
        }],
        omissions: Vec::new(),
    }
}

#[test]
fn packet_hash_is_stable_and_changes_with_content() {
    let packet = packet();
    assert_eq!(packet.content_hash().unwrap(), packet.content_hash().unwrap());
    let mut changed = packet.clone();
    changed.title = "different".to_owned();
    assert_ne!(packet.content_hash().unwrap(), changed.content_hash().unwrap());
}

#[test]
fn validates_ranges_against_pinned_content_and_encodes_path() {
    let reference = validate_pinned_reference(&packet(), SourceSide::After, "src/with space.rs", 1, 2).unwrap();
    assert_eq!(
        reference.href,
        format!(
            "https://github.com/acme/widget/blob/{}/src/with%20space.rs#L1-L2",
            "c".repeat(40)
        )
    );
    assert!(validate_pinned_reference(&packet(), SourceSide::After, "src/with space.rs", 3, 3).is_err());
}

#[test]
fn reference_validation_rejects_a_tampered_source_hash() {
    let mut packet = packet();
    packet.files[0].after.as_mut().unwrap().content_hash = Some("wrong".to_owned());
    assert!(validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).is_err());
}

fn live_comparison(packet: &SourcePacket) -> PinnedComparison {
    PinnedComparison {
        base_sha: packet.observed_base_sha.clone(),
        head_sha: packet.head_sha.clone(),
    }
}

#[test]
fn rendered_navigation_constructs_a_files_changed_target() {
    let packet = packet();
    let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
    let hash = hex_digest(b"src/with space.rs");
    match rendered_target_or_pinned_fallback(&packet, &reference, &live_comparison(&packet), "unused") {
        RenderedTarget::Validated {
            href,
            adapter_version,
            evidence,
        } => {
            assert_eq!(adapter_version, RENDERED_TARGET_ADAPTER_VERSION);
            assert_eq!(
                href,
                format!("https://github.com/acme/widget/pull/4/files#diff-{hash}R1")
            );
            assert_eq!(evidence.file_path, "src/with space.rs");
            assert_eq!(evidence.hunk, "@@ -1 +1 @@");
            assert_eq!(evidence.head_sha, packet.head_sha);
        }
        other => panic!("expected Validated, got {other:?}"),
    }
}

#[test]
fn rendered_navigation_falls_back_when_the_range_no_longer_validates() {
    let packet = packet();
    let mut reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
    reference.end_line = 99;
    assert!(matches!(
        rendered_target_or_pinned_fallback(
            &packet,
            &reference,
            &live_comparison(&packet),
            "range exceeds captured source"
        ),
        RenderedTarget::PinnedFallback { .. }
    ));
}

#[test]
fn rendered_navigation_falls_back_for_an_out_of_hunk_line() {
    let packet = packet();
    let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 2, 2).unwrap();
    match rendered_target_or_pinned_fallback(&packet, &reference, &live_comparison(&packet), "unused") {
        RenderedTarget::PinnedFallback { reason, .. } => {
            assert!(reason.contains("outside every captured diff hunk"), "{reason}");
        }
        other => panic!("expected PinnedFallback, got {other:?}"),
    }
}

#[test]
fn rendered_navigation_falls_back_when_the_file_has_no_patch() {
    let mut packet = packet();
    packet.files[0].patch = None;
    let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
    match rendered_target_or_pinned_fallback(&packet, &reference, &live_comparison(&packet), "unused") {
        RenderedTarget::PinnedFallback { reason, .. } => {
            assert!(reason.contains("omitted the API patch"), "{reason}");
        }
        other => panic!("expected PinnedFallback, got {other:?}"),
    }
}

#[test]
fn rendered_navigation_falls_back_when_the_head_moved() {
    let packet = packet();
    let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
    let mut current = live_comparison(&packet);
    current.head_sha = "moved-head".to_owned();
    match rendered_target_or_pinned_fallback(&packet, &reference, &current, "unused") {
        RenderedTarget::PinnedFallback { reason, .. } => {
            assert!(reason.contains("is not the live comparison"), "{reason}");
        }
        other => panic!("expected PinnedFallback, got {other:?}"),
    }
}

#[test]
fn rendered_navigation_hashes_the_current_filename_on_both_rename_sides() {
    let mut packet = packet();
    packet.files[0].path = "new.rs".to_owned();
    packet.files[0].previous_path = Some("old.rs".to_owned());
    packet.files[0].change_kind = ChangeKind::Renamed;
    packet.files[0].patch = Some("@@ -1 +1 @@".to_owned());
    packet.files[0].before.as_mut().unwrap().path = "old.rs".to_owned();
    packet.files[0].after.as_mut().unwrap().path = "new.rs".to_owned();
    packet.files[0].before.as_mut().unwrap().content = Some("was\n".to_owned());
    packet.files[0].before.as_mut().unwrap().content_hash = Some(hex_digest(b"was\n"));
    packet.files[0].after.as_mut().unwrap().content = Some("now\n".to_owned());
    packet.files[0].after.as_mut().unwrap().content_hash = Some(hex_digest(b"now\n"));
    let before = validate_pinned_reference(&packet, SourceSide::Before, "old.rs", 1, 1).unwrap();
    let after = validate_pinned_reference(&packet, SourceSide::After, "new.rs", 1, 1).unwrap();
    let hash = hex_digest(b"new.rs");
    let current = live_comparison(&packet);
    match rendered_target_or_pinned_fallback(&packet, &before, &current, "unused") {
        RenderedTarget::Validated { href, evidence, .. } => {
            assert_eq!(
                href,
                format!("https://github.com/acme/widget/pull/4/files#diff-{hash}L1")
            );
            assert_eq!(evidence.file_path, "new.rs");
        }
        other => panic!("expected Validated before-side, got {other:?}"),
    }
    match rendered_target_or_pinned_fallback(&packet, &after, &current, "unused") {
        RenderedTarget::Validated { href, evidence, .. } => {
            assert_eq!(
                href,
                format!("https://github.com/acme/widget/pull/4/files#diff-{hash}R1")
            );
            assert_eq!(evidence.file_path, "new.rs");
        }
        other => panic!("expected Validated after-side, got {other:?}"),
    }
}

#[test]
fn omitted_source_makes_the_packet_incomplete() {
    let mut packet = packet();
    packet.files[0].after = Some(PinnedSource::omitted(
        "acme/widget",
        &"c".repeat(40),
        "src/with space.rs",
        "pinned source read failed: timeout".to_owned(),
        None,
        false,
    ));
    assert!(!packet.is_complete());
}

#[test]
fn terminal_symlink_omission_settles_the_packet() {
    let mut packet = packet();
    let link = boss_github::trees::PinnedTreeEntry {
        path: "link".to_owned(),
        object_sha: "e".repeat(40),
        mode: "120000".to_owned(),
        object_type: "blob".to_owned(),
        size: Some(11),
    };
    let source = PinnedSource::omitted(
        "acme/widget",
        &"c".repeat(40),
        "link",
        SYMLINK_SOURCE_OMISSION.to_owned(),
        Some(&link),
        true,
    );
    assert!(!source.is_captured());
    assert!(source.is_settled());
    packet.files[0].after = Some(source);
    assert!(packet.is_complete());
}

#[test]
fn omitted_api_patch_does_not_hide_available_pinned_sources() {
    let mut packet = packet();
    packet.files[0].patch = None;
    packet.omissions.push(SourceOmission {
        path: Some("src/with space.rs".to_owned()),
        side: None,
        reason: "GitHub omitted the API patch; pinned source was collected instead".to_owned(),
        terminal: true,
    });
    assert!(packet.is_complete());
}

#[test]
fn binary_source_is_an_explicit_immutable_omission_without_lossy_text() {
    let source = PinnedSource::omitted_binary(
        "acme/widget",
        &"c".repeat(40),
        "image.bin",
        vec![0, 0xff, 1],
        &boss_github::trees::PinnedTreeEntry {
            path: "image.bin".to_owned(),
            object_sha: "d".repeat(40),
            mode: "100644".to_owned(),
            object_type: "blob".to_owned(),
            size: Some(3),
        },
    );
    assert!(source.content.is_none());
    assert_eq!(source.byte_count, Some(3));
    assert!(source.content_hash.is_some());
    assert!(source.omission.as_deref().unwrap().contains("non-UTF-8"));
    assert!(source.is_captured());
    let mut packet = packet();
    packet.files[0].after = Some(source);
    packet.omissions.push(SourceOmission {
        path: Some("image.bin".to_owned()),
        side: Some(SourceSide::After),
        reason: BINARY_SOURCE_OMISSION.to_owned(),
        terminal: true,
    });
    assert!(
        packet.is_complete(),
        "a successfully hashed binary side must not make the packet incomplete"
    );
}

#[test]
fn symlink_tree_entries_are_omitted_rather_than_followed() {
    let link = boss_github::trees::PinnedTreeEntry {
        path: "link".to_owned(),
        object_sha: "e".repeat(40),
        mode: "120000".to_owned(),
        object_type: "blob".to_owned(),
        size: Some(11),
    };
    let reason = pinned_entry_omission(&link).expect("symlink must be omitted");
    assert!(reason.contains("symlink"));
    let source = PinnedSource::omitted("acme/widget", &"c".repeat(40), "link", reason, Some(&link), true);
    assert!(!source.is_captured());
    assert!(source.is_settled());
    assert_eq!(source.object_sha.as_deref(), Some(link.object_sha.as_str()));
    assert_eq!(source.mode.as_deref(), Some("120000"));
}

#[test]
fn unrecognised_file_status_is_not_flattened_into_modified() {
    assert_eq!(ChangeKind::from_api("renamed"), ChangeKind::Renamed);
    assert_eq!(
        ChangeKind::from_api("unchanged"),
        ChangeKind::Unknown("unchanged".to_owned())
    );
    assert!(ChangeKind::from_api("unchanged").has_before());
    assert!(ChangeKind::from_api("unchanged").has_after());
}

#[test]
fn same_count_force_push_is_rejected_by_endpoint_revalidation() {
    let observed = PinnedComparison {
        base_sha: "base".to_owned(),
        head_sha: "head-one".to_owned(),
    };
    let moved = boss_github::pr_files::PrComparisonMetadata {
        number: 4,
        title: "Capture source".to_owned(),
        body: None,
        base_repository: "acme/widget".to_owned(),
        head_repository: "acme/widget".to_owned(),
        head_ref_name: "feature".to_owned(),
        base_sha: "base".to_owned(),
        head_sha: "head-two".to_owned(),
        changed_files: 1,
    };
    let err = require_stable_endpoints(&observed, &moved).unwrap_err().to_string();
    assert!(err.contains("head-one"));
    assert!(err.contains("head-two"));
}

#[test]
fn oversized_tree_entry_is_omitted_before_contents_read() {
    let entry = boss_github::trees::PinnedTreeEntry {
        path: "huge.bin".to_owned(),
        object_sha: "f".repeat(40),
        mode: "100644".to_owned(),
        object_type: "blob".to_owned(),
        size: Some(MAX_PINNED_SOURCE_BYTES + 1),
    };
    assert!(
        pinned_entry_omission(&entry)
            .unwrap()
            .contains("per-file capture budget")
    );
}

#[test]
fn permalink_leaves_tilde_unescaped_like_tree_paths() {
    let mut packet = packet();
    packet.files[0].path = "a~b/c d".to_owned();
    packet.files[0].after.as_mut().unwrap().path = "a~b/c d".to_owned();
    packet.files[0].after.as_mut().unwrap().content = Some("first\n".to_owned());
    packet.files[0].after.as_mut().unwrap().content_hash = Some(hex_digest(b"first\n"));
    let reference = validate_pinned_reference(&packet, SourceSide::After, "a~b/c d", 1, 1).unwrap();
    assert_eq!(
        reference.href,
        format!("https://github.com/acme/widget/blob/{}/a~b/c%20d#L1", "c".repeat(40))
    );
}

#[tokio::test]
async fn bounded_reads_overlap_without_exceeding_eight() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let requests = (0..25).map(|index| {
        let active = active.clone();
        let peak = peak.clone();
        async move {
            assert_eq!(
                boss_gh_telemetry::current_caller(),
                boss_gh_telemetry::callers::REVIEW_GUIDE_SOURCE_CAPTURE
            );
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(count, Ordering::SeqCst);
            tokio::task::yield_now().await;
            active.fetch_sub(1, Ordering::SeqCst);
            index
        }
    });
    let mut result = boss_gh_telemetry::scope(
        boss_gh_telemetry::callers::REVIEW_GUIDE_SOURCE_CAPTURE,
        resolve_bounded(requests),
    )
    .await
    .unwrap();
    result.sort();
    assert_eq!(result, (0..25).collect::<Vec<_>>());
    assert_eq!(peak.load(Ordering::SeqCst), 8);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[test]
fn rendered_navigation_defaults_to_an_explicit_pinned_fallback() {
    let reference = validate_pinned_reference(&packet(), SourceSide::After, "src/with space.rs", 1, 1).unwrap();
    assert!(matches!(
        rendered_target_or_pinned_fallback(
            &packet(),
            &reference,
            &PinnedComparison {
                base_sha: "unknown".into(),
                head_sha: "unknown".into()
            },
            "rendered fragment was not validated"
        ),
        RenderedTarget::PinnedFallback { .. }
    ));
}
