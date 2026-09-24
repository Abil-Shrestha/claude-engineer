//! Forgeline core: the domain model, event types and pure scheduling logic.
//!
//! This crate does no I/O. Everything that touches the outside world (agents,
//! git, the database, the network) lives in other crates and talks to the core
//! through these types.

pub mod budget;
pub mod graph;
pub mod ids;

pub use budget::{Budget, BudgetExceeded, Usage};
pub use graph::{DepGraph, GraphError};
pub use ids::{ApprovalId, AttemptId, ParseIdError, RunId, TaskId, WorkspaceId};
