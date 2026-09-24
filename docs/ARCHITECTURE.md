# Forgeline architecture

Forgeline is an open-source software factory: it takes work items (a feature
request, an issue, a bug report), turns them into a plan of tasks, runs a swarm
of coding agents on those tasks in isolated workspaces, verifies and reviews
their work, integrates it and hands back a pull request — with a live,
replayable view of everything that happened.

This document is the design. The research it is based on is summarized in
[`research/SYNTHESIS.md`](research/SYNTHESIS.md); the build order is in
[`ROADMAP.md`](ROADMAP.md). Sections marked **(built)** exist in the code today;
everything else is the plan.

---

## 1. Goals and non-goals

**Goals**

- Run many agents in parallel on one repository without them stepping on each
  other, and without a human babysitting each one.
- Be agent-agnostic: Claude Code, Codex, Gemini CLI, Goose, OpenCode, any ACP
  agent, or a built-in agent loop on a model API.
- Make "done" mean *verified*: deterministic checks the agent cannot game, then
  independent review, then a human merge.
- Be durable: kill the process at any moment and it resumes where it was.
- Expose everything as a sequenced event stream so a great UI (and CLI, and
  bots) can be built on top without touching the engine.
- Ship as one binary that works on a laptop, and scale out later.

**Non-goals (for now)**

- Being a general workflow engine. Pipelines are purpose-built for software
  work.
- Hosting models or agents. Forgeline drives existing harnesses.
- Replacing the issue tracker. GitHub/Linear/beads stay the ledger when a team
  already has one; Forgeline has a built-in ledger for everyone else.

---

## 2. Principles

1. **Deterministic core, LLMs at the edges.** Scheduling, retries, merging,
   budgets and recovery are Rust state machines with tests. Models are called
   only for judgment: planning, implementing, reviewing.
2. **Events are the only way state changes.** Every change is an event in an
   append-only log; state is a fold over the log. Crash recovery, the live UI,
   audit and evals all read the same log.
3. **One owner per run.** A single engine task owns a run's state and
   reconciles with reality (git, processes, the tracker) before acting.
4. **Agents produce artifacts; Forgeline applies effects.** Agents never hold
   forge or tracker credentials. They leave commits and result files; a trusted
   publisher pushes branches, opens PRs and posts comments.
5. **Verification is code, not a prompt.** Check commands come from the trusted
   base branch, run on the host (or the sandbox) under Forgeline's control, and
   produce proof records bound to exact commits.
6. **Every loop is capped and every exit is typed.** Retries, fix rounds and
   review loops have limits; runs and tasks end in explicit terminal states with
   reasons.
7. **Fail closed.** If a sandbox cannot enforce a required policy, the attempt
   does not start. Permission prompts are decided by policy or a human, never
   auto-approved.
8. **Structured I/O only.** Agents are driven through machine protocols
   (stream-json, JSON-RPC, ACP), never by scraping terminals.

---

## 3. System overview

```
  work sources                         forgeline (one process)                              outputs
 ─────────────                ────────────────────────────────────────────────           ─────────
  CLI / UI request ──┐        ┌──────────────┐   commands   ┌───────────────────┐
  GitHub issues ─────┼──────▶ │    Engine    │ ───────────▶ │ Agent supervisor  │──▶ agent processes
  Linear / beads ────┘        │ (per-run     │ ◀─────────── │ (adapters, normal-│    in workspaces
                              │  actors)     │  AgentEvents │  ized events)     │
                              └──────┬───────┘              └───────────────────┘
                                     │ append / replay       ┌───────────────────┐
                                     ▼                       │ Workspace manager │──▶ git worktrees /
                              ┌──────────────┐               │ (backends,policy) │    containers / VMs
                              │  Event store │               └───────────────────┘
                              │ (SQLite log +│               ┌───────────────────┐
                              │  projections)│               │ Verifier          │──▶ proof records
                              └──────┬───────┘               └───────────────────┘
                                     │ subscribe             ┌───────────────────┐
                                     ▼                       │ Publisher (outbox)│──▶ branches, PRs,
                              ┌──────────────┐               └───────────────────┘    comments
                              │ HTTP/WS API  │──▶ web UI, CLI, bots
                              └──────────────┘
```

