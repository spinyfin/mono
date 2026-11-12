//! Global driver traffic allocation: how ordinary dispatch is split four
//! ways between the `grok`, `claude`, `codex`, and `agy` drivers.
//!
//! This generalises the earlier single "Codex percentage" (one integer, with
//! an implicit remainder falling through to whatever the row would otherwise
//! have used) into an explicit four-way split. The modelling choice is
//! deliberate on three counts:
//!
//! 1. **Four explicit integers, with no implied remainder or
//!    normalised weights.** An implied remainder makes the invariant invisible at
//!    the call site and makes the UI's "which of the others gives way?"
//!    question unanswerable without extra state. Normalised weights would
//!    quietly turn a typo (`50/50/50`) into a valid-looking allocation, which
//!    is exactly the silent-repair failure mode this must not have.
//!    [`DriverTrafficSplit::validate`] instead requires the shares to sum to
//!    exactly 100 and rejects anything else loudly.
//!
//! 2. **All-four-zero is rejected by the same rule that catches every other
//!    bad split** — it does not sum to 100 — so there is no separate
//!    "nowhere to dispatch" special case to forget. Up to *three*
//!    shares may be zero; a zero share is a literally empty bucket (see
//!    [`DriverTrafficSplit::driver_for_bucket`]), not a very small one.
//!
//! 3. **The whole split is one value.** Persisting and transporting it as a
//!    single unit (one metadata row, one wire field) means an operator edit is
//!    one atomic write; a concurrent dispatch reading the allocation can never
//!    observe a half-applied edit that transits through an invalid state.

use serde::{Deserialize, Serialize};

/// The four drivers traffic allocation distributes between, in the order
/// they occupy the `[0, 100)` bucket line (see
/// [`DriverTrafficSplit::driver_for_bucket`]).
pub const DRIVER_SLUG_CODEX: &str = "codex";
pub const DRIVER_SLUG_CLAUDE: &str = "claude";
pub const DRIVER_SLUG_GROK: &str = "grok";
pub const DRIVER_SLUG_AGY: &str = "agy";

/// A four-way allocation of eligible dispatch traffic. Shares are whole
/// percentage points and MUST sum to exactly 100 — see the module doc.
///
/// [`Default`] is `claude = 100`, with `grok`, `codex`, and `agy` at zero. That
/// is the behaviour-preserving state: `claude` is
/// `boss_engine_effort::ENGINE_DEFAULT_DRIVER`, so an engine that has never
/// had a split configured allocates exactly where it always did, and the
/// `agy` driver receives no traffic until its configured share is explicitly increased.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverTrafficSplit {
    /// Share routed to the `claude` driver.
    pub claude: u8,
    /// Share routed to the `codex` driver.
    pub codex: u8,
    /// Share routed to the `grok` driver.
    pub grok: u8,
    /// Share routed to the `agy` (Antigravity) driver.
    pub agy: u8,
}

impl Default for DriverTrafficSplit {
    fn default() -> Self {
        Self {
            claude: 100,
            codex: 0,
            grok: 0,
            agy: 0,
        }
    }
}

/// Why a proposed [`DriverTrafficSplit`] was refused. Surfaced verbatim to
/// the operator rather than repaired — see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DriverTrafficSplitError {
    #[error(
        "driver traffic split must sum to exactly 100 (got {total}: grok={grok}, claude={claude}, codex={codex}, agy={agy}); \
         up to three shares may be 0, but the four together allocate all eligible traffic"
    )]
    NotOneHundred {
        grok: u8,
        claude: u8,
        codex: u8,
        agy: u8,
        total: u16,
    },
}

