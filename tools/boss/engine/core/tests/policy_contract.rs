//! Public pricing and merge-policy contracts, compiled independently of core unit tests.

// Retain the original module paths so test filters and names stay stable.
mod cost_pricing {
    mod tests {
        use boss_engine::cost_pricing::{ModelPriceUsdPerMillion, estimate_usd, price_for_model};

        #[test]
        fn matches_family_substring_case_insensitively() {
            assert_eq!(price_for_model("claude-sonnet-5-20260101"), price_for_model("SONNET"));
            assert!(price_for_model("claude-opus-5").is_some());
            assert!(price_for_model("claude-haiku-4-5-20251001").is_some());
        }

        #[test]
        fn unknown_model_has_no_price() {
            assert!(price_for_model("<synthetic>").is_none());
            assert!(price_for_model("gpt-5-codex").is_none());
        }

        #[test]
        fn estimate_sums_every_token_category_at_its_own_rate() {
            let price = ModelPriceUsdPerMillion {
                input: 3.0,
                output: 15.0,
                cache_write: 3.75,
                cache_read: 0.30,
            };
            // 1M input + 1M output + 1M cache-write + 1M cache-read tokens.
            let usd = estimate_usd(price, 1_000_000, 1_000_000, 1_000_000, 1_000_000);
            assert!((usd - (3.0 + 15.0 + 3.75 + 0.30)).abs() < 1e-9, "usd={usd}");
        }

        #[test]
        fn zero_tokens_estimate_to_zero() {
            let price = price_for_model("sonnet").expect("sonnet priced");
            assert_eq!(estimate_usd(price, 0, 0, 0, 0), 0.0);
        }
    }
}

mod merge_mechanism {
    mod tests {
        use boss_engine::merge_mechanism::{
            MergeMechanism, is_push_restriction_error, render_push_restriction_attention_body,
        };

        #[test]
        fn none_and_direct_parse_to_direct() {
            assert_eq!(MergeMechanism::parse(None).unwrap(), MergeMechanism::Direct);
            assert_eq!(MergeMechanism::parse(Some("direct")).unwrap(), MergeMechanism::Direct);
        }

        #[test]
        fn trunk_queue_parses_with_default_target_branch() {
            assert_eq!(
                MergeMechanism::parse(Some("trunk_queue")).unwrap(),
                MergeMechanism::TrunkQueue {
                    target_branch: "main".to_string()
                }
            );
        }

        #[test]
        fn unknown_value_is_rejected() {
            let err = MergeMechanism::parse(Some("bogus")).unwrap_err();
            assert!(err.to_string().contains("bogus"));
        }

        #[test]
        fn push_restriction_markers_are_detected_case_insensitively() {
            assert!(is_push_restriction_error(
                "gh pr merge failed: GH013: Repository rule violations found"
            ));
            assert!(is_push_restriction_error(
                "remote: error: GH013: repository rule violations found for refs/heads/main"
            ));
            assert!(is_push_restriction_error("remote: protected branch hook declined"));
            assert!(is_push_restriction_error(
                "Changes must be made through a pull request."
            ));
            assert!(is_push_restriction_error(
                "this repository requires a merge queue for pull request merges"
            ));
            assert!(is_push_restriction_error("5 of 5 required status checks are expected"));
        }

        #[test]
        fn ordinary_merge_failures_are_not_push_restrictions() {
            assert!(!is_push_restriction_error(
                "gh pr merge failed: pull request is not mergeable: conflicts"
            ));
            assert!(!is_push_restriction_error("gh: authentication required"));
            assert!(!is_push_restriction_error("pull request #42 is already merged"));
        }

        #[test]
        fn attention_body_names_the_fix() {
            let body =
                render_push_restriction_attention_body("Flunge", "flunge", "GH013: Repository rule violations found");
            assert!(body.contains("Flunge"));
            assert!(body.contains("merge_mechanism=trunk_queue"));
            assert!(body.contains("`flunge`"));
            assert!(body.contains("GH013"));
        }
    }
}
