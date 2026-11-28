//! Experiment registry: the one way to start, split and stop a token-spend
//! experiment.
//!
//! An experiment is one [`ExperimentSpec`] in [`EXPERIMENTS`] plus flags in
//! the ordinary [`crate::REGISTRY`], category [`EXPERIMENT_FLAG_CATEGORY`]:
//!
//! | Flag                | Default | Effect                                                                                     |
//! | ------------------- | ------- | ------------------------------------------------------------------------------------------ |
//! | `<key>`             | off     | The experiment's own switch. Off: every family gets today's behaviour and nothing is recorded. |
//! | `<key>_ab_split`    | on      | On: half of PR families are treated. Off: every family is treated.                          |
//! | `token_experiments` | on      | Master switch. Off: every experiment resolves to off at once.                               |
//!
//! This is the just-in-time conflict-resolution pair
//! ([`crate::ConflictDeferralArm`]) generalised, with a master switch of the
//! kind `worker_proposals` already is for the proposal seams.
//!
//! The functions here are pure: they take flag values and a bucket and return
//! an answer. The bucket is a hash of the PR family and the experiment key,
//! computed by the work database (`boss_work_db::experiment_bucket`), so this
//! crate gains no dependency and an execution's arm can be recomputed from its
//! family id alone.
//!
//! What an execution actually launched under is the arm set recorded in its
//! launch record, not what the flags say now. [`ExperimentArmSet`] is that
//! record's canonical text encoding.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::FeatureFlagsStore;

/// Master switch for every experiment. Off resolves every experiment to
/// off at once; the experiments' own flags are left as they were.
pub const TOKEN_EXPERIMENTS_FLAG: &str = "token_experiments";

/// Debug-pane category of every experiment flag, including the master switch.
pub const EXPERIMENT_FLAG_CATEGORY: &str = "experiments";

/// Families whose bucket is below this limit are treated, the rest are
/// control. Fixed at half: the variance of a difference between two arms is
/// smallest at an equal split, and a changeable share would have to be
/// recorded with every arm.
pub const TREATMENT_BUCKET_LIMIT: u8 = 50;

/// Key of the first experiment: tell Standard workers to issue independent
/// searches and file reads together in one turn.
pub const BATCHED_SEARCH_EXPERIMENT: &str = "batched_search";

/// Splits [`BATCHED_SEARCH_EXPERIMENT`] between half of families and all.
pub const BATCHED_SEARCH_AB_SPLIT_FLAG: &str = "batched_search_ab_split";

/// Key of the Claude auto-compact experiment: a 200k window and compact
/// instructions in the worker rules. No lane; it may overlap any other
/// experiment.
pub const WORKER_AUTOCOMPACT_EXPERIMENT: &str = "worker_autocompact";

/// Splits [`WORKER_AUTOCOMPACT_EXPERIMENT`] between half of families and all.
pub const WORKER_AUTOCOMPACT_AB_SPLIT_FLAG: &str = "worker_autocompact_ab_split";

/// Key of the delta re-review experiment: from the second pre-merge review
/// round on, run one Light-tier reviewer with no supervisor and a delta brief.
pub const DELTA_REREVIEW_EXPERIMENT: &str = "delta_rereview";

/// Splits [`DELTA_REREVIEW_EXPERIMENT`] between half of families and all.
pub const DELTA_REREVIEW_AB_SPLIT_FLAG: &str = "delta_rereview_ab_split";
/// A surface on which at most one experiment may be assigned at a time.
///
/// Two experiments that rewrite the same instructions, or reshape the same
/// review, cannot be told apart by any amount of careful assignment, because
/// the treated run received both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExperimentLane {
    /// The rules and brief text a Standard worker is given.
    WorkerPrompt,
    /// Which pre-merge reviews run, their shape, and what they mint.
    PreMergeReview,
    /// Whether a post-merge review runs, its shape, and what it mints.
    PostMergeReview,
    /// How the coordinator's context is bounded.
    CoordinatorContext,
}

impl ExperimentLane {
    /// Stable name for reports and the operator doc.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkerPrompt => "worker_prompt",
            Self::PreMergeReview => "pre_merge_review",
            Self::PostMergeReview => "post_merge_review",
            Self::CoordinatorContext => "coordinator_context",
        }
    }
}

/// One experiment's registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExperimentSpec {
    /// Stable key. Names one treatment for the life of the key: changing a
    /// threshold is a new key, and a key is never reused once adopted or
    /// dropped. Also the name of the experiment's own flag.
    pub key: &'static str,
    /// The lane the experiment holds while it runs, or `None` when it may
    /// overlap anything (a launch flag, a tool guard, a model choice for one
    /// role). Independent assignment keeps such experiments apart instead.
    pub lane: Option<ExperimentLane>,
    /// Flags that must be on for the experiment to run, for example
    /// `review_batch_fanout` for an experiment on batched reviews. With one
    /// off the experiment resolves to off and records nothing.
    pub requires: &'static [&'static str],
    /// Whether the experiment has a `<key>_ab_split` flag. When `false` it
    /// treats every family while it is on, with no control arm. For singletons
    /// such as the coordinator, which are compared week against week.
    pub splits: bool,
}

