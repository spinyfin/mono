//! Global driver traffic allocation: how ordinary dispatch is split three
//! ways between the `grok`, `claude`, and `codex` drivers.
//!
//! This generalises the earlier single "Codex percentage" (one integer, with
//! an implicit remainder falling through to whatever the row would otherwise
//! have used) into an explicit three-way split. The modelling choice is
//! deliberate on three counts:
//!
//! 1. **Three explicit integers, not two-with-an-implied-third and not
//!    normalised weights.** An implied third makes the invariant invisible at
//!    the call site and makes the UI's "which of the others gives way?"
//!    question unanswerable without extra state. Normalised weights would
//!    quietly turn a typo (`50/50/50`) into a valid-looking allocation, which
//!    is exactly the silent-repair failure mode this must not have.
//!    [`DriverTrafficSplit::validate`] instead requires the shares to sum to
//!    exactly 100 and rejects anything else loudly.
//!
//! 2. **All-three-zero is rejected by the same rule that catches every other
//!    bad split** — it does not sum to 100 — so there is no separate
//!    "nowhere to dispatch" special case to forget. Any *one* or any *two*
//!    shares may be zero; a zero share is a literally empty bucket (see
//!    [`DriverTrafficSplit::driver_for_bucket`]), not a very small one.
//!
//! 3. **The whole triple is one value.** Persisting and transporting it as a
//!    single unit (one metadata row, one wire field) means an operator edit is
//!    one atomic write; a concurrent dispatch reading the allocation can never
//!    observe a half-applied edit that transits through an invalid state.

use serde::{Deserialize, Serialize};

/// The three drivers traffic allocation distributes between, in the order
/// they occupy the `[0, 100)` bucket line (see
/// [`DriverTrafficSplit::driver_for_bucket`]).
pub const DRIVER_SLUG_CODEX: &str = "codex";
pub const DRIVER_SLUG_CLAUDE: &str = "claude";
pub const DRIVER_SLUG_GROK: &str = "grok";

/// A three-way allocation of eligible dispatch traffic. Shares are whole
/// percentage points and MUST sum to exactly 100 — see the module doc.
///
/// [`Default`] is `claude = 100`, with both `grok` and `codex` at zero. That
/// is the behaviour-preserving state: `claude` is
/// `boss_engine_effort::ENGINE_DEFAULT_DRIVER`, so an engine that has never
/// had a split configured allocates exactly where it always did, and the
/// `grok` driver — whose spawn path is currently under investigation —
/// receives nothing at all until an operator deliberately raises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverTrafficSplit {
    /// Share routed to the `claude` driver.
    pub claude: u8,
    /// Share routed to the `codex` driver.
    pub codex: u8,
    /// Share routed to the `grok` driver.
    pub grok: u8,
}

impl Default for DriverTrafficSplit {
    fn default() -> Self {
        Self {
            claude: 100,
            codex: 0,
            grok: 0,
        }
    }
}

/// Why a proposed [`DriverTrafficSplit`] was refused. Surfaced verbatim to
/// the operator rather than repaired — see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DriverTrafficSplitError {
    #[error(
        "driver traffic split must sum to exactly 100 (got {total}: grok={grok}, claude={claude}, codex={codex}); \
         any one or two shares may be 0, but the three together allocate all eligible traffic"
    )]
    NotOneHundred {
        grok: u8,
        claude: u8,
        codex: u8,
        total: u16,
    },
}