---

## 4. Crates

| Crate | Responsibility | Status |
|---|---|---|
| `forgeline-core` | Ids, domain model, events, `RunState` fold, dependency graph, budgets. No I/O. | **built** |
| `forgeline-store` | Append-only event log on SQLite, validation in the write transaction, idempotency keys, run summaries, live subscriptions. | **built** |
| `forgeline-workspace` | Workspace backends. Git layer (worktrees, commits, diffs, merges) today; `WorkspaceBackend` trait, OS sandbox, Docker, E2B later. | git layer **built** |
| `forgeline-agents` | `AgentRuntime` trait, adapters (Claude Code, Codex, ACP, mock), normalization into `AgentEvent`, capabilities. | M1 |
| `forgeline-engine` | Per-run actors: `decide(state) → commands`, dispatch, concurrency limits, retries, verification, review, integration, approvals. | M1 |
| `forgeline-config` | `forgeline.toml`, pipelines, role prompts, override stack. | M1 |
| `forgeline-server` | axum REST + SSE/WebSocket event stream; serves the UI; exports TypeScript types. | M2 |
| `forgeline-cli` | The `forgeline` binary: `init`, `run`, `runs`, `show`, `watch`, `approve`, `serve`, `doctor`. | M1 |
| `ui/` | The web app (open for design). Consumes only the public API. | M3 |

---

## 5. Domain model (built)

```
Run ──< Task ──< Attempt ──< AgentEvent (normalized)
 │        │         ├──< CheckResult (proof)
 │        │         └──< Review (verdict + typed findings)
 │        └── depends_on: [Task keys]  → DepGraph
 └──< Approval (plan / merge / question / permission)
```

- **Run**: one work item flowing through a pipeline. Has a `RunSpec` (title,
  request, source, base ref, pipeline, budget) and a status:
  `pending → running ⇄ waiting_for_approval → succeeded | failed | cancelled`.
- **Task**: a node in the run's plan with a stable key (`T001`), a role,
  dependencies by key, and acceptance criteria. Status:
  `pending → ready → running → done | failed | skipped | cancelled`.
- **Attempt**: one agent session working on one task in one workspace on one
  branch. Retries create new attempts (the previous attempt's feedback is
  carried forward). Outcome: `succeeded{commit} | failed{kind, message} |
  cancelled`. `FailureKind` (`agent`, `verification`, `review`, `budget`,
  `timeout`, `infrastructure`, `merge_conflict`) drives the retry policy.
- **Approval**: a durable, typed request for a human decision with an id,
  shown in the attention inbox until resolved.

All ids are UUIDv7 (time-ordered) with a type prefix (`run_…`, `task_…`,
`att_…`, `ws_…`, `apr_…`).

---

## 6. Event log and durability (built: log, validation, idempotency; planned: leases, timers, outbox)

**Schema** (`forgeline-store`):

- `events(seq INTEGER PRIMARY KEY AUTOINCREMENT, run_id, at_ms, type, payload, payload_version)`
- `runs(run_id, status, updated_at_ms, summary)`: list-view projection updated in the same transaction
- `idempotency(key, first_seq, last_seq)`

**Invariants**

- `seq` is gapless and strictly increasing across the whole store. A client
  that has seen `seq = n` resumes with `events_after(n)`.
- Every append is validated by folding the new events into the run's current
  `RunState` *inside* the write transaction. If the fold rejects an event
  (unknown task, out of order, duplicate key…), the whole batch rolls back. The
  log therefore never contains an event the projection cannot apply.
- An append with an idempotency key that was already used returns the
  originally stored events and writes nothing. Every externally visible side
  effect (create branch, open PR, post comment) is keyed this way.
- Committed events are broadcast to subscribers; a subscriber that lags
  re-reads from the log by `seq` (no silent drops).

**Planned additions** (M2):