/// Name of the flag that switches `key` between half of families and all.
pub fn ab_split_flag_name(key: &str) -> String {
    format!("{key}_ab_split")
}

/// Every experiment. Append in the order the experiments should win a lane:
/// when two experiments in one lane are switched on, the one registered first
/// runs and the other is held.
pub const EXPERIMENTS: &[ExperimentSpec] = &[
    ExperimentSpec {
        key: BATCHED_SEARCH_EXPERIMENT,
        lane: Some(ExperimentLane::WorkerPrompt),
        requires: &[],
        splits: true,
    },
    ExperimentSpec {
        key: WORKER_AUTOCOMPACT_EXPERIMENT,
        lane: None,
        requires: &[],
        splits: true,
    },
    ExperimentSpec {
        key: DELTA_REREVIEW_EXPERIMENT,
        lane: Some(ExperimentLane::PreMergeReview),
        requires: &["review_batch_fanout"],
        splits: true,
    },
];

/// What the flags say an experiment is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperimentState {
    /// Not running: its own flag, the master switch or a required flag is
    /// off, or the key is not registered.
    Off,
    /// Switched on, but another experiment in the same lane is also switched
    /// on and was registered first. Resolves to off.
    Held {
        /// The experiment that holds the lane.
        holder: &'static str,
    },
    /// Running on half of PR families; the rest are control.
    Split,
    /// Running on every PR family.
    All,
}

impl ExperimentState {
    /// Whether launches are assigned an arm.
    pub fn is_running(self) -> bool {
        matches!(self, Self::Split | Self::All)
    }
}

/// One arm of one experiment for one PR family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperimentArm {
    /// Today's behaviour, recorded as explicitly as treatment.
    Control,
    /// The change under test.
    Treatment,
    /// The experiment was running at this execution's first launch and had
    /// been turned off by a later run's. Such executions, and their families,
    /// are left out of every comparison.
    Stopped,
}

impl ExperimentArm {
    /// Stable value in the launch record.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Treatment => "treatment",
            Self::Stopped => "stopped",
        }
    }

    /// Inverse of [`Self::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "control" => Some(Self::Control),
            "treatment" => Some(Self::Treatment),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

fn candidate(spec: &ExperimentSpec, flag: &dyn Fn(&str) -> bool) -> bool {
    flag(TOKEN_EXPERIMENTS_FLAG) && flag(spec.key) && spec.requires.iter().all(|required| flag(required))
}

/// Resolve what `key` is doing from flag values, in `specs`.
///
/// `flag` answers whether a registered flag is on. An unregistered key is
/// [`ExperimentState::Off`]. The lane rule is enforced here: only experiments
/// that would otherwise run compete for a lane, so one held back by a missing
/// required flag does not keep another out of it.
pub fn experiment_state(specs: &[ExperimentSpec], key: &str, flag: &dyn Fn(&str) -> bool) -> ExperimentState {
    let Some(spec) = specs.iter().find(|spec| spec.key == key) else {
        return ExperimentState::Off;
    };
    if !candidate(spec, flag) {
        return ExperimentState::Off;
    }
    if let Some(lane) = spec.lane
        && let Some(holder) = specs
            .iter()
            .find(|other| other.lane == Some(lane) && candidate(other, flag))
        && holder.key != spec.key
    {
        return ExperimentState::Held { holder: holder.key };
    }
    if spec.splits && flag(&ab_split_flag_name(spec.key)) {
        ExperimentState::Split
    } else {
        ExperimentState::All
    }
}

/// The arm a family with `bucket` (in `[0, 100)`) is assigned in an
/// experiment in `state`, or `None` when it is not running.
pub fn assign_arm(state: ExperimentState, bucket: u8) -> Option<ExperimentArm> {
    match state {
        ExperimentState::Off | ExperimentState::Held { .. } => None,
        ExperimentState::All => Some(ExperimentArm::Treatment),
        ExperimentState::Split if bucket < TREATMENT_BUCKET_LIMIT => Some(ExperimentArm::Treatment),
        ExperimentState::Split => Some(ExperimentArm::Control),
    }
}

/// The arm set a family launches under: an entry for every running experiment
/// in `specs` and none for the rest.
///
/// `bucket_for_key` returns the family's bucket for one experiment key. Two
/// keys hash differently, so two experiments' assignments are independent of
/// each other.
pub fn resolve_arms(
    specs: &[ExperimentSpec],
    flag: &dyn Fn(&str) -> bool,
    bucket_for_key: &dyn Fn(&str) -> u8,
) -> ExperimentArmSet {
    specs
        .iter()
        .filter_map(|spec| {
            let state = experiment_state(specs, spec.key, flag);
            let bucket = if state.is_running() {
                bucket_for_key(spec.key)
            } else {
                0
            };
            assign_arm(state, bucket).map(|arm| (spec.key, arm))
        })
        .collect()
}

