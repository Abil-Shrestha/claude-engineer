//! Events: the only way state changes.
//!
//! Every change to a run is an [`EventKind`] appended to the run's log. The
//! log is the source of truth for crash recovery, for the live UI (which
//! subscribes to it) and for replay/time-travel debugging.

use serde::{Deserialize, Serialize};

use crate::budget::{BudgetExceeded, Usage};
use crate::ids::{ApprovalId, AttemptId, RunId, TaskId, WorkspaceId};
use crate::model::{
    AgentRef, ApprovalKind, AttemptOutcome, CheckResult, Decision, Finding, ReviewVerdict, RunSpec,
    RunStatus, TaskSpec, TaskStatus,
};

/// A stored event: a change plus where it sits in the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct Event {
    /// Position in the global log. Gapless and strictly increasing, so a
    /// client that saw `seq = n` resumes with "everything after n".
    pub seq: u64,
    pub run_id: RunId,
    /// Unix time in milliseconds.
    pub at_ms: i64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    RunCreated {
        spec: RunSpec,
    },
    RunStatusChanged {
        status: RunStatus,
        /// The pipeline stage the run is in, when running.
        stage: Option<String>,
        reason: Option<String>,
    },
    PlanProposed {
        summary: String,
        tasks: Vec<TaskSpec>,
    },
    TaskCreated {
        task_id: TaskId,
        spec: TaskSpec,
    },
    TaskStatusChanged {
        task_id: TaskId,
        status: TaskStatus,
        reason: Option<String>,
    },
    AttemptStarted {
        attempt_id: AttemptId,
        task_id: TaskId,
        /// 1-based attempt number for this task.
        number: u32,
        agent: AgentRef,
        workspace_id: WorkspaceId,
        branch: String,
    },
    /// Something the agent did, normalized across runtimes.
    Agent {
        attempt_id: AttemptId,
        event: AgentEvent,
    },
    CheckCompleted {
        attempt_id: AttemptId,
        result: CheckResult,
    },
    ReviewCompleted {
        attempt_id: AttemptId,
        reviewer: AgentRef,
        verdict: ReviewVerdict,
        findings: Vec<Finding>,
    },
    AttemptFinished {
        attempt_id: AttemptId,
        outcome: AttemptOutcome,
        usage: Usage,
    },
    ApprovalRequested {
        approval_id: ApprovalId,
        kind: ApprovalKind,
        title: String,
        details: String,
        task_id: Option<TaskId>,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        decision: Decision,
        by: String,
        comment: Option<String>,
    },
    BranchIntegrated {
        task_id: TaskId,
        branch: String,
        commit: String,
    },
    /// A verified attempt could not be integrated (merge conflict, or checks
    /// failing on the combined code); the task will be retried on top of the
    /// new integration head.
    IntegrationFailed {
        task_id: TaskId,
        attempt_id: AttemptId,
        reason: String,
    },
    PullRequestOpened {
        url: String,
        number: Option<u64>,
    },
    BudgetExceeded {
        exceeded: BudgetExceeded,
    },
}

impl EventKind {
    /// The event's `type` tag, handy for logs and indexing.
    pub fn name(&self) -> &'static str {
        match self {
            Self::RunCreated { .. } => "run_created",
            Self::RunStatusChanged { .. } => "run_status_changed",
            Self::PlanProposed { .. } => "plan_proposed",
            Self::TaskCreated { .. } => "task_created",
            Self::TaskStatusChanged { .. } => "task_status_changed",
            Self::AttemptStarted { .. } => "attempt_started",
            Self::Agent { .. } => "agent",
            Self::CheckCompleted { .. } => "check_completed",
            Self::ReviewCompleted { .. } => "review_completed",
            Self::AttemptFinished { .. } => "attempt_finished",
            Self::ApprovalRequested { .. } => "approval_requested",
            Self::ApprovalResolved { .. } => "approval_resolved",
            Self::BranchIntegrated { .. } => "branch_integrated",
            Self::IntegrationFailed { .. } => "integration_failed",
            Self::PullRequestOpened { .. } => "pull_request_opened",
            Self::BudgetExceeded { .. } => "budget_exceeded",
        }
    }
}

/// What an agent did, normalized across Claude Code, Codex, ACP agents and
/// any other runtime. Adapters map their native stream into these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    SessionStarted {
        /// The runtime's own session id, used to resume.
        session_id: Option<String>,
        model: Option<String>,
    },
    /// Assistant-visible text.
    Message { text: String },
    /// Reasoning summary or progress note, when the runtime exposes one.
    Thinking { text: String },
    ToolCall {
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    /// The agent is blocked waiting for a permission decision.
    PermissionRequested {
        request_id: String,
        tool: String,
        input: serde_json::Value,
    },
    /// Cumulative usage for the attempt so far (not a delta), so a dropped or
    /// repeated report never skews totals.
    Usage { usage: Usage },
    /// Diagnostics that are not part of the conversation (stderr, warnings).
    Log { level: LogLevel, message: String },
    /// The agent's turn ended.
    Finished {
        success: bool,
        summary: Option<String>,
        usage: Usage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_with_a_flat_type_tag() {
        let event = Event {
            seq: 7,
            run_id: RunId::new(),
            at_ms: 1_700_000_000_000,
            kind: EventKind::TaskStatusChanged {
                task_id: TaskId::new(),
                status: TaskStatus::Ready,
                reason: None,
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "task_status_changed");
        assert_eq!(json["status"], "ready");
        assert_eq!(json["seq"], 7);
        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn name_matches_serde_tag() {
        let kind = EventKind::Agent {
            attempt_id: AttemptId::new(),
            event: AgentEvent::Message { text: "hi".into() },
        };
        let json = serde_json::to_value(&kind).unwrap();
        assert_eq!(json["type"], kind.name());
        assert_eq!(json["event"]["type"], "message");
    }
}
