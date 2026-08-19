//! Wire-level types shared between `boss-engine` and the `boss` CLI.
//!
//! Anything that goes over the engine's frontend socket — both the
//! request/response envelope and the data shapes those carry — lives in this
//! crate so that engine and clients link against the same types.

mod boothby;
mod engine_app;
mod health_wire;
mod host_registry_wire;
mod hosted_pane_status;
mod live_status_debug;
mod live_worker_state;
mod metrics_wire;
pub mod planner;
mod tmux_worker_status;
mod types;
mod wire;
mod work_item_id;
mod worker_event;
mod worker_names;

pub use boothby::*;
pub use engine_app::*;
pub use health_wire::*;
pub use host_registry_wire::*;
pub use hosted_pane_status::*;
pub use live_status_debug::*;
pub use live_worker_state::*;
pub use metrics_wire::*;
pub use planner::{
    ApplyResult, Confidence, DocRef, PlannerInput, PlannerOutput, ProductContext, ProjectContext, ProposedEdge,
    ProposedMergeOrderHint, ProposedTask, TaskBrief, planner_output_schema,
};
pub use tmux_worker_status::*;
pub use types::*;
pub use wire::*;
pub use work_item_id::{
    WORK_ITEM_ID_AMBIGUOUS_MARKER, WORK_ITEM_ID_NOT_FOUND_MARKER, WORK_ITEM_ID_VALUE_NAME, WorkItemSelector,
    is_friendly_work_item_selector, is_typed_work_item_id, parse_short_id_number, parse_work_item_selector,
    short_id_wire_form,
};
pub use worker_event::*;
pub use worker_names::*;
