//! Host filesystem execution shared by the hub's local adapter and the spoke.
//! Work policy, GitHub decisions and scheduling remain in the hub.
pub mod capture;
pub mod cube;
pub mod cube_op_budget;
pub mod homes;
pub mod input;
pub mod process;
pub mod recovery_ops;
/// Recovery-PR base lookup shared by every `recovery_pr_base` default.
pub mod recovery_pr;
pub mod types;
mod validation;
pub mod worker;
pub mod workspace;

pub use validation::validate_workspace;

#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
