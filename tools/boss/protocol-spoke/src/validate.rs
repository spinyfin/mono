//! Structural validation of decoded `boss.spoke.v1` messages.
//!
//! Protobuf decoding is total: a missing message decodes as `None`, a missing
//! scalar as zero, an unknown enum number as the raw `i32` and an unknown oneof
//! variant as `None`. None of those is a usable value, so a receiver validates
//! every frame before acting on it and treats a failure as a rejected record,
//! never as success.
//!
//! This module checks what the schema can express on its own: identities that
//! must be set, oneofs that must be present, enums that must be specified,
//! sizes against [`crate::limits`], and which messages are run-scoped. It does
//! not decide policy: whether a `Spawn` environment variable is allowed for a
//! driver, or which `WorkspaceOp` fields a given operation needs, belong to the
//! components that execute those commands.

use crate::identity::is_valid_host_id;
use crate::limits::{
    DIGEST_BYTES, MAX_CAPTURE_BYTES, MAX_CHUNK_BYTES, MAX_PAYLOAD_BYTES, MAX_SPAWN_BYTES, MAX_STOP_GRACE_MS,
};
use crate::v1::{self, command, command_result, hub_frame, record, spoke_frame, worker_event};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// A required singular message is absent.
    #[error("{field}: required message is missing")]
    MissingMessage { field: &'static str },
    /// A oneof has no variant set: malformed, or a variant newer than this build.
    #[error("{field}: no body set (malformed, or a variant this build does not know)")]
    MissingBody { field: &'static str },
    /// A scalar identity is empty or zero.
    #[error("{field}: must be set")]
    Unset { field: &'static str },
    /// An enum is `*_UNSPECIFIED` or carries a number this build does not know.
    #[error("{field}: {value} is not a specified value of this enum")]
    UnknownEnum { field: &'static str, value: i32 },
    #[error("{field}: {len} bytes exceeds the {max} byte limit")]
    TooLarge {
        field: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{field}: {reason}")]
    Invalid { field: &'static str, reason: &'static str },
    #[error("{field}: not permitted on {context}")]
    Forbidden { field: &'static str, context: &'static str },
}

impl ValidationError {
    /// Whether the failure may only mean the sender is newer than this build
    /// (an unknown enum number or oneof variant). Such records are quarantined
    /// or answered with a typed unsupported result; every other failure is a
    /// malformed record.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::MissingBody { .. } | Self::UnknownEnum { .. })
    }
}

pub trait Validate {
    fn validate(&self) -> Result<(), ValidationError>;
}

type Check = Result<(), ValidationError>;

fn nonempty(field: &'static str, value: &str) -> Check {
    if value.is_empty() {
        Err(ValidationError::Unset { field })
    } else {
        Ok(())
    }
}

fn nonzero(field: &'static str, value: u64) -> Check {
    if value == 0 {
        Err(ValidationError::Unset { field })
    } else {
        Ok(())
    }
}

fn required<'a, T>(field: &'static str, value: &'a Option<T>) -> Result<&'a T, ValidationError> {
    value.as_ref().ok_or(ValidationError::MissingMessage { field })
}

fn body<'a, T>(field: &'static str, value: &'a Option<T>) -> Result<&'a T, ValidationError> {
    value.as_ref().ok_or(ValidationError::MissingBody { field })
}

fn bounded(field: &'static str, len: usize, max: usize) -> Check {
    if len > max {
        Err(ValidationError::TooLarge { field, len, max })
    } else {
        Ok(())
    }
}

fn utf8(field: &'static str, bytes: &[u8]) -> Check {
    std::str::from_utf8(bytes)
        .map(|_| ())
        .map_err(|_| ValidationError::Invalid {
            field,
            reason: "not valid UTF-8",
        })
}

fn digest(field: &'static str, bytes: &[u8]) -> Check {
    if bytes.is_empty() || bytes.len() == DIGEST_BYTES {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field,
            reason: "digest must be empty or 32 bytes",
        })
    }
}

/// Rejects `*_UNSPECIFIED` (always zero) and numbers outside the enum.
fn specified<E: TryFrom<i32>>(field: &'static str, raw: i32) -> Result<E, ValidationError> {
    if raw == 0 {
        return Err(ValidationError::UnknownEnum { field, value: raw });
    }
    E::try_from(raw).map_err(|_| ValidationError::UnknownEnum { field, value: raw })
}

/// Whether a message is tied to one run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunScope {
    Required,
    Forbidden,
    Optional,
}

