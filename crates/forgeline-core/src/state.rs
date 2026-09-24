//! Folding a run's event log into its current state.
//!
//! `RunState::apply` is the single place where events turn into state. The
//! engine uses it to decide what to do next, the server uses it to answer
//! queries, and replaying the log through it after a crash reproduces exactly
//! the state before the crash.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::budget::Usage;
use crate::events::{AgentEvent, Event, EventKind};
use crate::graph::DepGraph;
use crate::ids::{ApprovalId, AttemptId, RunId, TaskId, WorkspaceId};
use crate::model::{
    AgentRef, ApprovalKind, AttemptOutcome, CheckResult, Decision, Finding, ReviewVerdict, RunSpec,
    RunStatus, TaskSpec, TaskStatus,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("the first event of a run must be `run_created`, got `{0}`")]
    NotCreated(&'static str),
    #[error("run already created")]
    AlreadyCreated,
    #[error("event for run {got} applied to run {expected}")]
    WrongRun { expected: RunId, got: RunId },
    #[error("event seq {got} is not after {last}")]
    OutOfOrder { last: u64, got: u64 },
    #[error("unknown task {0}")]
    UnknownTask(TaskId),
    #[error("task key `{0}` is already used")]
    DuplicateTaskKey(String),
    #[error("unknown attempt {0}")]
    UnknownAttempt(AttemptId),
    #[error("unknown approval {0}")]
    UnknownApproval(ApprovalId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskState {
    pub id: TaskId,
    pub spec: TaskSpec,
    pub status: TaskStatus,
    pub status_reason: Option<String>,
    pub attempts: Vec<AttemptId>,
    /// Commit on the integration branch once the task's work has landed.
    pub integrated_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: AgentRef,
    pub verdict: ReviewVerdict,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptState {
    pub id: AttemptId,
    pub task_id: TaskId,
    pub number: u32,
    pub agent: AgentRef,
    pub workspace_id: WorkspaceId,
    pub branch: String,
    /// The runtime's session id, once known (used to resume the session).
    pub session_id: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub outcome: Option<AttemptOutcome>,
    /// Latest cumulative usage reported for this attempt.
    pub usage: Usage,
    pub checks: Vec<CheckResult>,
    pub reviews: Vec<Review>,
    /// Number of agent activity events seen (the events themselves stay in
    /// the log; the UI pages through them).
    pub activity_count: u64,
}

impl AttemptState {
    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub decision: Decision,
    pub by: String,
    pub comment: Option<String>,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalState {
    pub id: ApprovalId,
    pub kind: ApprovalKind,
    pub title: String,
    pub details: String,
    pub task_id: Option<TaskId>,
    pub requested_at_ms: i64,
    pub resolution: Option<Resolution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub url: String,
    pub number: Option<u64>,
}

/// The current state of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub id: RunId,
    pub spec: RunSpec,
    pub status: RunStatus,
    pub stage: Option<String>,
    pub status_reason: Option<String>,
    pub plan_summary: Option<String>,
    pub tasks: BTreeMap<TaskId, TaskState>,
    pub attempts: BTreeMap<AttemptId, AttemptState>,
    pub approvals: BTreeMap<ApprovalId, ApprovalState>,
    pub pull_request: Option<PullRequest>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Sequence number of the last event applied.
    pub last_seq: u64,
    task_keys: BTreeMap<String, TaskId>,
}

impl RunState {
    /// Starts a run's state from its `run_created` event.
    pub fn new(event: &Event) -> Result<Self, ApplyError> {
        let EventKind::RunCreated { spec } = &event.kind else {
            return Err(ApplyError::NotCreated(event.kind.name()));
        };
        Ok(Self {
            id: event.run_id,
            spec: spec.clone(),
            status: RunStatus::Pending,
            stage: None,
            status_reason: None,
            plan_summary: None,
            tasks: BTreeMap::new(),
            attempts: BTreeMap::new(),
            approvals: BTreeMap::new(),
            pull_request: None,
            created_at_ms: event.at_ms,
            updated_at_ms: event.at_ms,
            last_seq: event.seq,
            task_keys: BTreeMap::new(),
        })
    }

    /// Rebuilds a run's state from its full log.
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a Event>) -> Result<Self, ApplyError> {
        let mut events = events.into_iter();
        let first = events.next().ok_or(ApplyError::NotCreated("<none>"))?;
        let mut state = Self::new(first)?;
        for event in events {
            state.apply(event)?;
        }
        Ok(state)
    }

    /// Applies one event. Events must belong to this run and arrive in order.
    pub fn apply(&mut self, event: &Event) -> Result<(), ApplyError> {
        if event.run_id != self.id {
            return Err(ApplyError::WrongRun {
                expected: self.id,
                got: event.run_id,
            });
        }
        if event.seq <= self.last_seq {
            return Err(ApplyError::OutOfOrder {
                last: self.last_seq,
                got: event.seq,
            });
        }

        match &event.kind {
            EventKind::RunCreated { .. } => return Err(ApplyError::AlreadyCreated),
            EventKind::RunStatusChanged {
                status,
                stage,
                reason,
            } => {
                self.status = *status;
                self.stage.clone_from(stage);
                self.status_reason.clone_from(reason);
            }
            EventKind::PlanProposed { summary, .. } => {
                self.plan_summary = Some(summary.clone());
            }
            EventKind::TaskCreated { task_id, spec } => {
                if self.task_keys.contains_key(&spec.key) {
                    return Err(ApplyError::DuplicateTaskKey(spec.key.clone()));
                }
                self.task_keys.insert(spec.key.clone(), *task_id);
                self.tasks.insert(
                    *task_id,
                    TaskState {
                        id: *task_id,
                        spec: spec.clone(),
                        status: TaskStatus::Pending,
                        status_reason: None,
                        attempts: Vec::new(),
                        integrated_commit: None,
                    },
                );
            }
            EventKind::TaskStatusChanged {
                task_id,
                status,
                reason,
            } => {
                let task = self.task_mut(*task_id)?;
                task.status = *status;
                task.status_reason.clone_from(reason);
            }
            EventKind::AttemptStarted {
                attempt_id,
                task_id,
                number,
                agent,
                workspace_id,
                branch,
            } => {
                self.task_mut(*task_id)?.attempts.push(*attempt_id);
                self.attempts.insert(
                    *attempt_id,
                    AttemptState {
                        id: *attempt_id,
                        task_id: *task_id,
                        number: *number,
                        agent: agent.clone(),
                        workspace_id: *workspace_id,
                        branch: branch.clone(),
                        session_id: None,
                        started_at_ms: event.at_ms,
                        finished_at_ms: None,
                        outcome: None,
                        usage: Usage::default(),
                        checks: Vec::new(),
                        reviews: Vec::new(),
                        activity_count: 0,
                    },
                );
            }
            EventKind::Agent {
                attempt_id,
                event: agent_event,
            } => {
                let attempt = self.attempt_mut(*attempt_id)?;
                attempt.activity_count += 1;
                match agent_event {
                    AgentEvent::SessionStarted { session_id, .. } => {
                        if session_id.is_some() {
                            attempt.session_id.clone_from(session_id);
                        }
                    }
                    AgentEvent::Usage { usage } | AgentEvent::Finished { usage, .. } => {
                        attempt.usage = *usage;
                    }
                    _ => {}
                }
            }
            EventKind::CheckCompleted { attempt_id, result } => {
                self.attempt_mut(*attempt_id)?.checks.push(result.clone());
            }
            EventKind::ReviewCompleted {
                attempt_id,
                reviewer,
                verdict,
                findings,
            } => {
                self.attempt_mut(*attempt_id)?.reviews.push(Review {
                    reviewer: reviewer.clone(),
                    verdict: *verdict,
                    findings: findings.clone(),
                });
            }
            EventKind::AttemptFinished {
                attempt_id,
                outcome,
                usage,
            } => {
                let attempt = self.attempt_mut(*attempt_id)?;
                attempt.outcome = Some(outcome.clone());
                attempt.usage = *usage;
                attempt.finished_at_ms = Some(event.at_ms);
            }
            EventKind::ApprovalRequested {
                approval_id,
                kind,
                title,
                details,
                task_id,
            } => {
                self.approvals.insert(
                    *approval_id,
                    ApprovalState {
                        id: *approval_id,
                        kind: *kind,
                        title: title.clone(),
                        details: details.clone(),
                        task_id: *task_id,
                        requested_at_ms: event.at_ms,
                        resolution: None,
                    },
                );
            }
            EventKind::ApprovalResolved {
                approval_id,
                decision,
                by,
                comment,
            } => {
                let approval = self
                    .approvals
                    .get_mut(approval_id)
                    .ok_or(ApplyError::UnknownApproval(*approval_id))?;
                approval.resolution = Some(Resolution {
                    decision: *decision,
                    by: by.clone(),
                    comment: comment.clone(),
                    at_ms: event.at_ms,
                });
            }
            EventKind::BranchIntegrated {
                task_id, commit, ..
            } => {
                self.task_mut(*task_id)?.integrated_commit = Some(commit.clone());
            }
            EventKind::PullRequestOpened { url, number } => {
                self.pull_request = Some(PullRequest {
                    url: url.clone(),
                    number: *number,
                });
            }
            EventKind::BudgetExceeded { .. } => {}
        }

        self.last_seq = event.seq;
        self.updated_at_ms = event.at_ms;
        Ok(())
    }

    pub fn task_by_key(&self, key: &str) -> Option<&TaskState> {
        self.task_keys.get(key).and_then(|id| self.tasks.get(id))
    }

    /// The task dependency graph, keyed by task id.
    pub fn graph(&self) -> DepGraph<TaskId> {
        let mut graph = DepGraph::new();
        for id in self.tasks.keys() {
            graph.add_node(*id);
        }
        for (id, task) in &self.tasks {
            for dep_key in &task.spec.depends_on {
                if let Some(dep) = self.task_keys.get(dep_key) {
                    // Plans are validated before tasks are created, so this
                    // cannot introduce unknown nodes or self-edges.
                    let _ = graph.add_dependency(id, *dep);
                }
            }
        }
        graph
    }

    /// Tasks that are not yet running or finished and whose dependencies are
    /// all done, in a stable order.
    pub fn ready_tasks(&self) -> Vec<TaskId> {
        let done: BTreeSet<TaskId> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Done)
            .map(|t| t.id)
            .collect();
        let not_waiting: BTreeSet<TaskId> = self
            .tasks
            .values()
            .filter(|t| !matches!(t.status, TaskStatus::Pending | TaskStatus::Ready))
            .map(|t| t.id)
            .collect();
        self.graph().ready(&done, &not_waiting)
    }

    /// Approvals still waiting for a human.
    pub fn pending_approvals(&self) -> impl Iterator<Item = &ApprovalState> {
        self.approvals.values().filter(|a| a.resolution.is_none())
    }

    /// Usage summed over every attempt in the run.
    pub fn total_usage(&self) -> Usage {
        self.attempts.values().map(|a| a.usage).sum()
    }

    fn task_mut(&mut self, id: TaskId) -> Result<&mut TaskState, ApplyError> {
        self.tasks.get_mut(&id).ok_or(ApplyError::UnknownTask(id))
    }

    fn attempt_mut(&mut self, id: AttemptId) -> Result<&mut AttemptState, ApplyError> {
        self.attempts
            .get_mut(&id)
            .ok_or(ApplyError::UnknownAttempt(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FailureKind, WorkSource};

    struct Log {
        run_id: RunId,
        events: Vec<Event>,
    }

    impl Log {
        fn new() -> Self {
            let mut log = Self {
                run_id: RunId::new(),
                events: Vec::new(),
            };
            log.push(EventKind::RunCreated {
                spec: RunSpec {
                    title: "Add dark mode".into(),
                    request: "Users want dark mode".into(),
                    source: WorkSource::Manual,
                    base_ref: "main".into(),
                    pipeline: "default".into(),
                    budget: Default::default(),
                },
            });
            log
        }

        fn push(&mut self, kind: EventKind) {
            let seq = self.events.len() as u64 + 1;
            self.events.push(Event {
                seq,
                run_id: self.run_id,
                at_ms: 1_000 * seq as i64,
                kind,
            });
        }

        fn task(&mut self, key: &str, deps: &[&str]) -> TaskId {
            let task_id = TaskId::new();
            self.push(EventKind::TaskCreated {
                task_id,
                spec: TaskSpec {
                    key: key.into(),
                    title: key.into(),
                    description: String::new(),
                    role: "implementer".into(),
                    depends_on: deps.iter().map(|d| d.to_string()).collect(),
                    acceptance: Vec::new(),
                },
            });
            task_id
        }

        fn status(&mut self, task_id: TaskId, status: TaskStatus) {
            self.push(EventKind::TaskStatusChanged {
                task_id,
                status,
                reason: None,
            });
        }

        fn state(&self) -> RunState {
            RunState::replay(&self.events).unwrap()
        }
    }

    #[test]
    fn replays_tasks_attempts_and_usage() {
        let mut log = Log::new();
        let a = log.task("T1", &[]);
        let b = log.task("T2", &["T1"]);
        log.status(a, TaskStatus::Running);
        let attempt_id = AttemptId::new();
        log.push(EventKind::AttemptStarted {
            attempt_id,
            task_id: a,
            number: 1,
            agent: AgentRef {
                runtime: "mock".into(),
                model: None,
            },
            workspace_id: WorkspaceId::new(),
            branch: "fl/t1-1".into(),
        });
        log.push(EventKind::Agent {
            attempt_id,
            event: AgentEvent::SessionStarted {
                session_id: Some("sess-1".into()),
                model: None,
            },
        });
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cost_usd: 0.01,
            ..Usage::default()
        };
        log.push(EventKind::Agent {
            attempt_id,
            event: AgentEvent::Usage { usage },
        });
        log.push(EventKind::AttemptFinished {
            attempt_id,
            outcome: AttemptOutcome::Succeeded {
                commit: Some("abc".into()),
            },
            usage,
        });
        log.status(a, TaskStatus::Done);

        let state = log.state();
        assert_eq!(state.tasks[&a].status, TaskStatus::Done);
        assert_eq!(state.tasks[&a].attempts, vec![attempt_id]);
        let attempt = &state.attempts[&attempt_id];
        assert_eq!(attempt.session_id.as_deref(), Some("sess-1"));
        assert_eq!(attempt.activity_count, 2);
        assert!(attempt.is_finished());
        assert_eq!(state.total_usage(), usage);
        assert_eq!(state.ready_tasks(), vec![b]);
        assert_eq!(state.task_by_key("T2").unwrap().id, b);
        assert_eq!(state.last_seq, log.events.len() as u64);
    }

    #[test]
    fn ready_tasks_wait_for_dependencies_and_skip_active_ones() {
        let mut log = Log::new();
        let a = log.task("A", &[]);
        let b = log.task("B", &[]);
        let c = log.task("C", &["A", "B"]);
        assert_eq!(log.state().ready_tasks(), vec![a, b]);

        log.status(a, TaskStatus::Running);
        assert_eq!(log.state().ready_tasks(), vec![b]);

        log.status(a, TaskStatus::Done);
        log.status(b, TaskStatus::Failed);
        assert!(log.state().ready_tasks().is_empty());
        assert_eq!(log.state().graph().transitive_dependents(&b), [c].into());
    }

    #[test]
    fn approvals_resolve() {
        let mut log = Log::new();
        let approval_id = ApprovalId::new();
        log.push(EventKind::ApprovalRequested {
            approval_id,
            kind: ApprovalKind::Plan,
            title: "Approve plan".into(),
            details: "3 tasks".into(),
            task_id: None,
        });
        assert_eq!(log.state().pending_approvals().count(), 1);
        log.push(EventKind::ApprovalResolved {
            approval_id,
            decision: Decision::Approved,
            by: "abil".into(),
            comment: None,
        });
        let state = log.state();
        assert_eq!(state.pending_approvals().count(), 0);
        assert_eq!(
            state.approvals[&approval_id]
                .resolution
                .as_ref()
                .unwrap()
                .decision,
            Decision::Approved
        );
    }

    #[test]
    fn rejects_inconsistent_logs() {
        let mut log = Log::new();
        let mut state = log.state();
        let ghost = TaskId::new();
        let bad = Event {
            seq: 2,
            run_id: log.run_id,
            at_ms: 0,
            kind: EventKind::TaskStatusChanged {
                task_id: ghost,
                status: TaskStatus::Done,
                reason: None,
            },
        };
        assert_eq!(state.apply(&bad), Err(ApplyError::UnknownTask(ghost)));

        let stale = Event {
            seq: 1,
            ..bad.clone()
        };
        assert_eq!(
            state.apply(&stale),
            Err(ApplyError::OutOfOrder { last: 1, got: 1 })
        );

        let other_run = Event {
            run_id: RunId::new(),
            ..bad
        };
        assert!(matches!(
            state.apply(&other_run),
            Err(ApplyError::WrongRun { .. })
        ));

        log.task("T1", &[]);
        log.task("T1", &[]);
        assert_eq!(
            RunState::replay(&log.events),
            Err(ApplyError::DuplicateTaskKey("T1".into()))
        );
    }

    #[test]
    fn failed_attempt_outcome_is_recorded() {
        let mut log = Log::new();
        let t = log.task("T1", &[]);
        let attempt_id = AttemptId::new();
        log.push(EventKind::AttemptStarted {
            attempt_id,
            task_id: t,
            number: 1,
            agent: AgentRef {
                runtime: "mock".into(),
                model: None,
            },
            workspace_id: WorkspaceId::new(),
            branch: "b".into(),
        });
        log.push(EventKind::AttemptFinished {
            attempt_id,
            outcome: AttemptOutcome::Failed {
                kind: FailureKind::Verification,
                message: "tests failed".into(),
            },
            usage: Usage::default(),
        });
        let state = log.state();
        assert!(matches!(
            state.attempts[&attempt_id].outcome,
            Some(AttemptOutcome::Failed {
                kind: FailureKind::Verification,
                ..
            })
        ));
    }
}
