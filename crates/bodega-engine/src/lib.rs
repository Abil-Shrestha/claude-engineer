//! The Bodega engine.
//!
//! [`Engine::drive`] owns one run from start to finish:
//!
//! 1. Ready tasks (dependencies integrated) are dispatched to agents, up to
//!    the concurrency limit. Each attempt gets its own worktree and branch,
//!    starting from the run's integration branch, so it sees the work of the
//!    tasks it depends on.
//! 2. When an agent finishes, Bodega commits its work and runs the
//!    configured checks. Failing checks go back to the same agent session as
//!    feedback, for a bounded number of fix rounds.
//! 3. Verified branches are merged into the integration branch one at a time
//!    (a merge queue). Checks run again on the merged code; a conflict or a
//!    failure sends the task back for another attempt on top of the new head.
//! 4. Tasks that run out of attempts fail, and everything downstream of them
//!    is skipped. The run ends when every task is finished.
//!
//! Every step is an event in the store, so a crashed run resumes where it was
//! and the UI can show (and replay) exactly what happened.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bodega_agents::AgentRuntime;
use bodega_config::{Config, Plan};
use bodega_core::{
    AgentRef, ApprovalId, AttemptId, AttemptOutcome, Budget, Decision, EventKind, FailureKind,
    RunId, RunSpec, RunState, RunStatus, TaskId, TaskStatus, WorkSource, WorkspaceId,
};
use bodega_store::{EventStore, StoreError};
use bodega_workspace::{GitError, GitRepo, Worktree};
use tokio::task::JoinSet;

mod attempt;
pub mod checks;
pub mod prompts;
pub mod schedule;