/// The arm of each experiment that was running for one execution's first
/// launch. Stored as one text value in the launch record.
///
/// A missing key means the experiment was not running for that launch, never
/// that the execution was control. An empty set means "recorded, and nothing
/// was running".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExperimentArmSet(BTreeMap<String, ExperimentArm>);

impl ExperimentArmSet {
    /// An empty set: recorded, and nothing was running.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `key`'s arm, replacing an earlier one.
    ///
    /// # Panics
    ///
    /// When `key` is not a valid experiment key (see [`is_valid_key`]). Keys
    /// are constants in [`EXPERIMENTS`], so an invalid one is a programming
    /// error; the restriction is what lets [`Self::encode`] write JSON without
    /// escaping.
    pub fn insert(&mut self, key: impl Into<String>, arm: ExperimentArm) {
        let key = key.into();
        assert!(is_valid_key(&key), "invalid experiment key {key:?}");
        self.0.insert(key, arm);
    }

    /// `key`'s arm, or `None` when it was not running.
    pub fn get(&self, key: &str) -> Option<ExperimentArm> {
        self.0.get(key).copied()
    }

    /// Remove `key` from the arm set, returning its prior arm if present.
    pub fn remove(&mut self, key: &str) -> Option<ExperimentArm> {
        self.0.remove(key)
    }

    /// Whether the execution runs `key`'s treatment. A stopped experiment is
    /// not treated: stopping withdraws the change for later runs.
    pub fn is_treated(&self, key: &str) -> bool {
        self.get(key) == Some(ExperimentArm::Treatment)
    }

    /// Number of experiments recorded.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every recorded experiment in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, ExperimentArm)> {
        self.0.iter().map(|(key, arm)| (key.as_str(), *arm))
    }

    /// Rewrite every key for which `is_running` is false to
    /// [`ExperimentArm::Stopped`], whichever arm it held.
    ///
    /// Control is rewritten as well as treatment so both arms lose such
    /// executions alike. A key that is already stopped stays stopped, even if
    /// the experiment has been turned back on since: a later start is not
    /// picked up by an execution that has already launched.
    pub fn stop_where_not_running(&mut self, mut is_running: impl FnMut(&str) -> bool) {
        for (key, arm) in &mut self.0 {
            if *arm != ExperimentArm::Stopped && !is_running(key) {
                *arm = ExperimentArm::Stopped;
            }
        }
    }

    /// Canonical text: a JSON object from key to `control`, `treatment` or
    /// `stopped`, keys sorted, no whitespace. `{}` for the empty set.
    pub fn encode(&self) -> String {
        let members: Vec<String> = self
            .0
            .iter()
            .map(|(key, arm)| format!("\"{key}\":\"{}\"", arm.as_str()))
            .collect();
        format!("{{{}}}", members.join(","))
    }

    /// Parse [`Self::encode`]'s text. Whitespace and key order are accepted;
    /// anything that is not an object of valid keys and known arms, each key at
    /// most once, is an error.
    pub fn decode(text: &str) -> Result<Self> {
        let mut rest = text
            .trim_start()
            .strip_prefix('{')
            .ok_or_else(|| anyhow::anyhow!("experiment arm set must be a JSON object, got {text:?}"))?
            .trim_start();
        let mut set = Self::new();
        if let Some(after) = rest.strip_prefix('}') {
            return finish(set, after, text);
        }
        loop {
            let (key, after_key) = take_string(rest, text)?;
            let after_colon = after_key
                .trim_start()
                .strip_prefix(':')
                .ok_or_else(|| anyhow::anyhow!("experiment arm set expects ':' after {key:?} in {text:?}"))?
                .trim_start();
            let (value, after_value) = take_string(after_colon, text)?;
            if !is_valid_key(key) {
                bail!("experiment arm set has invalid key {key:?} in {text:?}");
            }
            let Some(arm) = ExperimentArm::parse(value) else {
                bail!("experiment arm set names unknown arm {value:?} for {key:?}");
            };
            if set.0.insert(key.to_owned(), arm).is_some() {
                bail!("experiment arm set names {key:?} twice in {text:?}");
            }
            let after_value = after_value.trim_start();
            if let Some(after) = after_value.strip_prefix('}') {
                return finish(set, after, text);
            }
            rest = after_value
                .strip_prefix(',')
                .ok_or_else(|| anyhow::anyhow!("experiment arm set expects ',' or '}}' in {text:?}"))?
                .trim_start();
        }
    }
}

/// Whether `key` can name an experiment: non-empty lowercase snake case. The
/// restriction keeps the canonical text free of anything JSON would escape.
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Take a `"..."` string with no escapes off the front of `text`.
fn take_string<'a>(text: &'a str, whole: &str) -> Result<(&'a str, &'a str)> {
    let inner = text
        .strip_prefix('"')
        .ok_or_else(|| anyhow::anyhow!("experiment arm set expects a string in {whole:?}"))?;
    let end = inner
        .find('"')
        .ok_or_else(|| anyhow::anyhow!("experiment arm set has an unterminated string in {whole:?}"))?;
    let (string, after) = inner.split_at(end);
    if string.contains(['\\', '\n']) {
        bail!("experiment arm set has an escape in a string in {whole:?}");
    }
    Ok((string, &after[1..]))
}