impl DriverTrafficSplit {
    /// The drivers in bucket-line order, low to high. The UI renders shares
    /// in this order too, so the on-screen bar reads left-to-right as the
    /// same line the hash is compared against.
    pub const DRIVERS_IN_BUCKET_ORDER: [&'static str; 3] = [DRIVER_SLUG_CODEX, DRIVER_SLUG_CLAUDE, DRIVER_SLUG_GROK];

    pub fn new(grok: u8, claude: u8, codex: u8) -> Self {
        Self { claude, codex, grok }
    }

    /// Sum of the three shares, in `u16` so a bad split (e.g. `100/100/100`)
    /// is reported at its real total instead of wrapping a `u8`.
    pub fn total(&self) -> u16 {
        u16::from(self.grok) + u16::from(self.claude) + u16::from(self.codex)
    }

    /// `Ok(())` only when the shares sum to exactly 100. Never repairs,
    /// clamps, or normalises — the caller is expected to propagate the error
    /// to whoever proposed the split.
    pub fn validate(&self) -> Result<(), DriverTrafficSplitError> {
        let total = self.total();
        if total == 100 {
            Ok(())
        } else {
            Err(DriverTrafficSplitError::NotOneHundred {
                grok: self.grok,
                claude: self.claude,
                codex: self.codex,
                total,
            })
        }
    }

    /// Which driver owns `bucket`, where `bucket` is a stable hash of a work
    /// item's id in `[0, 100)`.
    ///
    /// The line is laid out `codex | claude | grok`, low to high:
    ///
    /// - `[0, codex)` → `codex`
    /// - `[codex, codex + claude)` → `claude`
    /// - `[100 - grok, 100)` → `grok`
    ///
    /// Two properties fall out of half-open intervals on a sum-to-100 split
    /// and both are load-bearing:
    ///
    /// - **Zero is zero.** A share of 0 makes its interval empty, so that
    ///   driver is unreachable through allocation — not merely improbable.
    /// - **Changing one boundary only moves rows across that boundary.**
    ///   Raising `grok` at `claude`'s expense moves the `claude`/`grok` cut
    ///   and leaves every `codex`-allocated row where it was. Reassignment
    ///   when the split changes is expected and deliberate; the invariant is
    ///   "same row, same split, same answer", which holds because this is a
    ///   pure function of `bucket` and `self`.
    ///
    /// `codex` keeps the low end of the line so that a row already allocated
    /// under the superseded single-Codex-percentage scheme (which used
    /// `bucket < percentage`) keeps its answer under the equivalent split.
    pub fn driver_for_bucket(&self, bucket: u8) -> &'static str {
        if bucket < self.codex {
            DRIVER_SLUG_CODEX
        } else if u16::from(bucket) < u16::from(self.codex) + u16::from(self.claude) {
            DRIVER_SLUG_CLAUDE
        } else {
            DRIVER_SLUG_GROK
        }
    }

