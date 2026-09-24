# 02 — Platform agents: Open SWE, OpenHands, claude-code-action

These three projects cover three different ways to build a "software factory." **Open SWE** is a hosted, multi-tenant service. It sits on LangGraph's durable-run platform and gives each thread its own persistent remote sandbox, so the platform supplies durability and the app layers product policy on top. **OpenHands** has turned into a distributed toolkit. The agent loop runs *inside* the sandbox as an "Agent Server" with a REST/WebSocket API. A separate automation service decides *when* to run, and a React "Agent Canvas" lets you connect to many such servers and switch between them. It can also drive Claude Code, Codex and Gemini as subprocesses over the Agent Client Protocol (ACP). **claude-code-action** goes the other way: GitHub is the database, the queue and the UI. Each event gets one ephemeral runner, one Claude Code run and one tracking comment. Most of its engineering effort goes into supply-chain and prompt-injection hardening. Taken together, they give three answers to the same questions: who owns durability (the platform, the file system, or GitHub), where the loop runs (the server, the sandbox, or the CI runner), and how humans watch (an SSE transcript, a WebSocket event stream, or an edited comment). All repos were read at HEAD on 2026-09-24. File paths are relative to each repo.

> **Scope note (OpenHands).** `All-Hands-AI/OpenHands` now redirects to `OpenHands/OpenHands`, which today holds **only the Agent Canvas frontend**. The V0 Python platform (AgentController, EventStream, Runtime/ActionExecutionServer) is no longer on `main`. To map the architecture I also cloned `OpenHands/software-agent-sdk` (SDK, Agent Server, tools, workspaces) and `OpenHands/automation` (scheduler and dispatcher). Paths in that section are prefixed `canvas:`, `sdk:` or `automation:`.

---

## 1. langchain-ai/open-swe

### What it is
It is a Python 3.14 backend on LangGraph and Deep Agents, about 90k LOC in `agent/`, plus about 80k LOC of TypeScript across the Vite dashboard (`ui/`) and an experimental Electron desktop app (`desktop/`). It is MIT-licensed and describes itself as "under active development", but it is production-shaped: 36 Alembic migrations, an eval harness and analytics. Last commit: 2026-09-24 (`9d4ce03`).

### Architecture
```
GitHub / Slack / Linear webhooks ─┐          Dashboard SPA ◄── SSE /threads/{id}/transcript/events?after=v
LangGraph crons ──────────────────┤                │ run.start (LangGraph v3 stream)
                                  ▼                ▼
               agent/webapp.py (FastAPI mounted inside the LangGraph Agent Server)
                                  │ dispatch.create_durable_run()   (one contract for every trigger)
                                  ▼
       LangGraph runtime: threads · runs · checkpoints · store · crons
       graphs (langgraph.json): agent | reviewer | analyzer | review-scout | chat | scheduler
                                  │ build_agent(config)  ← a fresh graph is assembled per run
                                  ▼
   Deep Agents harness + ~30 middlewares ─ tools ─ SandboxBackendProxy ─► LangSmith | Modal | Daytona | E2B | Runloop | local
                                  │
       Postgres: transcript log, findings, PRs, workspaces, users, analytics
```
A webhook is verified for signature and allowlist in `agent/github/routes.py`. It is then mapped to a **deterministic thread id** in `agent/thread_ids.py` (uuid5 of `owner/repo/pr/N/reviewer`, `slack:channel:ts`, and so on), which lets follow-ups find the same thread and sandbox. `dispatch_agent_run` then creates a LangGraph run. For each run, the factory `agent/server.py::build_agent` resolves model, tools, skills and MCP groups for this user, surface and workspace, then calls `create_deep_agent(...)`. `PrepareAgentRunMiddleware` attaches the sandbox before the first model call. The run finishes by calling `open_pull_request`, and the platform POSTs `/webhooks/run-complete` (`agent/completion.py`).

**Brief vs code.** The "planner/programmer/reviewer" split described in the brief is the v1 design. Today there is **one** coding graph. Planning is a `save_plan` tool plus the Deep Agents todo list. Parallel work goes to a forked `general-purpose` subagent through `task`. Review, style learning and diff walkthroughs are separate graphs (`agent/reviewer.py`, `agent/analyzer.py`, `agent/review_scout/graph.py`).

### Core abstractions / domain model
- **Thread**: the durable conversation. Thread metadata holds `sandbox_id`, proxy config and resolved model settings. **Invocation**: one run, identified by `invocation_id` (`agent/invocation.py`, which still dual-writes the legacy `prepare_run_id`). **Run** statuses come from LangGraph: pending, running, success, error, timeout, interrupted.
- **Workspace** (`agent/workspaces/store.py`, `docs/reference/workspaces.md`): repos (each repo belongs to exactly one workspace), snapshot, setup and update scripts, sandbox sizing, Slack channels and MCP connections. Settings are tiered **instance → workspace → user → thread**.
- **Transcript events** (`agent/transcript/events.py`): `thread.created`, `turn.requested|started|queued|completed|failed|interrupted`, `turn.checkpoint.completed` (per-turn file diff), `message.appended|completed`, `tool.started|completed`, `run.notice`. `ThreadStatus` is `idle|running|error`.
- **Finding** (`agent/review/findings.py`): severity, confidence, file/line/side, `in_diff`, `status`, `surface_state` (surfaced → resolve_pending → resolved), `first_seen_sha`/`last_confirmed_sha`, `fingerprint` and GitHub thread ids.
- **BabySitWatch** (`agent/baby_sit.py`): `head_sha`, `retry_count`, `dispatch_keys`, `delivery_ids`.

### Orchestration model
- **Sources.** Webhooks (GitHub issues, PR comments, reviews and check runs; Slack; Linear), the dashboard, the desktop app, and LangGraph crons. Crons feed the `scheduler` graph (`agent/scheduler.py`), which switches on `task` and runs reconcile, baby_sit, workspace refresh, cost/feedback jobs or scheduled agent runs.
- **One dispatch contract** (`agent/dispatch.py::create_durable_run`):
  - `multitask_strategy="interrupt"` by default. A follow-up halts the active run and resumes with full history. Background work such as `/baby-sit` uses `enqueue`.
  - `durability="sync"`, so the run checkpoints before every step.
  - `stream_resumable=True`, so a dashboard that attaches late can replay the run.
  - A signed completion `webhook`.