fn check_run(run: &Option<v1::Run>, scope: RunScope, context: &'static str) -> Check {
    match (scope, run) {
        (RunScope::Required, None) => Err(ValidationError::MissingMessage { field: "run" }),
        (RunScope::Forbidden, Some(_)) => Err(ValidationError::Forbidden { field: "run", context }),
        (_, Some(run)) => run.validate(),
        _ => Ok(()),
    }
}

impl Validate for v1::Stamp {
    fn validate(&self) -> Check {
        nonempty("stamp.boot_id", &self.boot_id)
    }
}

impl Validate for v1::Cursor {
    fn validate(&self) -> Check {
        nonempty("cursor.journal_id", &self.journal_id)
    }
}

impl Validate for v1::Feature {
    fn validate(&self) -> Check {
        let well_formed = !self.name.is_empty()
            && self.name.len() <= 64
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !well_formed {
            return Err(ValidationError::Invalid {
                field: "feature.name",
                reason: "must be 1-64 of [a-z0-9-]",
            });
        }
        nonzero("feature.revision", self.revision.into())
    }
}

impl Validate for v1::Run {
    fn validate(&self) -> Check {
        nonempty("run.execution_id", &self.execution_id)?;
        nonempty("run.run_id", &self.run_id)?;
        nonzero("run.assignment_generation", self.assignment_generation)?;
        nonempty("run.spawn_token", &self.spawn_token)
    }
}

impl Validate for v1::Inventory {
    fn validate(&self) -> Check {
        nonempty("inventory.os", &self.os)?;
        nonempty("inventory.arch", &self.arch)
    }
}

impl Validate for v1::Hello {
    fn validate(&self) -> Check {
        if !is_valid_host_id(&self.host_id) {
            return Err(ValidationError::Invalid {
                field: "hello.host_id",
                reason: "not a valid host id",
            });
        }
        nonempty("hello.journal_id", &self.journal_id)?;
        nonempty("hello.boot_id", &self.boot_id)?;
        nonzero("hello.min_epoch", self.min_epoch.into())?;
        if self.max_epoch < self.min_epoch {
            return Err(ValidationError::Invalid {
                field: "hello.max_epoch",
                reason: "below min_epoch",
            });
        }
        self.features.iter().try_for_each(Validate::validate)?;
        if let Some(cursor) = &self.received_commands
            && cursor.through > 0
        {
            cursor.validate()?;
        }
        if let Some(inventory) = &self.inventory {
            inventory.validate()?;
        }
        if self.instance_nonce.is_empty() || self.instance_nonce.len() > 64 {
            return Err(ValidationError::Invalid {
                field: "hello.instance_nonce",
                reason: "must be 1-64 bytes",
            });
        }
        bounded("hello.build", self.build.len(), 256)
    }
}

impl Validate for v1::Welcome {
    fn validate(&self) -> Check {
        nonzero("welcome.epoch", self.epoch.into())?;
        nonempty("welcome.hub_id", &self.hub_id)?;
        nonempty("welcome.authority_generation", &self.authority_generation)?;
        nonzero("welcome.connection_generation", self.connection_generation)?;
        required("welcome.received_records", &self.received_records)?.validate()?;
        nonempty("welcome.command_journal_id", &self.command_journal_id)?;
        self.features.iter().try_for_each(Validate::validate)?;
        // Unlike Hello, which offers every revision it speaks, Welcome is the
        // hub's single choice: one revision per feature name.
        let mut names = std::collections::BTreeSet::new();
        if !self.features.iter().all(|f| names.insert(f.name.as_str())) {
            return Err(ValidationError::Invalid {
                field: "welcome.features",
                reason: "names a feature more than once; a welcome selects one revision per feature",
            });
        }
        Ok(())
    }
}

