//! One attempt: an agent session working on one task in its own worktree,
//! followed by deterministic checks and bounded fix rounds.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bodega_agents::{AgentRuntime, AgentSession, PermissionDecision, PermissionMode, StartRequest};
use bodega_config::{
    AgentConfig, Check, Limits, PermissionModeSetting, PermissionPolicy, PolicyDecision,
};
use bodega_core::{
    AgentEvent, ApprovalId, ApprovalKind, AttemptId, AttemptOutcome, Budget, Decision, EventKind,
    FailureKind, RunId, RunSpec, TaskId, TaskSpec, Usage,
};
use bodega_store::EventStore;
use bodega_workspace::{GitRepo, Worktree};
use tokio::time::Instant;

use crate::{checks, prompts, wait_for_decision};

pub(crate) struct AttemptContext {
    pub store: EventStore,
    pub repo: GitRepo,
    pub run_id: RunId,
    pub run: RunSpec,
    pub task_id: TaskId,
    pub task: TaskSpec,
    pub attempt_id: AttemptId,
    pub number: u32,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub base_sha: String,
    pub runtime: Arc<dyn AgentRuntime>,
    pub agent: AgentConfig,
    pub checks: Vec<Check>,
    pub policy: PermissionPolicy,
    pub limits: Limits,
    /// This attempt's share of the run's remaining spend, enforced by the
    /// agent itself where the runtime supports it (Claude Code does), so a
    /// single long turn cannot overshoot the run's budget.
    pub max_budget_usd: Option<f64>,
    pub feedback: Option<String>,
    /// The run's budget, and what the rest of the run had spent when this
    /// attempt started.
    pub budget: Budget,
    pub spent_before: Usage,
}

#[derive(Debug)]
pub(crate) struct AttemptReport {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub branch: String,
    pub outcome: AttemptOutcome,
}

/// Runs the attempt to completion and records its outcome.
pub(crate) async fn run(ctx: AttemptContext) -> AttemptReport {
    let mut usage = Usage::default();
    let outcome = execute(&ctx, &mut usage).await;
    let finished = ctx
        .store
        .append(
            ctx.run_id,
            vec![EventKind::AttemptFinished {
                attempt_id: ctx.attempt_id,
                outcome: outcome.clone(),
                usage,
            }],
            Some(format!("attempt-finished:{}", ctx.attempt_id)),
        )
        .await;
    if let Err(e) = finished {
        tracing::error!(attempt = %ctx.attempt_id, "could not record attempt outcome: {e}");
    }
    // Successful work lives on in its branch; failed worktrees are kept for
    // inspection.
    if matches!(outcome, AttemptOutcome::Succeeded { .. })
        && let Err(e) = ctx.repo.remove_worktree(&ctx.worktree_path, true).await
    {
        tracing::warn!(attempt = %ctx.attempt_id, "could not remove worktree: {e}");
    }
    AttemptReport {
        task_id: ctx.task_id,
        attempt_id: ctx.attempt_id,
        branch: ctx.branch.clone(),
        outcome,
    }
}

fn failed(kind: FailureKind, message: impl Into<String>) -> AttemptOutcome {
    AttemptOutcome::Failed {
        kind,
        message: message.into(),
    }
}

async fn execute(ctx: &AttemptContext, usage: &mut Usage) -> AttemptOutcome {
    let worktree = match ctx
        .repo
        .add_worktree(&ctx.worktree_path, &ctx.branch, &ctx.base_sha)
        .await
    {
        Ok(worktree) => worktree,
        Err(e) => {
            return failed(
                FailureKind::Infrastructure,
                format!("could not create the worktree: {e}"),
            );
        }
    };

    let request = StartRequest {
        workdir: worktree.path.clone(),
        prompt: prompts::task_prompt(&ctx.run, &ctx.task, &ctx.checks, ctx.feedback.as_deref()),
        append_system_prompt: Some(match &ctx.agent.append_system_prompt {
            Some(extra) => format!("{}\n\n{extra}", prompts::IMPLEMENTER_SYSTEM),
            None => prompts::IMPLEMENTER_SYSTEM.to_owned(),
        }),
        model: ctx.agent.model.clone(),
        session_id: Some(uuid::Uuid::new_v4().to_string()),
        resume: None,
        permission_mode: match ctx.agent.permission_mode {
            PermissionModeSetting::Ask => PermissionMode::Ask,
            PermissionModeSetting::AcceptEdits => PermissionMode::AcceptEdits,
            PermissionModeSetting::Plan => PermissionMode::Plan,
        },
        allowed_tools: ctx.agent.allowed_tools.clone(),
        disallowed_tools: ctx.agent.disallowed_tools.clone(),
        env: Default::default(),
        max_turns: ctx.agent.max_turns,
        max_budget_usd: ctx.max_budget_usd,
    };
    let mut session = match ctx.runtime.start(request).await {
        Ok(session) => session,
        Err(e) => {
            return failed(
                FailureKind::Infrastructure,
                format!("could not start {}: {e}", ctx.runtime.name()),
            );
        }
    };

    let limit = Duration::from_secs(ctx.limits.attempt_timeout_secs);
    let outcome = tokio::time::timeout_at(
        Instant::now() + limit,
        drive_session(ctx, &worktree, &mut session, usage),
    )
    .await
    .unwrap_or_else(|_| {
        failed(
            FailureKind::Timeout,
            format!(
                "the attempt did not finish within {}s",
                ctx.limits.attempt_timeout_secs
            ),
        )
    });
    if let Err(e) = session.control.shutdown().await {
        tracing::debug!(attempt = %ctx.attempt_id, "shutdown: {e}");
    }
    outcome
}

