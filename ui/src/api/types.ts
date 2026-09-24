// Generated from Bodega's Rust types. Do not edit by hand.
// Regenerate: UPDATE_TYPES=1 cargo test -p bodega-server --test typescript

export type RunId = string;

export type TaskId = string;

export type AttemptId = string;

export type WorkspaceId = string;

export type ApprovalId = string;

export type Event = { 
/**
 * Position in the global log. Gapless and strictly increasing, so a
 * client that saw `seq = n` resumes with "everything after n".
 */
seq: number, run_id: RunId, 
/**
 * Unix time in milliseconds.
 */
at_ms: number, } & ({ "type": "run_created", spec: RunSpec, } | { "type": "run_status_changed", status: RunStatus, 
/**
 * The pipeline stage the run is in, when running.
 */
stage: string | null, reason: string | null, } | { "type": "plan_proposed", summary: string, tasks: Array<TaskSpec>, } | { "type": "task_created", task_id: TaskId, spec: TaskSpec, } | { "type": "task_status_changed", task_id: TaskId, status: TaskStatus, reason: string | null, } | { "type": "attempt_started", attempt_id: AttemptId, task_id: TaskId, 
/**
 * 1-based attempt number for this task.
 */
number: number, agent: AgentRef, workspace_id: WorkspaceId, branch: string, } | { "type": "agent", attempt_id: AttemptId, event: AgentEvent, } | { "type": "check_completed", attempt_id: AttemptId, result: CheckResult, } | { "type": "review_completed", attempt_id: AttemptId, reviewer: AgentRef, verdict: ReviewVerdict, findings: Array<Finding>, } | { "type": "attempt_finished", attempt_id: AttemptId, outcome: AttemptOutcome, usage: Usage, } | { "type": "approval_requested", approval_id: ApprovalId, kind: ApprovalKind, title: string, details: string, task_id: TaskId | null, } | { "type": "approval_resolved", approval_id: ApprovalId, decision: Decision, by: string, comment: string | null, } | { "type": "branch_integrated", task_id: TaskId, branch: string, commit: string, } | { "type": "integration_failed", task_id: TaskId, attempt_id: AttemptId, reason: string, } | { "type": "pull_request_opened", url: string, number: number | null, } | { "type": "budget_exceeded", exceeded: BudgetExceeded, });

export type EventKind = { "type": "run_created", spec: RunSpec, } | { "type": "run_status_changed", status: RunStatus, 
/**
 * The pipeline stage the run is in, when running.
 */
stage: string | null, reason: string | null, } | { "type": "plan_proposed", summary: string, tasks: Array<TaskSpec>, } | { "type": "task_created", task_id: TaskId, spec: TaskSpec, } | { "type": "task_status_changed", task_id: TaskId, status: TaskStatus, reason: string | null, } | { "type": "attempt_started", attempt_id: AttemptId, task_id: TaskId, 
/**
 * 1-based attempt number for this task.
 */
number: number, agent: AgentRef, workspace_id: WorkspaceId, branch: string, } | { "type": "agent", attempt_id: AttemptId, event: AgentEvent, } | { "type": "check_completed", attempt_id: AttemptId, result: CheckResult, } | { "type": "review_completed", attempt_id: AttemptId, reviewer: AgentRef, verdict: ReviewVerdict, findings: Array<Finding>, } | { "type": "attempt_finished", attempt_id: AttemptId, outcome: AttemptOutcome, usage: Usage, } | { "type": "approval_requested", approval_id: ApprovalId, kind: ApprovalKind, title: string, details: string, task_id: TaskId | null, } | { "type": "approval_resolved", approval_id: ApprovalId, decision: Decision, by: string, comment: string | null, } | { "type": "branch_integrated", task_id: TaskId, branch: string, commit: string, } | { "type": "integration_failed", task_id: TaskId, attempt_id: AttemptId, reason: string, } | { "type": "pull_request_opened", url: string, number: number | null, } | { "type": "budget_exceeded", exceeded: BudgetExceeded, };

export type AgentEvent = { "type": "session_started", 
/**
 * The runtime's own session id, used to resume.
 */
session_id: string | null, model: string | null, } | { "type": "message", text: string, } | { "type": "thinking", text: string, } | { "type": "tool_call", call_id: string, tool: string, input: JsonValue, } | { "type": "tool_result", call_id: string, output: string, is_error: boolean, } | { "type": "permission_requested", request_id: string, tool: string, input: JsonValue, } | { "type": "usage", usage: Usage, } | { "type": "log", level: LogLevel, message: string, } | { "type": "finished", success: boolean, summary: string | null, usage: Usage, };

export type LogLevel = "debug" | "info" | "warn" | "error";

export type RunSpec = { title: string, 
/**
 * The request itself: an issue body, a feature description, a bug report.
 */
request: string, source: WorkSource, 
/**
 * Branch or commit the run's work forks from.
 */
base_ref: string, 
/**
 * Name of the pipeline (stages and gates) this run follows.
 */
pipeline: string, budget: Budget, };

export type WorkSource = { "kind": "manual" } | { "kind": "github_issue", repo: string, number: number, } | { "kind": "linear", key: string, } | { "kind": "file", path: string, };

export type RunStatus = "pending" | "running" | "waiting_for_approval" | "succeeded" | "failed" | "cancelled";

