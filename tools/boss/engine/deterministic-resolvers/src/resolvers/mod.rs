mod generated_lock;
pub(crate) use generated_lock::GeneratedLockResolver;
pub use generated_lock::is_generated_lockfile;
mod bazel_module_lock;
mod cargo_lock;
mod keep_both;
mod recipe;

pub use bazel_module_lock::BazelModuleLockResolver;
pub use cargo_lock::CargoLockResolver;
pub use keep_both::{KeepBothRuleResolver, KeepBothUnionResolver, rustfmt_command};
pub use recipe::{InvalidRecipe, RecipeResolver};
