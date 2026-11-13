//! Online worker reads use the same durable cursor and command effects as
//! proposals. A request id is scoped to its assignment, never to a host alone.

use super::*;
use boss_protocol::FrontendEvent;
use boss_protocol_spoke::v1::{WorkerMethod, WorkerReply};

pub const WORKER_REPLY: &str = "worker_reply";

/// Exactly the methods emitted by worker-api/1. Proposal status is reserved
/// on the wire; the gateway lists its local receipts instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRead {
    Context,
    PrStatus { refresh: bool },
    PrBody,
}

pub enum WorkerResponse {
    Read(Box<FrontendEvent>),
    Denied(String),
    Retry(String),
}

impl Inner {
    pub(super) async fn apply_worker(
        &self,
        observation: WorkerObservation,
        currentness: Currentness,
        owed: &mut Vec<Owed>,
    ) -> Result<(), Interrupted> {
        let request = &observation.request;
        let key = format!("{}:{}", observation.origin.run.run_id, request.request_id);
        let (host, lookup) = (observation.origin.host_id.clone(), key.clone());
        if self
            .blocking(move |db| db.spoke_effect_exists(&host, WORKER_REPLY, &lookup))
            .await?
        {
            return Ok(());
        }
        let method = match WorkerMethod::try_from(request.method) {
            Ok(WorkerMethod::RunContext) if !request.refresh && request.proposal_id.is_empty() => {
                Some(WorkerRead::Context)
            }
            Ok(WorkerMethod::PrStatus) if request.proposal_id.is_empty() => Some(WorkerRead::PrStatus {
                refresh: request.refresh,
            }),
            Ok(WorkerMethod::PrBody) if !request.refresh && request.proposal_id.is_empty() => Some(WorkerRead::PrBody),
            _ => None,
        };
        let response = match (currentness, method) {
            (Currentness::Superseded(reason), _) => {
                WorkerResponse::Denied(format!("worker_assignment_denied: {reason}"))
            }
            (_, None) => WorkerResponse::Denied("worker_method_denied: method is outside worker-api/1".into()),
            (Currentness::Current, Some(method)) => self.applier.worker_request(&observation, method).await,
        };
        let mut reply = WorkerReply {
            request_id: request.request_id.clone(),
            ..Default::default()
        };
        match response {
            WorkerResponse::Retry(reason) => return Err(Interrupted::Retry(reason)),
            WorkerResponse::Denied(reason) => {
                reply.code = ResultCode::Rejected as i32;
                reply.reason = reason.chars().take(MAX_REASON_CHARS).collect();
            }
            WorkerResponse::Read(event) => {
                let bytes = serde_json::to_vec(&event).map_err(|error| Interrupted::Retry(error.to_string()))?;
                if bytes.len() > MAX_PAYLOAD_BYTES {
                    reply.code = ResultCode::Rejected as i32;
                    reply.reason = "worker_reply_too_large: response exceeds the wire payload limit".into();
                } else {
                    reply.code = ResultCode::Ok as i32;
                    reply.revision = hex(&Sha256::digest(&bytes));
                    reply.observed_unix_ms = unix_ms(SystemTime::now());
                    reply.result_json = bytes;
                }
            }
        }
        let command = Command {
            command_id: format!("worker-reply-{}", hex(&Sha256::digest(key.as_bytes())[..12])),
            run: Some(observation.origin.run),
            body: Some(command::Body::WorkerReply(reply)),
            ..Default::default()
        };
        owed.push(Owed {
            kind: WORKER_REPLY,
            key,
            payload: command.encode_to_vec(),
            failure: None,
        });
        Ok(())
    }
}
