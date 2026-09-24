# 01 — Tracker-driven daemons: Symphony, Kata Symphony, OpenFactory

All three projects run a long-lived daemon that takes its work queue from an external system (Linear, GitHub, Jira, Asana or GitLab issues, or webhooks) and turns each work item into an isolated agent run that ends in a PR plus a human handoff. They sit at different points on one axis: how much of the workflow is **policy written in a prompt** and how much is a **typed state machine the orchestrator owns**.

- **openai/symphony** is spec-first and minimal. `SPEC.md` is language-agnostic, and the Elixir reference implementation is about 13k LOC. Its orchestrator only *reads* the tracker. It schedules, retries and reconciles. Everything else (moving tickets between states, creating PRs, handling review, merging) happens in a roughly 300-line prompt the agent runs with tracker tools.
- **gannonh/kata-symphony** started as a Rust implementation of that spec (about 86k LOC). It has grown into a staged, durable "software factory": triage → spec → implement → review → verify. It uses SQLite, leases, content-addressed git bundles, credential-free workers and a verification gate the agent cannot override.
- **kunalm2345/openfactory** is a small TypeScript orchestrator (about 7k LOC). It pushes isolation furthest: a disposable cloud VM per run, and a second fresh VM for adversarial review. It uses a compare-and-swap (CAS) state machine over SQLite and has explicit human gates.

Read together, they show the tradeoffs among the prompt-as-workflow approach, orchestrator-enforced gates, and durable effects.

Paths below are relative to each repo's root. Clones are in `scratchpad/research/<repo>`. Line numbers are from shallow clones taken on 2026-09-24.

---

## openai/symphony

### What it is
The repo holds two things:
- A normative, RFC-2119 service specification (`SPEC.md`, 2,312 lines, "Draft v1").
- An Elixir/OTP reference implementation in `elixir/`: about 13.1k LOC in `lib/`, about 15.3k LOC of ExUnit tests, Phoenix LiveView and Bandit.

It is Apache-2.0 and self-described as a "low-key engineering preview" and "prototype software intended for evaluation only". The last commit is 2026-09-15; nightly Burrito binaries are built for 4 platforms. The README's first suggestion for adopting it is: *tell your coding agent to implement SPEC.md in the language of your choice*.

### Architecture
The spec's components (§3.1) map one-to-one onto Elixir modules:

```
WORKFLOW.md ──(1 s stat+hash poll)──► WorkflowStore ──► Config (Ecto embedded schemas)
                                                           │
Tracker adapter ◄── read: fetch_issues_by_states / by_ids ── Orchestrator (GenServer, sole state owner)
 (linear|github|gitlab|jira|asana|memory)                  │ tick: reconcile → validate → fetch → sort → dispatch
      ▲                                                    │ Task.Supervisor.start_child + Process.monitor
      │ host-side tool execution                           ▼
DynamicTool ◄── item/tool/call ── Codex.AppServer ◄── AgentRunner (workspace → hooks → N turns)
                                   │ JSON-RPC over stdio: `bash -lc "codex app-server"` (local, or via ssh)
                                   ▼
             send(orchestrator, {:codex_worker_update, id, ev}) ─► PubSub ping ─► LiveView re-pulls snapshot
```

- `AgentRuntimeSupervisor` (`elixir/lib/symphony_elixir/agent_runtime_supervisor.ex`) runs the `Task.Supervisor` and the `Orchestrator` under `:one_for_all`. If the orchestrator crashes, all its workers die with it. That is consistent with "no durable state": a restarted orchestrator re-derives everything from the tracker.
- Workers never mutate scheduling state. They only send messages (`{:codex_worker_update, …}` and `{:worker_runtime_info, …}`) and exit. The orchestrator turns the `:DOWN` reason into a state transition (`orchestrator.ex` `handle_info({:DOWN,…})` → `handle_agent_down`).

### Core abstractions / domain model
- **Issue** (SPEC §4.1.1, `tracker/issue.ex`). A normalized record with these fields:
  - `id`: an opaque dispatch identity. It may be a project-item ID rather than the ticket ID.
  - `identifier`: the human key, used for the workspace name and API routes.
  - `native_ref`: opaque, non-secret provider IDs, passed only to tools.
  - `dispatchable`: an explicit boolean the adapter computes (assignee, blockers, board membership). The scheduler never re-derives provider rules.
  - `labels`: lowercased.
  - `blocked_by`, `priority`, `created_at`.
- **Claim state** (§7.1): `Unclaimed → Claimed → {Running | RetryQueued} → Released`. The spec keeps this separate from tracker state. Elixir adds a fifth, undocumented-in-spec state: a `blocked` map, used when Codex asks for input or approval (`orchestrator.ex` `block_issue_from_entry`, L757).
- **Run attempt phases** (§7.2): 11 phases from `PreparingWorkspace` through `Succeeded/Failed/TimedOut/Stalled/CanceledByReconciliation`. The spec stresses that "distinct terminal reasons are important". Elixir does not model them as an enum; they collapse into Task exit reasons.
- **Live session**: `session_id = "<thread_id>-<turn_id>"`, token counters, and `last_reported_*` counters for delta accounting. **RetryEntry** holds `attempt` and a monotonic `due_at_ms`. Elixir adds a `retry_token` ref so stale timer messages are ignored (`schedule_issue_retry` L1034, `pop_retry_attempt_state` L1075).

### Orchestration model
- **Work source:** the tracker is polled on a fixed cadence (default 30 s; the shipped `elixir/WORKFLOW.md` uses 5 s). `POST /api/v1/refresh` triggers an immediate, coalesced tick.
- **Tick** (§8.1, `maybe_dispatch` L256):
  1. Reconcile running issues: stall detection, then a tracker refresh of running issues by ID.
  2. Preflight-validate the config.
  3. Fetch candidates in active states.
  4. Sort by priority (1–4, then null), then `created_at`, then identifier.
  5. Dispatch while slots remain.

  A validation failure skips dispatch but still reconciles.
- **Eligibility** (§8.2, `should_dispatch_issue?` L816): active and not terminal, `dispatchable`, has all `required_labels`, not already claimed or running, and slots are free under all three limits:
  - global `max_concurrent_agents` (default 10)
  - per-state `max_concurrent_agents_by_state`
  - per-SSH-host `worker.max_concurrent_agents_per_host`

  Elixir also re-fetches each issue right before spawning (`refresh_issue_for_dispatch` L920) to avoid stale dispatch. That costs one extra tracker call per dispatch.
- **Worker loop** (`agent_runner.ex` `do_run_codex_turns`): one Codex process and one thread per worker lifetime, up to `max_turns` (default 20) turns. After each turn the worker re-fetches the issue and continues only while it is still active and routable. Turn 1 sends the full rendered prompt. Later turns send a short fixed "Continuation guidance" block (`build_turn_prompt`) rather than resending the task.
- **Retries** (§8.4):
  - A normal exit gets a **continuation retry** after 1 s (attempt 1), so the orchestrator re-checks whether more work is needed.
  - An abnormal exit gets `min(10 s·2^(n-1), max_retry_backoff_ms=5 min)` (`failure_retry_delay` L1240).
  - When a retry fires, the orchestrator re-fetches the issue by ID. It then releases the claim, cleans up if terminal, dispatches, or requeues with `"no available orchestrator slots"`.
