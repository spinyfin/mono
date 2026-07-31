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
    pub cache_write: f64,
    pub cache_read: f64,
}

/// `(family substring, price)` pairs, checked in order against the
/// lowercased model string. Update this table when list prices change —
/// it is the single place a human needs to edit.
pub const MODEL_PRICING: &[(&str, ModelPriceUsdPerMillion)] = &[
    (
        "opus",
        ModelPriceUsdPerMillion {
            input: 15.0,
            output: 75.0,
            cache_write: 18.75,
            cache_read: 1.50,
        },
    ),
    (
        "sonnet",
        ModelPriceUsdPerMillion {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        },
    ),
    (
        "haiku",
        ModelPriceUsdPerMillion {
            input: 0.80,
            output: 4.0,
            cache_write: 1.0,
            cache_read: 0.08,
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

/// Estimated USD for one run's recorded token counts under `price`.
/// `cache_creation_tokens` is priced at the flat `cache_write` rate —
/// the 5m/1h TTL split affects Anthropic's actual billing but is not
/// worth modelling in an estimate a human is told to sanity-check.
pub fn estimate_usd(
    price: ModelPriceUsdPerMillion,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
) -> f64 {
    const PER_MILLION: f64 = 1_000_000.0;
    (input_tokens as f64 * price.input
        + output_tokens as f64 * price.output
        + cache_creation_tokens as f64 * price.cache_write
        + cache_read_tokens as f64 * price.cache_read)
        / PER_MILLION
}

#[cfg(test)]
mod tests {
    use super::*;

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