    /// The share currently allocated to `driver`, or `None` for a slug this
    /// split does not cover.
    pub fn share_for(&self, driver: &str) -> Option<u8> {
        match driver {
            DRIVER_SLUG_CODEX => Some(self.codex),
            DRIVER_SLUG_CLAUDE => Some(self.claude),
            DRIVER_SLUG_GROK => Some(self.grok),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_claude() {
        let split = DriverTrafficSplit::default();
        assert_eq!(split, DriverTrafficSplit::new(0, 100, 0));
        split.validate().unwrap();
    }

    #[test]
    fn validate_rejects_sums_other_than_one_hundred() {
        for split in [
            DriverTrafficSplit::new(0, 0, 0),
            DriverTrafficSplit::new(50, 50, 50),
            DriverTrafficSplit::new(1, 1, 1),
            DriverTrafficSplit::new(0, 99, 0),
            DriverTrafficSplit::new(0, 101, 0),
        ] {
            assert!(split.validate().is_err(), "{split:?} must be rejected");
        }
    }

    #[test]
    fn validate_accepts_any_one_or_two_shares_at_zero() {
        for split in [
            DriverTrafficSplit::new(100, 0, 0),
            DriverTrafficSplit::new(0, 100, 0),
            DriverTrafficSplit::new(0, 0, 100),
            DriverTrafficSplit::new(50, 50, 0),
            DriverTrafficSplit::new(50, 0, 50),
            DriverTrafficSplit::new(0, 50, 50),
            DriverTrafficSplit::new(33, 33, 34),
        ] {
            split.validate().expect("valid split rejected");
        }
    }

    #[test]
    fn all_zero_error_names_the_real_total() {
        let err = DriverTrafficSplit::new(0, 0, 0).validate().unwrap_err();
        assert_eq!(
            err,
            DriverTrafficSplitError::NotOneHundred {
                grok: 0,
                claude: 0,
                codex: 0,
                total: 0,
            }
        );
    }

    /// The error reports the true total rather than a wrapped `u8`.
    #[test]
    fn oversized_split_reports_unwrapped_total() {
        let err = DriverTrafficSplit::new(100, 100, 100).validate().unwrap_err();
        let DriverTrafficSplitError::NotOneHundred { total, .. } = err;
        assert_eq!(total, 300);
    }

    /// Every bucket in `[0, 100)` lands on exactly the share each driver was
    /// given — no rounding, no gap, no overlap.
    #[test]
    fn buckets_partition_exactly_by_share() {
        for split in [
            DriverTrafficSplit::new(0, 100, 0),
            DriverTrafficSplit::new(10, 60, 30),
            DriverTrafficSplit::new(0, 0, 100),
            DriverTrafficSplit::new(100, 0, 0),
            DriverTrafficSplit::new(1, 98, 1),
            DriverTrafficSplit::new(33, 33, 34),
        ] {
            let mut counts = (0u8, 0u8, 0u8);
            for bucket in 0..100u8 {
                match split.driver_for_bucket(bucket) {
                    DRIVER_SLUG_GROK => counts.0 += 1,
                    DRIVER_SLUG_CLAUDE => counts.1 += 1,
                    DRIVER_SLUG_CODEX => counts.2 += 1,
                    other => panic!("unexpected driver {other}"),
                }
            }
            assert_eq!(counts, (split.grok, split.claude, split.codex), "{split:?}");
        }
    }

    /// A driver at 0 must be unreachable across the whole bucket space, not
    /// merely unlikely.
    #[test]
    fn zero_share_is_unreachable() {
        let split = DriverTrafficSplit::new(0, 40, 60);
        for bucket in 0..100u8 {
            assert_ne!(split.driver_for_bucket(bucket), DRIVER_SLUG_GROK);
        }
        let split = DriverTrafficSplit::new(60, 40, 0);
        for bucket in 0..100u8 {
            assert_ne!(split.driver_for_bucket(bucket), DRIVER_SLUG_CODEX);
        }
        let split = DriverTrafficSplit::new(60, 0, 40);
        for bucket in 0..100u8 {
            assert_ne!(split.driver_for_bucket(bucket), DRIVER_SLUG_CLAUDE);
        }
    }

    /// Raising `grok` at `claude`'s expense must not disturb any row already
    /// allocated to `codex` — the anchoring property the bucket order buys.
    #[test]
    fn moving_the_claude_grok_boundary_leaves_codex_rows_alone() {
        let before = DriverTrafficSplit::new(0, 70, 30);
        let after = DriverTrafficSplit::new(20, 50, 30);
        for bucket in 0..100u8 {
            if before.driver_for_bucket(bucket) == DRIVER_SLUG_CODEX {
                assert_eq!(after.driver_for_bucket(bucket), DRIVER_SLUG_CODEX, "bucket {bucket}");
            }
        }
    }

    /// A split equivalent to the superseded single-Codex-percentage scheme
    /// (`bucket < percentage` → codex) must allocate identically, so rows
    /// decided under it keep their driver.
    #[test]
    fn codex_low_end_matches_the_superseded_percentage_scheme() {
        for percentage in [0u8, 1, 25, 50, 99, 100] {
            let split = DriverTrafficSplit::new(0, 100 - percentage, percentage);
            for bucket in 0..100u8 {
                let expected_codex = bucket < percentage;
                assert_eq!(
                    split.driver_for_bucket(bucket) == DRIVER_SLUG_CODEX,
                    expected_codex,
                    "percentage {percentage}, bucket {bucket}"
                );
            }
        }
    }

    #[test]
    fn share_for_covers_exactly_the_three_drivers() {
        let split = DriverTrafficSplit::new(10, 60, 30);
        assert_eq!(split.share_for(DRIVER_SLUG_GROK), Some(10));
        assert_eq!(split.share_for(DRIVER_SLUG_CLAUDE), Some(60));
        assert_eq!(split.share_for(DRIVER_SLUG_CODEX), Some(30));
        assert_eq!(split.share_for("copilot"), None);
    }

    #[test]
    fn json_round_trips_and_requires_every_share() {
        let split = DriverTrafficSplit::new(10, 60, 30);
        let json = serde_json::to_string(&split).unwrap();
        assert_eq!(serde_json::from_str::<DriverTrafficSplit>(&json).unwrap(), split);
        // A hand-edited value missing a share is a parse error, not a
        // silently-defaulted zero.
        assert!(serde_json::from_str::<DriverTrafficSplit>(r#"{"grok":0,"claude":100}"#).is_err());
    }
}