- **Parallelism.** One active run per thread, with any number of threads in parallel. Inside a run, `task` subagents (`"mode": "fork"`, `agent/server.py::_general_purpose_subagent`) **share the same sandbox**. The app has no global concurrency cap; limits come from LangGraph worker capacity.
- **Mid-run input.** `check_message_queue_before_model` injects queued messages from the LangGraph store before each model call.
- **Retries and backoff.**
  - `retry_transient_sandbox_errors` (`agent/sandboxes/retry.py`) retries **only** errors the SDK guarantees happened before the command started: exponential backoff from 0.5 s to 8 s with ±20% jitter and a 10-minute attach budget.
  - `ToolRetryMiddleware(max_retries=2, tools=["task"])`.
  - `ModelFallbackMiddleware` and `ModelCallTimeoutMiddleware` (innermost, so a timeout escalates to the fallback).
  - `ModelCallLimitMiddleware(run_limit=5000)`.
  - `TimeoutWrapupMiddleware`: after 45 minutes the model is told to wrap up.
- **Reconciliation.** `agent/reconcile.py::reconcile_stale_runs` cancels `pending` runs older than 30 minutes on busy threads, as a safety net for lost completion webhooks. `completion.py` makes sure an `error` or `timeout` run always posts a message back to the originating surface ("decouples the user gets an answer from the agent remembered to reply").
- **Locks and idempotency.**
  - Baby-sit uses a LangGraph thread as a distributed lock: `threads.create(if_exists="raise", ttl=5)` (`_watch_lock`).
  - A dispatch fingerprint `sha(head_sha, retry_count)` prevents dispatching the same failure twice.
  - GitHub `delivery_id`s are deduplicated.

### State & persistence
- LangGraph checkpoints on the platform's Postgres hold agent state. `langgraph.json` deletes them after a TTL of 43,200 minutes (30 days). The app's own Postgres (SQLAlchemy + Alembic) holds transcript, findings, PRs, users, workspaces and analytics.
- **Transcript engine** (`agent/transcript/engine.py`). `append` is the only writer. It takes a per-thread advisory lock and assigns **gapless per-thread versions**. **Command receipts** make appends idempotent. Events, projections, the new head and `pg_notify` commit in one transaction. The transcript middleware is best-effort ("a transcript problem can never fail a run"), so the UI log and the checkpoint can drift apart. `agent/transcript/rebuild.py` exists to repair projections.
- **Crash and restart.**
  - With sync durability, a crashed run resumes from its last checkpoint.
  - A persistent sandbox that cannot be reached raises `SandboxUnreachableError` and is **never silently replaced**, because the sandbox holds the only copy of uncommitted work (`agent/sandboxes/AGENTS.md`, `lifecycle.py::ensure_sandbox_for_thread`). The read-only reviewer is the exception (`allow_replacement`).
  - A *deleted* sandbox (`SandboxGoneError`) is replaced.
  - A thread is bound to a sandbox id only after the sandbox has finished initializing, so a half-built box is never adopted.

### Isolation & workspaces
- Each thread gets one persistent remote sandbox, booted from the workspace snapshot. If the snapshot is stale, the update script (e.g. `git pull`) runs before the first model call. That step is bounded and non-fatal (`lifecycle.py::SandboxCreateConfig.run_update_script`). Nightly refresh jobs rebuild snapshots.
- **Secrets.** On LangSmith sandboxes, GitHub credentials never enter the box. The sandbox egress proxy injects a **workspace-scoped installation token**. Because tokens expire after one hour, a before-model middleware refreshes the proxy (`agent/github/proxy.py`, `refresh_github_proxy_before_model`). If the workspace is unknown or the lookup fails, the run gets *no* credentials rather than installation-wide ones (README). Model keys stay in the server process.
- **Local/desktop mode.** `LocalShellBackend` with **no isolation**. It strips provider keys from the environment (`agent/sandboxes/providers/local.py::LOCAL_SHELL_ENV_EXCLUDE`), redirects `GIT_CONFIG_GLOBAL`, uses a project allowlist (`agent/desktop.py`), and gives each desktop thread its own git worktree (`desktop/src/local-thread-store.cjs`).
- The code has no egress network policy. The README says only that "sandboxes can have network access."

### Agent integration
- There is no CLI agent. Chat models (Anthropic, OpenAI Responses, Google, Fireworks, Baseten) run in-process through LangChain. Reasoning effort, adaptive fast/strong routing (`ModelSelectionMiddleware`) and a fallback model are all configurable. Resolved settings are **frozen into thread metadata**, so a thread keeps its model (`server.py`, around lines 1200–1290).
- Output is LangGraph state and messages, streamed in v3 mode (`stream_subgraphs=True`). Webhook-triggered runs get the same stream shape so the dashboard can see them.
- About six middlewares exist only to paper over provider quirks, such as `SanitizeThinkingBlocks`, `RepairOrphanedToolCalls` and `StableToolResultOrder`.
- **Delegation.** Deep Agents `task` hands work to a forked subagent with filtered tools and `_SubagentToolGuard`. Across graphs, the reviewer launches review-scout on its own thread and waits for it.

### Verification & quality gates
- **Reviewer graph** (`agent/reviewer.py`).
  - It preps the repo deterministically and precomputes the diff line set, so `add_finding` rejects comments on lines outside the diff when the finding is created.
  - Its only tools are `add_finding`, `update_finding`, `list_findings` and `publish_review`.
  - Findings persist across pushes and are reconciled with the GitHub review threads (`agent/review/reconcile.py`). Each review is capped at 6 findings.
  - Prompts wrap PR text in "untrusted data" fences (`agent/resources/prompts/reviewer/*.md`).