impl Validate for v1::Reject {
    fn validate(&self) -> Check {
        nonempty("reject.code", &self.code)
    }
}

impl Validate for v1::Pulse {
    fn validate(&self) -> Check {
        if self.nonce.is_empty() || self.nonce.len() > 64 {
            return Err(ValidationError::Invalid {
                field: "pulse.nonce",
                reason: "must be 1-64 bytes",
            });
        }
        self.stamp.as_ref().map_or(Ok(()), Validate::validate)
    }
}

impl Validate for v1::SpokeFrame {
    fn validate(&self) -> Check {
        match body("spoke_frame.body", &self.body)? {
            spoke_frame::Body::Hello(hello) => hello.validate(),
            spoke_frame::Body::Record(record) => record.validate(),
            spoke_frame::Body::CommandReceipt(cursor) => cursor.validate(),
            spoke_frame::Body::PulseReply(pulse) => pulse.validate(),
        }
    }
}

impl Validate for v1::HubFrame {
    fn validate(&self) -> Check {
        let on_a_stream = || nonzero("hub_frame.connection_generation", self.connection_generation);
        match body("hub_frame.body", &self.body)? {
            hub_frame::Body::Welcome(welcome) => welcome.validate(),
            hub_frame::Body::Reject(reject) => reject.validate(),
            hub_frame::Body::Command(command) => on_a_stream().and_then(|()| command.validate()),
            hub_frame::Body::RecordReceipt(cursor) => on_a_stream().and_then(|()| cursor.validate()),
            hub_frame::Body::Pulse(pulse) => on_a_stream().and_then(|()| pulse.validate()),
        }
    }
}

impl Validate for v1::Record {
    fn validate(&self) -> Check {
        nonzero("record.sequence", self.sequence)?;
        required("record.observed", &self.observed)?.validate()?;
        let (scope, context) = match body("record.body", &self.body)? {
            record::Body::Event(_)
            | record::Body::Pane(_)
            | record::Body::Transcript(_)
            | record::Body::Proposal(_)
            | record::Body::Capture(_)
            | record::Body::Request(_) => (RunScope::Required, "run record"),
            record::Body::Capacity(_) | record::Body::Inventory(_) => (RunScope::Forbidden, "host-scoped record"),
            record::Body::Result(_) | record::Body::Artifact(_) | record::Body::Gap(_) => (RunScope::Optional, ""),
        };
        check_run(&self.run, scope, context)?;
        match body("record.body", &self.body)? {
            record::Body::Result(result) => result.validate(),
            record::Body::Event(event) => event.validate(),
            record::Body::Pane(pane) => pane.validate(),
            record::Body::Transcript(delta) => delta.validate(),
            record::Body::Capacity(capacity) => capacity.validate(),
            record::Body::Proposal(proposal) => proposal.validate(),
            record::Body::Artifact(chunk) => chunk.validate(),
            record::Body::Inventory(snapshot) => snapshot.validate(),
            record::Body::Gap(gap) => gap.validate(),
            record::Body::Capture(capture) => capture.validate(),
            record::Body::Request(request) => request.validate(),
        }
    }
}

