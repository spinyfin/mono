mod bazel_module_lock;
mod cargo_lock;
mod registry_append_union;

pub use bazel_module_lock::BazelModuleLockResolver;
pub use cargo_lock::CargoLockResolver;
pub use registry_append_union::RegistryAppendUnionResolver;