fn finish(set: ExperimentArmSet, after_object: &str, whole: &str) -> Result<ExperimentArmSet> {
    if !after_object.trim().is_empty() {
        bail!("experiment arm set has text after the object in {whole:?}");
    }
    Ok(set)
}

impl<K: Into<String>> FromIterator<(K, ExperimentArm)> for ExperimentArmSet {
    fn from_iter<I: IntoIterator<Item = (K, ExperimentArm)>>(iter: I) -> Self {
        let mut set = Self::new();
        for (key, arm) in iter {
            set.insert(key, arm);
        }
        set
    }
}

impl FeatureFlagsStore {
    /// What `key` is doing right now, from the live flag values.
    pub fn experiment_state(&self, key: &str) -> ExperimentState {
        experiment_state(EXPERIMENTS, key, &|name| self.is_enabled(name))
    }

    /// The arm set a family launches under right now. `bucket_for_key` is the
    /// family's bucket for one experiment key.
    pub fn resolve_experiment_arms(&self, bucket_for_key: impl Fn(&str) -> u8) -> ExperimentArmSet {
        resolve_arms(EXPERIMENTS, &|name| self.is_enabled(name), &bucket_for_key)
    }

    /// Rewrite every key of `arms` whose experiment is not running now to
    /// [`ExperimentArm::Stopped`]. See [`ExperimentArmSet::stop_where_not_running`].
    pub fn stop_experiments_not_running(&self, arms: &mut ExperimentArmSet) {
        arms.stop_where_not_running(|key| self.experiment_state(key).is_running());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::REGISTRY;

    const ALPHA: ExperimentSpec = ExperimentSpec {
        key: "alpha",
        lane: Some(ExperimentLane::WorkerPrompt),
        requires: &[],
        splits: true,
    };
    const BETA: ExperimentSpec = ExperimentSpec {
        key: "beta",
        lane: Some(ExperimentLane::WorkerPrompt),
        requires: &[],
        splits: true,
    };
    const GAMMA: ExperimentSpec = ExperimentSpec {
        key: "gamma",
        lane: Some(ExperimentLane::PreMergeReview),
        requires: &["review_batch_fanout"],
        splits: true,
    };
    const DELTA: ExperimentSpec = ExperimentSpec {
        key: "delta",
        lane: None,
        requires: &[],
        splits: false,
    };
    const SPECS: &[ExperimentSpec] = &[ALPHA, BETA, GAMMA, DELTA];

    fn state(on: &[&str], key: &str) -> ExperimentState {
        experiment_state(SPECS, key, &|name| on.contains(&name))
    }

    fn arms(on: &[&str], bucket: u8) -> ExperimentArmSet {
        resolve_arms(SPECS, &|name| on.contains(&name), &|_| bucket)
    }

    #[test]
    fn nothing_runs_until_an_experiment_is_switched_on() {
        // Even with the master switch and every split flag on, as their defaults are.
        let defaults = [
            TOKEN_EXPERIMENTS_FLAG,
            "alpha_ab_split",
            "beta_ab_split",
            "gamma_ab_split",
        ];
        for key in ["alpha", "beta", "gamma", "delta"] {
            assert_eq!(state(&defaults, key), ExperimentState::Off, "{key}");
        }
        assert!(arms(&defaults, 0).is_empty());
        assert_eq!(arms(&defaults, 0).encode(), "{}");
    }

    #[test]
    fn a_key_on_with_the_split_on_runs_on_half_of_families() {
        let on = [TOKEN_EXPERIMENTS_FLAG, "alpha", "alpha_ab_split"];
        assert_eq!(state(&on, "alpha"), ExperimentState::Split);
        for bucket in 0..TREATMENT_BUCKET_LIMIT {
            assert_eq!(
                arms(&on, bucket).get("alpha"),
                Some(ExperimentArm::Treatment),
                "{bucket}"
            );
        }
        for bucket in TREATMENT_BUCKET_LIMIT..100 {
            assert_eq!(arms(&on, bucket).get("alpha"), Some(ExperimentArm::Control), "{bucket}");
        }
    }

    #[test]
    fn the_split_is_exactly_half_of_the_bucket_space() {
        let treated = (0..100u8)
            .filter(|bucket| assign_arm(ExperimentState::Split, *bucket) == Some(ExperimentArm::Treatment))
            .count();
        assert_eq!(treated, 50);
    }

    #[test]
    fn with_the_split_off_every_family_is_treated() {
        let on = [TOKEN_EXPERIMENTS_FLAG, "alpha"];
        assert_eq!(state(&on, "alpha"), ExperimentState::All);
        for bucket in [0, 49, 50, 99] {
            assert_eq!(
                arms(&on, bucket).get("alpha"),
                Some(ExperimentArm::Treatment),
                "{bucket}"
            );
        }
    }

    #[test]
    fn an_experiment_that_does_not_split_treats_every_family_even_with_a_split_flag_on() {
        let on = [TOKEN_EXPERIMENTS_FLAG, "delta", "delta_ab_split"];
        assert_eq!(state(&on, "delta"), ExperimentState::All);
        assert_eq!(arms(&on, 99).get("delta"), Some(ExperimentArm::Treatment));
    }

    #[test]
    fn the_split_flag_alone_changes_nothing() {
        let on = [TOKEN_EXPERIMENTS_FLAG, "alpha_ab_split"];
        assert_eq!(state(&on, "alpha"), ExperimentState::Off);
        assert!(arms(&on, 0).is_empty());
    }

    #[test]
    fn the_master_switch_stops_every_experiment_at_once() {
        let on = ["alpha", "alpha_ab_split", "delta", "review_batch_fanout", "gamma"];
        for key in ["alpha", "gamma", "delta"] {
            assert_eq!(state(&on, key), ExperimentState::Off, "{key}");
        }
        assert!(arms(&on, 0).is_empty());
        let mut with_master = on.to_vec();
        with_master.push(TOKEN_EXPERIMENTS_FLAG);
        assert_eq!(arms(&with_master, 0).len(), 3);
    }

    #[test]
    fn an_experiment_whose_required_flag_is_off_resolves_to_off_and_records_nothing() {
        let without = [TOKEN_EXPERIMENTS_FLAG, "gamma", "gamma_ab_split"];
        assert_eq!(state(&without, "gamma"), ExperimentState::Off);
        assert!(arms(&without, 0).is_empty());
        let with = [TOKEN_EXPERIMENTS_FLAG, "gamma", "gamma_ab_split", "review_batch_fanout"];
        assert_eq!(state(&with, "gamma"), ExperimentState::Split);
        assert_eq!(arms(&with, 0).get("gamma"), Some(ExperimentArm::Treatment));
    }

    #[test]
    fn the_experiment_registered_first_wins_a_lane_and_the_other_is_held() {
        let on = [TOKEN_EXPERIMENTS_FLAG, "alpha", "beta"];
        assert_eq!(state(&on, "alpha"), ExperimentState::All);
        assert_eq!(state(&on, "beta"), ExperimentState::Held { holder: "alpha" });
        assert!(!state(&on, "beta").is_running());
        let resolved = arms(&on, 0);
        assert_eq!(resolved.get("alpha"), Some(ExperimentArm::Treatment));
        assert_eq!(resolved.get("beta"), None, "a held experiment records nothing");
    }

    #[test]
    fn a_lane_frees_up_when_its_holder_is_switched_off() {
        assert_eq!(state(&[TOKEN_EXPERIMENTS_FLAG, "beta"], "beta"), ExperimentState::All);
    }

    #[test]
    fn an_experiment_held_back_by_a_required_flag_does_not_hold_its_lane() {
        // ALPHA and GAMMA are in different lanes, so give BETA a requirement
        // to stand in for "registered first but cannot run".
        const NEEDS_FLAG: ExperimentSpec = ExperimentSpec {
            key: "needs_flag",
            lane: Some(ExperimentLane::WorkerPrompt),
            requires: &["other_flag"],
            splits: false,
        };
        const LATER: ExperimentSpec = ExperimentSpec {
            key: "later",
            lane: Some(ExperimentLane::WorkerPrompt),
            requires: &[],
            splits: false,
        };
        let specs = [NEEDS_FLAG, LATER];
        let on = [TOKEN_EXPERIMENTS_FLAG, "needs_flag", "later"];
        let flag = |name: &str| on.contains(&name);
        assert_eq!(experiment_state(&specs, "later", &flag), ExperimentState::All);
        let with_requirement = |name: &str| on.contains(&name) || name == "other_flag";
        assert_eq!(
            experiment_state(&specs, "later", &with_requirement),
            ExperimentState::Held { holder: "needs_flag" }
        );
    }

    #[test]
    fn experiments_in_different_lanes_and_laneless_ones_overlap() {
        let on = [TOKEN_EXPERIMENTS_FLAG, "alpha", "gamma", "review_batch_fanout", "delta"];
        let resolved = arms(&on, 0);
        assert_eq!(resolved.len(), 3);
        for key in ["alpha", "gamma", "delta"] {
            assert!(resolved.get(key).is_some(), "{key} runs alongside the others");
        }
    }

    #[test]
    fn an_unregistered_key_is_off() {
        assert_eq!(
            state(&[TOKEN_EXPERIMENTS_FLAG, "nonsense"], "nonsense"),
            ExperimentState::Off
        );
    }

    #[test]
    fn assignment_is_a_pure_function_of_state_and_bucket() {
        let on = [TOKEN_EXPERIMENTS_FLAG, "alpha", "alpha_ab_split"];
        for bucket in 0..100 {
            assert_eq!(arms(&on, bucket), arms(&on, bucket));
        }
        // Each key is asked for its own bucket, so assignments are independent.
        let both = [
            TOKEN_EXPERIMENTS_FLAG,
            "alpha",
            "alpha_ab_split",
            "delta",
            "gamma",
            "gamma_ab_split",
            "review_batch_fanout",
        ];
        let resolved = resolve_arms(SPECS, &|name| both.contains(&name), &|key| {
            if key == "alpha" { 10 } else { 90 }
        });
        assert_eq!(resolved.get("alpha"), Some(ExperimentArm::Treatment));
        assert_eq!(resolved.get("gamma"), Some(ExperimentArm::Control));
    }

    #[test]
    fn the_arm_set_encodes_canonically_and_round_trips() {
        let set: ExperimentArmSet = [
            ("zeta", ExperimentArm::Stopped),
            ("alpha", ExperimentArm::Treatment),
            ("beta", ExperimentArm::Control),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            set.encode(),
            r#"{"alpha":"treatment","beta":"control","zeta":"stopped"}"#
        );
        assert_eq!(ExperimentArmSet::decode(&set.encode()).unwrap(), set);
        assert_eq!(ExperimentArmSet::new().encode(), "{}");
        assert_eq!(ExperimentArmSet::decode("{}").unwrap(), ExperimentArmSet::new());
    }

    #[test]
    fn decoding_accepts_whitespace_and_any_key_order() {
        let decoded = ExperimentArmSet::decode(" { \"b\" : \"control\" , \"a\" : \"treatment\" } ").unwrap();
        assert_eq!(decoded.encode(), r#"{"a":"treatment","b":"control"}"#);
    }

    #[test]
    fn decoding_rejects_text_that_is_not_an_arm_set() {
        for bad in [
            "",
            "null",
            "[]",
            r#"["alpha"]"#,
            r#"{"alpha":1}"#,
            r#"{"alpha":"Treatment"}"#,
            r#"{"alpha":"half"}"#,
            r#"{"alpha":"treatment""#,
            r#"{"alpha":"treatment",}"#,
            r#"{"alpha":"treatment" "beta":"control"}"#,
            r#"{"alpha" "treatment"}"#,
            r#"{"alpha":"treatment","alpha":"control"}"#,
            r#"{"Alpha":"treatment"}"#,
            r#"{"":"treatment"}"#,
            r#"{"al\"pha":"treatment"}"#,
            r#"{} {}"#,
            r#"{"alpha":"treatment"} x"#,
            r#"{alpha:"treatment"}"#,
        ] {
            assert!(ExperimentArmSet::decode(bad).is_err(), "{bad:?} must not decode");
        }
    }

    #[test]
    fn keys_are_lowercase_snake_case() {
        for valid in ["batched_search", "a", "r7_step_2"] {
            assert!(is_valid_key(valid), "{valid}");
        }
        for invalid in ["", "Batched", "batched-search", "batched search", "a\"b", "a\\b", "é"] {
            assert!(!is_valid_key(invalid), "{invalid:?}");
        }
        for spec in EXPERIMENTS {
            assert!(is_valid_key(spec.key), "{}", spec.key);
        }
    }

    #[test]
    #[should_panic(expected = "invalid experiment key")]
    fn inserting_a_key_that_would_need_escaping_is_a_programming_error() {
        ExperimentArmSet::new().insert("needs\"escaping", ExperimentArm::Control);
    }

    #[test]
    fn every_arm_round_trips_through_its_stored_value() {
        for arm in [ExperimentArm::Control, ExperimentArm::Treatment, ExperimentArm::Stopped] {
            assert_eq!(ExperimentArm::parse(arm.as_str()), Some(arm));
        }
        assert_eq!(ExperimentArm::parse(""), None);
    }

    #[test]
    fn stopping_rewrites_control_and_treatment_alike_and_keeps_stopped() {
        let mut set: ExperimentArmSet = [
            ("alpha", ExperimentArm::Treatment),
            ("beta", ExperimentArm::Control),
            ("gamma", ExperimentArm::Stopped),
            ("delta", ExperimentArm::Treatment),
        ]
        .into_iter()
        .collect();
        // gamma is running again, but it already stopped for this execution.
        set.stop_where_not_running(|key| matches!(key, "gamma" | "delta"));
        assert_eq!(set.get("alpha"), Some(ExperimentArm::Stopped));
        assert_eq!(set.get("beta"), Some(ExperimentArm::Stopped));
        assert_eq!(set.get("gamma"), Some(ExperimentArm::Stopped));
        assert_eq!(set.get("delta"), Some(ExperimentArm::Treatment));
        assert!(!set.is_treated("alpha"), "a stopped experiment is not treated");
    }

    #[test]
    fn a_stored_key_that_is_no_longer_registered_is_stopped() {
        let mut set: ExperimentArmSet = [("retired_experiment", ExperimentArm::Treatment)].into_iter().collect();
        let tmp = tempfile::TempDir::new().unwrap();
        let store = FeatureFlagsStore::new(tmp.path().join("feature-flags.toml"));
        store.stop_experiments_not_running(&mut set);
        assert_eq!(set.get("retired_experiment"), Some(ExperimentArm::Stopped));
    }

    #[test]
    fn registry_experiments_have_their_flags_registered_with_the_documented_defaults() {
        let flag = |name: &str| {
            REGISTRY
                .iter()
                .find(|spec| spec.name == name)
                .unwrap_or_else(|| panic!("flag {name} must be registered"))
        };
        let master = flag(TOKEN_EXPERIMENTS_FLAG);
        assert!(master.default_enabled, "the master switch defaults on");
        assert_eq!(master.category, EXPERIMENT_FLAG_CATEGORY);

        let mut expected = vec![TOKEN_EXPERIMENTS_FLAG.to_owned()];
        for spec in EXPERIMENTS {
            let own = flag(spec.key);
            assert!(!own.default_enabled, "{} ships inert", spec.key);
            assert_eq!(own.category, EXPERIMENT_FLAG_CATEGORY, "{}", spec.key);
            expected.push(spec.key.to_owned());
            if spec.splits {
                let split_name = ab_split_flag_name(spec.key);
                let split = flag(&split_name);
                assert!(split.default_enabled, "{split_name} defaults on");
                assert_eq!(split.category, EXPERIMENT_FLAG_CATEGORY, "{split_name}");
                expected.push(split_name);
            }
            for required in spec.requires {
                flag(required);
            }
        }
        let mut registered: Vec<String> = REGISTRY
            .iter()
            .filter(|spec| spec.category == EXPERIMENT_FLAG_CATEGORY)
            .map(|spec| spec.name.to_owned())
            .collect();
        registered.sort();
        expected.sort();
        assert_eq!(
            registered, expected,
            "every flag in the experiments category belongs to a registered experiment, and the reverse"
        );
    }

    #[test]
    fn registry_experiment_keys_are_unique() {
        let mut keys: Vec<_> = EXPERIMENTS.iter().map(|spec| spec.key).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before);
    }

    #[test]
    fn batched_search_is_the_first_experiment_in_the_prompt_lane() {
        let spec = EXPERIMENTS.first().expect("the registry has an experiment");
        assert_eq!(spec.key, BATCHED_SEARCH_EXPERIMENT);
        assert_eq!(spec.lane, Some(ExperimentLane::WorkerPrompt));
        assert_eq!(ExperimentLane::WorkerPrompt.as_str(), "worker_prompt");
        assert!(spec.splits);
        assert!(spec.requires.is_empty());
        assert_eq!(ab_split_flag_name(spec.key), BATCHED_SEARCH_AB_SPLIT_FLAG);
    }

    #[test]
    fn worker_autocompact_is_a_laneless_split() {
        let spec = EXPERIMENTS
            .iter()
            .find(|spec| spec.key == WORKER_AUTOCOMPACT_EXPERIMENT)
            .expect("worker_autocompact is registered");
        assert_eq!(spec.lane, None);
        assert!(spec.splits);
        assert!(spec.requires.is_empty());
        assert_eq!(ab_split_flag_name(spec.key), WORKER_AUTOCOMPACT_AB_SPLIT_FLAG);
    }

    #[test]
    fn worker_autocompact_runs_alongside_batched_search() {
        let on = [
            TOKEN_EXPERIMENTS_FLAG,
            BATCHED_SEARCH_EXPERIMENT,
            WORKER_AUTOCOMPACT_EXPERIMENT,
        ];
        let flag = |name: &str| on.contains(&name);
        assert_eq!(
            experiment_state(EXPERIMENTS, BATCHED_SEARCH_EXPERIMENT, &flag),
            ExperimentState::All
        );
        assert_eq!(
            experiment_state(EXPERIMENTS, WORKER_AUTOCOMPACT_EXPERIMENT, &flag),
            ExperimentState::All
        );
        let resolved = resolve_arms(EXPERIMENTS, &flag, &|_| 0);
        assert!(resolved.is_treated(BATCHED_SEARCH_EXPERIMENT));
        assert!(resolved.is_treated(WORKER_AUTOCOMPACT_EXPERIMENT));
    }

    fn store() -> (tempfile::TempDir, FeatureFlagsStore) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = FeatureFlagsStore::new(tmp.path().join("feature-flags.toml"));
        store.load().unwrap();
        (tmp, store)
    }