use attempt::{AttemptContext, AttemptReport};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("invalid plan: {0}")]
    Plan(#[from] bodega_config::ConfigError),
    #[error("run {0} does not exist")]
    UnknownRun(RunId),
    #[error("no agent runtime named `{0}` is registered (known: {1})")]
    UnknownRuntime(String, String),
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

/// What to start a run with.
#[derive(Debug, Clone)]
pub struct NewRun {
    pub title: String,
    pub request: String,
    pub source: WorkSource,
    /// Branch to build on; defaults to the configured base or the current
    /// branch.
    pub base_ref: Option<String>,
    pub plan: Plan,
    pub max_cost_usd: Option<f64>,
    pub max_tokens: Option<u64>,
}

/// Drives runs for one repository.
#[derive(Clone)]
pub struct Engine {
    store: EventStore,
    repo: GitRepo,
    config: Arc<Config>,
    runtimes: BTreeMap<String, Arc<dyn AgentRuntime>>,
    state_dir: PathBuf,
}

impl Engine {
    /// `state_dir` holds worktrees (usually `<repo>/.bodega`).
    pub fn new(store: EventStore, repo: GitRepo, config: Config, state_dir: PathBuf) -> Self {
        Self {
            store,
            repo,
            config: Arc::new(config),
            runtimes: BTreeMap::new(),
            state_dir,
        }
    }

    /// Registers an agent runtime under its name.
    pub fn with_runtime(mut self, runtime: Arc<dyn AgentRuntime>) -> Self {
        self.runtimes.insert(runtime.name().to_owned(), runtime);
        self
    }

    pub fn store(&self) -> &EventStore {
        &self.store
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Records a new run with its plan. Nothing executes until [`drive`].
    ///
    /// [`drive`]: Engine::drive
    pub async fn create_run(&self, new: NewRun) -> Result<RunId> {
        new.plan.validate()?;
        for task in &new.plan.tasks {
            self.runtime_for(&task.role)?;
        }
        let base_ref = match new
            .base_ref
            .or_else(|| self.config.project.base_ref.clone())
        {
            Some(base) => base,
            None => self
                .repo
                .current_branch()
                .await?
                .unwrap_or_else(|| "HEAD".into()),
        };
        // Fail early on a base that does not exist.
        self.repo.rev_parse(&base_ref).await?;

        let limits = &self.config.limits;
        let spec = RunSpec {
            title: new.title,
            request: new.request,
            source: new.source,
            base_ref,
            pipeline: "default".into(),
            budget: Budget {
                max_cost_usd: new.max_cost_usd.or(limits.max_cost_usd),
                max_tokens: new.max_tokens.or(limits.max_tokens),
                max_wall_clock_secs: None,
                max_attempts_per_task: limits.max_attempts_per_task,
            },
        };
        let run_id = RunId::new();
        let tasks = new.plan.task_specs();
        let mut events = vec![
            EventKind::RunCreated { spec },
            EventKind::PlanProposed {
                summary: new.plan.summary.clone(),
                tasks: tasks.clone(),
            },
        ];
        events.extend(tasks.into_iter().map(|spec| EventKind::TaskCreated {
            task_id: TaskId::new(),
            spec,
        }));
        self.store.append(run_id, events, None).await?;
        Ok(run_id)
    }

    /// Runs a run until every task is finished (or its budget runs out) and
    /// returns its final state. Safe to call again after a crash: attempts
    /// that were interrupted are marked failed and retried.
    pub async fn drive(&self, run_id: RunId) -> Result<RunState> {
        let state = self.load(run_id).await?;
        if state.status.is_terminal() {
            return Ok(state);
        }
        self.recover_interrupted(&state).await?;
        let integration = self.integration_worktree(&state).await?;
        if state.status != RunStatus::Running {
            self.append(
                run_id,
                vec![EventKind::RunStatusChanged {
                    status: RunStatus::Running,
                    stage: Some("implement".into()),
                    reason: None,
                }],
            )
            .await?;
        }

        let max_parallel = self.config.limits.max_parallel_agents.max(1);
        let mut running: JoinSet<AttemptReport> = JoinSet::new();
        let mut in_flight: HashMap<tokio::task::Id, (TaskId, AttemptId)> = HashMap::new();
        loop {
            let state = self.load(run_id).await?;
            if let Some(exceeded) = state.spec.budget.check(&state.total_usage(), 0) {
                running.shutdown().await;
                return self
                    .finish(
                        &state,
                        RunStatus::Failed,
                        Some(format!("budget exceeded: {exceeded}")),
                        vec![EventKind::BudgetExceeded { exceeded }],
                    )
                    .await;
            }

            for task_id in schedule::tasks_to_start(&state, running.len(), max_parallel) {
                let ctx = self.prepare_attempt(&state, task_id, &integration).await?;
                let ids = (task_id, ctx.attempt_id);
                let handle = running.spawn(attempt::run(ctx));
                in_flight.insert(handle.id(), ids);
            }

            let Some(joined) = running.join_next_with_id().await else {
                let state = self.load(run_id).await?;
                let (status, reason) = schedule::run_outcome(&state)
                    .unwrap_or((RunStatus::Failed, Some("no task can make progress".into())));
                return self.finish(&state, status, reason, Vec::new()).await;
            };
            let report = match joined {
                Ok((id, report)) => {
                    in_flight.remove(&id);
                    report
                }
                Err(join_error) => {
                    // The attempt task panicked; record it as our failure.
                    let (task_id, attempt_id) = in_flight
                        .remove(&join_error.id())
                        .expect("every spawned attempt is tracked");
                    let outcome = AttemptOutcome::Failed {
                        kind: FailureKind::Infrastructure,
                        message: format!("the attempt crashed: {join_error}"),
                    };
                    self.store
                        .append(
                            run_id,
                            vec![EventKind::AttemptFinished {
                                attempt_id,
                                outcome: outcome.clone(),
                                usage: Default::default(),
                            }],
                            Some(format!("attempt-finished:{attempt_id}")),
                        )
                        .await?;
                    AttemptReport {
                        task_id,
                        attempt_id,
                        branch: String::new(),
                        outcome,
                    }
                }
            };
            self.handle_report(run_id, &integration, report).await?;
        }
    }

    /// Resolves a pending approval (plan, permission, question…).
    pub async fn resolve_approval(
        &self,
        run_id: RunId,
        approval_id: ApprovalId,
        decision: Decision,
        by: &str,
        comment: Option<String>,
    ) -> Result<()> {
        self.store
            .append(
                run_id,
                vec![EventKind::ApprovalResolved {
                    approval_id,
                    decision,
                    by: by.to_owned(),
                    comment,
                }],
                None,
            )
            .await?;
        Ok(())
    }

    /// The branch holding a run's integrated work.
    pub fn integration_branch(run_id: RunId) -> String {
        format!("bodega/{}/integration", short_id(run_id))
    }

    async fn load(&self, run_id: RunId) -> Result<RunState> {
        self.store
            .run(run_id)
            .await?
            .ok_or(EngineError::UnknownRun(run_id))
    }

    async fn append(&self, run_id: RunId, events: Vec<EventKind>) -> Result<()> {
        if !events.is_empty() {
            self.store.append(run_id, events, None).await?;
        }
        Ok(())
    }

    fn runtime_for(&self, role: &str) -> Result<Arc<dyn AgentRuntime>> {
        let name = self.config.agent_for(role).runtime;
        self.runtimes.get(&name).cloned().ok_or_else(|| {
            EngineError::UnknownRuntime(
                name,
                self.runtimes.keys().cloned().collect::<Vec<_>>().join(", "),
            )
        })
    }

    /// Marks attempts that were running when the process died as failed so
    /// their tasks are retried.
    async fn recover_interrupted(&self, state: &RunState) -> Result<()> {
        for attempt in state.attempts.values().filter(|a| !a.is_finished()) {
            self.store
                .append(
                    state.id,
                    vec![EventKind::AttemptFinished {
                        attempt_id: attempt.id,
                        outcome: AttemptOutcome::Failed {
                            kind: FailureKind::Infrastructure,
                            message: "interrupted: Bodega stopped while this attempt was running"
                                .into(),
                        },
                        usage: attempt.usage,
                    }],
                    Some(format!("attempt-finished:{}", attempt.id)),
                )
                .await?;
        }
        let state = self.load(state.id).await?;
        for task in state
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running)
        {
            let events = schedule::after_failure(
                &state,
                task.id,
                "interrupted by a restart",
                true,
                state.spec.budget.max_attempts_per_task,
            );
            self.append(state.id, events).await?;
        }
        Ok(())
    }

    async fn integration_worktree(&self, state: &RunState) -> Result<Worktree> {
        let branch = Self::integration_branch(state.id);
        let path = self.run_dir(state.id).join("integration");
        let registered = self
            .repo
            .list_worktrees()
            .await?
            .into_iter()
            .any(|w| w.branch.as_deref() == Some(branch.as_str()));
        if registered {
            return Ok(Worktree { path, branch });
        }
        if self.repo.branch_exists(&branch).await? {
            Ok(self.repo.attach_worktree(&path, &branch).await?)
        } else {
            Ok(self
                .repo
                .add_worktree(&path, &branch, &state.spec.base_ref)
                .await?)
        }
    }

    fn run_dir(&self, run_id: RunId) -> PathBuf {
        self.state_dir.join("worktrees").join(run_id.to_string())
    }

    /// Reserves an attempt (so the task is not dispatched twice) and builds
    /// everything it needs to run.
    async fn prepare_attempt(
        &self,
        state: &RunState,
        task_id: TaskId,
        integration: &Worktree,
    ) -> Result<AttemptContext> {
        let task = &state.tasks[&task_id];
        let runtime = self.runtime_for(&task.spec.role)?;
        let agent = self.config.agent_for(&task.spec.role);
        let number = task.attempts.len() as u32 + 1;
        let attempt_id = AttemptId::new();
        let branch = format!("bodega/{}/{}-{}", short_id(state.id), task.spec.key, number);
        let feedback = schedule::feedback_for(state, task_id);
        let base_sha = integration.head().await?;
        self.append(
            state.id,
            vec![
                EventKind::TaskStatusChanged {
                    task_id,
                    status: TaskStatus::Running,
                    reason: None,
                },
                EventKind::AttemptStarted {
                    attempt_id,
                    task_id,
                    number,
                    agent: AgentRef {
                        runtime: runtime.name().to_owned(),
                        model: agent.model.clone(),
                    },
                    workspace_id: WorkspaceId::new(),
                    branch: branch.clone(),
                },
            ],
        )
        .await?;
        Ok(AttemptContext {
            store: self.store.clone(),
            repo: self.repo.clone(),
            run_id: state.id,
            run: state.spec.clone(),
            task_id,
            task: task.spec.clone(),
            attempt_id,
            number,
            worktree_path: self
                .run_dir(state.id)
                .join(format!("{}-{}", task.spec.key, number)),
            branch,
            base_sha,
            runtime,
            agent,
            checks: self.config.checks.clone(),
            policy: self.config.permissions.clone(),
            limits: self.config.limits.clone(),
            feedback,
            budget: state.spec.budget.clone(),
            spent_before: state.total_usage(),
        })
    }

    /// Integrates a verified attempt (the merge queue: one at a time), or
    /// decides what happens after a failed one.
    async fn handle_report(
        &self,
        run_id: RunId,
        integration: &Worktree,
        report: AttemptReport,
    ) -> Result<()> {
        let state = self.load(run_id).await?;
        let max_attempts = state.spec.budget.max_attempts_per_task;
        let (reason, retryable) = match &report.outcome {
            AttemptOutcome::Succeeded { .. } => {
                match self.integrate(run_id, integration, &report).await? {
                    None => return Ok(()),
                    Some(reason) => (reason, true),
                }
            }
            AttemptOutcome::Failed { kind, message } => {
                (format!("{kind:?}: {message}"), kind.is_retryable())
            }
            AttemptOutcome::Cancelled => ("cancelled".to_owned(), true),
        };
        let state = self.load(run_id).await?;
        let events =
            schedule::after_failure(&state, report.task_id, &reason, retryable, max_attempts);
        self.append(run_id, events).await
    }

    /// Merges an attempt's branch into the integration branch and re-runs
    /// checks there. Returns `Some(reason)` if the work could not land.
    async fn integrate(
        &self,
        run_id: RunId,
        integration: &Worktree,
        report: &AttemptReport,
    ) -> Result<Option<String>> {
        let before = integration.head().await?;
        let state = self.load(run_id).await?;
        let key = state.tasks[&report.task_id].spec.key.clone();
        let failure = match integration
            .merge(&report.branch, &format!("bodega: integrate {key}"))
            .await
        {
            Ok(merge_commit) => {
                let checks = &self.config.checks;
                let results = if self.config.limits.verify_after_merge {
                    checks::run_checks(&integration.path, checks, "integration:").await
                } else {
                    Vec::new()
                };
                self.append(
                    run_id,
                    results
                        .iter()
                        .map(|result| EventKind::CheckCompleted {
                            attempt_id: report.attempt_id,
                            result: result.clone(),
                        })
                        .collect(),
                )
                .await?;
                let failed: Vec<_> = results.iter().filter(|r| !r.passed).collect();
                if failed.is_empty() {
                    self.append(
                        run_id,
                        vec![
                            EventKind::BranchIntegrated {
                                task_id: report.task_id,
                                branch: report.branch.clone(),
                                commit: merge_commit,
                            },
                            EventKind::TaskStatusChanged {
                                task_id: report.task_id,
                                status: TaskStatus::Done,
                                reason: None,
                            },
                        ],
                    )
                    .await?;
                    return Ok(None);
                }
                integration.reset_hard(&before).await?;
                format!(
                    "checks failed on the integrated code: {}",
                    failed
                        .iter()
                        .map(|r| format!("`{}`:\n{}", r.command, r.output_tail))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
            Err(GitError::MergeConflict { files, .. }) => {
                format!("merge conflict in {}", files.join(", "))
            }
            Err(e) => return Err(e.into()),
        };
        self.append(
            run_id,
            vec![EventKind::IntegrationFailed {
                task_id: report.task_id,
                attempt_id: report.attempt_id,
                reason: failure.clone(),
            }],
        )
        .await?;
        Ok(Some(failure))
    }

    async fn finish(
        &self,
        state: &RunState,
        status: RunStatus,
        reason: Option<String>,
        mut events: Vec<EventKind>,
    ) -> Result<RunState> {
        let state = self.load(state.id).await?;
        for attempt in state.attempts.values().filter(|a| !a.is_finished()) {
            events.push(EventKind::AttemptFinished {
                attempt_id: attempt.id,
                outcome: AttemptOutcome::Cancelled,
                usage: attempt.usage,
            });
        }
        for task in state.tasks.values().filter(|t| !t.status.is_terminal()) {
            events.push(EventKind::TaskStatusChanged {
                task_id: task.id,
                status: TaskStatus::Cancelled,
                reason: reason.clone(),
            });
        }
        events.push(EventKind::RunStatusChanged {
            status,
            stage: None,
            reason,
        });
        self.append(state.id, events).await?;
        self.load(state.id).await
    }
}

/// Waits until a human resolves `approval_id`. Polls the store, so approvals
/// given from another process (the CLI, the server) are seen too.
pub(crate) async fn wait_for_decision(
    store: &EventStore,
    run_id: RunId,
    approval_id: ApprovalId,
) -> Option<(Decision, Option<String>)> {
    loop {
        match store.run(run_id).await {
            Ok(Some(state)) => {
                if let Some(resolution) = state
                    .approvals
                    .get(&approval_id)
                    .and_then(|a| a.resolution.as_ref())
                {
                    return Some((resolution.decision, resolution.comment.clone()));
                }
            }
            Ok(None) | Err(_) => return None,
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A short, stable, branch-friendly id for a run (its random tail).
pub fn short_id(run_id: RunId) -> String {
    let simple = run_id.as_uuid().simple().to_string();
    simple[simple.len() - 8..].to_owned()
}