- **Reconciliation** (§8.5):
  - Terminal state: kill the worker and delete the workspace.
  - Non-active or unroutable: kill the worker and keep the workspace.
  - Missing from the tracker: kill the worker.
  - A refresh error keeps workers running.
  - A stall (no event for `stall_timeout_ms`, 5 min) means kill and retry.
  - A separate `turn_timeout_ms` (1 h) is a *silence* timeout inside the stdio receive loop, not a total-time cap.
- **Humans control runs through tracker state.** Moving a ticket to a terminal or non-active state stops the run (§14.4). A "successful" run usually ends at a handoff state such as `Human Review`, not `Done`.

### State & persistence
The design is deliberately in-memory (§2.1, §14.3). After a restart, no retry timers, blocked entries or token totals survive. Recovery works as follows:
1. Startup terminal cleanup deletes workspaces of issues that are already terminal.
2. The orchestrator re-polls the tracker.
3. It re-dispatches eligible work into the preserved per-issue workspaces.

Continuity lives in two places outside the daemon: the workspace on disk, and the tracker. The shipped workflow makes a single `## Codex Workpad` Linear comment the agent's durable progress journal, with plan, acceptance criteria, validation and notes (`elixir/WORKFLOW.md`). The spec lists "persist retry queue and session metadata" as a TODO (§18.2).

### Isolation & workspaces
- **Layout:** one directory per issue, `<workspace.root>/<workspace_key>`. The key is sanitized to `[A-Za-z0-9._-]`. If sanitizing changed the identifier, a SHA-256 suffix of the original is appended (`workspace.ex` `workspace_key` L268). Workspaces are reused across attempts and not deleted on success.
- **Safety invariants** (§9.5): cwd must equal the workspace, and the workspace must be under the root. Elixir canonicalizes both paths and explicitly rejects symlink escape (`codex/app_server.ex` `validate_workspace_cwd`).
- **No built-in VCS.** The `after_create` hook does `git clone`. `before_remove` runs `mix workspace.before_remove`, which closes the branch's open PRs (`lib/mix/tasks/workspace.before_remove.ex`).
- **Sandboxing** is delegated to Codex:
  - The defaults are safe: `thread_sandbox: workspace-write` and an `approval_policy` reject-map (`config/schema.ex`).
  - The shipped `elixir/WORKFLOW.md` loosens this to `approval_policy: never`, `shell_environment_policy.inherit=all` and `networkAccess: true`.
  - The spec explicitly declines to mandate a sandbox (§2.2, §15).
- **Secrets:** each adapter declares `secret_environment_names`. The launcher both `unset`s them and removes them from the Port env (`tracker_secret_port_env` and `tracker_secret_unset_command` in `app_server.ex`). Tracker tools execute host-side. However, the tools are raw GraphQL/REST (`linear_graphql`, `github_api`, `jira_rest`…). `project_slug` scopes only the scheduler's reads, and "the tool can access whatever the configured Linear token can access" (`elixir/README.md`, Linear profile).
- **SSH workers** (Appendix A, `ssh.ex`): the orchestrator stays central and runs `ssh -T host bash -lc '<cmd>'` with stdio as transport. It keeps host affinity on retries and waits rather than falling back to local execution when all hosts are full.

### Agent integration
Codex app-server is the only agent. The launch is `bash -lc "<codex.command>"` with cwd set to the workspace, speaking line-delimited JSON-RPC 2.0 (`codex/app_server.ex`).

1. `initialize` (clientInfo, `experimentalApi`) → `initialized`.
2. `thread/start` with `{approvalPolicy, sandbox, cwd, dynamicTools}`.
3. Per turn: `turn/start` with `{threadId, input:[{type:text}], cwd, title:"<ID>: <title>", approvalPolicy, sandboxPolicy}`. The client reads the stream until `turn/completed|failed|cancelled`.

Requests from the server to the client are handled in `maybe_handle_approval_request`:

| Request | Handling |
|---|---|
| Command/file-change approvals (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, legacy `execCommandApproval` and `applyPatchApproval`) | Auto-accepted "for session" only when `approval_policy == "never"`; otherwise the run becomes `approval_required` and is blocked |
| `item/tool/call` | Executed synchronously inside the worker process via the adapter |
| `item/tool/requestUserInput` | MCP-approval questions are auto-answered by picking "Approve this Session" |
| `mcpServer/elicitation/request` and heuristically detected `turn/*input*` methods | Treated as input-required, so the run is blocked |

- **Output parsing:** the client tolerates non-JSON lines and logs them. The Port is opened with `:stderr_to_stdout`, which contradicts SPEC §10.3/§17.5 ("keep protocol stream handling separate from diagnostic stderr").
- **Token accounting** (`elixir/docs/token_accounting.md`): prefer absolute `thread/tokenUsage/updated` totals, ignore `last_token_usage` deltas, and diff against the last reported value.
- **No agent-to-agent handoff.** Handoff is to humans via tracker states. The repo's `.codex/skills/` (commit, push, pull, land, linear, debug) give the agent procedures. `land/land_watch.py` is an async CI and review-comment watcher the agent runs before squash-merging.

### Verification & quality gates
The orchestrator enforces none. Gates live entirely in the `WORKFLOW.md` prompt:
- A status map: `Todo → In Progress → Human Review → (human) → Merging → Done`, plus `Rework`.
- A "Completion bar before Human Review": ticket-provided validation items executed, tests green, a "PR feedback sweep" in which every reviewer comment is addressed or explicitly pushed back on, CI checks green, and the `symphony` PR label.
- Reproduce-first.
- A `land` skill for merging after a human moves the ticket to `Merging`.

The human gate is a person changing tracker state. The README's "proof of work" (CI status, review feedback, walkthrough videos) is requested by the prompt, not checked by code. The orchestrator cannot tell an agent that followed the protocol from one that claimed it did.

### Observability & UI
- **Logs:** stable `key=value` logs that must carry `issue_id`, `issue_identifier` and `session_id` (SPEC §13.1, `elixir/docs/logging.md`). OTP writes a rotating disk log (`log_file.ex`).
- **Terminal dashboard:** `status_dashboard.ex` (1.9k LOC), with a throughput sparkline and TPS; tests use snapshot fixtures.
- **Web:** a Phoenix LiveView page at `/`, plus JSON at `/api/v1/state`, `/api/v1/<identifier>` and a coalesced `POST /api/v1/refresh` (`router.ex`).
- **Live updates** use notify-then-pull. Every Codex event calls `notify_dashboard` → `ObservabilityPubSub.broadcast_update` (a bare `:observability_updated` atom). Every connected LiveView then does a `GenServer.call(:snapshot)` on the orchestrator (`dashboard_live.ex`). This is simple, but it fans out one snapshot per viewer per agent event onto the single state owner.
- **Gaps against the spec:** `/api/v1/<id>` returns only the last event as `recent_events` and always `codex_session_logs: []` (`symphony_elixir_web/presenter.ex` `recent_events_payload`). That is much thinner than the SPEC §13.7.2 example.