- `leases(subject, holder, expires_at_ms)`: who is running an attempt; renewed
  by heartbeats from the agent supervisor. Expired leases are reclaimed on
  startup and by a periodic reconciler.
- `timers(run_id, fire_at_ms, kind)`: durable sleeps (retry backoff, approval
  timeouts, stall watchdogs).
- `outbox(effect_id, run_id, kind, payload, status)`: side effects the
  publisher applies idempotently, with reconciliation ("does PR for branch X
  already exist?") on recovery.
- Large blobs (full transcripts, diffs, logs) stored on disk by content hash
  and referenced from events.

**Recovery procedure** on startup: replay each non-terminal run → for each
attempt that was running, check the lease and the process/workspace → resume
the agent session if the adapter supports it, otherwise restart the attempt
from the workspace's last commit → re-arm timers → continue deciding.

A `Storage` trait keeps Postgres (and a Temporal-backed engine for teams that
already run Temporal) possible later without touching the engine.

---

## 7. The engine (M1)

Each active run is owned by one tokio task (an actor) with a bounded mailbox.
The loop is:

```
loop {
    state = fold(events)                     // RunState, in memory
    commands = decide(&state, &config, now)  // pure, unit-tested
    for cmd in commands { dispatch(cmd) }    // effects run as separate tasks
    msg = mailbox.recv()                     // agent events, check results,
                                             // approvals, timers, cancel
    append(events_for(msg))                  // through the store (validated)
}
```

`decide` is a pure function from state to commands, so the whole scheduling
policy is testable without agents, git or a database:

| Command | Emitted when |
|---|---|
| `StartStage(plan)` | run is pending and the pipeline starts with planning |
| `RequestApproval(plan)` | plan proposed and the gate policy says human |
| `CreateTasks` | plan accepted (graph validated: no cycles, no unknown deps) |
| `StartAttempt(task)` | task ready, a slot is free under all concurrency limits, no footprint conflict with running attempts |
| `RunChecks(attempt)` | agent finished its turn with commits |
| `StartReview(attempt)` | checks passed and the pipeline has a review stage |
| `ResumeWithFeedback(attempt)` | checks failed or review requested changes, fix-round cap not reached |
| `Integrate(task)` | attempt verified (and approved by review) → enqueue in merge queue |
| `SkipDependents(task)` | task failed for good |
| `Finish(run)` | every task terminal |

**Concurrency limits** (all configurable): global max agents, per-run max
agents, per-runtime and per-model caps (rate limits and subscription windows
differ), and footprint exclusivity (two attempts predicted to touch the same
files never run at once; unknown footprint runs alone when the policy says
so).

**Retry policy** keys off `FailureKind`: `infrastructure` retries with
exponential backoff without charging the task's attempt budget;
`agent`/`verification`/`review` consume an attempt and carry the failure output
forward as feedback; `budget` never retries; `merge_conflict` re-runs the task
on top of the new integration head. Stale timers are ignored via generation
counters.

**Budgets** (built in core): cost, tokens and wall clock per run and per
attempt, checked on every usage event. Crossing one stops dispatch, cancels
running attempts for that scope and records `budget_exceeded`.

---

## 8. Pipelines (M1 minimal, M2 full)

A pipeline is data: stages, the role that runs each, and a gate policy.

Default pipeline (from the research consensus; every stage is optional and
configurable):

| # | Stage | Who | Artifact | Default gate |
|---|---|---|---|---|
| 0 | Triage | triage agent (read-only) | `intent.json`: kind, size, route | auto |
| 1 | Specify | planner (read-only) | `spec.md` with FR/AC ids, ≤ 3 open questions | human if open questions or large |
| 2 | Plan | planner | `plan.json`: tasks with keys, deps, roles, acceptance, file footprints | human for large runs, else auto |
| 3 | Implement | implementer per task, own workspace | commits on the task branch | loop until verify passes (capped) |
| 4 | Verify | engine | `ProofRecord`s | **hard gate, never LLM-judged** |
| 5 | Review | reviewer lenses, fresh context | typed findings | `patch` → back to 3; `intent_gap` → human; capped |
| 6 | Converge | auditor | acceptance coverage map | gaps become new tasks (capped) |
| 7 | Integrate | engine | merge queue: rebase → re-verify → merge into integration branch | auto |
| 8 | Publish | publisher | PR with spec, proof and triage log | human merges |

Trivial work skips 1, 2 and 6. Bugs require a failing reproduction test first.

**Configuration** lives in the repository so it is versioned with the code:

```toml
# forgeline.toml
[project]
base_ref = "main"

[agents.implementer]
runtime = "claude-code"
model = "claude-opus-5"
max_parallel = 4

[agents.reviewer]
runtime = "codex"          # a different model family reviews

[checks]                   # read from the *base* branch, never the agent's branch
build = "cargo build --workspace"
test  = "cargo test --workspace"
lint  = "cargo clippy --workspace -- -D warnings"

[limits]
max_parallel_agents = 8
max_attempts_per_task = 3
max_fix_rounds = 3
max_cost_usd = 25.0
```

Role prompts are Markdown files in `.forgeline/roles/<role>.md` with an
override stack (built-in → user → repo). Their content hash is recorded in each
run so behavior is reproducible.

**Non-configurable invariants**: verify is deterministic; every loop has a cap;
every exit is a typed state; every event is persisted.

---

## 9. Agents (M1: mock + Claude Code; M2: ACP, Codex)

Three layers, following vibe-kanban's executor design:

1. **Protocol drivers**: stdio JSON lines (Claude Code `stream-json`), JSON-RPC
   over stdio (Codex app-server, ACP), HTTP + SSE.
2. **Adapters**: one per agent. Know the command line, version pin, how to
   resume a session, how permissions are asked, and map native messages into
   `AgentEvent`.
3. **Normalized events** (built, `forgeline_core::AgentEvent`):
   `session_started`, `message`, `thinking`, `tool_call`, `tool_result`,
   `permission_requested`, `usage` (cumulative), `log`, `finished`.

```rust
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> AgentCapabilities;   // resume, interrupt, permissions,
                                                   // streaming usage, models, …
    async fn start(&self, req: StartRequest) -> Result<Box<dyn AgentSession>>;
}

#[async_trait]
pub trait AgentSession: Send {
    fn events(&mut self) -> &mut (dyn Stream<Item = AgentEvent> + Unpin + Send);
    async fn send(&mut self, input: UserInput) -> Result<()>;           // follow-up / feedback
    async fn respond_permission(&mut self, id: &str, allow: bool) -> Result<()>;
    async fn interrupt(&mut self) -> Result<()>;
    fn session_id(&self) -> Option<&str>;                                 // for resume
}
```

`StartRequest` carries the workspace path, prompt, model, role, allowed tools,
permission mode, environment (secrets resolved outside the agent's reach where
the backend allows), an optional session to resume, and limits.

**Permission policy.** Adapters never auto-approve. A permission request becomes
a `permission_requested` event; the engine answers from the role's policy
(allow/deny lists by tool and path) and escalates anything else to a human
approval.

**How each agent is driven** (details and citations in
[research note 06](research/06-agent-harnesses-and-protocols.md)):

- **Claude Code**: `claude --output-format stream-json --verbose
  --input-format stream-json --permission-prompt-tool stdio
  --permission-mode <mode> --session-id=<uuid> [--resume=<id>] [--model …]
  [--allowedTools …] [--max-turns N] [--max-budget-usd X]`. Prompts and
  follow-ups go in on stdin as `{"type":"user",…}`; control requests
  (`initialize`, `interrupt`, `set_permission_mode`) and our answers to the
  CLI's `can_use_tool` requests (`allow` with optional rewritten input, or
  `deny`) travel on the same pipes. Stdout carries `system/init`, `assistant`,
  `user` (tool results), `system/session_state_changed`, rate-limit events and
  a `result` per turn with `total_cost_usd`, usage and `session_id`. Forgeline
  generates the session id itself (`--session-id`) so it never has to scrape
  it, passes untrusted values as `--flag=value`, strips `CLAUDECODE` from the
  environment and isolates state with `CLAUDE_CONFIG_DIR` per workspace.
