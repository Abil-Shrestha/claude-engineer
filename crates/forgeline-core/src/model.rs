//! The domain model: runs, tasks, attempts, checks, reviews and approvals.
//!
//! These are plain data types. State changes happen only by appending
//! [`crate::events::EventKind`]s; [`crate::state::RunState`] folds them into
//! the current picture.

use serde::{Deserialize, Serialize};

use crate::budget::Budget;

/// Where a run's work came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkSource {
    /// Typed into the CLI or UI.
    Manual,
    GithubIssue {
        repo: String,
        number: u64,
    },
    Linear {
        key: String,
    },
    /// A task file checked into the repository.
    File {
        path: String,
    },
}

/// Everything needed to start a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct RunSpec {
    pub title: String,
    /// The request itself: an issue body, a feature description, a bug report.
    pub request: String,
    pub source: WorkSource,
    /// Branch or commit the run's work forks from.
    pub base_ref: String,
    /// Name of the pipeline (stages and gates) this run follows.
    pub pipeline: String,
    #[serde(default)]
    pub budget: Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    /// Blocked on a human decision.
    WaitingForApproval,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// One unit of work in a run's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct TaskSpec {
    /// Stable, human-readable key within the run (e.g. `T001`). Plans refer to
    /// dependencies by key so they can be written before ids exist.
    pub key: String,
    pub title: String,
    pub description: String,
    /// Which agent role executes the task (e.g. `implementer`).
    pub role: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Acceptance criteria the reviewer and the converge step check against.
    #[serde(default)]
    pub acceptance: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting for dependencies.
    Pending,
    /// Dependencies done; waiting for a free agent slot.
    Ready,
    Running,
    Done,
    /// Out of attempts or failed in a way retries cannot fix.
    Failed,
    /// Not run because something it depends on failed.
    Skipped,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Skipped | Self::Cancelled
        )
    }
}

/// Which agent runtime (and model) did something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct AgentRef {
    /// Adapter name, e.g. `claude-code`, `codex`, `acp:gemini`.
    pub runtime: String,
    pub model: Option<String>,
}

/// Why an attempt failed. The engine's retry policy keys off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The agent crashed, errored or gave up.
    Agent,
    /// Deterministic checks (build, lint, tests) failed.
    Verification,
    /// The reviewer requested changes and the fix-loop cap was reached.
    Review,
    Budget,
    Timeout,
    /// Our side broke (sandbox, git, network) — not the agent's fault.
    Infrastructure,
    MergeConflict,
}

impl FailureKind {
    /// Whether another attempt could plausibly succeed.
    pub fn is_retryable(self) -> bool {
        !matches!(self, Self::Budget)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AttemptOutcome {
    Succeeded {
        /// Commit holding the attempt's work, if it changed anything.
        commit: Option<String>,
    },
    Failed {
        kind: FailureKind,
        message: String,
    },
    Cancelled,
}

/// Result of a deterministic check (build, lint, test command).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct CheckResult {
    pub name: String,
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// The last lines of combined output, for display and for feeding back to
    /// the agent on failure.
    pub output_tail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    /// The reviewer believes the task or spec itself is wrong; a human decides.
    Escalate,
}

/// How a review finding should be handled (triage categories).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A defect in the change; send back to the implementer.
    Patch,
    /// The spec or plan is wrong or incomplete.
    BadSpec,
    /// The change does not match what was asked for; needs a human.
    IntentGap,
    /// Real but out of scope; file as follow-up work.
    Defer,
    /// Not actually a problem.
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Finding {
    pub kind: FindingKind,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Plan,
    Merge,
    /// An agent asked a question only a human can answer.
    Question,
    /// An agent wants to run a tool its policy does not allow by default.
    Permission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    Rejected,
}
