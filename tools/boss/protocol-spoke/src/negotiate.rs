//! Wire-epoch and feature-contract selection.
//!
//! `Hello` carries an epoch range and named feature revisions; the hub picks
//! one common epoch and, per feature, the highest revision both sides support,
//! once per connection. Build fingerprints never take part: they identify
//! releases, not compatibility.
//!
//! The epoch stays 1 across ordinary releases. Additive optional fields, new
//! optional messages and new driver support do not change it; only an
//! incompatible base envelope, handshake or ordering/ack rule does. With no
//! common epoch the hub answers with a [`Rejection`] that still travels as a
//! readable `Reject` (its own fields never change meaning), so an incompatible
//! peer learns what to upgrade instead of seeing silence.

use std::collections::{BTreeMap, BTreeSet};

use crate::v1::{Feature, Hello, Reject, Welcome};
use crate::validate::{Validate, ValidationError};

/// The only wire epoch defined so far.
pub const EPOCH: u32 = 1;

/// Feature contract names. A feature is a name plus a revision; an
/// incompatible change gets a new revision, additive optional fields keep it.
pub mod feature {
    pub const EXECUTION: &str = "execution";
    pub const JOURNAL: &str = "journal";
    pub const WORKSPACE: &str = "workspace";
    pub const WORKER_EVENT: &str = "worker-event";
    pub const WORKER_API: &str = "worker-api";
    pub const PROPOSAL_JSON: &str = "proposal-json";
    pub const TRANSCRIPT_DELTA: &str = "transcript-delta";
    pub const ARTIFACT: &str = "artifact";
    pub const PANE_CAPTURE: &str = "pane-capture";
    pub const UPGRADE: &str = "upgrade";
}

/// Features without which no execution traffic is possible. A peer missing one
/// is refused; there is no degraded mode for them.
pub const REQUIRED_FEATURES: [&str; 8] = [
    feature::EXECUTION,
    feature::JOURNAL,
    feature::WORKSPACE,
    feature::WORKER_EVENT,
    feature::WORKER_API,
    feature::PROPOSAL_JSON,
    feature::TRANSCRIPT_DELTA,
    feature::ARTIFACT,
];

/// Negotiated per connection when both sides offer them; a mixed-version
/// connection keeps execution without them.
pub const OPTIONAL_FEATURES: [&str; 2] = [feature::PANE_CAPTURE, feature::UPGRADE];

/// `Reject.code` values.
pub mod reject_code {
    pub const NO_COMMON_EPOCH: &str = "no_common_epoch";
    pub const MISSING_REQUIRED_FEATURE: &str = "missing_required_feature";
    pub const INVALID_HELLO: &str = "invalid_hello";
}

/// What one side of the connection supports.
#[derive(Debug, Clone, PartialEq)]
pub struct Contracts {
    pub min_epoch: u32,
    pub max_epoch: u32,
    /// Every `(name, revision)` this build can speak. A name may appear with
    /// several revisions.
    pub features: Vec<Feature>,
    /// Feature names this side will not run without.
    pub required: Vec<String>,
}