- **Codex**: `codex exec --json` (one-shot JSONL: `thread.started`,
  `item.*`, `turn.completed{usage}`) for the first adapter;
  `codex app-server` (JSON-RPC: `thread/start|resume`, `turn/start|steer|interrupt`,
  approval requests, `turn/diff/updated`) for full control. Codex reports
  tokens but not dollars; Forgeline prices usage from a model table.
- **Everything else via ACP v1** (`agent-client-protocol` crate): Gemini CLI,
  Goose, OpenCode, Cursor, Copilot and ~40 other agents. ACP lacks cost and
  turn ids, so it is the universal fallback rather than the path for Claude and
  Codex.

**Adapter order**: `mock` (scripted, for tests and demos) → `claude-code` →
`codex` (`exec --json`, then app-server) → `acp` → `builtin` (a small
mini-swe-agent-style loop on the model APIs for environments without any CLI)
→ a replay adapter that plays back recorded transcripts for tests. Later,
Forgeline itself is exposed as an ACP agent and an MCP server
(`start_run`, `send_message`, `view_run`, `interrupt`) so editors and other
agents can drive the factory.

Every adapter probes the installed agent version at startup (`forgeline
doctor`) because CLI flags drift between releases.

---

## 10. Workspaces and sandboxing (M1: worktrees; M3: OS sandbox, Docker; later: E2B, Substrate)

