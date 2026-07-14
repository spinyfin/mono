//! Self-contained leaf utilities extracted from `boss-engine` (engine/core).
//!
//! These are small, pure, dependency-light helpers with no reference back to
//! the rest of the engine. They live in their own crate so that changes to
//! the ~220k-line `engine/core` monolith do not force them to recompile, and
//! vice versa. The dependency edge is strictly one-directional: `engine/core`
//! depends on `boss_engine_utils`, never the reverse.

pub mod env_parse;
pub mod epoch_time;
pub mod iso8601;
pub mod json_extract;
pub mod string_clip;