### Config & extensibility
- **File format:** `WORKFLOW.md` is YAML front matter plus a strict Liquid prompt body (`Solid` with `strict_variables`/`strict_filters`; `prompt_builder.ex`). Template variables are `issue` and `attempt`.
- **Environment indirection:** `$VAR` resolves only where a value explicitly references it. Environment variables never globally override YAML (§6.1).
- **Dynamic reload is REQUIRED** (§6.2). `WorkflowStore` polls `{mtime,size,phash2(content)}` every 1 s and keeps the last-known-good config on parse errors. Every `Config.settings!()` is a `GenServer.call` that re-stats and re-reads the file (`workflow_store.ex` `reload_state`/`current_stamp`). The orchestrator calls it many times per tick.
- **Tool binding snapshot:** tools are bound per session (`Tracker.bind_agent_tools`), so a reload can't make one session advertise one provider's tools and execute another's (§10.5).
- **Hooks:** `after_create`, `before_run`, `after_run`, `before_remove`, with `timeout_ms` (60 s) and defined failure semantics (§9.4).
- **Adapters:** a `Tracker` behaviour with optional `agent_tool_specs`/`execute_agent_tool` callbacks (`tracker.ex`). Each adapter must publish a documented profile (§11.2).
- **Extensions:** new top-level keys such as `server`, `worker` and `observability`.

### Strengths — steal these
- **The spec as an artifact.** It has normative language, a typed error taxonomy, reference pseudo-code (§16), a conformance test matrix split into core/extension/real-integration profiles (§17), and a definition-of-done checklist (§18). We should write our own SPEC.md the same way, both so agents can implement adapters against it and so we can test conformance (`SPEC.md`).
- **One owner of scheduling state.** All mutation is serialized; workers only emit messages; `claimed`/`running` are checked before every launch (SPEC §7.4; `orchestrator.ex`).
- **A small tracker kernel.** Two reads, `fetch_issues_by_states` and `fetch_issues_by_ids`. The adapter computes `dispatchable`, and the spec deliberately has *no* generic CRUD. Absence has precise semantics: state-list calls may drop malformed records, but ID refreshes must *fail* instead (SPEC §11.1–11.3).
- **Reconcile before dispatch, every tick,** with the tracker as the operator control plane (SPEC §8.5, §14.4).
- **Continuation retry vs failure retry,** and multi-turn work on one thread with a short continuation prompt (`agent_runner.ex`).
- **Host-side tool execution** with secret-env stripping and a per-session tool binding (`tracker.ex` `bind_agent_tools`; `app_server.ex`).
- **Generation tokens on retry timers** (`orchestrator.ex` L1034–1090).
- **Collision-resistant workspace keys** and symlink-escape checks (`workspace.ex`, `app_server.ex`).
- **Token-accounting rules** based on absolute totals plus deltas (`elixir/docs/token_accounting.md`).
- **A single persistent "workpad" comment** as the agent's human-readable progress journal (`elixir/WORKFLOW.md`).
- **Last-known-good config reload** that never crashes the service (`workflow_store.ex`).

### Weaknesses & tradeoffs
- **No durability.** A restart drops retries, blocked entries and totals. Correct resumption depends on the agent re-reading the workpad and the reused workspace.
- **The workflow is a prompt.** Gates such as "tests green before Human Review" are unenforced. Adding a stage means editing a 300-line prompt, not code, and different adapters get different "workflow" prompts.
- **Codex-only.** The spec defers protocol details to the Codex app-server schema (§10). Supporting another agent is a spec change.
- **Tracker tools are over-privileged.** Raw GraphQL/REST with the full token scope, and no idempotency keys or scope guards (the README says so explicitly).
- **The orchestrator does blocking HTTP.** Tracker calls happen inside the GenServer, including the extra per-dispatch refresh. A slow tracker delays event ingestion and snapshots (15 s snapshot timeout, `Orchestrator.snapshot/0`).
- **Config reads hit disk.** Every settings lookup re-reads and hashes WORKFLOW.md.
- **Code diverges from the spec:**
  - stderr is merged into the protocol stream;
  - `recent_events` and `logs` are stubs;
  - `blocked` exists but is not in the §7.1 state machine;
  - a blocked issue waits indefinitely until a human changes tracker state or restarts the service, and there is no operator answer channel.
- **Weak isolation.** A directory plus the Codex OS sandbox. The shipped workflow runs with approvals off and network on. There is no container or VM option.

### Implications for a Rust framework
- **Orchestrator actor.** Build it as one tokio task that owns `State` and has an `mpsc` inbox of typed messages (`Tick`, `WorkerEvent`, `WorkerExited{reason}`, `RetryDue{token}`, `Refresh`). Workers run in a `JoinSet` with `AbortHandle`s, which replaces `Task.Supervisor` plus `monitor`. Use `tokio_util::time::DelayQueue` for retries, keyed by `(issue_id, generation)`.
- **Keep network I/O off the actor.** Tracker fetches run in spawned tasks and return messages, so snapshot and UI reads never wait on HTTP. Publish snapshots through `tokio::sync::watch` or `ArcSwap`.
- **Make states explicit enums.** Encode §7.1 claim states *and* §7.2 run phases, with a typed `TerminalReason`, since Elixir leaves phases implicit.
- **Tracker trait.** Mirror `tracker.ex`: `fetch_by_states`, `fetch_by_ids`, `tool_specs`, `execute_tool(ctx)`, `secret_env_names`. Add an optional *scoped* typed tool set rather than raw GraphQL.
- **Agent trait.** Generalize the Codex app-server client into an `AgentSession` trait (start, turn, events stream, respond to approval/input, stop). Keep stderr on a separate channel, and map each backend's approval requests onto one `ApprovalRequest` type.
- **Port the §17 conformance matrix** as integration tests against an in-memory tracker and a fake agent. Elixir already has `tracker/memory.ex`.
- **Config reload.** Use the `notify` crate plus periodic defensive revalidation. Cache parsed config in `ArcSwap<Config>` and bind a config snapshot per session.
- **Durability.** Add SQLite persistence for claims, retries and session metadata from day one; that is the spec's own TODO.

---

## gannonh/kata-symphony

### What it is
The monorepo's orchestrator is a **Rust** binary, `apps/symphony`: about 85.7k LOC in `src/` and about 27k LOC of tests. It uses tokio, axum (ws), ratatui, rusqlite (bundled), liquid, notify, reqwest and tracing. It is at v2.3.3 under MIT. The TypeScript parts are:
- the Pi Coding Agent extension/console (`apps/symphony/pi-extension`, about 3.3k LOC);
- the Kata CLI planning tool (`apps/cli`, about 9k LOC);
- shared packages (about 60k LOC).

The last commit is 2026-09-02. The project is very active and ADR-driven (`docs/adrs/0001–0006`). Note that the brief's "TS + some Rust" is inverted: the orchestrator is all Rust.

### Architecture
The orchestrator runs two planes:
- a **"legacy" plane**: a faithful but extended Symphony-spec worker loop, in memory;
- a **"factory" plane**: stages A1–A5 (triage, spec, implementation, review, verification), durable in SQLite.

