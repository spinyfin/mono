//! Read-time metric series types — backs `GetMetricCatalog` /
//! `GetMetricSeries` and the `boss metrics catalog|series` CLI.
//!
//! Aggregation happens in the engine over the primary tables. A bucket
//! with no fact is absent from the reply; a bucket with facts whose
//! value is zero is present with `n > 0`. The client never fabricates
//! a zero. See `tools/boss/docs/designs/performance-metrics-timeseries-capture-and-interactive-swiftui-charts.md`.

use serde::{Deserialize, Serialize};

/// One dimension filter on a [`crate::FrontendRequest::GetMetricSeries`]
/// query. Filters AND across dimensions and OR within `values`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricFilter {
    pub dimension: String,
    pub values: Vec<String>,
}

/// Named filter preset published on a series so a client can offer a
/// chip without inventing semantics. The catalog's "failed or reaped"
/// preset is `status in (failed, orphaned, abandoned)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct MetricFilterPreset {
    pub id: String,
    pub filters: Vec<MetricFilter>,
    pub title: String,
}

/// Kind of value a series produces. The app renders by this tag; adding
/// a series later needs no client change as long as the kind is known.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricValueKind {
    Count,
    Duration,
    Points,
    Tokens,
    Usd,
}

impl MetricValueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Duration => "duration",
            Self::Points => "points",
            Self::Tokens => "tokens",
            Self::Usd => "usd",
        }
    }
}

/// One cell's aggregated value. Tagged so a client can switch on
/// `kind` without knowing the series id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricValue {
    Count {
        n: u32,
    },
    Duration {
        max_ms: i64,
        mean_ms: i64,
        p50_ms: i64,
        p90_ms: i64,
    },
    Points {
        points: i64,
    },
    Tokens {
        cache_read: i64,
        cache_write: i64,
        input: i64,
        output: i64,
    },
    Usd {
        unpriceable_runs: u32,
        partial: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimated: Option<f64>,
    },
}

/// One group's contribution to a bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricCell {
    pub group: String,
    pub n: u32,
    pub value: MetricValue,
}

/// One time bucket. Absent from the reply when it contains no facts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricBucket {
    pub start_epoch_s: i64,
    pub cells: Vec<MetricCell>,
}

/// Why a coverage band or caveat applies to a series reply.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoverageNoteKind {
    BucketCoarsened,
    CaptureStarted,
    DimensionStarted,
    PricingFlatRates,
    PricingGaps,
    RetentionBounded,
}

impl CoverageNoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BucketCoarsened => "bucket_coarsened",
            Self::CaptureStarted => "capture_started",
            Self::DimensionStarted => "dimension_started",
            Self::PricingFlatRates => "pricing_flat_rates",
            Self::PricingGaps => "pricing_gaps",
            Self::RetentionBounded => "retention_bounded",
        }
    }
}

/// One coverage caveat. `epoch_s` is the instant the note applies at,
/// when there is one (capture start, dimension-first-seen, coarsen).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CoverageNote {
    pub detail: String,
    pub kind: CoverageNoteKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_s: Option<i64>,
}

/// Honest-range metadata for a series reply or catalog entry.
///
/// `data_from_epoch_s` is the earliest fact for the series after filters.
/// `dimension_from_epoch_s` is the earliest fact where the group-by
/// dimension is non-NULL. A client shades the requested range before
/// those instants rather than drawing a line through unmeasured history.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SeriesCoverage {
    pub notes: Vec<CoverageNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_from_epoch_s: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension_from_epoch_s: Option<i64>,
}

/// One observed value of a catalog dimension, used to build filter chips
/// from data rather than a hardcoded list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct MetricDimensionValue {
    pub value: String,
    pub count: u32,
    pub first_seen_epoch_s: i64,
}

/// One sliceable dimension and the values currently present in the data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct MetricDimensionInfo {
    pub id: String,
    pub title: String,
    pub values: Vec<MetricDimensionValue>,
}

/// Catalog entry for one engine-defined series.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct MetricSeriesInfo {
    pub id: String,
    pub coverage: SeriesCoverage,
    pub dimensions: Vec<String>,
    pub presets: Vec<MetricFilterPreset>,
    pub title: String,
    pub value_kind: MetricValueKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_group_by: Option<String>,
}

/// Reply to [`crate::FrontendRequest::GetMetricCatalog`]: every series
/// the engine can serve, the dimensions they support, observed dimension
/// values, and per-series coverage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct MetricCatalog {
    pub dimensions: Vec<MetricDimensionInfo>,
    pub generated_at_epoch_s: i64,
    pub series: Vec<MetricSeriesInfo>,
}

/// Reply to [`crate::FrontendRequest::GetMetricSeries`]: one series
/// bucketed over `[since_epoch_s, until_epoch_s)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct MetricSeriesReport {
    pub series: String,
    pub bucket_secs: i64,
    pub buckets: Vec<MetricBucket>,
    pub coverage: SeriesCoverage,
    pub generated_at_epoch_s: i64,
    pub groups: Vec<String>,
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    pub value_kind: MetricValueKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_value_count_round_trips_tagged() {
        let value = MetricValue::Count { n: 3 };
        let encoded = serde_json::to_value(&value).unwrap();
        assert_eq!(encoded["kind"], "count");
        assert_eq!(encoded["n"], 3);
        let decoded: MetricValue = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn metric_value_duration_round_trips_tagged() {
        let value = MetricValue::Duration {
            max_ms: 9_000,
            mean_ms: 4_000,
            p50_ms: 3_000,
            p90_ms: 8_000,
        };
        let encoded = serde_json::to_value(&value).unwrap();
        assert_eq!(encoded["kind"], "duration");
        let decoded: MetricValue = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn series_coverage_skips_none_instants_on_encode() {
        let coverage = SeriesCoverage::default();
        let encoded = serde_json::to_value(&coverage).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("data_from_epoch_s"));
        assert!(!obj.contains_key("dimension_from_epoch_s"));
        assert_eq!(obj["notes"], serde_json::json!([]));
    }
}