    #[test]
    fn the_store_ships_every_experiment_inert() {
        let (_tmp, store) = store();
        assert_eq!(store.experiment_state(BATCHED_SEARCH_EXPERIMENT), ExperimentState::Off);
        for bucket in [0, 50, 99] {
            assert!(store.resolve_experiment_arms(|_| bucket).is_empty());
        }
    }

    #[test]
    fn the_store_starts_splits_and_stops_batched_search_from_its_flags() {
        let (_tmp, store) = store();
        store.set(BATCHED_SEARCH_EXPERIMENT, true).unwrap();
        assert_eq!(
            store.experiment_state(BATCHED_SEARCH_EXPERIMENT),
            ExperimentState::Split
        );
        assert_eq!(
            store.resolve_experiment_arms(|_| 10).get(BATCHED_SEARCH_EXPERIMENT),
            Some(ExperimentArm::Treatment)
        );
        assert_eq!(
            store.resolve_experiment_arms(|_| 90).get(BATCHED_SEARCH_EXPERIMENT),
            Some(ExperimentArm::Control)
        );

        store.set(BATCHED_SEARCH_AB_SPLIT_FLAG, false).unwrap();
        assert_eq!(store.experiment_state(BATCHED_SEARCH_EXPERIMENT), ExperimentState::All);
        assert!(
            store
                .resolve_experiment_arms(|_| 90)
                .is_treated(BATCHED_SEARCH_EXPERIMENT)
        );

        store.set(TOKEN_EXPERIMENTS_FLAG, false).unwrap();
        assert!(store.resolve_experiment_arms(|_| 10).is_empty());
        store.set(TOKEN_EXPERIMENTS_FLAG, true).unwrap();
        store.set(BATCHED_SEARCH_EXPERIMENT, false).unwrap();
        assert!(store.resolve_experiment_arms(|_| 10).is_empty());
    }