```
main.rs ─ WorkflowStore(notify) ─ Orchestrator::run  (one tokio task, &mut self)          src/orchestrator.rs L2278
  every poll: triage_runtime.poll()  → A1..A5 coordinators ─► FactoryRunStore (SQLite, exclusive lock)
              tick_with_refresh()   → legacy poll/reconcile/dispatch  ─► OrchestratorPort (sync; block_in_place)
  select! { sleep_until | worker_event_rx | worker_result_rx | escalation_rx | steer_rx | refresh.notified }
      │ spawn worker task: workspace(worktree|clone|docker) → hooks → prompt(by_state) → Pi RPC | Codex app-server
      ├─► EventHub (tokio::broadcast, AtomicU64 sequence) ─► axum WS /api/v1/events ─► pi-extension console
      ├─► SnapshotHandle (Arc<RwLock<Snapshot>>) ─► ratatui TUI, /api/v1/state, 2 s-polling HTML dashboard
      └─► Supervisor task (heuristics over EventHub) ─► SharedContextStore / steer / EscalationRegistry
```

`OrchestratorPort` (`orchestrator.rs` L2008) is a *synchronous* trait. The Linear and GitHub implementations wrap async clients with `tokio::task::block_in_place(|| Handle::current().block_on(..))` (`main.rs` L202, L275). The single orchestrator task therefore stops draining worker events while it waits on tracker HTTP; the events buffer in unbounded mpsc channels.

### Core abstractions / domain model
- **Legacy plane** (`src/domain.rs`):
  - `Issue`, extended with `children_count`/`parent_identifier`;
  - `RunAttempt`, with `status: String` rather than an enum;
  - `WorkerSessionInfo`, `RetryEntry`, `OrchestratorState`;
  - an `AgentEvent` enum: `SessionStarted…TurnInputRequired, ApprovalRequired, ToolCall*, Escalation{Created,Responded,TimedOut,Cancelled}, Notification, Malformed`;
  - `SymphonyEventEnvelope {version, sequence, timestamp, kind, severity, issue, event, payload}`, where `kind` is one of Snapshot, Runtime, Worker, Tool, Heartbeat, Escalation*, SharedContext*, Supervisor* or Triage.
- **Factory plane** (`src/triage/migrations/001_init.sql` through `010_verification_stage.sql`):
  - `factory_runs`, unique on `(forge_host, repository, issue_id)`;
  - `stage_runs`: `stage, issue_revision, configuration_revision, attempt, owner_instance, pid, process_group_id, process_start_token, lease_heartbeat_at, status, harness, model, tokens`. A **partial unique index** allows only one pending or running attempt per `(run, issue_revision, configuration_revision)`;
  - immutable per-stage artifacts: triage artifacts, *versioned* spec artifacts, implementation manifests plus bundle metadata, and review findings keyed by `(run, reviewed_head_sha, base_sha)`;
  - verification command runs, evidence and gates;
  - per-stage `*_publication_intents` with `completed_steps_json`, `retry_count`, `expected_projection_json` and lease columns;
  - an append-only `factory_events` table.

### Orchestration model
**Legacy plane.** It follows the spec for polling, per-state and per-host slots, continuation and failure retries, retry tokens, and stall detection with a per-session override (`detect_stalled_workers` L3858). It deviates from the spec in these ways:
- **The orchestrator writes to the tracker.** It moves `Todo → In Progress` at dispatch (`spawn_workers_for_dispatched`, about L2440) and posts completion comments (`CompletionCommentBuilder` L311).
- **Per-state prompts** (`prompts.by_state`). Because of them, an active-state change *ends* the session so the issue is re-dispatched with the correct prompt (CHANGELOG 2.3.0).
- **`Agent Review` needs a real PR.** Dispatch is gated on an open PR existing, and the orchestrator self-heals to `In Progress` otherwise (CHANGELOG 2.2.1, `AgentReviewPrStatus`).

**Factory plane.** Each stage coordinator, `src/{triage,spec,implementation,review,verification}/coordinator.rs`, works the same way:
1. It **claims** a stage run in SQLite (`store.rs` `claim_attempt` L684). The lease heartbeat is renewed while the turn runs; the stale threshold is 60 s and the turn timeout about 900 s.
2. It launches a fresh, isolated harness invocation.
3. It strictly validates the agent's JSON output, rejecting unknown fields and wrong identities.
4. It stores an **immutable artifact**.
5. It creates a **publication intent**, which a trusted publisher later applies idempotently.

Publisher behaviour (ADR-0004, ADR-0005):
- It writes marked comments such as `<!-- symphony:implementation:{intent_id} -->` and recovers when the effect was created but not recorded.
- It never force-pushes.
- Every pending-row write is a lease-owner CAS.
- Retries: 30 s doubling to 30 min. After 8 *failures* the intent becomes `blocked`, and an operator runs `symphony publication reset`. "Waiting on a precondition" does not count against the budget.

Other factory rules:
- Spec review is a bounded adversarial loop (`max_review_cycles`); unresolved findings become open decisions (ADR-0003).
- A3 claims work before the legacy candidate fetch, and durable dispatch guards stop the legacy worker from racing approved work (ADR-0004 §6).

### State & persistence
- **Legacy plane:** in memory, as in the spec.
- **Factory plane:**
  - SQLite stored under the platform data dir, namespaced `symphony/triage/<forge>/<owner>/<repo>/`. The store holds an `fs2` **exclusive lock for the process lifetime** (`triage/store.rs` L515), so only one instance can run. There are 10 additive migrations. Operator CLI commands go through HTTP first and fall back to the store only when the daemon is down (ADR-0004).
  - Code-sized data lives outside SQLite as **content-addressed git bundles**: `{storage}.artifacts/sha256/<aa>/<digest>`, written temp→rename and re-hashed before any push (ADR-0004 §2).
  - **Crash recovery of child processes.** The `(pid, pgid, OS start token)` triple is persisted right after spawn. A replacement daemon re-checks that identity, sends SIGTERM, waits 5 s, sends SIGKILL, and confirms no group member remains (ADR-0002, `triage/process_identity.rs`).
  - A5 goes further with a **launch barrier**: the payload waits on a pipe until its identity is durably recorded. Docker containers are created with labels and their IDs persisted *before* `docker start` (ADR-0006 §4).
  - The ADRs document the remaining race windows honestly: the spawn-to-persist gap and TOCTOU on group signals.

### Isolation & workspaces
- **Built-in git bootstrap** via `workspace.git_strategy` (`clone-local | clone-remote | worktree | auto`) and branches named `<branch_prefix>/<identifier>`. Clean worktrees are fast-forwarded from `clone_branch`, and dirty or diverged ones are reported (`workspace.rs` `bootstrap_repository` L412).
- **Hooks run with cwd = the WORKFLOW.md directory**, not the workspace (`workspace.rs` L953; README). That deviates from spec §9.4.
- **Docker isolation** (`workspace.isolation: docker`) creates a per-issue container with `docker run --rm -d` and runs the agent through `docker exec -i … sh -lc` (`docker.rs`). The run arguments include no network, capability or read-only restrictions.
- **Factory-stage workers are credential-free.** `env_clear()` plus an allowlist (PATH, TERM, LANG/LC_*, TMP*, ANTHROPIC/OPENAI/CLAUDE API keys). They get an isolated `HOME`, which is seeded with Pi auth files only for the turn and scrubbed afterwards (fail closed). Push URLs are disabled and `process_group(0)` is set (`triage/runner.rs` `build_isolated_env` L1320, L632–637).
- **Only the publisher holds forge credentials.** It gets the token through a subprocess-scoped `GIT_CONFIG_*` HTTP header, never through argv (ADR-0004 §7, ADR-0006 §3).
- **Credential scrubbing.** At startup Symphony snapshots `GH_TOKEN`, `GITHUB_TOKEN`, `KATA_GITHUB_TOKEN` and `LINEAR_API_KEY`, then removes them from its own environment once config is loaded. This stops children from reading `/proc/<ppid>/environ` (`credential_env.rs`; `main.rs` L835).
- **Legacy workers are the exception.** They explicitly *receive* GitHub tokens (`codex/app_server.rs` L282, `pi_agent/rpc_bridge.rs` L696) and write to the tracker via `symphony helper`, which is the opposite of the spec's host-side tool model.
- **Likely latent bug:** the Docker legacy path reads `LINEAR_API_KEY`, `GH_TOKEN` and `GITHUB_TOKEN` with `std::env::var` *after* the scrub (`orchestrator.rs` about L902), so containers probably start without credentials.