export type TaskSpec = { 
/**
 * Stable, human-readable key within the run (e.g. `T001`). Plans refer to
 * dependencies by key so they can be written before ids exist.
 */
key: string, title: string, description: string, 
/**
 * Which agent role executes the task (e.g. `implementer`).
 */
role: string, depends_on: Array<string>, 
/**
 * Acceptance criteria the reviewer and the converge step check against.
 */
acceptance: Array<string>, };

export type TaskStatus = "pending" | "ready" | "running" | "done" | "failed" | "skipped" | "cancelled";

export type AgentRef = { 
/**
 * Adapter name, e.g. `claude-code`, `codex`, `acp:gemini`.
 */
runtime: string, model: string | null, };

export type FailureKind = "agent" | "verification" | "review" | "budget" | "timeout" | "infrastructure" | "merge_conflict";

export type AttemptOutcome = { "result": "succeeded", 
/**
 * Commit holding the attempt's work, if it changed anything.
 */
commit: string | null, } | { "result": "failed", kind: FailureKind, message: string, } | { "result": "cancelled" };

export type CheckResult = { name: string, command: string, passed: boolean, exit_code: number | null, duration_ms: number, 
/**
 * The last lines of combined output, for display and for feeding back to
 * the agent on failure.
 */
output_tail: string, };

export type ReviewVerdict = "approve" | "request_changes" | "escalate";

export type FindingKind = "patch" | "bad_spec" | "intent_gap" | "defer" | "reject";

export type Finding = { kind: FindingKind, message: string, file: string | null, line: number | null, };

export type ApprovalKind = "plan" | "merge" | "question" | "permission";

export type Decision = "approved" | "rejected";

export type Usage = { input_tokens: number, output_tokens: number, cache_read_tokens: number, cache_write_tokens: number, 
/**
 * Cost in US dollars, when the agent reports it.
 */
cost_usd: number, };

export type Budget = { max_cost_usd: number | null, max_tokens: number | null, max_wall_clock_secs: number | null, 
/**
 * How many attempts a single task may use before it fails for good.
 */
max_attempts_per_task: number, };

export type BudgetExceeded = { "kind": "cost", limit: number, spent: number, } | { "kind": "tokens", limit: number, spent: number, } | { "kind": "wall_clock", limit_secs: number, elapsed_secs: number, };

export type RunState = { id: RunId, spec: RunSpec, status: RunStatus, stage: string | null, status_reason: string | null, plan_summary: string | null, tasks: { [key in TaskId]: TaskState }, attempts: { [key in AttemptId]: AttemptState }, approvals: { [key in ApprovalId]: ApprovalState }, pull_request: PullRequest | null, created_at_ms: number, updated_at_ms: number, 
/**
 * Sequence number of the last event applied.
 */
last_seq: number, };

export type TaskState = { id: TaskId, spec: TaskSpec, status: TaskStatus, status_reason: string | null, attempts: Array<AttemptId>, 
/**
 * Commit on the integration branch once the task's work has landed.
 */
integrated_commit: string | null, 
/**
 * Why the latest verified attempt could not be integrated, if it could
 * not. Fed back to the next attempt; cleared on integration.
 */
last_integration_error: string | null, };

export type AttemptState = { id: AttemptId, task_id: TaskId, number: number, agent: AgentRef, workspace_id: WorkspaceId, branch: string, 
/**
 * The runtime's session id, once known (used to resume the session).
 */
session_id: string | null, started_at_ms: number, finished_at_ms: number | null, outcome: AttemptOutcome | null, 
/**
 * Latest cumulative usage reported for this attempt.
 */
usage: Usage, checks: Array<CheckResult>, reviews: Array<Review>, 
/**
 * Number of agent activity events seen (the events themselves stay in
 * the log; the UI pages through them).
 */
activity_count: number, };

export type ApprovalState = { id: ApprovalId, kind: ApprovalKind, title: string, details: string, task_id: TaskId | null, requested_at_ms: number, resolution: Resolution | null, };

export type Resolution = { decision: Decision, by: string, comment: string | null, at_ms: number, };

export type Review = { reviewer: AgentRef, verdict: ReviewVerdict, findings: Array<Finding>, };

export type PullRequest = { url: string, number: number | null, };

export type RunSummary = { run_id: RunId, title: string, status: RunStatus, stage: string | null, tasks_total: number, tasks_done: number, pending_approvals: number, usage: Usage, created_at_ms: number, updated_at_ms: number, last_seq: number, };

export type CreateRun = { title: string, request?: string, 
/**
 * Without a plan, the whole request is one task.
 */
plan?: Plan, base_ref?: string, max_cost_usd?: number, };

export type Created = { run_id: RunId, };

export type PendingApproval = { run_id: RunId, run_title: string, id: ApprovalId, kind: ApprovalKind, title: string, details: string, task_id: TaskId | null, requested_at_ms: number, resolution: Resolution | null, };

export type Resolve = { decision: Decision, comment?: string, by?: string, };

export type Plan = { summary: string, tasks: Array<PlanTask>, };

export type PlanTask = { key: string, title: string, description: string, role: string, depends_on: Array<string>, acceptance: Array<string>, };

export type JsonValue = number | string | boolean | Array<JsonValue> | { [key in string]: JsonValue } | null;