- **Style learning** (`agent/analyzer.py`, `agent/review/style_collector.py`) mines human review comments from recent PRs (20 PRs, 10 reviewers, 6 samples each) and the outcomes of the reviewer's own findings (resolved, dismissed, 👍/👎). It turns them into a per-repo style prompt.
- **`/baby-sit`** (`agent/baby_sit.py`).
  - It combines a 10-minute cron with check-run webhooks.
  - It re-runs only jobs the agent has **recorded evidence** of being flaky (`record_retry` requires evidence), with at most 3 reruns per head SHA.
  - Before declaring a PR ready, it checks branch-protection *required* checks.
- **Guards as middleware.**
  - `WorkflowPushGuardMiddleware` requires human approval (Slack or dashboard) before any push that touches `.github/workflows/`.
  - `PullRequestCreationGuardMiddleware` blocks `gh pr create`, `curl …/pulls` and similar, so PRs go through `open_pull_request` and are attributed correctly.
  - Draft PRs can be set per user.
  - "Expedited review" (experimental) records a human's Slack approval and then merges on their behalf (`docs/reference/expedited-slack-review.md`).

### Observability & UI
- Every graph traces to LangSmith, startup phases are timed (`agent/utils/startup_trace.py`), and usage and cost go through an analytics outbox (`agent/analytics/`).
- **Live UI.** The UI subscribes over SSE: `GET /threads/{id}/transcript/events?after=<version>` (`agent/transcript/routes.py`). Postgres NOTIFY carries ids only, with an in-process fast path (`transcript/listener.py`). Snapshots are read in a single REPEATABLE READ transaction. A client more than 1000 events or 8 MB behind gets a fresh snapshot instead of a replay. Tool outputs travel as 2,000-character previews and turns are paginated 40 at a time (`transcript/snapshot.py`).
- Each turn records a **file-diff checkpoint** built with a scratch git index, so it doesn't contend with the agent's own git (`agent/utils/turn_checkpoint.py`).

### Config & extensibility
- Graphs are defined in `langgraph.json`. Prompts are Markdown files in `agent/resources/prompts/` with `$name` placeholders; `AGENTS.md` bans inline prompt text. Sandbox providers are registered in `agent/sandboxes/providers/registry.py`.
- Skills come from bundled, organization, user and repo `AGENTS.md` sources. MCP servers are tiered instance → workspace → user, and load lazily through `DynamicToolMiddleware`. See `docs/CUSTOMIZATION.md`.

### Strengths — steal these
- **A single dispatch function with explicit multitask semantics** (interrupt vs enqueue), sync durability and a mandatory completion signal (`agent/dispatch.py`, `agent/completion.py`).
- **Never auto-replace a stateful workspace.** Distinguish *unreachable* from *gone*, and bind a sandbox id only after init (`agent/sandboxes/lifecycle.py`, `state.py`).
- **Retry only failures proven to be pre-execution**, so a retry can never run a command twice (`agent/sandboxes/retry.py`).
- **An event-sourced transcript** with gapless versions, idempotent command receipts, ids-only NOTIFY, a snapshot-or-replay threshold and tool-output previews (`agent/transcript/*`).
- **Deterministic thread ids from external keys**, so routing needs no lookup table (`agent/thread_ids.py`).
- **Credentials held by an egress proxy** with scoped, auto-refreshed tokens (`agent/github/proxy.py`).
- **Policy guards as tool-call middleware** (workflow push approval, PR-creation funnel) instead of relying on the prompt (`agent/middleware/workflow_push_guard.py`, `pr_creation_guard.py`).
- **Reviewer design.** Findings validated against the diff, stable fingerprints across pushes, a per-review cap, and style learned from human feedback (`agent/review/findings.py`, `agent/analyzer.py`).
- **Evidence-gated CI babysitting** with per-SHA retry budgets and delivery dedupe (`agent/baby_sit.py`).

### Weaknesses & tradeoffs
- It is tightly coupled to LangGraph Platform. Crons, locks, run queues and durability all ride on LangGraph threads, and production self-hosting needs the licensed Agent Server (README).
- A new graph is assembled per run from about 30 order-sensitive middlewares, and `agent/server.py` alone is 1,704 lines. That is hard to reason about and slows cold starts, which is why `aphase` timing exists.
- Subagents running in parallel share one working tree. Parallelism only really happens *across* threads.
- There are two sources of truth: the checkpoint and the transcript, mirrored best-effort.
- Interrupting on follow-up is great for chat UX but kills in-flight tool calls, and depends on checkpoint quality and the `RepairOrphanedToolCalls` middleware.
- Compatibility shims are piling up (`prepare_run_id`, `__event_streaming_v2`, legacy finding shapes).
- Local mode has no sandbox at all. The static tool list is large (around 60 tools) and is only partly mitigated by dynamic tool groups.

### Implications for a Rust framework
- Make the dispatch contract a first-class type, e.g. `enum Multitask { Interrupt, Enqueue, Reject }` plus `Durability`, and implement it natively rather than inheriting it from a platform.
- Build the transcript with `sqlx` on Postgres or SQLite: a per-thread advisory lock, a `version` sequence, a receipts table, and `pg_notify` with ids only, feeding a `tokio::broadcast` fan-out for SSE (`axum::response::Sse`).
- Give the sandbox trait typed errors (`Unreachable`, `Gone`, `TransientPreExec`) and let the orchestrator's policy decide on replacement.
- Express guards as `tower::Layer`s around tool execution.
- Derive UUIDv5 ids from external keys (the `uuid` crate) for idempotent routing.

---

## 2. OpenHands (Agent Canvas + software-agent-sdk + automation)

### What it is
- `canvas:` is a React/TS SPA with about 1,440 files and around 150k TS lines including tests. It is labeled "beta", and its last commit was 2026-09-24 (`2db9c3b`).
- `sdk:` is Python with about 125k non-test LOC: sdk 75k, agent-server 30k, tools 17k, workspace 3k. Last commit 2026-09-24.
- `automation:` is about 30k LOC of Python (FastAPI, Postgres or SQLite).
- The frontend's `AGENTS.md` sets the dependency direction as Agent Server → OpenAPI → TypeScript client → Canvas.

