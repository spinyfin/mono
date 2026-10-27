//! Host filesystem execution shared by the hub's local adapter and the spoke.
//! Work policy, GitHub decisions and scheduling remain in the hub.
pub mod cube;
pub mod cube_op_budget;
pub mod input;
pub mod types;
mod validation;
pub mod workspace;

pub use validation::validate_workspace;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recovery_tests;