Every attempt gets its own workspace on its own branch (`forgeline/<run>/<task>-<n>`).
The engine talks to workspaces only through traits, so backends are swappable:

```rust
#[async_trait]
pub trait WorkspaceBackend: Send + Sync {
    fn capabilities(&self) -> Capabilities;   // isolation, fork, snapshot, egress enforcement, …
    async fn provision(&self, spec: &WorkspaceSpec) -> Result<Box<dyn Workspace>>;
    async fn attach(&self, id: WorkspaceId) -> Result<Box<dyn Workspace>>;  // after restart
    async fn list(&self) -> Result<Vec<WorkspaceStatus>>;                   // orphan GC
}

#[async_trait]
pub trait Workspace: Send + Sync {
    fn id(&self) -> WorkspaceId;
    fn root(&self) -> &Path;
    async fn apply_policy(&self, p: &SandboxPolicy) -> Result<PolicyReport>; // fail closed
    async fn exec(&self, req: ExecRequest) -> Result<ExecHandle>;           // checks, setup
    async fn sync_out(&self, message: &str) -> Result<Option<CommitSha>>;   // code leaves via git
    async fn snapshot(&self) -> Result<SnapshotRef>;                        // where supported
    async fn destroy(self: Box<Self>) -> Result<()>;
}
```

- **Policy fails closed**: `apply_policy` returns which rules are enforced,
  advisory or unsupported; if a rule the pipeline marks *required* is not
  enforced, the attempt does not start.
- **Secrets are references**, resolved as far outside the sandbox as the
  backend allows (egress header injection > per-exec env > file). Never in
  templates, snapshots or logs.
- **Code leaves only through git.** Forgeline commits whatever the agent left
  uncommitted, and reports anything excluded (for example, writes under
  `.github/workflows` are dropped unless the role is allowed to change CI).
- **Backend order**: git worktree (+ Landlock/bubblewrap on Linux, Seatbelt on
  macOS) → Docker/devcontainer → E2B → Agent Substrate / AX.

---

## 11. Verification, proof and review (M1: checks + proof; M2: anti-gaming, lenses)

- **Checks** (`build`, `lint`, `test`, …) are read from `forgeline.toml` *on the
  base ref*, so an agent cannot weaken its own gate by editing the config.
- Each check produces a `CheckResult` and, in M2, a **`ProofRecord`**:
  `{tree_hash, commit, command, exit_code, output_sha256, duration}`. Only a
  complete set of passing proof records for the exact commit can move a task
  to review or integration.