### Agent integration
There are two backends, selected by `agent.name`.
- **Pi RPC** (`pi --mode rpc`, JSONL). The commands are `prompt`, `abort`, `get_state`, `get_session_stats` and `follow_up`. Events include `agent_start`, `turn_start/end`, `message_update` and extension-UI requests (`pi_agent/protocol.rs`). Cumulative session stats are turned into deltas.
- **Codex app-server JSON-RPC**, spec-compatible, with stderr drained separately (`codex/app_server.rs` L328).

The per-state model is set with `agent.model_by_state`. Other integration points:
- **Agent → tracker:** the agent runs `"$SYMPHONY_BIN" helper <op> --workflow $SYMPHONY_WORKFLOW_PATH --input file.json`. The operations are `issue.get`, `issue.list-children`, `comment.upsert`, `issue.update-state`, `issue.create-followup`, `document.read/write`, and GitHub-only `pr.inspect-feedback/checks/land-status`. Each returns an `{ok,data}|{ok:false,error}` envelope (`helper.rs`).
- **Steering:** `POST /api/v1/steer` sends a Pi `follow_up` into a live session (`run_turn_with_followups` L772).
- **Escalation:** Pi UI requests (confirm, select, input) become an `EscalationRequest` in the orchestrator's registry. It is broadcast on the WS stream and answered through `POST /api/v1/escalations/:id/respond`. A timeout (default 5 min) falls back to reject or cancel (`rpc_bridge.rs` `handle_escalation_ui_request` L363).
- **Factory stages use a file contract.** The agent must write JSON to `$…_OUTPUT` and read only allowlisted inputs. The strict manifest parser is the contract (`triage/runner.rs` about L336).
- **Worker-to-worker communication:** an in-memory `SharedContextStore` (TTL 24 h, at most 100 entries, 500 characters each) exposed on `/api/v1/context` and injected into prompts (`shared_context.rs`).

### Verification & quality gates
This is the most rigorous of the three projects.
- **A2 spec.** Draft, review and revise each run in a *fresh* clone, home and output directory. The reviewer sees only the issue and the current spec: no conversation and no prior findings. That keeps the adversarial review from being weakened by hidden context. The human gate is the `spec-approved` or `spec-revise` label, and approval **pins** an exact artifact version for implementation (ADR-0003).
- **A3 implementation.** Implement and repair turns, validation cycles, a bundle, then a draft PR.
- **A4 review.** A read-only reviewer runs on the exact PR `(head, base)`. Findings are published as a formal GitHub review, fenced by leases (ADR-0005). A change to either SHA opens a new review cycle (migration 009).
- **A5 verification.**
  1. The configured commands run against the exact reviewed head in a credential-free bundle clone.
  2. HEAD, the tree and tracked files are re-verified afterwards, with `core.filemode=true` forced.
  3. Evidence is bounded, digest-addressed and metadata-only over HTTP.
  4. A read-only verifier emits a criterion matrix.
  5. `compute_gate` passes **only if** every expected command ran and passed, *and* every approved acceptance criterion appears exactly once as `pass` with valid evidence references. The verifier cannot waive a failure; a failed gate holds and is not auto-retried (`verification/gate.rs` L68; ADR-0006).
- **Defaults.** Most stages default to **preview** mode, which only posts an owned comment. Automatic mode is exercised in isolated UAT.
- **Legacy plane:** gates are prompt-level, as in Symphony (`prompts/agent-review.md`, `merging.md`, `rework.md`).

### Observability & UI
- **Logs:** tracing JSON (`SYMPHONY_LOG`), optionally to rolling files.
- **TUI:** ratatui, redrawn from `SnapshotHandle` on an interval (`tui.rs` L344).
- **HTML dashboard:** polls `/api/v1/state` every 2 s (`http_server.rs` L2224).
- **WebSocket stream `/api/v1/events`** (`http_server.rs` `handle_events_socket` L2531). It subscribes to the broadcast channel *first* and then sends a snapshot envelope. It supports server-side filters (issue, type, severity), sends 5 s heartbeats, and gives each client a bounded writer queue. `Lagged(n)` is counted and the socket closes with reason `backpressure` past a threshold.
- **Pi extension console** (`pi-extension/src/event-stream.ts`): reconnects with exponential backoff. There is **no replay from `sequence`**; a reconnect relies on the fresh snapshot.
- **Other surfaces:**
  - factory-run APIs (`/api/v1/factory-runs`, metrics, artifacts, verification evidence metadata);
  - Slack webhooks with event filters (`notifications.rs`);
  - `symphony doctor` preflight checks (`doctor.rs`, 2.7k LOC).
- **Security gap:** the mutating routes (steer, refresh, context delete, publication reset) are unauthenticated. ADR-0004 acknowledges this ("authenticated control is PRD slice B2"). The default bind is 127.0.0.1, but `server.host: 0.0.0.0` is documented.

### Config & extensibility
- `symphony init` writes `.symphony/{WORKFLOW.md, prompts/*.md, .env.example, docs/WORKFLOW-REFERENCE.md}`.
- The front matter extends the spec with:
  - `prompts{system, repo, by_state, default}`, concatenated with `---`;
  - `agent{name, command, model, model_by_state}`;
  - `workspace{git_strategy, isolation, docker{…}}`;
  - `notifications`, `supervisor`, `shared_context`, `storage`;
  - one section per factory stage (`triage`, `spec`, `implementation`, `review`, `verification`), each with `mode: preview|automatic`, prompts, models and limits.

  The shipped example is `apps/symphony/WORKFLOW.md`, and the config layer is `config.rs` (3.1k LOC).
- Templates are Liquid and include `issue.children_count` for Kata-planned slices.

### Strengths — steal these
- **A durable stage model.** `factory_run → stage_runs(lease, attempt, issue_revision, configuration_revision) → immutable artifact → publication intent (completed steps, retry budget)`, with a partial unique index enforcing one live attempt (`triage/migrations/001_init.sql`).
- **Agent output is separate from side effects.** Agents produce validated artifacts; a trusted publisher applies forge and tracker effects idempotently, using markers, create-before-record recovery and CAS-fenced leases (ADR-0004/0005; `implementation/publisher.rs`, `review/publisher.rs`).
- **A gate the verifier cannot override,** with complete criterion coverage and evidence references (`verification/gate.rs`).
- **Exact-head pinning:** review and verification identities are `(head_sha, base_sha)`, and drift supersedes the attempt without publishing (ADR-0006; migration 009).
- **Credential-free workers:** `env_clear` plus an allowlist, an isolated HOME, push disabled, and credentials only in the publisher via `GIT_CONFIG_*` (`triage/runner.rs`, `credential_env.rs`).
- **Orphan-process recovery** by `(pid, pgid, start-token)`, and launch barriers (`triage/process_identity.rs`; ADR-0002/0006).
- **Adversarial review with fresh context** (ADR-0003 §3).
- **A versioned event envelope** with a global sequence, kind and severity, plus a WS with snapshot bootstrap, heartbeat and an explicit backpressure close policy (`event_stream.rs`, `http_server.rs`).
- **Human-in-the-loop answers** with a timeout and default deny (`rpc_bridge.rs`), and **mid-run steering** (`/api/v1/steer`).
- **Retry budgets that separate failure from waiting,** and an auditable operator reset (ADR-0004).
- **A `doctor` preflight command**, and ADRs that document the residual risks honestly.

