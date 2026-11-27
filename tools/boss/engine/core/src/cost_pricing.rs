//! Human-maintained USD-per-million-token pricing, used **only** for the
//! optional `boss cost ... --usd` estimate on top of the token totals
//! [`crate::cost_report`] computes.
//!
//! This table is not billing truth. It is a best-effort estimate an
//! operator edits directly in this file when list prices change — every
//! figure derived from it is labelled "estimated" wherever it reaches a
//! human (see `boss cost`'s CLI output). If a model string encountered in
//! `work_runs.model` matches no entry here (an unknown model, or the
//! literal `<synthetic>` string some rows carry with no attributable
//! driver), its tokens are excluded from the estimate and the caller is
//! told the estimate is partial rather than silently treating it as free.
//!
//! Matching is substring-based against the lowercased model string
//! (`"claude-sonnet-5-20260101"` matches the `"sonnet"` entry) so a new
//! dated model snapshot needs no code change as long as it keeps the
//! family name.

/// USD list price per one million tokens, by token category.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPriceUsdPerMillion {
    pub input: f64,
    pub output: f64,
    /// 5-minute cache write rate (also used as the single-rate fallback when
    /// a run only recorded an unsplit `cache_creation_tokens` count).
    pub cache_write: Option<f64>,
    /// 1-hour cache write rate.
    pub cache_write_1h: Option<f64>,
    pub cache_read: Option<f64>,
}

/// `(family substring, price)` pairs, checked in order against the
/// lowercased model string. Update this table when list prices change —
/// it is the single place a human needs to edit.
pub const MODEL_PRICING: &[(&str, ModelPriceUsdPerMillion)] = &[
    // Claude Fable 5.2 (source: https://platform.claude.com/docs/en/about-claude/pricing, retrieved 2026-11-27)
    (
        "fable",
        ModelPriceUsdPerMillion {
            input: 8.0,
            output: 40.0,
            cache_write: Some(10.0),
            cache_write_1h: Some(16.0),
            cache_read: Some(0.20),
        },
    ),
    // Claude Opus 5.5 (source: https://platform.claude.com/docs/en/about-claude/pricing, retrieved 2026-11-27)
    (
        "opus",
        ModelPriceUsdPerMillion {
            input: 4.0,
            output: 20.0,
            cache_write: Some(5.0),
            cache_write_1h: Some(8.0),
            cache_read: Some(0.20),
        },
    ),
    // Claude Sonnet 5.5 (source: https://platform.claude.com/docs/en/about-claude/pricing, retrieved 2026-11-27)
    (
        "sonnet",
        ModelPriceUsdPerMillion {
            input: 2.0,
            output: 10.0,
            cache_write: Some(2.50),
            cache_write_1h: Some(4.0),
            cache_read: Some(0.10),
        },
    ),
    (
        "haiku",
        ModelPriceUsdPerMillion {
            input: 0.80,
            output: 4.0,
            cache_write: Some(1.0),
            cache_write_1h: None,
            cache_read: Some(0.08),
        },
    ),
    // Gemini 4 API reference rates (USD per million tokens, as of November 2026;
    // documented in `tools/boss/docs/designs/effort-and-model-estimation.md`).
    // Subscription agy runs remain unpriced in cost_report::price_for_record.
    // Cache rates are omitted because published cache pricing is not configured
    // here, so nonzero cache usage cannot be estimated.
    (
        "gemini-4-hydrogen",
        ModelPriceUsdPerMillion {
            input: 1.50,
            output: 7.50,
            cache_write: None,
            cache_write_1h: None,
            cache_read: None,
        },
    ),
    (
        "gemini-4-neon",
        ModelPriceUsdPerMillion {
            input: 3.0,
            output: 15.0,
            cache_write: None,
            cache_write_1h: None,
            cache_read: None,
        },
    ),
    // Regular list price; the introductory $2/$10 offer is temporary.
    (
        "gemini-4-carbon",
        ModelPriceUsdPerMillion {
            input: 4.0,
            output: 20.0,
            cache_write: None,
            cache_write_1h: None,
            cache_read: None,
        },
    ),
];

/// The priced family whose substring matches `model`, if any. `None` for
/// an unrecognised model (including the literal `<synthetic>` marker) —
/// callers must treat that as "no price available", never as zero cost.
pub fn price_for_model(model: &str) -> Option<ModelPriceUsdPerMillion> {
    let lower = model.to_ascii_lowercase();
    MODEL_PRICING
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, price)| *price)
}

/// Estimated USD for one run's recorded token counts under `price`, using a
/// single `cache_write` rate for `cache_creation_tokens`.
pub fn estimate_usd(
    price: ModelPriceUsdPerMillion,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
) -> Option<f64> {
    estimate_usd_with_cache_split(
        price,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        None,
        None,
        cache_read_tokens,
    )
}