### Architecture
```
Agent Canvas (canvas:)  ── REST + WS /sockets/events/{id}?resend_mode=since&after_timestamp=… ──┐
  backend registry: N Agent Servers (host / Docker / VM / Cloud); client-side tools (canvas_ui,  │
  launch_child_conversation)                                                                        ▼
automation: scheduler → PENDING rows → dispatcher (FOR UPDATE SKIP LOCKED) ─► Agent Server (sdk:openhands-agent-server)
            watchdog (timeout_at) ◄── POST /v1/runs/{id}/complete ──────────    ConversationService (≤10 concurrent runs)
                                                                                 └ EventService (per conversation, PubSub, lease)
                                                                                    └ LocalConversation.run() → Agent.step()
                                                                                        ├ LLM via LiteLLM   | ACPAgent → subprocess (claude-agent-acp, codex-acp, gemini)
                                                                                        ├ tools: terminal, file_editor, browser, delegate, task, …
                                                                                        └ EventLog: conversations/<id>/events/event-00042-<uuid>.json
Workspace impls: Local | Docker (runs the agent-server image) | RemoteAPI | Cloud | Apptainer | k8s AgentSandbox
```
The central change from V0 is that **the loop runs inside the sandbox**. `DockerWorkspace` runs `docker run --rm -p <port>:8000 <agent-server-image>`, waits for a health check, and hands the client a `RemoteWorkspace` (`sdk:openhands-workspace/openhands/workspace/docker/workspace.py`). Python `RemoteConversation` and the TS client speak the same API (`sdk:openhands-sdk/openhands/sdk/conversation/impl/remote_conversation.py`).

### Core abstractions / domain model
- **Event** (`sdk:…/sdk/event/base.py`): a frozen Pydantic model with `id`, `timestamp`, `source` and **`parent_id`**, so the log forms a **conversation tree** that supports `fork()` and `navigate_to()`.
  - LLM-convertible events: `SystemPromptEvent`, `MessageEvent`, `ActionEvent`, `ObservationEvent`, `UserRejectObservation`, `AgentErrorEvent`, `CondensationSummaryEvent`.
  - Control and telemetry events: `Condensation`, `CondensationRequest`, `ConversationStateUpdateEvent`, `PauseEvent`, `InterruptEvent`, `HookExecutionEvent`, `StreamingDeltaEvent`, `ACPToolCallEvent`, `ConversationErrorEvent`.
- **ConversationState** (`sdk:…/conversation/state.py`).
  - `execution_status` ∈ {IDLE, RUNNING, PAUSED, WAITING_FOR_CONFIRMATION, FINISHED, ERROR, STUCK, DELETING}. `is_terminal()` covers FINISHED, ERROR and STUCK.
  - It also holds `confirmation_policy`, `security_analyzer`, `leaf_event_id` (the HEAD of the tree), `stats` and `secret_registry`.
  - Every attribute change triggers an automatic save.
- **Agent** is stateless and frozen, so one instance can be shared across conversations (`agent/agent.py`). **ACPAgent** is in `agent/acp_agent.py`.
- **Tools** declare the resources they touch with `declared_resources()`. **Workspace** is in `sdk/workspace/base.py`. Other pluggable pieces: Condenser, Critic, SecurityAnalyzer, and ConfirmationPolicy {AlwaysConfirm, NeverConfirm, ConfirmRisky}.
- **AutomationRun** {PENDING, RUNNING, COMPLETED, FAILED, CANCELLED} carries `timeout_at` and `current_phase` (`automation:openhands/automation/models.py`).

### Orchestration model
- **Inner loop** (`LocalConversation.run`, `local_conversation.py:1903`). Each iteration takes a FIFO state lock and then:
  1. Exits on PAUSED or STUCK.
  2. On FINISHED, runs **stop hooks**, which can veto the stop and inject feedback.
  3. Runs `StuckDetector`, which catches repeated action/observation pairs, error loops and monologues.
  4. Calls `agent.step()`.
  5. Breaks on WAITING_FOR_CONFIRMATION, on budget exceeded, or on `max_iteration_per_run`, which moves the conversation to ERROR and emits `ConversationErrorEvent`.
- **`Agent._step`** (`agent.py:645`).
  1. It first executes any pending, now-confirmed actions.
  2. It builds messages from an incrementally maintained `View` and the condenser.
  3. It calls the LLM with `add_security_risk_prediction=True`, so the model labels each action's risk.
  4. It maps errors to recovery paths instead of crashing:
     - a malformed tool call becomes a user feedback message;
     - a content-filter block becomes a nudge;
     - context overflow or malformed history becomes a `CondensationRequest`.
  5. Tool calls go through `ParallelToolExecutor`, which has a per-agent concurrency limit and a `ResourceLockManager` that serializes tools declaring the same resource (`agent/parallel_executor.py`).
- **Interrupt.** asyncio cancellation propagates through the LLM stream, the step and the loop, then sets PAUSED and emits `InterruptEvent`. Actions left without an observation get a synthetic error (`_emit_orphaned_action_errors`).
- **Server.**
  - `ConversationService.max_concurrent_runs = 10` (a thread pool) plus idle eviction.
  - A per-conversation **owner lease** with generation fencing: 45 s TTL, a PID-liveness check, and a write guard on the EventLog (`agent_server/conversation_lease.py`). This lets several server processes share a conversations directory safely.
- **Automation.**
  - The scheduler turns cron entries into PENDING rows. The dispatcher claims them with `FOR UPDATE SKIP LOCKED` and dispatches fire-and-forget. Completion arrives by callback.
  - The watchdog fails RUNNING runs that pass `timeout_at` only after asking the backend whether bash is still running; if it is, the watchdog extends the deadline up to a hard cap.
  - When an organization hits its concurrency limit, the run is skipped with a status marked transient (`automation:dispatcher.py`, `watchdog.py`).
