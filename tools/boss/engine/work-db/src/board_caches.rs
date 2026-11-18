//! The engine-wide caches a board assembly may draw on, so the production
//! load path and the benchmarks hand `assemble_work_tree` one handle.
use super::board_review_cache::BoardReviewCache;
use super::board_runtime_cache::BoardRuntimeCache;

#[derive(Default)]
pub struct BoardCaches {
    /// Per-item `TaskRuntime`, validated by `board_runtime_epoch`.
    pub runtimes: BoardRuntimeCache,
    /// Per-owner AI-review projection, validated by `board_review_epoch`.
    pub reviews: BoardReviewCache,
}