async fn drive_session(
    ctx: &AttemptContext,
    worktree: &Worktree,
    session: &mut AgentSession,
    usage: &mut Usage,
) -> AttemptOutcome {
    let mut fix_round = 0;
    loop {
        let Some(event) = session.events.recv().await else {
            return failed(
                FailureKind::Agent,
                "the agent stopped without finishing its turn",
            );
        };
        record(ctx, &event).await;
        match event {
            AgentEvent::PermissionRequested {
                request_id,
                tool,
                input,
            } => {
                let decision = decide_permission(ctx, &tool, &input).await;
                if let Err(e) = session
                    .control
                    .respond_permission(&request_id, decision)
                    .await
                {
                    tracing::warn!(attempt = %ctx.attempt_id, "could not answer permission request: {e}");
                }
            }
            AgentEvent::Usage { usage: total } => {
                *usage = total;
                if let Some(exceeded) = ctx.budget.check(&(ctx.spent_before + total), 0) {
                    let _ = session.control.interrupt().await;
                    return failed(FailureKind::Budget, exceeded.to_string());
                }
            }
            AgentEvent::Finished {
                success,
                summary,
                usage: total,
            } => {
                *usage = total;
                if let Some(exceeded) = ctx.budget.check(&(ctx.spent_before + total), 0) {
                    return failed(FailureKind::Budget, exceeded.to_string());
                }
                let message = format!(
                    "bodega: {} {} (attempt {})",
                    ctx.task.key, ctx.task.title, ctx.number
                );
                if let Err(e) = worktree.commit_all(&message).await {
                    return failed(
                        FailureKind::Infrastructure,
                        format!("could not commit the agent's work: {e}"),
                    );
                }
                if !success {
                    return failed(
                        FailureKind::Agent,
                        summary.unwrap_or_else(|| "the agent reported a failure".into()),
                    );
                }
                let head = match worktree.head().await {
                    Ok(head) => head,
                    Err(e) => return failed(FailureKind::Infrastructure, e.to_string()),
                };
                if head == ctx.base_sha {
                    return failed(
                        FailureKind::Agent,
                        "the agent finished without changing any files",
                    );
                }

                let results = checks::run_checks(&worktree.path, &ctx.checks, "").await;
                let events = results
                    .iter()
                    .map(|result| EventKind::CheckCompleted {
                        attempt_id: ctx.attempt_id,
                        result: result.clone(),
                    })
                    .collect();
                if let Err(e) = ctx.store.append(ctx.run_id, events, None).await {
                    tracing::error!(attempt = %ctx.attempt_id, "could not record checks: {e}");
                }
                if results.iter().all(|r| r.passed) {
                    return AttemptOutcome::Succeeded { commit: Some(head) };
                }
                if fix_round < ctx.limits.max_fix_rounds && ctx.runtime.capabilities().multi_turn {
                    fix_round += 1;
                    let feedback = prompts::check_failure_feedback(
                        &results,
                        fix_round,
                        ctx.limits.max_fix_rounds,
                    );
                    if session.control.send(feedback).await.is_ok() {
                        continue;
                    }
                }
                let failing: Vec<&str> = results
                    .iter()
                    .filter(|r| !r.passed)
                    .map(|r| r.name.as_str())
                    .collect();
                return failed(
                    FailureKind::Verification,
                    format!(
                        "checks still failing after {fix_round} fix round(s): {}",
                        failing.join(", ")
                    ),
                );
            }
            _ => {}
        }
    }
}

async fn record(ctx: &AttemptContext, event: &AgentEvent) {
    let appended = ctx
        .store
        .append(
            ctx.run_id,
            vec![EventKind::Agent {
                attempt_id: ctx.attempt_id,
                event: event.clone(),
            }],
            None,
        )
        .await;
    if let Err(e) = appended {
        tracing::error!(attempt = %ctx.attempt_id, "could not record agent event: {e}");
    }
}

/// Answers a permission request from policy, escalating to a human when the
/// policy has no rule for it. Never auto-approves unknown requests.
async fn decide_permission(
    ctx: &AttemptContext,
    tool: &str,
    input: &serde_json::Value,
) -> PermissionDecision {
    match ctx.policy.decide(tool, input) {
        PolicyDecision::Allow => PermissionDecision::Allow {
            updated_input: None,
        },
        PolicyDecision::Deny { rule } => PermissionDecision::Deny {
            message: format!("Bodega's permission policy denies this (rule `{rule}`)."),
            interrupt: false,
        },
        PolicyDecision::Escalate => {
            let approval_id = ApprovalId::new();
            let details = serde_json::to_string_pretty(input).unwrap_or_default();
            let requested = ctx
                .store
                .append(
                    ctx.run_id,
                    vec![EventKind::ApprovalRequested {
                        approval_id,
                        kind: ApprovalKind::Permission,
                        title: format!("{}: allow `{tool}`?", ctx.task.key),
                        details,
                        task_id: Some(ctx.task_id),
                    }],
                    None,
                )
                .await;
            if let Err(e) = requested {
                tracing::error!("could not request approval: {e}");
                return PermissionDecision::Deny {
                    message: "Bodega could not ask a human for approval.".into(),
                    interrupt: false,
                };
            }
            match wait_for_decision(&ctx.store, ctx.run_id, approval_id).await {
                Some((Decision::Approved, _)) => PermissionDecision::Allow {
                    updated_input: None,
                },
                Some((Decision::Rejected, comment)) => PermissionDecision::Deny {
                    message: comment.unwrap_or_else(|| "A human denied this request.".into()),
                    interrupt: false,
                },
                None => PermissionDecision::Deny {
                    message: "The approval could not be read.".into(),
                    interrupt: false,
                },
            }
        }
    }
}