/// Estimated USD for one run's recorded token counts under `price`.
/// When both `cache_creation_5m_tokens` and `cache_creation_1h_tokens` are
/// present, each lifetime is priced at its own rate; otherwise
/// `cache_creation_tokens` falls back to the single `cache_write` rate.
pub fn estimate_usd_with_cache_split(
    price: ModelPriceUsdPerMillion,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_creation_5m_tokens: Option<i64>,
    cache_creation_1h_tokens: Option<i64>,
    cache_read_tokens: i64,
) -> Option<f64> {
    const PER_MILLION: f64 = 1_000_000.0;
    let cache_cost = |tokens: i64, rate: Option<f64>| {
        if tokens == 0 {
            Some(0.0)
        } else {
            rate.map(|rate| tokens as f64 * rate)
        }
    };
    let cache_write_cost = match (cache_creation_5m_tokens, cache_creation_1h_tokens) {
        (Some(tokens_5m), Some(tokens_1h)) => {
            cache_cost(tokens_5m, price.cache_write)? + cache_cost(tokens_1h, price.cache_write_1h)?
        }
        _ => cache_cost(cache_creation_tokens, price.cache_write)?,
    };
    Some(
        (input_tokens as f64 * price.input
            + output_tokens as f64 * price.output
            + cache_write_cost
            + cache_cost(cache_read_tokens, price.cache_read)?)
            / PER_MILLION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_families_match_fable_opus_sonnet_with_sourced_rates() {
        for (model, expected_input, expected_output, expected_5m, expected_1h, expected_read) in [
            ("fable", 8.0, 40.0, 10.0, 16.0, 0.20),
            ("claude-fable-5-2", 8.0, 40.0, 10.0, 16.0, 0.20),
            ("CLAUDE-FABLE-5", 8.0, 40.0, 10.0, 16.0, 0.20),
            ("opus", 4.0, 20.0, 5.0, 8.0, 0.20),
            ("claude-opus-5-5-20260901", 4.0, 20.0, 5.0, 8.0, 0.20),
            ("sonnet", 2.0, 10.0, 2.50, 4.0, 0.10),
            ("claude-sonnet-5-5-20260901", 2.0, 10.0, 2.50, 4.0, 0.10),
        ] {
            let price = price_for_model(model).unwrap_or_else(|| panic!("{model} should match"));
            assert_eq!(price.input, expected_input, "{model}");
            assert_eq!(price.output, expected_output, "{model}");
            assert_eq!(price.cache_write, Some(expected_5m), "{model}");
            assert_eq!(price.cache_write_1h, Some(expected_1h), "{model}");
            assert_eq!(price.cache_read, Some(expected_read), "{model}");
        }
        assert!(price_for_model("gpt-5-codex").is_none());
        assert!(price_for_model("grok-4.5-build").is_none());
        assert!(price_for_model("<synthetic>").is_none());
    }

    #[test]
    fn estimate_usd_with_cache_split_prices_5m_and_1h_separately_when_present() {
        let opus = price_for_model("claude-opus-5-5").unwrap();
        let split = estimate_usd_with_cache_split(
            opus,
            1_000_000,
            1_000_000,
            1_000_000,
            Some(400_000),
            Some(600_000),
            1_000_000,
        )
        .unwrap();
        let expected_split = 4.0 + 20.0 + (0.4 * 5.0 + 0.6 * 8.0) + 0.20;
        assert!((split - expected_split).abs() < 1e-9, "split={split}");

        let unsplit =
            estimate_usd_with_cache_split(opus, 1_000_000, 1_000_000, 1_000_000, None, None, 1_000_000).unwrap();
        let expected_unsplit = 4.0 + 20.0 + 5.0 + 0.20;
        assert!((unsplit - expected_unsplit).abs() < 1e-9, "unsplit={unsplit}");
    }

    #[test]
    fn gemini_families_match_all_thinking_suffixes_without_inventing_cache_prices() {
        for (family, input, output) in [("hydrogen", 1.5, 7.5), ("neon", 3.0, 15.0), ("carbon", 4.0, 20.0)] {
            for rung in ["low", "medium", "high", "xhigh"] {
                let price = price_for_model(&format!("GEMINI-4-{family}-{rung}")).unwrap();
                assert_eq!(price.input, input);
                assert_eq!(price.output, output);
                assert_eq!(estimate_usd(price, 1_000_000, 1_000_000, 0, 0), Some(input + output));
                assert_eq!(estimate_usd(price, 0, 0, 1, 0), None);
                assert_eq!(estimate_usd(price, 0, 0, 0, 1), None);
            }
        }
    }
}