- **`/goal`**: a second LLM judges completion and re-prompts until the goal is met or a cap is reached (`sdk:…/conversation/goal/controller.py`).

### State & persistence
- State is file-based: `conversations/<hex>/meta.json`, `base_state.json`, and **one JSON file per event** named `events/event-{idx:05d}-{uuid}.json` (`conversation/persistence_const.py`). `EventLog` uses flock plus a length-marker file, and warns that flock is unreliable on NFS (`conversation/event_store.py`). Secrets are encrypted with `OH_SECRET_KEY`; without it, persisted secrets don't survive a restart.
- **Crash recovery.** At boot the server loads every conversation persisted as RUNNING. Any `ActionEvent` without an observation gets an `AgentErrorEvent` ("A restart occurred while this tool was in progress…"), explicitly parented to that action so the tree stays consistent, and the status moves to ERROR (`agent_server/conversation_service.py:~2220`, `event_service.py:~1215`).
- **Compatibility is strict.** Old events must always deserialize, so deprecation handlers are permanent (`sdk:…/sdk/AGENTS.md`). Persisted settings carry a `schema_version`, a migration chain, golden fixtures and a CI gate (root `AGENTS.md`).

### Isolation & workspaces
- Isolation is a spectrum:
  - **none**: the default `agent-canvas` launcher runs the agent server on the host, and the README warns about full filesystem access;
  - one Docker container with `$PROJECTS_PATH` mounted at `/projects`;
  - Apptainer;
  - a VM;
  - OpenHands Cloud;
  - a Kubernetes agent-sandbox.
- **Per-conversation git worktrees** live at `/tmp/conversation-worktrees/<conversation_id>/<repo>`. Each is cut from a freshly fetched origin default branch and comes with injected guidance text (`agent_server/conversation_service.py::_create_conversation_worktree`, `_get_worktree_start_point`). Child conversations can choose `isolation: worktree|shared` and `target: local|cloud` (`canvas:src/constants/child-conversation.ts`).
- **Secrets** go through `SecretRegistry`, `LookupSecret` (fetched from a URL at use time) and a single redaction path (`sdk/utils/pydantic_secrets.py`).
- **Warm pools.** A server started with `OH_DEFERRED_INIT` stays dormant until `POST /api/init` delivers the per-user config (`agent_server/init_router.py`).
- There is no network policy in the SDK; it is left to the container runtime.

### Agent integration
- **Native agent.** A LiteLLM-backed `LLM` that works with any provider, with function calling, reasoning, caching and streaming. Model quirks live in a registry (`llm/utils/model_features.py`), not in `if` branches.
- **Third-party agents via ACP** (`agent/acp_agent.py`, 4.7k lines).
  - It spawns `claude-agent-acp`, `codex-acp` or `gemini` as a subprocess speaking stdio JSON-RPC (`acp.client.connection.ClientSideConnection`).
  - One `step()` equals one remote turn, which ends with a synthetic `FinishAction`.
  - `session_update` messages (message and thought chunks, `ToolCallStart`/`ToolCallProgress`, `UsageUpdate`) are mapped to OpenHands events.
  - It implements the client-side fs and terminal methods.
  - It uses an **idle** timeout, reset on every token or tool update, for prompts and a **hard** timeout for startup. `acp_resume_session_id` resumes sessions, and each conversation gets its own CLI config directory.
  - **`request_permission` auto-approves the first option** (`acp_agent.py:1602`), so the subagent's own permission prompts are bypassed.
- **Delegation.** `DelegateExecutor` (`sdk:openhands-tools/…/delegate/impl.py`) spawns named child `LocalConversation`s (`max_children=5`) and runs them in parallel threads. The parent **blocks** until they finish, and children share the parent's working directory. Subagent definitions are Markdown files with YAML frontmatter in `.agents/agents/*.md`, with first-registration-wins precedence (`sdk/subagent/AGENTS.md`).
- **Client tools.** The frontend can execute tools itself, e.g. switching Canvas panels or launching a child conversation (`sdk/tool/client_tool.py`, `canvas:src/api/canvas-ui-client-tool.ts`).

### Verification & quality gates
- **Confirmation mode.** Risky actions pause the loop in WAITING_FOR_CONFIRMATION. Calling `run()` again executes them (implicit approval), and `reject_pending_actions` records a `UserRejectObservation`. Risk comes from the LLM's self-labeling plus analyzers: an LLM analyzer, GraySwan, an ensemble, a shell-AST parser and defense-in-depth (`sdk/security/`).
- **Hooks**: PreToolUse, UserPromptSubmit and Stop. **Critics** (`sdk/critic/`: agent_finished, empty_patch, pass, API) with `IterativeRefinementConfig(success_threshold, max_iterations)` for automatic retries. Also the `/goal` judge and the stuck detector.
- PR review of the project itself runs through an OpenHands Cloud automation, not a workflow in the repo (`sdk:AGENTS.md`).

### Observability & UI
- Each conversation has a `PubSub`. The WebSocket `/sockets/events/{id}` takes `resend_mode=all|since&after_timestamp` (`agent_server/sockets.py`). Streaming token deltas are opt-in per subscriber (`pub_sub.py`). The server also supports webhooks, Laminar/OTel tracing (`sdk/observability`) and telemetry exporters.
- **Client handshake.** Subscribe to the WebSocket first, then reconcile over REST and deduplicate by event id (`RemoteEventsList.reconcile`, `remote_conversation.py:350,1152`). Canvas loads history over REST, then opens the WebSocket with `since` (`canvas:src/hooks/query/use-conversation-history.ts`, `src/contexts/conversation-websocket-context.tsx`).
- **Caveats.** Resends are keyed on `timestamp = datetime.now().isoformat()`, a naive local time with `>=` semantics. The wire protocol has **no monotonic sequence number**, so clients have to deduplicate.

