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
    pub cache_write: Option<f64>,
    pub cache_read: Option<f64>,
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
            cache_write: Some(18.75),
            cache_read: Some(1.50),
        },
    ),
    (
        "sonnet",
        ModelPriceUsdPerMillion {
            input: 3.0,
            output: 15.0,
            cache_write: Some(3.75),
            cache_read: Some(0.30),
        },
    ),
    (
        "haiku",
        ModelPriceUsdPerMillion {
            input: 0.80,
            output: 4.0,
            cache_write: Some(1.0),
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
            cache_read: None,
        },
    ),
    (
        "gemini-4-neon",
        ModelPriceUsdPerMillion {
            input: 3.0,
            output: 15.0,
            cache_write: None,
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
) -> Option<f64> {
    const PER_MILLION: f64 = 1_000_000.0;
    let cache_cost = |tokens: i64, rate: Option<f64>| {
        if tokens == 0 {
            Some(0.0)
        } else {
            rate.map(|rate| tokens as f64 * rate)
        }
    };
    Some(
        (input_tokens as f64 * price.input
            + output_tokens as f64 * price.output
            + cache_cost(cache_creation_tokens, price.cache_write)?
            + cache_cost(cache_read_tokens, price.cache_read)?)
            / PER_MILLION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