    #[test]
    fn the_store_stops_a_recorded_experiment_that_has_been_turned_off() {
        let (_tmp, store) = store();
        store.set(BATCHED_SEARCH_EXPERIMENT, true).unwrap();
        let mut recorded = store.resolve_experiment_arms(|_| 90);
        assert_eq!(recorded.get(BATCHED_SEARCH_EXPERIMENT), Some(ExperimentArm::Control));

        // Still running: the record is untouched, whichever arm the family drew.
        let mut still_running = recorded.clone();
        store.stop_experiments_not_running(&mut still_running);
        assert_eq!(still_running, recorded);

        store.set(BATCHED_SEARCH_EXPERIMENT, false).unwrap();
        store.stop_experiments_not_running(&mut recorded);
        assert_eq!(recorded.get(BATCHED_SEARCH_EXPERIMENT), Some(ExperimentArm::Stopped));
    }

    #[test]
    fn delta_rereview_is_registered_in_pre_merge_lane_requiring_fanout() {
        let spec = EXPERIMENTS
            .iter()
            .find(|spec| spec.key == DELTA_REREVIEW_EXPERIMENT)
            .expect("delta_rereview is registered");
        assert_eq!(spec.lane, Some(ExperimentLane::PreMergeReview));
        assert_eq!(ExperimentLane::PreMergeReview.as_str(), "pre_merge_review");
        assert_eq!(spec.requires, &["review_batch_fanout"]);
        assert!(spec.splits);
        assert_eq!(ab_split_flag_name(spec.key), DELTA_REREVIEW_AB_SPLIT_FLAG);
    }