- **Anti-gaming checks** (M2): flag modified or deleted pre-existing tests,
  `|| true`/`exit 0` wrappers in test commands, edits to acceptance fixtures,
  and suites that did not actually run.
- **Review** runs in a fresh session (optionally a different runtime or model)
  that sees the task, the acceptance criteria and the diff, not the
  implementer's conversation. Findings are typed (`patch`, `bad_spec`,
  `intent_gap`, `defer`, `reject`). Fix rounds are counted per review, not per
  finding, and capped.
- **Stale verdicts**: if the integration branch moves after a review, the
  merge queue re-runs checks on the rebased commit before merging.

---

## 12. Integration and publishing (M1: local integration branch; M2: publisher + PRs)

- A **merge queue** per run integrates verified task branches one at a time
  into `forgeline/<run>/integration`: rebase/merge → re-run checks → advance.
  A conflict produces `merge_conflict` and the task is re-attempted on top of
  the new head with the conflict as feedback.
- The **publisher** is the only component holding forge credentials. It
  applies effects from the outbox with idempotency keys (a PR is keyed by its
  branch) and reconciles on restart. PR bodies are generated from the spec,
  the proof records and the review triage log. Merging stays with humans by
  default.

---

## 13. Humans in the loop (M1: approvals via CLI; M2: API; M3: UI)

Approvals are durable objects: plan approval, merge approval, agent questions
and permission escalations. Each has an id, a title, details, an optional
timeout with a default decision, and is resolved by `forgeline approve <id>`,
the API or the UI.

The server computes an **attention state** per run and per attempt, which the
UI renders as an inbox sorted by urgency: `waiting_on_you` → `error` →
`unread_result` → `running` → `idle`.

---

## 14. API and UI contract (M2)

The UI is a pure client of a documented API; the engine never renders UI.

- `GET /api/runs`: run summaries (the `runs` projection).
- `GET /api/runs/{id}`: full `RunState` snapshot including `last_seq`.
- `GET /api/runs/{id}/events?after={seq}&limit=`: paged history.
- `GET /api/stream?after={seq}` (SSE): live events across runs. The server
  subscribes first, then sends the backlog from the log, then live events,
  de-duplicated by `seq`; a lagging client is sent back to the log, never
  silently dropped. Heartbeats keep proxies open.
- `POST /api/runs`: start a run. `POST /api/approvals/{id}`: resolve an
  approval. `POST /api/runs/{id}/cancel`.
- `WS /api/attempts/{id}/terminal` (later): interactive terminal into a
  workspace.

TypeScript types for every event and snapshot are generated from the Rust
types (`ts-rs`), with a CI check that they are up to date, so the UI and engine
cannot drift.

UI ideas the research says nobody has built well yet: a swarm timeline across
agents, best-of-N comparison, run replay/time travel from the log, cross-agent
conflict prediction, and first-class verification badges.

---

## 15. Security model

- Agents run with the least privilege the backend can enforce; no forge or
  tracker tokens inside workspaces.
- Inputs from issues and comments are untrusted: stripped of hidden content,
  and repository-controlled agent config (`CLAUDE.md`, `.mcp.json`,
  `forgeline.toml`) is read from the base branch.
- No auto-approval of permission prompts; unknown requests escalate.
- The API binds to localhost by default; remote access requires a token.
- Agent binaries are version-pinned per adapter and checked by `forgeline doctor`.

---

## 16. Observability

`tracing` spans for every run, task and attempt; OpenTelemetry export (M3);
metrics for queue depth, agents running, spend, and pass rates. Everything a
dashboard needs is also derivable from the event log.

---

## 17. Measuring the factory (M4)

`forgeline bench` mines merged, issue-linked PRs from your own repositories into
SWE-bench-style tasks (base commit, problem statement, hidden tests with
FAIL_TO_PASS / PASS_TO_PASS), runs them through the real pipeline in eval mode
(tests hidden, gates answered by a script, no PRs) and reports resolve rate,
regressions, cost and loop counts, so every prompt, model or pipeline change
can be measured.