### Weaknesses & tradeoffs
- **Size and two planes.** `orchestrator.rs` is 6.9k lines and `triage/store.rs` 10.5k. Durable and in-memory planes coexist, with guards between them, and a new stage adds tables, migrations, coordinators and publishers (10 migrations for five stages).
- **Docs drift.**
  - The README module map omits the whole factory plane.
  - The README cites test files that don't exist (`tests/path_safety_tests.rs`, `tests/live_e2e_tests.rs`).
  - The README says it is a symlink to AGENTS.md; neither file is a symlink.
  - `prompts/supervisor.md` describes an LLM supervisor agent, but `supervisor.rs` is regex and threshold heuristics. `SupervisorConfig.model` is "for future model-backed supervisor decisions".
- **Synchronous tracker I/O** (`block_in_place`) inside the single orchestrator task, plus **unbounded** `mpsc` channels.
- **A probable memory leak.** `emit_runtime_event` pushes onto `events: Vec<RuntimeEvent>` with no cap (`orchestrator.rs` L3963), including per-tick `Reconcile` and `Validate` events.
- **Unauthenticated mutating HTTP.**
- **Legacy workers hold GitHub tokens.** Tracker writes come from the agent via the helper CLI, weakening the isolation story the factory plane works hard for.
- **Single instance only** (exclusive file lock), and GitHub-centric: Linear parity exists only in the legacy plane. Most factory stages are still preview.

### Implications for a Rust framework
- **The crate stack is validated at scale:** tokio, axum+ws, ratatui, rusqlite (bundled), liquid, notify, reqwest, tracing, thiserror, clap, fs2 and sha2.
- **Make ports `async`** (`async_trait` or native AFIT) and run them in spawned tasks that report back via messages; never `block_in_place` in the actor.
- **Bound everything.** Use a ring buffer for runtime history and bounded channels with explicit coalescing (keep the latest token and progress counters; drop duplicates), and treat `Lagged` as a signal to resync from a snapshot.
- **Use SQLite in WAL mode from day one,** but prefer a **generic schema**: `runs`, `stage_attempts`, `artifacts(kind, schema_version, digest, json)`, and an `effects` (outbox) table with lease, CAS, steps and budget. Kata's per-stage tables show how quickly bespoke schemas multiply.
- **Build a process supervisor module:**
  - spawn with `CommandExt::process_group(0)`;
  - persist identity before releasing the payload;
  - on Linux, prefer `pidfd` (`pidfd_open`/`pidfd_send_signal`) to close the TOCTOU window the ADR admits.
- **Event protocol:** a `sequence` plus `?since=` replay from a bounded server-side buffer. Kata's clients can't resume.
- **Keep the "helper CLI" idea,** since it is agent-agnostic and works for any harness with a shell. But route it through a host-side broker, for example a Unix socket or MCP server owned by the daemon, so the agent never holds tracker or forge tokens.

---

## kunalm2345/openfactory

### What it is
TypeScript on Node ≥ 24, run as TS directly via native type stripping. It is about 7.4k LOC in `src/` plus about 1k LOC of `node:test` tests. Dependencies are `@asciidev/box-sdk`, `hono`, `@sentry/node` and `node:sqlite`. It is v0.1.0, MIT, from a single author; the last commit is 2026-08-14. It is tightly coupled to the ascii.dev **Box** cloud-VM platform, which provides fork, stop, resume, snapshots, a prompt API, commands, files and events.

### Architecture
One Node process runs on a "factory box" VM. It contains three independent timers plus an HTTP server (`src/index.ts`, `docs/design.md` §2):

```
GitHub/Linear/Sentry ─webhooks(HMAC)─► server.ts (hono) ─► ingest.ts ─► runs row (TRIAGING)
                                           │ dashboard (token cookie), approvals, cancel, merge
┌── FACTORY BOX ──────────────────────────────────────────────────────────────────┐
│ engine.ts      3 s  tick: for each due run → HANDLERS[state](run) → next_poll_at │
│ template.ts    3 s  verify/refresh jobs for user's template box                  │
│ reconciler.ts 60 s  mirror box states, TTL guard, orphan sweep, cost accrual     │
│ triage agent: Box prompt API against THIS box + local repo checkout              │
└──────┬───────────────────────────────────────────────────────────────────────────┘
       │ Box API (the only control plane; boxes never call the orchestrator)
TEMPLATE BOX ─fork─► BUILD BOX (plan, build, fix) ─stop→snapshot; git bundle pulled to orchestrator
      └──────fork─► VERIFY BOX (bundle applied; cross-family reviewer) ─resume→ git push + gh pr create
```

### Core abstractions / domain model
SQLite in WAL mode (`src/db.ts` L205+) holds these tables:
- `runs(issue_key, source, title, body, branch, state, fix_count, ctx JSON, plan_md, plan_meta, verdict, pr_url, error, next_poll_at)`;
- `boxes(run_id, role build|verify|pr, box_id, last_seen_state, snapshot_*, inspect_until, alive_ms)`;
- an append-only `events` table;
- `approvals(gate plan|pr, status, actor)`, `template_jobs`, a `config` key/value store (settings plus prompt templates), and `webhook_deliveries` (dedupe).

The `ctx` JSON is each handler's idempotency scratch space: box IDs, prompt IDs, step markers and retry counters.

The run state machine is:

```
TRIAGING → QUEUED → PROVISIONING → PLANNING → (AWAITING_PLAN_APPROVAL) → BUILDING → SNAPSHOTTING
         → VERIFYING → (FIXING → SNAPSHOTTING …) → CREATING_PR → PR_OPEN → (MERGING) → MERGED
terminal: MERGED | FAILED | REJECTED | CANCELLED | SKIPPED
```

