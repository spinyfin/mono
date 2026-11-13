//! Admission lane of a request, taken from the shared request contract.
//!
//! The contract in `boss_protocol` classifies every `FrontendRequest` with one
//! exhaustive match, so a new RPC must choose its lane (and its replay safety)
//! there. This module only gives the engine's name for the result.
use boss_protocol::{FrontendRequest, request_contract};

pub(in crate::app) use boss_protocol::AdmissionLane as ReadLane;

pub(in crate::app) fn read_lane(request: &FrontendRequest) -> ReadLane {
    request_contract(request).admission
}