    #[test]
    fn delta_rereview_ships_inert_requires_fanout_and_splits_from_its_flags() {
        let (_tmp, store) = store();
        assert_eq!(store.experiment_state(DELTA_REREVIEW_EXPERIMENT), ExperimentState::Off);

        // Switching delta_rereview on while review_batch_fanout is off leaves it off.
        store.set(DELTA_REREVIEW_EXPERIMENT, true).unwrap();
        assert_eq!(store.experiment_state(DELTA_REREVIEW_EXPERIMENT), ExperimentState::Off);
        assert!(store.resolve_experiment_arms(|_| 10).is_empty());

        // Turning on its required flag starts the half-and-half split.
        store.set("review_batch_fanout", true).unwrap();
        assert_eq!(
            store.experiment_state(DELTA_REREVIEW_EXPERIMENT),
            ExperimentState::Split
        );
        assert_eq!(
            store.resolve_experiment_arms(|_| 10).get(DELTA_REREVIEW_EXPERIMENT),
            Some(ExperimentArm::Treatment)
        );
        assert_eq!(
            store.resolve_experiment_arms(|_| 90).get(DELTA_REREVIEW_EXPERIMENT),
            Some(ExperimentArm::Control)
        );

        // Turning the split off treats every family.
        store.set(DELTA_REREVIEW_AB_SPLIT_FLAG, false).unwrap();
        assert_eq!(store.experiment_state(DELTA_REREVIEW_EXPERIMENT), ExperimentState::All);
        assert!(
            store
                .resolve_experiment_arms(|_| 90)
                .is_treated(DELTA_REREVIEW_EXPERIMENT)
        );

        // Turning the master switch or experiment flag off stops it.
        store.set(TOKEN_EXPERIMENTS_FLAG, false).unwrap();
        assert_eq!(store.experiment_state(DELTA_REREVIEW_EXPERIMENT), ExperimentState::Off);
        store.set(TOKEN_EXPERIMENTS_FLAG, true).unwrap();
        store.set(DELTA_REREVIEW_EXPERIMENT, false).unwrap();
        assert_eq!(store.experiment_state(DELTA_REREVIEW_EXPERIMENT), ExperimentState::Off);
    }
}