impl DriverTrafficSplit {
    /// The drivers in bucket-line order, low to high. The UI renders shares
    /// in this order too, so the on-screen bar reads left-to-right as the
    /// same line the hash is compared against.
    pub const DRIVERS_IN_BUCKET_ORDER: [&'static str; 4] =
        [DRIVER_SLUG_CODEX, DRIVER_SLUG_CLAUDE, DRIVER_SLUG_GROK, DRIVER_SLUG_AGY];

    pub fn new(grok: u8, claude: u8, codex: u8, agy: u8) -> Self {
        Self {
            claude,
            codex,
            grok,
            agy,
        }
    }

    /// Sum of the four shares, in `u16` so a bad split (e.g. `100/100/100/100`)
    /// is reported at its real total instead of wrapping a `u8`.
    pub fn total(&self) -> u16 {
        u16::from(self.grok) + u16::from(self.claude) + u16::from(self.codex) + u16::from(self.agy)
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
                agy: self.agy,
                total,
            })
        }
    }

    /// Which driver owns `bucket`, where `bucket` is a stable hash of a work
    /// item's id in `[0, 100)`.
    ///
    /// The line is laid out `codex | claude | grok | agy`, low to high:
    ///
    /// - `[0, codex)` → `codex`
    /// - `[codex, codex + claude)` → `claude`
    /// - `[codex + claude, 100 - agy)` → `grok`
    /// - `[100 - agy, 100)` → `agy`
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
        } else if u16::from(bucket) < u16::from(self.codex) + u16::from(self.claude) + u16::from(self.grok) {
            DRIVER_SLUG_GROK
        } else {
            DRIVER_SLUG_AGY
        }
    }

    /// The share currently allocated to `driver`, or `None` for a slug this
    /// split does not cover.
    pub fn share_for(&self, driver: &str) -> Option<u8> {
        match driver {
            DRIVER_SLUG_CODEX => Some(self.codex),
            DRIVER_SLUG_CLAUDE => Some(self.claude),
            DRIVER_SLUG_GROK => Some(self.grok),
            DRIVER_SLUG_AGY => Some(self.agy),
            _ => None,
        }
    }

    /// Which eligible driver owns `bucket` once this split is renormalised
    /// over `eligible`.
    ///
    /// **The rule: proportional, by cumulative flooring.** Each eligible
    /// driver keeps its configured share *relative to the other eligible
    /// ones* — the operator's proportions are preserved as far as they can
    /// be inside the subset, so restricting `20/50/30` to `{codex, claude}`
    /// yields `28/72`, not `50/50` and not `20/50` with 30 points stranded.
    /// The arithmetic is integer-only: the boundary after the *i*th eligible
    /// driver is `floor(cumulative_i * 100 / eligible_total)`, and each
    /// driver's share is the difference between consecutive boundaries (see
    /// [`Self::boundaries_over`]). That gives four properties this must have:
    ///
    /// - **Exact partition.** Consecutive differences of a monotone sequence
    ///   ending at exactly 100 leave no gap and no overlap, so every bucket
    ///   in `[0, 100)` still lands on exactly one driver — no rounding
    ///   fudge, no leftover point handed to an arbitrary driver.
    /// - **Zero stays zero.** A 0 share contributes nothing to the running
    ///   total, so its two boundaries coincide and its interval is empty.
    ///   Renormalisation can never resurrect a driver the operator switched
    ///   off, which is the whole reason it is not "spread the shortfall
    ///   evenly".
    /// - **Bounded distortion.** Cumulative flooring gives each driver either
    ///   the floor or the ceiling of its exact proportional share, never
    ///   further off; the sub-point residue lands on whichever eligible
    ///   driver the flooring happens to favour, deterministically.
    /// - **Determinism.** A pure function of `self` and `eligible`: same
    ///   split, same eligible set, same answer, every time.
    ///
    /// This is the allocation primitive: `bucket` is a stable hash of a work
    /// item's id in `[0, 100)`, and the answer is the driver that item goes
    /// to. It is a strict generalisation of [`Self::driver_for_bucket`] —
    /// with every driver eligible the two agree bucket-for-bucket, because
    /// the eligible total is then 100 and every boundary is already integral.
    ///
    /// A driver **outside** `eligible` can never be returned. That is the
    /// hard invariant: allocating a row to a driver that cannot satisfy its
    /// kind's contract strands the row, so the restriction is applied to the
    /// bucket line itself rather than checked afterwards.
    pub fn driver_for_bucket_among(
        &self,
        bucket: u8,
        eligible: &[&str],
    ) -> Result<&'static str, EligibleAllocationError> {
        for (driver, end) in self.boundaries_over(eligible)? {
            if bucket < end {
                return Ok(driver);
            }
        }
        // Unreachable: `boundaries_over` ends at exactly 100 and `bucket` is
        // in `[0, 100)`. Expressed as an error rather than a panic or a
        // silent default so a future change to the boundary arithmetic that
        // broke the partition would surface as a loud dispatch failure.
        Err(EligibleAllocationError::BucketOutOfRange { bucket })
    }

    /// Half-open interval ends (exclusive) for each eligible driver, in
    /// bucket-line order, ending at exactly 100.
    ///
    /// Unrecognised slugs in `eligible` are ignored (this split covers
    /// exactly four drivers); duplicates are collapsed, and the order of
    /// `eligible` does not matter — the result is always in
    /// [`Self::DRIVERS_IN_BUCKET_ORDER`].
    ///
    /// Errors rather than repairing when the restriction leaves nothing to
    /// allocate: see [`EligibleAllocationError`].
    fn boundaries_over(&self, eligible: &[&str]) -> Result<Vec<(&'static str, u8)>, EligibleAllocationError> {
        let kept: Vec<(&'static str, u8)> = Self::DRIVERS_IN_BUCKET_ORDER
            .into_iter()
            .filter(|driver| eligible.contains(driver))
            .map(|driver| {
                let share = self
                    .share_for(driver)
                    .expect("DRIVERS_IN_BUCKET_ORDER names exactly the drivers share_for covers");
                (driver, share)
            })
            .collect();
        if kept.is_empty() {
            return Err(EligibleAllocationError::NoEligibleDriver);
        }
        let total: u32 = kept.iter().map(|(_, share)| u32::from(*share)).sum();
        if total == 0 {
            return Err(EligibleAllocationError::EveryEligibleDriverAtZero {
                eligible: kept.iter().map(|(driver, _)| *driver).collect(),
                split: *self,
            });
        }
        let mut cumulative = 0u32;
        Ok(kept
            .into_iter()
            .map(|(driver, share)| {
                cumulative += u32::from(share);
                // `cumulative <= total`, so this is in `[0, 100]`, and the
                // last entry is exactly 100.
                (driver, (cumulative * 100 / total) as u8)
            })
            .collect())
    }
}