### Config & extensibility
- `AgentSettings` and `ConversationSettings` export a JSON schema that drives the UI (`sdk/settings/`). Beyond that: LLM profiles, an MCP server map, AgentSkills with progressive disclosure, plugins and marketplaces, hooks, and file-based subagents. `@openhands/extensions` supplies public skills and automations.
- Canvas injects a `<RUNTIME_SERVICES>` block into the system prompt so agents stop guessing ports (`canvas:AGENTS.md`).

### Strengths — steal these
- **The sandbox as a server.** One API serves the local, Docker, VM and cloud backends, and a UI can connect to many servers (`sdk:workspace/docker/workspace.py`, `canvas:src/api/backend-registry`).
- **Events as a tree** (`parent_id` + `leaf_event_id`), which gives fork, branch and rewind cheaply (`event/base.py`, `state.py`).
- **Crash recovery by backfilling synthetic observations for orphaned tool calls**, which keeps LLM history valid (`event_service.py`, `local_conversation.py::_emit_orphaned_action_errors`).
- **Tool-level resource locks** allow parallel tool calls safely (`parallel_executor.py`, `resource_lock_manager.py`).
- **A lease with generation fencing** for conversation ownership (`conversation_lease.py`).
- **Automation queue**: `SKIP LOCKED`, a completion callback, and a watchdog that checks with the backend before failing a run (`automation:dispatcher.py`, `watchdog.py`).
- **ACP as the adapter** for Claude Code, Codex and Gemini, with an idle watchdog driven by activity (`acp_agent.py`).
- **Discipline around persisted-schema compatibility** (golden fixtures, migrations, permanent event deprecation handlers).
- **Error-to-recovery mapping in the step** (condense on overflow, nudge on content filter) and **stop hooks that can veto "done."**

### Weaknesses & tradeoffs
- It is split across four repos with a compatibility matrix (`config/defaults.json → compatibility.minimumAgentServer`), so any feature touches several of them.
- One JSON file per event on a local filesystem, locked with flock, has no database, is unsafe on NFS and scales poorly.
- It relies on Python threads sharing mutable conversation objects, and parallel safety holds only if each tool declares its resources correctly.
- ACP permissions are auto-approved. Delegated children share the workspace and block the parent. The default local mode has no sandbox.
- Resends keyed on timestamps are fragile.
- The core files are huge: `local_conversation.py` is 3.1k lines and `acp_agent.py` is 4.7k.

### Implications for a Rust framework
- Adopt an **agent-server-in-sandbox** protocol: the orchestrator talks to a small daemon in each workspace over HTTP/WS. A Rust daemon compiles to a static binary that is easy to inject into any container or VM.
- Use an event envelope with `id`, `parent_id`, a **monotonic `seq`** and a UTC timestamp. Resend by `seq`, not by time.
- Model an ACP client with `tokio::process` and JSON-RPC (e.g. the `agent-client-protocol` crate). Route **permission requests to a policy engine** instead of auto-approving them.
- Implement a `ResourceLock` registry (keyed async mutexes) for parallel tool and sub-agent execution.
- Keep a status enum like OpenHands's: WaitingForConfirmation, Stuck and Paused are distinct from Error.

---

## 3. anthropics/claude-code-action

### What it is
The action is TypeScript on Bun: about 11k LOC in `src/` and `base-action/src`, plus a 1.8k-line Python `agent-approval-check`. It is v1.0 and in wide use. It pins Claude Code 2.1.281 and Agent SDK 0.3.281. Last commit: 2026-09-23 (`8cf3482`).

### Architecture
```
GitHub event ─► workflow job (ephemeral runner) ─► action.yml (composite)
   setup-bun → bun install → [bubblewrap/socat if allowed_non_write_users]
   → bun src/entrypoints/run.ts
        prepare: parseGitHubContext → detectMode(tag|agent) → setupGitHubToken (OIDC→App token)
                 → checkWritePermissions → checkContainsTrigger → checkHumanActor
                 → createInitialComment (tracking) → fetchGitHubData → setupBranch → configureGitAuth
                 → createPrompt(file) → prepareMcpConfig → claudeArgs(--allowedTools, --permission-mode acceptEdits)
        install Claude Code (curl install.sh, pinned, ×3)
        restoreConfigFromBase (PR) → settings → plugins → runClaude → Agent SDK query()
        finally: updateCommentLink (final header, branch/PR links), step summary
   always(): cleanup SSH key · post buffered inline comments (Haiku-classified) · revoke App token
```

### Core abstractions / domain model
- `GitHubContext`, a discriminated union of entity events and automation events (`src/github/context.ts`).
- Mode `"tag" | "agent"` (`src/modes/detector.ts`). Tag mode handles `@claude` mentions, labels and assignees. Agent mode runs when a `prompt` input is present.
- The tracking comment id; `branchInfo {baseBranch, claudeBranch, currentBranch}`.
- `ClaudeRunResult {conclusion, executionFile, sessionId, structuredOutput}` (`base-action/src/run-claude-sdk.ts`). The execution file is the raw array of SDK messages.

### Orchestration model
- **One event → one job → one run.** Tag mode is triggered by `issue_comment`, `pull_request_review(_comment)` and `issues` (opened, assigned, labeled). With a `prompt`, `pull_request`, schedule, dispatch and `workflow_run` events run in agent mode.
- **There is no queue or concurrency control inside the action.** Two `@claude` comments on the same PR run as parallel jobs pushing to the same branch unless the user adds a GitHub `concurrency:` group, and neither the examples nor the docs mention one.
- **Retries.** CLI install is retried 3 times with a 5 s sleep (`run.ts::installClaudeCode`). The OIDC and token exchange use `retryWithBackoff` (3 attempts, 5 s initial delay, ×2; `base-action/src/retry.ts`). **The model run itself is never retried.**
- **State machine**: prepare → install → run → cleanup. `prepareCompleted` decides whether an error is reported as a prepare failure or a run failure (`run.ts`). As a hang guard, the loop breaks explicitly on the `result` message because `query()` sometimes never closes. A run that reports success but exceeded `maxTurns` is converted to a failure.