impl Contracts {
    /// The first-release contract set: epoch 1, every named feature at
    /// revision 1, the eight execution features required.
    pub fn first_release() -> Self {
        let revision_one = |name: &&str| Feature {
            name: (*name).to_owned(),
            revision: 1,
        };
        Self {
            min_epoch: EPOCH,
            max_epoch: EPOCH,
            features: REQUIRED_FEATURES
                .iter()
                .chain(OPTIONAL_FEATURES.iter())
                .map(revision_one)
                .collect(),
            required: REQUIRED_FEATURES.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    /// Selects the contracts for a connection from the peer's `Hello`.
    ///
    /// Order matters for readability across versions: the epoch range is
    /// checked before anything else in `Hello`, so a peer from a future epoch
    /// is told to upgrade even if the rest of its `Hello` no longer parses
    /// under this build's rules.
    pub fn select(&self, hello: &Hello) -> Result<Selection, Rejection> {
        if hello.min_epoch == 0 || hello.max_epoch < hello.min_epoch {
            return Err(Rejection::InvalidHello(ValidationError::Invalid {
                field: "hello.min_epoch",
                reason: "epoch range is empty or starts at zero",
            }));
        }
        let epoch = self
            .common_epoch(hello.min_epoch, hello.max_epoch)
            .ok_or(Rejection::NoCommonEpoch {
                local: (self.min_epoch, self.max_epoch),
                peer: (hello.min_epoch, hello.max_epoch),
            })?;
        hello.validate().map_err(Rejection::InvalidHello)?;
        self.select_features(epoch, &hello.features)
    }

    /// The spoke-side counterpart: confirms that the hub's `Welcome` selected
    /// something this build can run, so a hub bug or a stale cached answer
    /// cannot put the spoke on a contract it does not speak.
    pub fn confirm(&self, welcome: &Welcome) -> Result<Selection, Rejection> {
        welcome.validate().map_err(Rejection::InvalidHello)?;
        if welcome.epoch < self.min_epoch || welcome.epoch > self.max_epoch {
            return Err(Rejection::NoCommonEpoch {
                local: (self.min_epoch, self.max_epoch),
                peer: (welcome.epoch, welcome.epoch),
            });
        }
        // The hub chose one revision per feature; confirm each is one we speak.
        let selection = self.select_features(welcome.epoch, &welcome.features)?;
        if selection.features.len()
            != welcome
                .features
                .iter()
                .map(|f| f.name.as_str())
                .collect::<BTreeSet<_>>()
                .len()
        {
            return Err(Rejection::InvalidHello(ValidationError::Invalid {
                field: "welcome.features",
                reason: "selects a feature revision this build does not speak",
            }));
        }
        Ok(selection)
    }

    fn common_epoch(&self, peer_min: u32, peer_max: u32) -> Option<u32> {
        let highest = self.max_epoch.min(peer_max);
        (highest >= self.min_epoch.max(peer_min)).then_some(highest)
    }

    fn select_features(&self, epoch: u32, peer: &[Feature]) -> Result<Selection, Rejection> {
        let local: BTreeSet<(&str, u32)> = self.features.iter().map(|f| (f.name.as_str(), f.revision)).collect();
        let mut chosen: BTreeMap<String, u32> = BTreeMap::new();
        for offered in peer {
            if local.contains(&(offered.name.as_str(), offered.revision)) {
                let entry = chosen.entry(offered.name.clone()).or_insert(offered.revision);
                *entry = (*entry).max(offered.revision);
            }
        }
        let mut missing: Vec<String> = self
            .required
            .iter()
            .filter(|name| !chosen.contains_key(*name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            missing.sort();
            return Err(Rejection::MissingRequired { features: missing });
        }
        Ok(Selection {
            epoch,
            features: chosen,
        })
    }
}

/// The contracts a connection runs with, frozen until it reconnects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub epoch: u32,
    features: BTreeMap<String, u32>,
}

impl Selection {
    /// The selected revision of `name`, if both sides support it.
    pub fn revision(&self, name: &str) -> Option<u32> {
        self.features.get(name).copied()
    }

    /// Whether optional messages for `name` may be sent on this connection.
    pub fn has(&self, name: &str) -> bool {
        self.features.contains_key(name)
    }

    /// The selection in `Welcome.features` form, ordered by name.
    pub fn to_features(&self) -> Vec<Feature> {
        self.features
            .iter()
            .map(|(name, revision)| Feature {
                name: name.clone(),
                revision: *revision,
            })
            .collect()
    }
}

/// Why a connection cannot run, in a form that maps onto `Reject`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    #[error("no common wire epoch: this build supports {}..={}, the peer offered {}..={}", .local.0, .local.1, .peer.0, .peer.1)]
    NoCommonEpoch { local: (u32, u32), peer: (u32, u32) },
    #[error("peer lacks required features: {}", .features.join(", "))]
    MissingRequired { features: Vec<String> },
    #[error("invalid hello: {0}")]
    InvalidHello(ValidationError),
}

impl Rejection {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoCommonEpoch { .. } => reject_code::NO_COMMON_EPOCH,
            Self::MissingRequired { .. } => reject_code::MISSING_REQUIRED_FEATURE,
            Self::InvalidHello(_) => reject_code::INVALID_HELLO,
        }
    }

    /// The `Reject` frame body, carrying this build's supported epoch range so
    /// the peer can tell which side must upgrade.
    pub fn to_reject(&self, supported: &Contracts) -> Reject {
        Reject {
            code: self.code().to_owned(),
            diagnostic: self.to_string(),
            supported_min_epoch: supported.min_epoch,
            supported_max_epoch: supported.max_epoch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(min_epoch: u32, max_epoch: u32, features: Vec<Feature>) -> Hello {
        Hello {
            host_id: "spoke-1".into(),
            journal_id: "journal-1".into(),
            boot_id: "boot-1".into(),
            min_epoch,
            max_epoch,
            features,
            instance_nonce: vec![7; 16],
            ..Default::default()
        }
    }

    fn rev(name: &str, revision: u32) -> Feature {
        Feature {
            name: name.into(),
            revision,
        }
    }

    fn required_at(revision: u32) -> Vec<Feature> {
        REQUIRED_FEATURES.iter().map(|name| rev(name, revision)).collect()
    }

    #[test]
    fn current_peers_select_everything_they_share() {
        let local = Contracts::first_release();
        let selection = local
            .select(&hello(1, 1, Contracts::first_release().features))
            .expect("compatible");
        assert_eq!(selection.epoch, EPOCH);
        for name in REQUIRED_FEATURES.iter().chain(OPTIONAL_FEATURES.iter()) {
            assert_eq!(selection.revision(name), Some(1), "{name}");
        }
    }

    #[test]
    fn a_peer_without_optional_features_still_connects() {
        let local = Contracts::first_release();
        let selection = local
            .select(&hello(1, 1, required_at(1)))
            .expect("required features suffice");
        assert!(!selection.has(feature::PANE_CAPTURE));
        assert!(!selection.has(feature::UPGRADE));
        assert!(selection.has(feature::EXECUTION));
    }

    #[test]
    fn a_peer_missing_a_required_feature_is_rejected_by_name() {
        let local = Contracts::first_release();
        let mut offered = required_at(1);
        offered.retain(|f| f.name != feature::JOURNAL && f.name != feature::ARTIFACT);
        let rejection = local.select(&hello(1, 1, offered)).expect_err("incompatible");
        assert_eq!(rejection.code(), reject_code::MISSING_REQUIRED_FEATURE);
        assert_eq!(
            rejection,
            Rejection::MissingRequired {
                features: vec!["artifact".into(), "journal".into()]
            }
        );
    }

    #[test]
    fn a_required_feature_at_an_unknown_revision_counts_as_missing() {
        let local = Contracts::first_release();
        let rejection = local
            .select(&hello(1, 1, required_at(2)))
            .expect_err("revision 2 is unknown here");
        assert_eq!(rejection.code(), reject_code::MISSING_REQUIRED_FEATURE);
    }

    #[test]
    fn the_highest_common_revision_wins_and_unknown_features_are_ignored() {
        let mut local = Contracts::first_release();
        local.features.push(rev(feature::TRANSCRIPT_DELTA, 2));
        let mut offered = required_at(1);
        offered.push(rev(feature::TRANSCRIPT_DELTA, 2));
        offered.push(rev(feature::TRANSCRIPT_DELTA, 3));
        offered.push(rev("telepathy", 1));
        let selection = local.select(&hello(1, 4, offered)).expect("compatible");
        assert_eq!(selection.revision(feature::TRANSCRIPT_DELTA), Some(2));
        assert_eq!(selection.revision("telepathy"), None);
        assert_eq!(selection.epoch, 1, "the epoch is the highest one both support");
    }

    #[test]
    fn no_common_epoch_is_reported_before_the_rest_of_hello_is_judged() {
        let local = Contracts::first_release();
        // A future-epoch hello whose other fields this build cannot parse.
        let mut future = hello(2, 3, Vec::new());
        future.instance_nonce.clear();
        future.host_id.clear();
        let rejection = local.select(&future).expect_err("no common epoch");
        assert_eq!(
            rejection,
            Rejection::NoCommonEpoch {
                local: (1, 1),
                peer: (2, 3)
            }
        );

        let reject = rejection.to_reject(&local);
        assert_eq!(reject.code, reject_code::NO_COMMON_EPOCH);
        assert_eq!((reject.supported_min_epoch, reject.supported_max_epoch), (1, 1));
        assert!(reject.diagnostic.contains("2..=3"), "{}", reject.diagnostic);
    }

    #[test]
    fn a_malformed_hello_is_an_invalid_hello() {
        let local = Contracts::first_release();
        let mut bad = hello(1, 1, required_at(1));
        bad.journal_id.clear();
        assert_eq!(
            local.select(&bad).expect_err("invalid").code(),
            reject_code::INVALID_HELLO
        );
        assert_eq!(
            local.select(&hello(0, 1, Vec::new())).expect_err("invalid").code(),
            reject_code::INVALID_HELLO
        );
        assert_eq!(
            local.select(&hello(3, 2, Vec::new())).expect_err("invalid").code(),
            reject_code::INVALID_HELLO
        );
    }

    fn welcome(epoch: u32, features: Vec<Feature>) -> Welcome {
        Welcome {
            epoch,
            features,
            hub_id: "hub".into(),
            authority_generation: "a1".into(),
            connection_generation: 1,
            received_records: Some(crate::v1::Cursor {
                journal_id: "journal-1".into(),
                through: 0,
            }),
            command_journal_id: "commands-1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_spoke_confirms_the_hub_selection_it_receives() {
        let local = Contracts::first_release();
        let selection = local
            .confirm(&welcome(1, required_at(1)))
            .expect("hub selected what we speak");
        assert_eq!(selection.to_features().len(), REQUIRED_FEATURES.len());

        assert!(
            local.confirm(&welcome(2, required_at(1))).is_err(),
            "epoch outside our range"
        );
        assert!(
            local.confirm(&welcome(1, required_at(9))).is_err(),
            "revision we do not speak"
        );
        let mut partial = required_at(1);
        partial.pop();
        assert!(local.confirm(&welcome(1, partial)).is_err(), "required feature dropped");
    }
}