/// Why a [`DriverTrafficSplit`] could not allocate a work item restricted to
/// a set of eligible drivers.
///
/// Both real variants are deliberately errors rather than a fallback to the
/// engine default driver. Allocation exists to route work to a driver that
/// can actually run it; when nothing eligible has any share, quietly routing
/// to the default would either strand the row on a driver its kind refuses
/// or silently overrule an operator who set that driver's share to 0. Zero
/// means zero, so the honest outcome is a loud failure the operator can fix
/// by changing the split (or by declaring the driver eligible for the kind).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EligibleAllocationError {
    #[error(
        "no driver is eligible for this work item: the eligible set is empty (or names only drivers \
         the traffic split does not cover), so there is nothing to allocate to"
    )]
    NoEligibleDriver,
    #[error(
        "every driver eligible for this work item holds a 0 share ({}): the split is \
         grok={}, claude={}, codex={}, agy={} — raise one of the eligible drivers' shares, or make an \
         already-funded driver eligible for this kind",
        .eligible.join(", "),
        .split.grok,
        .split.claude,
        .split.codex,
        .split.agy,
    )]
    EveryEligibleDriverAtZero {
        eligible: Vec<&'static str>,
        split: DriverTrafficSplit,
    },
    #[error("bucket {bucket} is outside the [0, 100) bucket line")]
    BucketOutOfRange { bucket: u8 },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(driver, share)` pairs, derived from `boundaries_over`'s cumulative
    /// ends via consecutive differences. Test-only: nothing in production
    /// needs the per-driver share rather than the raw boundary line, so
    /// there is no `pub` method for this on [`DriverTrafficSplit`] itself.
    fn shares_over(
        split: &DriverTrafficSplit,
        eligible: &[&str],
    ) -> Result<Vec<(&'static str, u8)>, EligibleAllocationError> {
        let boundaries = split.boundaries_over(eligible)?;
        let mut shares = Vec::with_capacity(boundaries.len());
        let mut prev = 0u8;
        for (driver, end) in boundaries {
            shares.push((driver, end - prev));
            prev = end;
        }
        Ok(shares)
    }

    #[test]
    fn default_is_all_claude() {
        let split = DriverTrafficSplit::default();
        assert_eq!(split, DriverTrafficSplit::new(0, 100, 0, 0));
        assert_eq!(split.agy, 0, "Antigravity must receive no traffic by default");
        split.validate().unwrap();
    }

    #[test]
    fn validate_rejects_sums_other_than_one_hundred() {
        for split in [
            DriverTrafficSplit::new(0, 0, 0, 0),
            DriverTrafficSplit::new(50, 50, 50, 0),
            DriverTrafficSplit::new(1, 1, 1, 0),
            DriverTrafficSplit::new(0, 99, 0, 0),
            DriverTrafficSplit::new(0, 101, 0, 0),
            DriverTrafficSplit::new(0, 100, 0, 1),
            DriverTrafficSplit::new(0, 0, 0, 99),
        ] {
            assert!(split.validate().is_err(), "{split:?} must be rejected");
        }
    }

    #[test]
    fn validate_accepts_up_to_three_shares_at_zero() {
        for split in [
            DriverTrafficSplit::new(100, 0, 0, 0),
            DriverTrafficSplit::new(0, 100, 0, 0),
            DriverTrafficSplit::new(0, 0, 100, 0),
            DriverTrafficSplit::new(50, 50, 0, 0),
            DriverTrafficSplit::new(50, 0, 50, 0),
            DriverTrafficSplit::new(0, 50, 50, 0),
            DriverTrafficSplit::new(33, 33, 34, 0),
            DriverTrafficSplit::new(0, 0, 0, 100),
            DriverTrafficSplit::new(25, 25, 25, 25),
        ] {
            split.validate().expect("valid split rejected");
        }
    }

    #[test]
    fn all_zero_error_names_the_real_total() {
        let err = DriverTrafficSplit::new(0, 0, 0, 0).validate().unwrap_err();
        assert_eq!(
            err,
            DriverTrafficSplitError::NotOneHundred {
                grok: 0,
                claude: 0,
                codex: 0,
                agy: 0,
                total: 0,
            }
        );
    }

    /// The error reports the true total rather than a wrapped `u8`.
    #[test]
    fn oversized_split_reports_unwrapped_total() {
        let err = DriverTrafficSplit::new(255, 255, 255, 255).validate().unwrap_err();
        assert!(err.to_string().contains("agy=255"));
        let DriverTrafficSplitError::NotOneHundred { total, .. } = err;
        assert_eq!(total, 1020);
    }

    /// Every bucket in `[0, 100)` lands on exactly the share each driver was
    /// given — no rounding, no gap, no overlap.
    #[test]
    fn buckets_partition_exactly_by_share() {
        for split in [
            DriverTrafficSplit::new(0, 100, 0, 0),
            DriverTrafficSplit::new(10, 60, 30, 0),
            DriverTrafficSplit::new(0, 0, 100, 0),
            DriverTrafficSplit::new(100, 0, 0, 0),
            DriverTrafficSplit::new(1, 98, 1, 0),
            DriverTrafficSplit::new(33, 33, 34, 0),
            DriverTrafficSplit::new(0, 0, 0, 100),
            DriverTrafficSplit::new(10, 20, 30, 40),
            DriverTrafficSplit::new(0, 0, 99, 1),
            DriverTrafficSplit::new(0, 0, 1, 99),
        ] {
            let mut counts = (0u8, 0u8, 0u8, 0u8);
            for bucket in 0..100u8 {
                match split.driver_for_bucket(bucket) {
                    DRIVER_SLUG_GROK => counts.0 += 1,
                    DRIVER_SLUG_CLAUDE => counts.1 += 1,
                    DRIVER_SLUG_CODEX => counts.2 += 1,
                    DRIVER_SLUG_AGY => counts.3 += 1,
                    other => panic!("unexpected driver {other}"),
                }
            }
            assert_eq!(counts, (split.grok, split.claude, split.codex, split.agy), "{split:?}");
        }
    }

    /// A driver at 0 must be unreachable across the whole bucket space, not
    /// merely unlikely.
    #[test]
    fn zero_share_is_unreachable() {
        let split = DriverTrafficSplit::new(0, 40, 60, 0);
        for bucket in 0..100u8 {
            assert_ne!(split.driver_for_bucket(bucket), DRIVER_SLUG_GROK);
        }
        let split = DriverTrafficSplit::new(60, 40, 0, 0);
        for bucket in 0..100u8 {
            assert_ne!(split.driver_for_bucket(bucket), DRIVER_SLUG_CODEX);
        }
        let split = DriverTrafficSplit::new(60, 0, 40, 0);
        for bucket in 0..100u8 {
            assert_ne!(split.driver_for_bucket(bucket), DRIVER_SLUG_CLAUDE);
        }
        let split = DriverTrafficSplit::new(20, 40, 40, 0);
        for bucket in 0..100u8 {
            assert_ne!(split.driver_for_bucket(bucket), DRIVER_SLUG_AGY);
        }
    }

    /// Raising `grok` at `claude`'s expense must not disturb any row already
    /// allocated to `codex` — the anchoring property the bucket order buys.
    #[test]
    fn moving_the_claude_grok_boundary_leaves_codex_rows_alone() {
        let before = DriverTrafficSplit::new(0, 70, 30, 0);
        let after = DriverTrafficSplit::new(20, 50, 30, 0);
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
            let split = DriverTrafficSplit::new(0, 100 - percentage, percentage, 0);
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

    /// With every driver eligible, restricting changes nothing: the
    /// renormalised line is the configured line, bucket for bucket. Proves
    /// `driver_for_bucket_among` is a strict generalisation rather than a
    /// second, subtly different allocator.
    #[test]
    fn full_eligibility_reproduces_the_unrestricted_line_exactly() {
        let all = DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER;
        for split in [
            DriverTrafficSplit::new(0, 100, 0, 0),
            DriverTrafficSplit::new(10, 60, 30, 0),
            DriverTrafficSplit::new(33, 33, 34, 0),
            DriverTrafficSplit::new(1, 98, 1, 0),
            DriverTrafficSplit::new(0, 0, 100, 0),
            DriverTrafficSplit::new(10, 20, 30, 40),
            DriverTrafficSplit::new(0, 0, 0, 100),
        ] {
            for bucket in 0..100u8 {
                assert_eq!(
                    split.driver_for_bucket_among(bucket, &all).unwrap(),
                    split.driver_for_bucket(bucket),
                    "{split:?} bucket {bucket}",
                );
            }
            assert_eq!(
                shares_over(&split, &all).unwrap(),
                vec![
                    (DRIVER_SLUG_CODEX, split.codex),
                    (DRIVER_SLUG_CLAUDE, split.claude),
                    (DRIVER_SLUG_GROK, split.grok),
                    (DRIVER_SLUG_AGY, split.agy),
                ],
            );
        }
    }

    /// The operator's proportions survive the restriction: dropping a driver
    /// redistributes its share between the survivors *in proportion to what
    /// they already had*, not evenly.
    #[test]
    fn renormalising_preserves_the_configured_proportions() {
        let split = DriverTrafficSplit::new(30, 50, 20, 0);
        // {codex, claude} keep 20:50 → 28.57 : 71.43 → 28/72 by cumulative
        // flooring (codex's boundary floors to 28, claude takes the rest).
        assert_eq!(
            shares_over(&split, &[DRIVER_SLUG_CLAUDE, DRIVER_SLUG_CODEX]).unwrap(),
            vec![(DRIVER_SLUG_CODEX, 28), (DRIVER_SLUG_CLAUDE, 72)],
        );
        // claude:grok of 50:30 renormalises to 62/38 (floor(50 * 100 / 80) = 62).
        assert_eq!(
            shares_over(&split, &[DRIVER_SLUG_CLAUDE, DRIVER_SLUG_GROK]).unwrap(),
            vec![(DRIVER_SLUG_CLAUDE, 62), (DRIVER_SLUG_GROK, 38)],
        );
        // A single eligible driver takes everything it is allowed to take.
        assert_eq!(
            shares_over(&split, &[DRIVER_SLUG_GROK]).unwrap(),
            vec![(DRIVER_SLUG_GROK, 100)],
        );
    }

    /// The renormalised line is still an exact partition of `[0, 100)`: the
    /// per-driver bucket counts equal the renormalised shares, which sum to
    /// 100. No gap, no overlap, no bucket unaccounted for.
    #[test]
    fn renormalised_buckets_partition_exactly() {
        let subsets: [&[&str]; 6] = [
            &[DRIVER_SLUG_CODEX, DRIVER_SLUG_CLAUDE],
            &[DRIVER_SLUG_CLAUDE, DRIVER_SLUG_GROK],
            &[DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK],
            &[DRIVER_SLUG_CODEX],
            &[DRIVER_SLUG_CLAUDE],
            &[DRIVER_SLUG_CODEX, DRIVER_SLUG_CLAUDE, DRIVER_SLUG_GROK],
        ];
        for split in [
            DriverTrafficSplit::new(33, 33, 34, 0),
            DriverTrafficSplit::new(10, 60, 30, 0),
            DriverTrafficSplit::new(1, 98, 1, 0),
            DriverTrafficSplit::new(0, 50, 50, 0),
        ] {
            for eligible in subsets {
                let Ok(shares) = shares_over(&split, eligible) else {
                    continue; // covered by the every-eligible-driver-at-zero test
                };
                assert_eq!(
                    shares.iter().map(|(_, s)| u16::from(*s)).sum::<u16>(),
                    100,
                    "{split:?} over {eligible:?}",
                );
                for (driver, share) in shares {
                    let count = (0..100u8)
                        .filter(|b| split.driver_for_bucket_among(*b, eligible).unwrap() == driver)
                        .count();
                    assert_eq!(
                        count,
                        usize::from(share),
                        "{split:?} over {eligible:?}, driver {driver}"
                    );
                }
            }
        }
    }

    /// The hard invariant: a driver outside the eligible set is never
    /// returned, for any split and any bucket.
    #[test]
    fn an_ineligible_driver_is_never_allocated() {
        let subsets: [&[&str]; 3] = [
            &[DRIVER_SLUG_CODEX, DRIVER_SLUG_CLAUDE],
            &[DRIVER_SLUG_CLAUDE],
            &[DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK],
        ];
        for split in [
            DriverTrafficSplit::new(0, 100, 0, 0),
            DriverTrafficSplit::new(80, 10, 10, 0),
            DriverTrafficSplit::new(33, 33, 34, 0),
            DriverTrafficSplit::new(100, 0, 0, 0),
        ] {
            for eligible in subsets {
                for bucket in 0..100u8 {
                    match split.driver_for_bucket_among(bucket, eligible) {
                        Ok(driver) => assert!(
                            eligible.contains(&driver),
                            "{split:?} allocated ineligible {driver} (eligible {eligible:?}) at bucket {bucket}",
                        ),
                        // Only ever the all-eligible-at-zero case; never a
                        // silent fallback to an ineligible driver.
                        Err(err) => assert!(
                            matches!(err, EligibleAllocationError::EveryEligibleDriverAtZero { .. }),
                            "unexpected {err:?}",
                        ),
                    }
                }
            }
        }
    }

    /// Zero survives renormalisation: an eligible driver at 0 stays at 0
    /// rather than being handed a slice of the ineligible driver's share.
    #[test]
    fn a_zero_share_stays_zero_after_renormalising() {
        let split = DriverTrafficSplit::new(0, 40, 60, 0);
        let eligible = [DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK];
        assert_eq!(
            shares_over(&split, &eligible).unwrap(),
            vec![(DRIVER_SLUG_CODEX, 100), (DRIVER_SLUG_GROK, 0)],
        );
        for bucket in 0..100u8 {
            assert_eq!(
                split.driver_for_bucket_among(bucket, &eligible).unwrap(),
                DRIVER_SLUG_CODEX
            );
        }
    }

    /// An empty eligible set — or one naming only drivers this split does
    /// not cover — is an error, never a fallback.
    #[test]
    fn an_empty_eligible_set_is_an_error() {
        let split = DriverTrafficSplit::new(10, 60, 30, 0);
        assert_eq!(
            shares_over(&split, &[]).unwrap_err(),
            EligibleAllocationError::NoEligibleDriver,
        );
        assert_eq!(
            split.driver_for_bucket_among(0, &["copilot"]).unwrap_err(),
            EligibleAllocationError::NoEligibleDriver,
        );
    }

    /// Every eligible driver at 0 is an error too: the operator said "send
    /// nothing there", and allocation honours that rather than overruling it
    /// with a default. The message names both the eligible set and the split
    /// so the operator can see which of the two to change.
    #[test]
    fn every_eligible_driver_at_zero_is_an_error_naming_both_sides() {
        let split = DriverTrafficSplit::new(0, 100, 0, 0);
        let err = split
            .driver_for_bucket_among(7, &[DRIVER_SLUG_GROK, DRIVER_SLUG_CODEX])
            .unwrap_err();
        assert_eq!(
            err,
            EligibleAllocationError::EveryEligibleDriverAtZero {
                eligible: vec![DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK],
                split,
            },
        );
        let text = err.to_string();
        assert!(text.contains("codex, grok"), "{text}");
        assert!(text.contains("claude=100"), "{text}");
    }

    /// The eligible set is a set: order and duplicates do not change the
    /// answer, so two call sites that compute it differently cannot disagree.
    #[test]
    fn eligible_set_order_and_duplicates_do_not_matter() {
        let split = DriverTrafficSplit::new(20, 50, 30, 0);
        let canonical: Vec<_> = (0..100u8)
            .map(|b| {
                split
                    .driver_for_bucket_among(b, &[DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK])
                    .unwrap()
            })
            .collect();
        for eligible in [
            vec![DRIVER_SLUG_GROK, DRIVER_SLUG_CODEX],
            vec![DRIVER_SLUG_GROK, DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK],
        ] {
            let got: Vec<_> = (0..100u8)
                .map(|b| split.driver_for_bucket_among(b, &eligible).unwrap())
                .collect();
            assert_eq!(got, canonical, "{eligible:?}");
        }
    }

    #[test]
    fn share_for_covers_exactly_the_four_drivers() {
        let split = DriverTrafficSplit::new(10, 40, 30, 20);
        assert_eq!(split.share_for(DRIVER_SLUG_GROK), Some(10));
        assert_eq!(split.share_for(DRIVER_SLUG_CLAUDE), Some(40));
        assert_eq!(split.share_for(DRIVER_SLUG_CODEX), Some(30));
        assert_eq!(split.share_for(DRIVER_SLUG_AGY), Some(20));
        assert_eq!(split.share_for("copilot"), None);
    }

    #[test]
    fn json_round_trips_and_requires_every_share() {
        let split = DriverTrafficSplit::new(10, 20, 30, 40);
        let json = serde_json::to_string(&split).unwrap();
        assert_eq!(serde_json::from_str::<DriverTrafficSplit>(&json).unwrap(), split);
        // Every share is required on the wire; only the DB migration fills
        // in the fourth share for previously persisted three-way splits.
        for driver in DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER {
            let mut value = serde_json::to_value(split).unwrap();
            value.as_object_mut().unwrap().remove(driver);
            assert!(serde_json::from_value::<DriverTrafficSplit>(value).is_err(), "{driver}");
        }
    }

    #[test]
    fn four_way_renormalisation_covers_every_eligible_subset() {
        let drivers = DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER;
        for split in [
            DriverTrafficSplit::new(10, 20, 30, 40),
            DriverTrafficSplit::new(0, 33, 34, 33),
            DriverTrafficSplit::default(),
            DriverTrafficSplit::new(0, 0, 0, 100),
        ] {
            for mask in 1..16 {
                let eligible: Vec<_> = drivers
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, driver)| *driver)
                    .collect();
                let total: u16 = eligible
                    .iter()
                    .map(|driver| u16::from(split.share_for(driver).unwrap()))
                    .sum();
                let mut counts = [0u16; 4];
                for bucket in 0..100 {
                    let result = split.driver_for_bucket_among(bucket, &eligible);
                    if total == 0 {
                        assert!(matches!(
                            result,
                            Err(EligibleAllocationError::EveryEligibleDriverAtZero { .. })
                        ));
                    } else {
                        let driver = result.unwrap();
                        assert!(eligible.contains(&driver));
                        counts[drivers.iter().position(|d| *d == driver).unwrap()] += 1;
                    }
                }
                if total == 0 {
                    continue;
                }
                assert_eq!(counts.iter().sum::<u16>(), 100);
                for (i, driver) in drivers.iter().enumerate() {
                    let share = if eligible.contains(driver) {
                        u16::from(split.share_for(driver).unwrap())
                    } else {
                        0
                    };
                    let exact = share * 100;
                    assert!(
                        counts[i] == exact / total || counts[i] == exact.div_ceil(total),
                        "{split:?} {eligible:?} {driver}"
                    );
                    if share == 0 {
                        assert_eq!(counts[i], 0);
                    }
                }
            }
        }
        let split = DriverTrafficSplit::new(10, 20, 30, 40);
        assert_eq!(
            shares_over(&split, &[DRIVER_SLUG_CODEX, DRIVER_SLUG_AGY]).unwrap(),
            vec![(DRIVER_SLUG_CODEX, 42), (DRIVER_SLUG_AGY, 58)]
        );
    }
}