impl Validate for v1::Command {
    fn validate(&self) -> Check {
        nonzero("command.sequence", self.sequence)?;
        nonempty("command.command_id", &self.command_id)?;
        nonzero("command.not_after_unix_ms", self.not_after_unix_ms.max(0) as u64)?;
        let (scope, context) = match body("command.body", &self.body)? {
            command::Body::Spawn(_)
            | command::Body::Adopt(_)
            | command::Body::Deliver(_)
            | command::Body::Stop(_)
            | command::Body::Capture(_)
            | command::Body::ProposalDecision(_)
            | command::Body::WorkerReply(_) => (RunScope::Required, "run command"),
            command::Body::Inventory(_) | command::Body::Upgrade(_) => (RunScope::Forbidden, "host-scoped command"),
            command::Body::Workspace(_) | command::Body::Read(_) => (RunScope::Optional, ""),
        };
        check_run(&self.run, scope, context)?;
        match body("command.body", &self.body)? {
            command::Body::Spawn(spawn) => spawn.validate(),
            command::Body::Adopt(_) | command::Body::Inventory(_) => Ok(()),
            command::Body::Workspace(op) => specified::<v1::WorkspaceKind>("workspace.kind", op.kind).map(|_| ()),
            command::Body::Deliver(deliver) => deliver.validate(),
            command::Body::Stop(stop) => stop.validate(),
            command::Body::Read(read) => read.validate(),
            command::Body::Capture(capture) => capture.validate(),
            command::Body::ProposalDecision(decision) => decision.validate(),
            command::Body::Upgrade(upgrade) => upgrade.validate(),
            command::Body::WorkerReply(reply) => reply.validate(),
        }
    }
}