### Orchestration model
- **Sourcing** is webhook-driven, with no polling of trackers: GitHub `issues.opened`, Linear issue-created, Sentry alerts, or a manual prompt (`server.ts` L713+, `ingest.ts`). Deliveries are deduped, and there is at most one open run per `issue_key` (`ingestIssue`). Every run starts in `TRIAGING`. There, an agent on the factory box decides whether the issue deserves a run, using tools, a local checkout and a digest of recent runs. It **fails open** on every error or timeout (`engine.ts` `handleTriaging`, `triage.ts`).
- **Scheduling:** `tick()` (`engine.ts` L1242) selects every non-terminal run with `next_poll_at <= now` (`db.ts` `dueRuns` L434) and runs its state handler **sequentially**. The handler returns the next poll delay (3 s, 10 s or 30 s). Concurrency comes from the remote VMs, not the orchestrator, because handlers only start or poll asynchronous Box operations.
- **Admission control** (`handleQueued` L413) checks, in order:
  1. a template and a repo are configured;
  2. `countActiveRuns() < max_concurrent_runs` (TRIAGING, QUEUED and PR_OPEN don't count);
  3. no template refresh is pending;
  4. `limits().canStart` passes, with a client-side token bucket on box creations per minute.

  Then it forks the template.
- **Transitions are CAS.** `UPDATE runs SET state=? … WHERE id=? AND state=<expected>` (`db.ts` `transition` L486). A lost CAS means someone else, usually a human cancel, moved the run. Every transition re-stamps `ctx.phaseStartedAt` so per-phase **watchdogs** measure the current phase:

  | Phase | Watchdog |
  |---|---|
  | Planning | 30 min, with one retry |
  | Building | 3 h, then FAILED |
  | Verifying | 45 min, with one fresh-fork retry |
  | Fixing | 1 h |
  | Creating PR | 10 min, with 2 fresh boxes |

  (`design.md` §4.1)
- **Fix loop** (`handleVerifying` L744 → `FIXING` → `SNAPSHOTTING` …): findings are filtered by `verify_block_on` (`any | major | none`) and bounded by `max_fix_iterations`.
- **Errors:** transient Box and gateway errors become `box.retrying` events rather than failures. The watchdogs are the real deadlines (`tick` catch block; design §6.11).
- **Reconciler** (`reconciler.ts`, 60 s): observe-only. It mirrors box state, extends TTLs within 15 minutes of expiry for boxes the phase needs (`NEEDS_MACHINE`), nudges runs whose box auto-archived, and stops orphan boxes named with this instance's prefix `of-<instanceId>-…`.

### State & persistence
SQLite is the single source of truth, and the process can be killed at any time (`index.ts` shutdown just clears timers).
- **Recovery** relies on handlers being idempotent from `ctx`: if a prompt ID is present, keep polling; if it is missing, start the prompt.
- **Known crash windows.** `startPrompt` followed by a non-CAS `saveCtx` (`db.ts` L535) can double-start a prompt if the process dies between the two. `forkFrom` followed by `addBox` can leak a box, but the orphan sweep reaps it by name prefix.
- **Artifacts** are kept outside the DB:
  - git bundles in `data/` on the orchestrator (`bundlePath`);
  - diffs pulled for the run page;
  - stopped build and verify boxes kept as the **audit trail**, since their snapshots and transcripts outlive the machines (design §1).
- The `.oneignore` keeps the live DB out of the factory box's own snapshots.

### Isolation & workspaces
- **Strongest physical isolation of the group.** Each run's build box is a seconds-fast **fork of a user-prepared template VM snapshot**. The template is a box the user set up over SSH with their stack, logged-in dev tools and repo checkout.
- **Verification uses a fresh template fork**, not a fork of the build box. The design notes that forks of prompt-running boxes "inherit a broken agent daemon". The branch arrives as a **git bundle**, verified and fetched (`handleVerifying`). The reviewer can scribble freely; `CREATING_PR` does `git reset --hard <pinned branchHead>` before pushing (`captureBundle` pins `ctx.branchHead`; `engine.ts` L987, L1052).
- **Cost and safety through box lifecycle:**
  - **TTL as a dead-man's switch:** worker boxes auto-stop if the orchestrator dies.
  - **Stopped boxes cost nothing,** so human gates are free to wait on.
  - `looksLikeBrokenMachine()` detects lazily restored forks with missing shared libraries and re-forks (`engine.ts` L136; design §6.10).
- **Secrets are weak.** `GH_TOKEN` lives in the Box *account* environment, so **every box, builder included, inherits it** (README "Notes & caveats"). "Never push" in `prompts/build.md` is a prompt instruction, not an enforcement. The triage agent reads untrusted issue text on the factory box itself; the README says so plainly.
- **No network policy** beyond what Box provides.

### Agent integration
Claude Code or Codex runs through the **Box prompt API**: `api.prompt({boxId, provider, prompt, model, reasoningEffort})` (`boxapi.ts` `startPrompt` L217).
- The provider is derived from the model (`config.ts` `providerForModel`), and effort is clamped per model (`effortForModel`).
- Each stage has its own model. The **reviewer defaults to a different family** (`review_model: "gpt-5.5"`, `config.ts` L156).
- **Completion** comes only from `promptRunStatus`. Output comes from a **file contract**: every prompt ends with "write `/home/user/<X>.json`", namely `PLAN.md` + `PLAN_META.json`, `BUILD_RESULT.json`, `REVIEW.json`, `FIX_RESULT.json`, `MERGE_RESULT.json` and `of-triage/run-<id>.json`. A missing or unparseable file means failure regardless of prompt status (design §4.4).
- **Parsing is lenient:** `parseAgentJson` strips code fences and falls back to regex-extracting `{…}` (`triage.ts`).
- **Handoff between agents is through files and git:** plan → build prompt; `REVIEW.json` is written into the build box for the fix round, where the builder may list "disputed" findings (`prompts/fix.md`).
- **Deterministic steps are never prompts:** git sync, bundle create and apply, push, `gh pr create`, and merge through the API (design §1).

### Verification & quality gates
- **Adversarial review** in a separate fresh VM, by a different model family. The reviewer is told to diff, *re-run the tests itself*, and "try to break the change" (`prompts/review.md`). It returns a `pass|fail` verdict with severity-tagged findings.
- **Proof checks:** a "success" with zero commits fails immediately (`emptyBranchReason` L117). `BUILD_RESULT.json` must report `ok`.
- **Human gates:**
  - **Plan approval** (`plan_gate: auto|manual`). In manual mode the box is **stopped** while waiting, and approvals never time out.
  - **The GitHub PR itself**, merged by a human from GitHub or from the dashboard ("Merge" or "Fix & merge").
- **Docs vs code:**
  - The README advertises plan gate modes "auto / manual / manual-for-large", but `Hitl.plan_gate` is only `"auto" | "manual"`. `plan_meta.size` is logged, not used (`config.ts` L65; `handlePlanning`).
  - The README diagram shows a human gate before the PR, but `AWAITING_PR_APPROVAL` is a legacy state that "new runs never enter" (`handleAwaitingPrApproval`).
- **No deterministic test gate.** The orchestrator trusts the reviewer's verdict; `tests_passed` is recorded, not enforced.

### Observability & UI
- **Server-rendered HTML** (hono) with `data-live` regions that poll HTML partials (`/partials/runs/:id`) through `setInterval` + `fetch` (`ui/appjs.ts` L132–160). There is no SSE or WebSocket.
- **Run page** (`ui/run.ts`): a phase waterfall that separates PR-review wait from execution time, the event timeline, the diff, the plan and the verdict.
- **Agent transcripts** are proxied from the Box events API with cursor pagination (`/runs/:id/boxes/:boxId/events`, `server.ts` L281). They work for archived boxes too.
- **"Wake box for an hour"** to inspect an exact tree.
- **Costs** are estimated from per-box alive time multiplied by the account credit rate.
- **Errors:** Sentry capture, with transient errors deliberately excluded.
- **Event catalog** of about 60 kinds (design §5).
- **Auth:** dashboard routes are gated by `DASHBOARD_TOKEN` (compared with `===`, and stored raw in a cookie; `server.ts` L98, L110). Webhooks verify their HMAC with `timingSafeEqual` (L710+).

### Config & extensibility
- Environment variables carry **secrets only**. Everything behavioural lives in SQLite `config`, edited from the dashboard Settings page: repo, per-stage model and effort, plan gate, strictness, fix iterations, concurrency, and all six prompt templates, seeded from `prompts/*.md` (`prompts.ts` `renderPrompt` with `{{VAR}}` substitution).
- **Self-update from `main`**, fast-forward only (`selfupdate.ts`).
- **Extensibility is low.** There is no plugin model, and the Box SDK is used directly throughout `engine.ts`.

### Strengths — steal these
- **One CAS state machine,** plus per-handler idempotency through `ctx` and a `next_poll_at` per run: no queue and no broker (`db.ts` `transition`, `dueRuns`; `engine.ts` `tick`).
- **Separate owners:** "the reconciler observes, the engine owns transitions". Orphan sweep uses deterministic resource names with an instance prefix (`reconciler.ts`).
- **Verify in a pristine environment from the template.** The artifact moves as a git bundle; the PR is pushed from the pinned reviewed SHA (`engine.ts` `captureBundle`, `handleVerifying`, `handleCreatingPr`).
- **Cross-family reviewer** by default (`config.ts`).
- **Pause-for-human costs zero:** stop the sandbox while waiting on a gate (`handlePlanning` manual branch).
- **TTL dead-man's switch** plus a reconciler TTL guard, so runaway spend is bounded (`reconciler.ts`, design §1).
- **File-based output contract** that makes providers swappable, and "deterministic steps are commands, not prompts" (design §4.4, §1).
- **Per-phase watchdogs** with a phase clock reset on every transition (`db.ts` L495–498).
- **"Retries are not failures"** and "no recovery agents in plumbing" (design §6.9, §6.11).
- **Audit trail** as stopped snapshots and replayable transcripts, and **lessons-learned design notes** tied to concrete run numbers (design §6).

### Weaknesses & tradeoffs
- **Platform lock-in.** Box fork, snapshot, prompt and events are the whole execution layer. Without Box there is nothing.
- **Weak secret model:** every box holds `GH_TOKEN`; builder "no push" is prompt-only; the triage agent runs untrusted text on the orchestrator's own box.
- **Sequential engine tick:** a slow Box call (for example a large bundle upload) delays every run. The state-per-run polling is O(runs) API calls every 3–10 s.
- **Non-CAS `saveCtx` side writes** and crash windows between an external effect and its record.
- **Review verdict trusted.** No evidence requirement and no deterministic gate. Poll-based UI, no streaming.
- **Single repo** (`settings.repo_dir`), and GitHub-only for PRs. Docs have drifted on HITL gate options.

### Implications for a Rust framework
- **Write run state transitions as CAS by default:** `UPDATE … WHERE id=? AND state=? AND version=?`. Put *all* side-data writes under the same version check, or into an outbox.
- **Model sandboxes behind a `Sandbox` trait** with `fork/stop/resume/exec/put_file/get_file/snapshot/ttl`. Box, Docker, Firecracker, git-worktree and SSH hosts all fit. Make "pause while waiting for a human" a first-class capability (`suspend()`).
- **Name external resources deterministically** (`<instance>-<run>-<role>`) and run an observe-only reconciler that reaps orphans and extends leases or TTLs. Keep it separate from the transition-owning engine.
- **Move artifacts between sandboxes as git bundles** plus a pinned SHA, and push only from a pinned, reviewed commit. This composes well with Kata's exact-head gating.
- **Run the per-run handler loop as concurrent tasks** with a global semaphore, not a sequential `for` loop. Keep `next_poll_at` for backoff, and let event-driven wakeups short-circuit polling.

---

## Cross-repo takeaways

### Top ideas to adopt
1. **A single-authority orchestrator with a typed state machine, plus reconcile-before-dispatch.** Take Symphony's claim model (Unclaimed, Claimed, Running, RetryQueued, Released), make transitions CAS-persisted as OpenFactory does, and reconcile against the external source every tick. The tracker, or our own UI, stays the operator's control plane: moving a ticket stops a run.
2. **Keep "agent produces an artifact" separate from "trusted component applies effects".** Agents write strictly validated JSON or files (OpenFactory's `*_RESULT.json`, Kata's strict manifests). A credentialed publisher applies PR, comment and state effects idempotently from an outbox, using markers and leases (Kata ADR-0004/0005). Workers get no forge or tracker tokens (Kata `build_isolated_env`); tool calls execute host-side (Symphony §10.5).
3. **Pin verification to the exact commit and make the gate deterministic.** Move work between sandboxes as git bundles with pinned SHAs (OpenFactory). Review and verify in a *fresh* environment with fresh context, by a different model family (OpenFactory, Kata ADR-0003). The gate is computed by code from command results and criterion coverage, and the verifier cannot override it (Kata `verification/gate.rs`).
4. **Retry semantics that separate continuation, failure, waiting and blocked.** Continuation retry of 1 s vs exponential failure backoff (Symphony §8.4). Generation tokens against stale timers. Retry budgets that don't charge for waiting, plus an operator reset (Kata ADR-0004). Per-phase watchdogs that reset on each transition (OpenFactory).
5. **A real-time event protocol plus snapshots.** A versioned envelope (`sequence, kind, severity, issue, payload`). WebSocket or SSE that subscribes first, then sends a snapshot, then deltas, with heartbeats and a backpressure policy (Kata). Add `since=<seq>` replay from a bounded buffer, which Kata lacks, and keep the UI strictly read-model (Symphony §13.4). Include human-in-the-loop requests (escalations with timeout and default deny) and steering as first-class events (Kata).

Worth adopting too:
- A written SPEC.md with a conformance matrix (Symphony).
- A `doctor` preflight (Kata).
- Last-known-good hot reload of repo-owned `WORKFLOW.md` (Symphony).
- Zero-cost suspension at human gates (OpenFactory).

### Top pitfalls to avoid
1. **Workflow correctness living only in a prompt.** Symphony's gates ("tests green before Human Review") are unenforceable, and OpenFactory's "Do NOT push" is advisory while the token sits in the box. Enforce with credentials and code.
2. **Blocking I/O or unbounded buffers in the orchestrator actor.** Symphony's GenServer does tracker HTTP inline, and Kata uses `block_in_place` with unbounded mpsc and an ever-growing `events` Vec. In Rust: async ports, bounded channels, ring buffers and coalescing.
3. **Durability bolted on late, as a second plane.** Kata's in-memory legacy loop plus a SQLite factory needs cross-plane dispatch guards, and Symphony has none at all. Start with one durable model (runs, attempts, artifacts, effects) that covers every stage.
4. **Over-privileged agent tools and credential leakage.** Raw GraphQL/REST with the full token (Symphony), agent-held tokens via the helper CLI (Kata legacy), account-wide `GH_TOKEN` in every VM (OpenFactory). Scope tools per issue and keep secrets host-side.
5. **Docs and code drift in agent-built codebases.** Examples: Symphony's stderr handling and API stubs vs the spec; Kata's README module map, nonexistent tests, and a "supervisor agent" that is really heuristics; OpenFactory's gate options. Make the spec executable (conformance tests) and generate reference docs from config schemas.