### State & persistence
- The action is stateless. **GitHub is the store**: the tracking comment, the `claude/<entity>-<n>-<ts>` branch, and the PR.
- A `session_id` output lets a later job `--resume`. The execution JSON lives in `RUNNER_TEMP`. The prompt directory is wiped before each run because self-hosted runners don't reliably clear `RUNNER_TEMP` (`create-prompt/index.ts`).
- **Crash semantics.**
  - Token revocation, SSH-key cleanup and inline-comment posting are separate `always()` steps, so they survive a crash of `run.ts`.
  - The **final comment update happens in `run.ts`'s `finally`**, so if the job times out or is killed, the comment stays at "working…" with its spinner.
  - When no commits were made, the branch is deleted (`github/operations/branch-cleanup.ts`).

### Isolation & workspaces
- Isolation is the GitHub-hosted runner VM itself: ephemeral and one per job. The workspace is the `actions/checkout` directory.
- **Tool policy** (`src/modes/tag/index.ts`).
  - `--permission-mode acceptEdits` allows edits inside `$GITHUB_WORKSPACE` only. Write, Edit and MultiEdit are deliberately *not* allow-listed, because doing so would permit writes anywhere on the runner.
  - The explicit `--allowedTools` list is: Glob, Grep, LS and Read; the comment and CI MCP tools; and `Bash(git add:*)`, `Bash(git commit:*)`, `Bash(git rm:*)` and `Bash(<git-push.sh>:*)`.
  - Headless mode denies anything that falls through to "ask."