impl Validate for v1::Spawn {
    fn validate(&self) -> Check {
        nonempty("spawn.lease_id", &self.lease_id)?;
        nonempty("spawn.workspace_id", &self.workspace_id)?;
        nonempty("spawn.driver_id", &self.driver_id)?;
        if self.prompt_utf8.is_empty() {
            return Err(ValidationError::Unset {
                field: "spawn.prompt_utf8",
            });
        }
        utf8("spawn.prompt_utf8", &self.prompt_utf8)?;
        bounded(
            "spawn.prompt_utf8+settings_json",
            self.prompt_utf8.len() + self.settings_json.len(),
            MAX_SPAWN_BYTES,
        )?;
        bounded("spawn.run_context_json", self.run_context_json.len(), MAX_PAYLOAD_BYTES)?;
        bounded(
            "spawn.worker_policy_json",
            self.worker_policy_json.len(),
            MAX_PAYLOAD_BYTES,
        )?;
        if !self.run_context_json.is_empty() {
            nonempty("spawn.context_revision", &self.context_revision)?;
        }
        if !self.worker_policy_json.is_empty() {
            nonempty("spawn.policy_revision", &self.policy_revision)?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for env in &self.env {
            let name_ok = !env.name.is_empty()
                && env.name.len() <= 256
                && !env.name.starts_with(|c: char| c.is_ascii_digit())
                && env.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !name_ok {
                return Err(ValidationError::Invalid {
                    field: "spawn.env.name",
                    reason: "not a valid variable name",
                });
            }
            if env.value.contains('\0') {
                return Err(ValidationError::Invalid {
                    field: "spawn.env.value",
                    reason: "contains NUL",
                });
            }
            if !seen.insert(env.name.as_str()) {
                return Err(ValidationError::Invalid {
                    field: "spawn.env.name",
                    reason: "duplicate variable",
                });
            }
        }
        self.required_features.iter().try_for_each(Validate::validate)
    }
}

impl Validate for v1::Deliver {
    fn validate(&self) -> Check {
        specified::<v1::DeliveryKind>("deliver.kind", self.kind)?;
        bounded("deliver.input_utf8", self.input_utf8.len(), MAX_PAYLOAD_BYTES)?;
        utf8("deliver.input_utf8", &self.input_utf8)
    }
}

impl Validate for v1::Stop {
    fn validate(&self) -> Check {
        if self.grace_ms > MAX_STOP_GRACE_MS {
            return Err(ValidationError::Invalid {
                field: "stop.grace_ms",
                reason: "exceeds the 30 second cap",
            });
        }
        Ok(())
    }
}

impl Validate for v1::ReadArtifact {
    fn validate(&self) -> Check {
        nonempty("read.artifact_id", &self.artifact_id)?;
        specified::<v1::ArtifactKind>("read.kind", self.kind)?;
        bounded("read.max_bytes", self.max_bytes as usize, MAX_CHUNK_BYTES)
    }
}

impl Validate for v1::Capture {
    fn validate(&self) -> Check {
        nonempty("capture.subscription_id", &self.subscription_id)?;
        bounded("capture.max_bytes", self.max_bytes as usize, MAX_CAPTURE_BYTES)
    }
}

impl Validate for v1::ProposalDecision {
    fn validate(&self) -> Check {
        nonempty("proposal_decision.proposal_id", &self.proposal_id)?;
        specified::<v1::ResultCode>("proposal_decision.code", self.code)?;
        bounded(
            "proposal_decision.result_json",
            self.result_json.len(),
            MAX_PAYLOAD_BYTES,
        )
    }
}

impl Validate for v1::Upgrade {
    fn validate(&self) -> Check {
        nonempty("upgrade.release_id", &self.release_id)?;
        if !self.manifest_url.starts_with("https://") {
            return Err(ValidationError::Invalid {
                field: "upgrade.manifest_url",
                reason: "must be an https URL",
            });
        }
        if self.manifest_sha256.len() != DIGEST_BYTES {
            return Err(ValidationError::Invalid {
                field: "upgrade.manifest_sha256",
                reason: "must be 32 bytes",
            });
        }
        Ok(())
    }
}

impl Validate for v1::WorkerReply {
    fn validate(&self) -> Check {
        nonempty("worker_reply.request_id", &self.request_id)?;
        specified::<v1::ResultCode>("worker_reply.code", self.code)?;
        bounded("worker_reply.result_json", self.result_json.len(), MAX_PAYLOAD_BYTES)
    }
}

impl Validate for v1::WorkerRequest {
    fn validate(&self) -> Check {
        nonempty("worker_request.request_id", &self.request_id)?;
        let method = specified::<v1::WorkerMethod>("worker_request.method", self.method)?;
        if method == v1::WorkerMethod::ProposalStatus {
            nonempty("worker_request.proposal_id", &self.proposal_id)?;
        }
        Ok(())
    }
}

impl Validate for v1::CommandResult {
    fn validate(&self) -> Check {
        nonempty("result.command_id", &self.command_id)?;
        specified::<v1::ResultCode>("result.code", self.code)?;
        match &self.value {
            Some(command_result::Value::Pane(pane)) => pane.validate(),
            _ => Ok(()),
        }
    }
}

impl Validate for v1::PaneState {
    fn validate(&self) -> Check {
        let life = specified::<v1::PaneLife>("pane.life", self.life)?;
        if self.exit_code.is_some() && life != v1::PaneLife::Exited {
            return Err(ValidationError::Forbidden {
                field: "pane.exit_code",
                context: "a pane that has not exited",
            });
        }
        Ok(())
    }
}

impl Validate for v1::Capacity {
    fn validate(&self) -> Check {
        nonzero("capacity.generation", self.generation)?;
        self.inventory.as_ref().map_or(Ok(()), Validate::validate)
    }
}

impl Validate for v1::WorkerEvent {
    fn validate(&self) -> Check {
        nonempty("event.session_id", &self.session_id)?;
        match body("event.event", &self.event)? {
            worker_event::Event::PreToolUse(tool) | worker_event::Event::PostToolUse(tool) => {
                nonempty("event.tool.name", &tool.name)?;
                bounded("event.tool.input_json", tool.input_json.len(), MAX_PAYLOAD_BYTES)?;
                bounded("event.tool.response_json", tool.response_json.len(), MAX_PAYLOAD_BYTES)
            }
            worker_event::Event::UserPrompt(text)
            | worker_event::Event::Notification(text)
            | worker_event::Event::SessionEnd(text) => bounded("event.text", text.len(), MAX_PAYLOAD_BYTES),
            worker_event::Event::SessionStart(_) | worker_event::Event::Stop(_) => Ok(()),
        }
    }
}

impl Validate for v1::TranscriptDelta {
    fn validate(&self) -> Check {
        nonempty("transcript.generation", &self.generation)?;
        bounded("transcript.data", self.data.len(), MAX_CHUNK_BYTES)?;
        digest("transcript.sha256", &self.sha256)
    }
}

impl Validate for v1::ArtifactChunk {
    fn validate(&self) -> Check {
        nonempty("artifact.artifact_id", &self.artifact_id)?;
        specified::<v1::ArtifactKind>("artifact.kind", self.kind)?;
        bounded("artifact.data", self.data.len(), MAX_CHUNK_BYTES)?;
        digest("artifact.sha256", &self.sha256)?;
        if self.total_bytes != 0 && self.offset.saturating_add(self.data.len() as u64) > self.total_bytes {
            return Err(ValidationError::Invalid {
                field: "artifact.total_bytes",
                reason: "chunk ends past the total",
            });
        }
        Ok(())
    }
}

impl Validate for v1::Proposal {
    fn validate(&self) -> Check {
        nonempty("proposal.proposal_id", &self.proposal_id)?;
        specified::<v1::ProposalKind>("proposal.kind", self.kind)?;
        nonempty("proposal.payload_contract", &self.payload_contract)?;
        if self.canonical_json.is_empty() {
            return Err(ValidationError::Unset {
                field: "proposal.canonical_json",
            });
        }
        bounded("proposal.canonical_json", self.canonical_json.len(), MAX_PAYLOAD_BYTES)
    }
}

impl Validate for v1::InventorySnapshot {
    fn validate(&self) -> Check {
        for observation in &self.runs {
            required("inventory.runs.run", &observation.run)?.validate()?;
            required("inventory.runs.pane", &observation.pane)?.validate()?;
        }
        Ok(())
    }
}

impl Validate for v1::Gap {
    fn validate(&self) -> Check {
        nonempty("gap.resource", &self.resource)?;
        nonempty("gap.reason", &self.reason)?;
        if self.through_offset < self.from_offset {
            return Err(ValidationError::Invalid {
                field: "gap.through_offset",
                reason: "before from_offset",
            });
        }
        Ok(())
    }
}

impl Validate for v1::PaneCapture {
    fn validate(&self) -> Check {
        nonempty("capture.subscription_id", &self.subscription_id)?;
        bounded("capture.text_utf8", self.text_utf8.len(), MAX_CAPTURE_BYTES)?;
        utf8("capture.text_utf8", &self.text_utf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> v1::Run {
        v1::Run {
            execution_id: "exec-1".into(),
            run_id: "run-1".into(),
            assignment_generation: 1,
            spawn_token: "token-1".into(),
        }
    }

    fn stamp() -> v1::Stamp {
        v1::Stamp {
            boot_id: "boot-1".into(),
            wall_unix_ms: 1,
            awake_ns: 1,
            elapsed_ns: 1,
        }
    }

    fn event_record() -> v1::Record {
        v1::Record {
            sequence: 1,
            observed: Some(stamp()),
            run: Some(run()),
            body: Some(record::Body::Event(v1::WorkerEvent {
                session_id: "s".into(),
                event: Some(worker_event::Event::Notification("hi".into())),
            })),
        }
    }

    #[test]
    fn a_complete_record_validates() {
        event_record().validate().expect("valid");
    }

    #[test]
    fn zero_values_are_not_identities() {
        let mut bad = event_record();
        bad.run.as_mut().unwrap().spawn_token.clear();
        assert_eq!(
            bad.validate(),
            Err(ValidationError::Unset {
                field: "run.spawn_token"
            })
        );

        let mut bad = event_record();
        bad.sequence = 0;
        assert_eq!(
            bad.validate(),
            Err(ValidationError::Unset {
                field: "record.sequence"
            })
        );
    }

    #[test]
    fn an_unset_oneof_is_unsupported_not_success() {
        let mut unknown = event_record();
        unknown.body = None;
        let err = unknown.validate().expect_err("no body");
        assert!(err.is_unsupported(), "{err}");
    }

    #[test]
    fn run_scoping_is_enforced_per_body() {
        let mut capacity = event_record();
        capacity.body = Some(record::Body::Capacity(v1::Capacity {
            generation: 1,
            ..Default::default()
        }));
        assert!(matches!(
            capacity.validate(),
            Err(ValidationError::Forbidden { field: "run", .. })
        ));
        capacity.run = None;
        capacity.validate().expect("capacity is host-scoped");

        let mut event = event_record();
        event.run = None;
        assert_eq!(event.validate(), Err(ValidationError::MissingMessage { field: "run" }));
    }

    #[test]
    fn unspecified_and_unknown_enums_are_rejected() {
        let deliver = |kind| v1::Deliver {
            kind,
            input_utf8: b"x".to_vec(),
        };
        assert!(deliver(v1::DeliveryKind::Nudge as i32).validate().is_ok());
        let err = deliver(0).validate().expect_err("unspecified");
        assert!(err.is_unsupported());
        assert_eq!(
            deliver(99).validate(),
            Err(ValidationError::UnknownEnum {
                field: "deliver.kind",
                value: 99
            })
        );
    }

    #[test]
    fn spawn_enforces_utf8_size_and_env_shape() {
        let spawn = || v1::Spawn {
            lease_id: "l".into(),
            workspace_id: "w".into(),
            driver_id: "claude".into(),
            prompt_utf8: b"do the thing".to_vec(),
            ..Default::default()
        };
        spawn().validate().expect("valid");

        let mut bad = spawn();
        bad.prompt_utf8 = vec![0xff, 0xfe];
        assert!(matches!(
            bad.validate(),
            Err(ValidationError::Invalid {
                field: "spawn.prompt_utf8",
                ..
            })
        ));

        let mut bad = spawn();
        bad.settings_json = vec![b' '; MAX_SPAWN_BYTES];
        assert!(matches!(bad.validate(), Err(ValidationError::TooLarge { .. })));

        let mut bad = spawn();
        bad.env = vec![
            v1::Env {
                name: "A".into(),
                value: "1".into(),
            },
            v1::Env {
                name: "A".into(),
                value: "2".into(),
            },
        ];
        assert!(matches!(
            bad.validate(),
            Err(ValidationError::Invalid {
                reason: "duplicate variable",
                ..
            })
        ));

        let mut bad = spawn();
        bad.env = vec![v1::Env {
            name: "A=B".into(),
            value: String::new(),
        }];
        assert!(bad.validate().is_err());
    }

    #[test]
    fn exit_code_requires_an_exited_pane() {
        let pane = |life, exit_code| v1::PaneState {
            life: life as i32,
            exit_code,
            ..Default::default()
        };
        pane(v1::PaneLife::Exited, Some(0))
            .validate()
            .expect("exited with status");
        pane(v1::PaneLife::Exited, None).validate().expect("exited by signal");
        assert!(pane(v1::PaneLife::Alive, Some(0)).validate().is_err());
        assert!(pane(v1::PaneLife::Missing, None).validate().is_ok());
    }

    #[test]
    fn chunk_bounds_and_digests_are_checked() {
        let delta = |len: usize, sha: usize| v1::TranscriptDelta {
            generation: "g".into(),
            data: vec![0; len],
            sha256: vec![0; sha],
            ..Default::default()
        };
        delta(MAX_CHUNK_BYTES, DIGEST_BYTES).validate().expect("at the limit");
        assert!(matches!(
            delta(MAX_CHUNK_BYTES + 1, 0).validate(),
            Err(ValidationError::TooLarge { .. })
        ));
        assert!(delta(1, 5).validate().is_err());
    }

    #[test]
    fn hello_requires_identity_epochs_and_nonce() {
        let hello = || v1::Hello {
            host_id: "mac-1".into(),
            journal_id: "j".into(),
            boot_id: "b".into(),
            min_epoch: 1,
            max_epoch: 1,
            instance_nonce: vec![1; 16],
            ..Default::default()
        };
        hello().validate().expect("valid");

        let mut bad = hello();
        bad.host_id = "a:b".into();
        assert!(bad.validate().is_err());
        let mut bad = hello();
        bad.max_epoch = 0;
        assert!(bad.validate().is_err());
        let mut bad = hello();
        bad.instance_nonce.clear();
        assert!(bad.validate().is_err());
    }
}
