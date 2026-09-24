//! Bodega core: the domain model, event types and pure scheduling logic.
//!
//! This crate does no I/O. Everything that touches the outside world (agents,
//! git, the database, the network) lives in other crates and talks to the core
//! through these types.

pub mod budget;
pub mod events;
pub mod graph;
pub mod ids;
pub mod model;
pub mod state;

pub use budget::{Budget, BudgetExceeded, Usage};
pub use events::{AgentEvent, Event, EventKind, LogLevel};
pub use graph::{DepGraph, GraphError};
pub use ids::{ApprovalId, AttemptId, ParseIdError, RunId, TaskId, WorkspaceId};
pub use model::*;
pub use state::{ApplyError, ApprovalState, AttemptState, RunState, TaskState};