- **`scripts/git-push.sh`** accepts exactly `origin <ref>` and no flags. This closes an RCE through `--receive-pack` and exfiltration to another remote (H1 #3556799).
- **`restoreConfigFromBase`** (`src/github/operations/restore-config.ts`). On PRs it resets `.claude`, `.mcp.json`, `.claude.json`, `.gitmodules`, `.ripgreprc`, `CLAUDE.md`, `CLAUDE.local.md` and `.husky` to the base branch's versions before the CLI starts, then keeps those paths out of the commit.
- **Content hardening.**
  - Invisible Unicode, image alt text, link titles and hidden HTML attributes are stripped (`src/github/utils/sanitizer.ts`). `redactSecrets` scrubs comments and the step summary.
  - **TOCTOU filter**: comments created *or edited* after the trigger time are dropped (`src/github/data/fetcher.ts:~212–300`).
  - Only the trigger comment counts as instructions; everything else is context (prompt text in `create-prompt/index.ts`).
- **Credentials.** A GitHub App token is obtained through an OIDC exchange at `api.anthropic.com`; it is short-lived, scoped to the repo and revoked afterwards (`src/github/token.ts`). Anthropic API auth can use workload-identity federation, which also relies on OIDC (`base-action/src/workload-identity.ts`).
- **Actor checks.** The actor must have write access, and so must the upstream actor for `workflow_run` (`validation/permissions.ts`). Bots are refused unless listed in `allowed_bots` (`validation/actor.ts`).
- `allowed_non_write_users` enables bubblewrap subprocess isolation and environment scrubbing (`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`). The action has no network policy.

### Agent integration
- It supports Claude Code only. The binary is installed at a pinned version and driven in-process through `@anthropic-ai/claude-agent-sdk` `query()` with `pathToClaudeCodeExecutable` (`base-action/src/run-claude-sdk.ts`). It is not wrapped as a CLI subprocess.
- When a user request can be extracted, it sends a **two-block user message**: first the context and instructions, then the request. That way `@claude /review-pr` still triggers slash-command processing.
- `claude_args` are parsed into SDK options, and repeated `--allowedTools` and `--disallowedTools` flags are merged (`parse-sdk-options.ts`).
- Output is parsed from the `result` message into success or failure. With `--json-schema`, a `structured_output` field becomes an action output.
- **Reporting back goes through stdio MCP servers** launched with `bun` (`src/mcp/install-mcp-server.ts`):
  - `github_comment.update_claude_comment` edits the single tracking comment;
  - `github_file_ops` makes signed commits through the API;
  - `github_inline_comment` buffers comments to JSONL for later classification;
  - `github_ci` reads CI status and logs;
  - the official `github-mcp-server` runs in Docker.
- There is no multi-agent support.

### Verification & quality gates
- **Humans stay in the loop by design.** Tag mode never opens PRs. The model is told to output a pre-filled `compare/...?quick_pull=1` link, and a human has to click it (`docs/security.md`, `create-prompt/index.ts`). There are no formal approvals or merges.
- **Inline comments are classified after the session**: Haiku drops test or "probe" comments, and deduplicates confirmed ones (`src/entrypoints/post-buffered-inline-comments.ts`, `src/mcp/inline-comment-buffer.ts`).
- **`agent-approval-check`** detects agent-authored commits and requires N distinct human approvals from users with write access. Approval can come from a review or a `/approve <head-sha>` comment, and an approval goes stale when the head moves. The result is posted as a required commit status plus a sticky comment (`agent-approval-check/README.md`).
- Tests or lint run only if the user allow-lists the commands. The action has no built-in verification step.

### Observability & UI
- **The GitHub comment is the UI.**
  - The initial comment shows a spinner and a job link.
  - The model maintains a markdown checklist and edits it through MCP.
  - `updateCommentLink` writes the final header ("Claude finished @user's task in 2m 3s", or an error), plus branch and PR links (`src/github/operations/comment-logic.ts`, `entrypoints/update-comment-link.ts`).
  - Sticky mode reuses the bot's previous comment on the PR (`comments/create-initial.ts`).
- The step summary is rendered from the execution file (`entrypoints/format-turns.ts`). Full output is hidden by default because tool results can contain secrets.
- OTel env vars are passed through to Claude Code (`action.yml`). Outputs: `conclusion`, `execution_file`, `branch_name`, `session_id`, `structured_output`.
- Updates are only as live as the model's comment edits.

### Config & extensibility
- Action inputs cover trigger phrase, label and assignee triggers, the branch-name template, `allowed_bots`, actor include/exclude filters, `settings` JSON, `claude_args`, plugins and marketplaces, `additional_permissions`, commit signing, `use_sticky_comment`, `track_progress`, and Bedrock/Vertex/Foundry.
- Repo `CLAUDE.md` and `.claude/` are honored, but on PRs they are taken from the base branch.
- **Where docs and code disagree:**
  - `CLAUDE.md` says MCP servers are "auto-installed to `~/.claude/mcp/github-{type}-server/`". In the code they run from the action path via `bun`, and the official server runs via `docker`.
  - `createPrompt` still exports `ALLOWED_TOOLS`/`DISALLOWED_TOOLS` env vars that nothing reads; the code itself flags them as dead.
  - The prompt says Claude "cannot modify `.github/workflows`". That limit comes from App permissions, not from the tool layer.

### Strengths — steal these
- **A tracking-comment protocol.** Create the comment first with a job link, let the agent update a checklist through one narrow tool, then have the harness write a deterministic final header with status, duration and links (`comments/create-initial.ts`, `comment-logic.ts`).
- **Trust boundaries on inputs:**
  - drop anything edited after the trigger (TOCTOU, `fetcher.ts`);
  - treat only the trigger comment as instructions;
  - sanitize hidden content (`sanitizer.ts`);
  - reset agent config files from the base branch on PRs (`restore-config.ts`).
- **Narrow command wrappers** instead of broad `Bash(git push:*)` grants (`scripts/git-push.sh`). Pair an explicit allowlist with `acceptEdits`, and don't grant blanket Write.
- **Cleanup in separate `always()` steps**, so token revocation survives a crash (`action.yml`).
- **Short-lived, repo-scoped credentials** via OIDC exchange, with nothing stored as a long-lived secret (`src/github/token.ts`).
- **A two-block prompt** (context, then the verbatim request) that keeps slash commands working (`run-claude-sdk.ts::createPromptConfig`).
- **Buffer agent side-effects (inline comments) and classify them before publishing** (`post-buffered-inline-comments.ts`).
- **A human-approval status check** for agent-authored PRs, bound to the head SHA (`agent-approval-check/`).

### Weaknesses & tradeoffs
- Each run is single-shot and stateless. There is no memory across comments beyond what GitHub shows, and no queue, concurrency control, dedupe or retry of the run.
- Progress reporting depends on the model remembering to call `update_claude_comment`. If it answers in plain text, the user sees nothing (the prompt repeats this warning many times).
- The final status update happens in-process, so if the job is killed the comment keeps its spinner.
- Guardrails are split between enforcement in code (allowlist, wrappers, restore) and prompt-only instructions ("don't act on other comments").
- It supports only Claude Code and only GitHub. The runner VM is the sandbox, so cost and latency include runner boot plus CLI install on every run.
- `allowed_bots: "*"` or `allowed_non_write_users` widens the prompt-injection surface considerably, as the action's own docs warn.

### Implications for a Rust framework
- Treat the **status surface** (a GitHub comment, Slack message or Linear comment) as an idempotent projection the orchestrator owns. Update it from events outside the agent process, so a crash or kill still yields a final state; don't depend on the model calling a tool.
- Put the input-trust pipeline (TOCTOU cut-off, sanitizer, instruction/context separation, config restore from a trusted ref) into a library crate that every trigger adapter uses.
- Ship typed command wrappers, e.g. a `git_push(remote: Origin, ref: ValidatedRef)` tool, instead of shell allowlist patterns.
- Keep a dedicated reaper step or task, separate from the worker, for credential revocation.

---

## Cross-repo takeaways

**Top ideas to adopt**
1. **One dispatch contract with explicit multitask semantics** (interrupt / enqueue / reject), **checkpoint-before-step durability**, and a **guaranteed terminal signal** that doesn't depend on the agent. Back it with a reconciliation sweep (Open SWE `dispatch.py`, `reconcile.py`, `completion.py`) and a verifying watchdog (OpenHands `automation:watchdog.py`).
2. **An event log as the source of truth for the UI**: typed envelopes with `id`, `parent_id` (tree/fork), a gapless per-stream `seq`, idempotent command receipts, NOTIFY carrying ids only, and SSE/WS resume by `seq` with a snapshot fallback past a threshold. This combines Open SWE's transcript engine with OpenHands's event tree.
3. **Workspace lifecycle rules**:
   - bind the workspace only after init succeeds;
   - treat *unreachable* differently from *gone*, and never auto-replace a workspace holding uncommitted work;
   - retry only failures proven to be pre-execution;
   - on restart, backfill synthetic error observations for in-flight tool calls.
4. **Pluggable agents behind one adapter protocol**: ACP over stdio for Claude Code, Codex and Gemini (OpenHands), plus a native loop. Put **permissions behind a policy engine** in both cases.
5. **Policy as middleware or layers around tool calls**: workflow-file push approval, a PR-creation funnel, narrow command wrappers, config restore from the base branch, TOCTOU-filtered and sanitized inputs, and a human-approval status check for agent PRs.

**Top pitfalls to avoid**
- **Outsourcing durability to a platform you can't embed.** Open SWE depends on LangGraph Platform for crons, locks and queues, and needs a license to self-host.
- **Parallel sub-agents in a shared working tree.** Both Open SWE `task` and OpenHands `DelegateExecutor` do this. Give each parallel unit its own worktree or container by default.
- **Auto-approving an external agent's permission prompts.** OpenHands `ACPAgent.request_permission` does this.
- **Timestamp-based resend and file-per-event storage on local disk.** In OpenHands this means naive local times, `>=` duplicates and NFS-unsafe flock.
- **Progress or status that depends on the model calling a reporting tool, or on in-process `finally` blocks.** claude-code-action has both. Make status a projection that the orchestrator writes.
- **Running with no sandbox by default.** Both the OpenHands `agent-canvas` host mode and Open SWE desktop/local mode do this; make it an explicit, loudly labeled opt-in.
