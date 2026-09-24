# 04 — Execution & sandbox layer

Repos studied (code read at these commits; paths below are relative to each repo root):

| Repo | Commit | Date |
|---|---|---|
| `jrswab/axe` | `937b509` | 2026-09-16 |
| `google/ax` | `ace0360` | 2026-09-23 |
| `agent-substrate/substrate` | `31a5e0b` | 2026-09-24 |
| `dagger/container-use` | `2e43e62` | 2026-08-12 |
| `e2b-dev/E2B` | `ccaf9fc` | 2026-09-18 |

## Group overview

These five projects answer the question that sits under any software factory: *where does an agent's code run, what can it touch, and how does its work get out?* They cover a spectrum. **axe** is a single-agent executor that sandboxes at the tool level, inside its own process. **container-use** gives each agent a Dagger container plus its own git branch, and exposes both over MCP. **E2B** is a hosted Firecracker-microVM sandbox with the most complete lifecycle SDK of the group. **Substrate** is a Kubernetes actor runtime built around snapshot-based suspend/resume, resume-on-request routing and an egress policy enforcement point. **AX** is a declarative, kubectl-style control plane (Task/Workspace/Gateway/Model) that runs on top of Substrate.

The same lessons come up across all five:

1. Isolation has to be enforced *below* the agent. Both tool-level heuristics (axe's `run_command` guard) and MCP-only discipline (container-use) can be bypassed.
2. The strongest secret model resolves credentials *outside* the sandbox and injects them at an egress proxy. Substrate does this with credential providers, E2B with network `rules` and `${e2b.secrets.*}` placeholders.
3. Snapshot scope has to be explicit, because memory+fs and fs-only behave very differently on resume (Substrate `FULL`/`DATA`, E2B `keepMemory`).
4. Fork is the most useful primitive for speculative, best-of-N agent work. E2B has `fork({count})`, Substrate has Tags plus `source_tag`, and container-use has immutable container IDs plus git branches.
5. Git is the natural way code leaves a sandbox for review.

Finally, docs run ahead of code in every repo. The sections below list specific places where they disagree.

---

## 1. axe (jrswab/axe)

### What it is
A Go CLI (about 12.6k non-test LOC, 122 Go files, 4 direct deps) for running single-purpose LLM agents defined in TOML with SKILL.md context. It releases actively via goreleaser (`.goreleaser.yml`, `CHANGELOG.md`) and is pre-1.0. Last commit 2026-09-16 (prompt-caching tokens).

### Architecture
- `main.go` → `cmd/` (cobra) → **`pkg/runner.Run`** (`pkg/runner/run.go`, 1,022 lines). This is the only public package; `internal/` holds everything else. Spec `docs/plans/048_pkg_runner_spec.md` records why: "Option B", one curated public API.
- `Run` is a linear pipeline:
  - load the agent TOML (`internal/agent/agent.go`)
  - resolve workdir, globs and skill (`internal/resolve/resolve.go`)
  - load memory (`internal/memory/memory.go`)
  - build the provider, wrapped in a retry decorator (`internal/provider/retry.go`)
  - build the tool registry (`internal/tool/registry.go`)
  - connect to MCP servers (`internal/mcpclient/router.go`)
  - run the conversation loop, execute tools, then handle artifacts, budget and memory append.
- Sub-agents run through a **second, duplicated** conversation loop, `runConversationLoop` in `internal/tool/tool.go`.

### Core abstractions / domain model
- `AgentConfig` (TOML): model `provider/name`, skill, files, workdir, tools, `sub_agents`, `[budget]`, `[retry]`, `allowed_hosts`, `[artifacts]`, `[[mcp_servers]]`.
- `provider.Provider` has `Send`; an optional `StreamProvider` adds `SendStream`.
- `ToolEntry{Definition, Execute(ctx, call, ExecContext)}`. `ExecContext` carries `Workdir`, `AllowedHosts`, `ArtifactDir` and `ArtifactTracker`.
- `call_agent` delegates to a sub-agent opaquely: only the final text crosses back (`docs/design/sub-agent-pattern.md`).
- `budget.BudgetTracker` is a mutex-guarded cumulative token counter (`internal/budget/budget.go`).

### Orchestration / lifecycle model
"Executor, not scheduler" (`AGENTS.md`): one invocation runs to completion, with no daemon and no resume.
- The loop is capped by `max_turns` (default 50, `pkg/runner/run.go`). It stops on the first text-only response.
- Tool calls in a turn run in parallel by default.
- Sub-agent depth defaults to 3 with a hard max of 5. At max depth, `call_agent` is not offered. Each sub-agent has a 120 s timeout.
- `Options.Messages` (plan 051) lets an embedding caller seed history. That is the only continuation mechanism.

### State & persistence
- Per-agent memory is an append-only markdown log under XDG data, with LLM-assisted GC (`cmd/gc.go`).
- Artifacts go to an explicit dir or an auto-generated `XDG_CACHE/artifacts/<ts-rand>`. The auto-generated dir is deleted after the run unless `KeepArtifacts` is set.
- No run state is persisted. A crash loses everything.
- `pkg/runner/run.go:150` calls `os.Setenv("AXE_ARTIFACT_DIR", …)`. That is process-global state inside a library, so two concurrent `runner.Run` calls in one process clobber each other. This contradicts the "No global state" rule in `AGENTS.md`.

### Isolation, network & secrets
- **File tools:** `validatePath` (`internal/tool/path_validation.go`) rejects absolute paths and `..`, and resolves symlinks for *both* the target and the workdir before an `isWithinDir` boundary check. This is solid.
- **Shell:** `run_command` runs `sh -c` with a **heuristic** regex scanner (`internal/tool/command_validation.go`) for absolute paths, `..` and `~`. The code comment and `docs/plans/035_run_command_sandbox_spec.md` say plainly that it can be bypassed (variable expansion, `$(…)`, base64). The companion `sandboxEnv()` is the more valuable part: it sets `HOME` and `TMPDIR` to the workdir and strips every env var except PATH/LANG/LC_ALL/USER/LOGNAME/TERM, so provider API keys never reach child processes.
- **Egress:** `allowed_hosts` plus an always-on private-IP block, with DNS-pinned dialing (the resolved IP is re-checked in `DialContext`) and re-validation on every redirect (`internal/hostcheck/hostcheck.go`, `internal/tool/url_fetch.go`). This applies only to `url_fetch` and `web_search`. `run_command curl …` bypasses it entirely.
- **Docker:** the hardening in `docker-compose.yml` is `read_only`, `cap_drop: ALL` and `no-new-privileges`. The README admits the network is not restricted.

### Agent integration
axe *is* the agent loop. It speaks directly to Anthropic, OpenAI, Ollama, Bedrock (hand-rolled SigV4, `internal/provider/sigv4.go`), Gemini, OpenRouter, OpenCode and others. It does not drive Claude Code or Codex.

The MCP client supports only `sse` and `streamable-http` (`internal/mcpclient/mcpclient.go`; validated at `internal/agent/agent.go:198`). The docs disagree with the code here: `AGENTS.md` lists `stdio/http/https`, and the README's second `[[mcp_servers]]` example uses `transport = "stdio"`, which would fail validation.

### Verification, budgets & safety rails
- **Token budget:** cumulative input+output tokens, shared across sub-agents because the same `BudgetTracker` is passed down through `ExecuteOptions`. It is checked *before* each LLM call and *before* executing tools, so one turn can overshoot. Exceeding it gives exit code 4 and skips the memory append.
- **Other rails:** turn cap, request timeout, retry only on 429/5xx/timeouts, refusal detection (`internal/refusal/refusal.go`), a 100 KB cap on command output, and 1 KB truncation of stored tool-call details.
- **Exit codes** are meaningful: 0, 1 runtime, 2 config, 3 provider, 4 budget.

### Observability & UI
- `--json` produces an envelope with per-call `tool_call_details` (turn, duration, error), tokens (including cache read/write), cost, `retry_attempts` and the message history (`pkg/runner/result.go`).
- `--verbose` writes turn-by-turn logs to stderr. `--dry-run` has golden tests (`cmd/testdata/golden/`).
- There is **no event callback** in `runner.Options`, only `io.Writer`s and a final `Result`. An embedding orchestrator cannot stream per-turn events to a UI without scraping stderr.

### Config & extensibility
- Precedence is flags > TOML > env > defaults, everywhere.
- Agent lookup order: `--agents-dir` > `<cwd>/axe/agents` > XDG.
- **Development process worth copying:** `docs/plans/` holds 121 files in `NNN_*_spec.md` / `NNN_*_implement.md` pairs. A *spec* has "Context & Constraints", "Decisions already made", "Approaches ruled out", "Open questions resolved" and a file/line-count table of the code involved. An *implement* doc is a red/green TDD checkbox list that names each test function and file, e.g. `docs/plans/035_run_command_sandbox_implement.md`. This format suits a coding agent well: bounded, verifiable, and it records rejected designs.

### Strengths — steal these
- Symlink-aware path confinement for file tools (`internal/tool/path_validation.go`).
- Env scrubbing for spawned commands, with `HOME`/`TMPDIR` set inside the workspace (`sandboxEnv` in `internal/tool/command_validation.go`).
- SSRF-safe fetch: DNS pinning, per-redirect checks and a private-range deny list (`internal/hostcheck/hostcheck.go`, `internal/tool/url_fetch.go`).
- One token budget shared across a whole sub-agent tree, with a distinct exit code (`internal/budget/budget.go`, `pkg/runner/errors.go`).
- Opaque sub-agents with depth limits as "safety rails, not features" (`AGENTS.md`, `internal/tool/tool.go`).
- Categorized provider errors plus a retry decorator (`internal/provider/provider.go`, `internal/provider/retry.go`).
- The spec→implement plan pair as the unit of agent work (`docs/plans/*_spec.md` / `*_implement.md`).

### Weaknesses & tradeoffs
- The shell "sandbox" is advisory. Real confinement is delegated to Docker, which still has open network.
- `allowed_hosts` does not cover the shell.
- There are two conversation loops (`pkg/runner/run.go` and `internal/tool/tool.go`) that must be kept in sync.
- A sub-agent resolves its **own** workdir from its TOML (`resolve.Workdir("", cfg.Workdir)` in `internal/tool/tool.go`), so a child can operate outside the parent's workspace.
- `exec.CommandContext` kills only `sh`, not its process group, so grandchildren survive a timeout.
- No streaming event API and no persisted state.

### Implications for a Rust framework
- Tool-level checks are defense in depth, not the boundary. Enforce isolation in the workspace layer (OS sandbox, container or VM). Still ship axe's cheap wins: env scrubbing, path confinement and SSRF checks.
- Model budgets as a shared `Arc<Budget>` that *reserves* before a call rather than checking after. Propagate it to child agents and surface it in a distinct error variant.
- The runner API must expose an event stream (`Stream<AgentEvent>`), not only a final result.
- Spawn with a new process group (`setsid`/`process_group(0)`) and kill the group on timeout.
- Adopt the spec/implement plan format for our own "plan → implement" task templates.

---

## 2. AX (google/ax)

### What it is
A Go control plane (about 7k hand-written LOC plus 4.4k generated protobuf) for running agent tasks declaratively on Agent Substrate. It is `v1alpha1`, and the README warns of major breaking changes. Last commit 2026-09-23.

### Architecture
The request path is `ax` CLI → `ax-server` → Redis → `ax-controller` → Substrate Control API (`DESIGN.md`):

- **`ax` CLI** (`cmd/ax/main.go`, kubectl-shaped) talks gRPC to `ax-server`. It auto-tunnels based on the kube context (`internal/tunnel/`).
- **`ax-server`** (`internal/server/server.go`) is stateless. It validates manifests and persists them to **Redis**: a JSON value per object, a ZSET index, a **Stream** for work and **PubSub** for watch (`internal/store/redis/store.go`).
- **`ax-controller`** workers join a Redis consumer group (`internal/controller/worker.go`) and call `TaskReconciler.Reconcile` (`internal/controller/reconciler.go`). That drives the **Substrate Control API** (`internal/substrate/client.go`).
- Inside each actor, **`ax-task-runner`** runs as PID 1 (`runner/runner.go`, `cmd/ax-task-runner/main.go`).
- DESIGN.md's rationale: putting millions of short-lived tasks in etcd as CRDs would overload it, hence Redis.

### Core abstractions / domain model
All in `pkg/apis/v1alpha1/ax.proto`, scoped by an **atespace**.

| Kind | What it holds |
|---|---|
| `Task` | image, command, env, resources, `workspaces[]` (each with `name`, `path`, `goal`), gateway ref, `suspend`, `debug` |
| `Workspace` | `git[]` (repo, branch, depth, dir), `mcp` (servers and registries), `skills` (registries and path). Reusable across tasks and composable, with several mounted per task |
| `Gateway` | listeners and an egress allowlist of host+port |
| `Model` | provider, model, `secretKey` ref, parameters |

Status is a `phase` plus K8s-style conditions: `WorkspaceReady` (sticky), `GatewayReady` and `Ready` (`docs/concepts.md`).

### Orchestration / lifecycle model
`SaveTask` writes the object and `XADD`s a `reconcile` event. A worker then runs:

1. `EnsureAtespace`
2. Build a per-task `ActorTemplate`, named by a sha256 of image+env (`taskTemplateName`, `reconciler.go`)
3. `EnsureActor`
4. `ApplyEgressPolicy`
5. If `spec.suspend` → `SuspendActor`, else `ResumeActor`
6. Poll the runner's `/readyz` for up to 15 s

Suspend and resume are **desired state**: `SuspendTask` just flips `spec.suspend` and re-saves (`server.go`). Delete is two-phase: `MarkTaskDeleting` sets `Terminating` and enqueues `delete`; the controller deletes the actor and all `…-tmpl-*` templates, with retries, then removes the record.

The **runner contract** (`docs/runner.md`) is one of the best parts of the repo:
- fixed entrypoint `/usr/local/bin/ax-task-runner`
- specs passed as `AX_TASK_YAML` and `AX_WORKSPACES_YAML`
- `/healthz`, plus `/readyz` returning 503 until every workspace is prepared
- prepare each workspace **once**, recorded by a marker
- run the command in its own **process group**
- **stay up after the command exits**, so the sandbox stays inspectable
- on SIGTERM, signal the group, wait a 10 s grace period, then SIGKILL
- serve guest exec/fs gRPC only when `spec.debug`

`runner.Run` is embeddable and has an `OnCommandExit` hook (`runner/runner.go`).

### State & persistence
- Redis objects, with an optional TTL.
- `/workspace` is a Substrate `DurableDir` that survives suspend. Snapshot scope is hard-coded to `DATA` (files only, fresh process tree; `BuildActorTemplate` in `internal/substrate/client.go`). The README line "pick up exactly where it left off" therefore overstates it for AX tasks.
- The "prepare once" marker is written to `/ax/initialized-*` (`internal/workspace/setup.go`, `AXDir="/ax"`). That is the container root fs, *outside* the durable volume, and `DATA` snapshots exclude root-fs changes. By my reading the marker does not survive suspend/resume. Setup would then re-run `git fetch` + `checkout -f FETCH_HEAD` over the restored repo (`fetchRepo`), which is exactly the clobbering that `docs/runner.md` warns against.

### Isolation, network & secrets
- The sandbox class is hard-coded to gVisor (`client.go:273`). The default snapshot bucket is a test GCS bucket (`client.go:209`).
- Egress uses Substrate `EgressPolicy`, with several problems:
  - `ApplyEgressPolicy` **drops the port** from Gateway host rules.
  - `*` maps to `All` (every port).
  - With no Gateway, the default is `*:443` (`reconciler.go:193`), which becomes all egress.
  - A failure to apply policy is logged and **ignored** (`reconciler.go:201`: "could not apply egress policy (continuing)"). This fails *open*.
- **Secrets:** `GEMINI_API_KEY` is read from a K8s secret and baked into the ActorTemplate env as plaintext (`reconciler.go:152`), and the template name hashes it.
- `spec.resources` is never passed to `BuildActorTemplate`, so the documented CPU/memory limits are not enforced.
- Guest services (`internal/guest/client.go`) use insecure gRPC creds and are debug-gated.

### Agent integration
The task itself is agent-agnostic: any image and command. Goal-driven workspace bootstrap hands the `goal` to a Gemini/Antigravity agent confined to the workspace (`cmd/ax-task-runner/antigravity_bootstrap.py`) with a 10 m timeout.

`Model` resources and `workspace.Planner` (`internal/workspace/planner.go`) are **not wired into any production path**. MCP config is not materialized (`setup.go` only `mkdir`s the skills path) even though `docs/runner.md` says the runner "writes any MCP configuration". `docs/manifests.md` references an `examples/multi-workspace.yaml` that does not exist.

### Verification, budgets & safety rails
Budget and approval policies were **removed**: field 9 `policies` is reserved in `ax.proto`. `PendingApproval` and `UsageStats` in `TaskStatus` are never populated. The only rails are the debug gate, the egress policy, the bootstrap timeout and the SIGTERM grace period.

### Observability & UI
- `WatchTask` is server-streaming over Redis PubSub, and it ends when phase hits `Running`, before `WorkspaceReady`.
- `ax describe` shows conditions.
- `ax ssh` goes through guest services and the atenet router header `ate-target-actor`.
- `docs/runner.md` states that the command's exit status is **not reported back** to the control plane.

### Config & extensibility
- `store.Store` interface with memory and Redis impls.
- Three runner customization levels: extend the image, embed `runner.Run`, or rewrite against the contract.
- Multi-document YAML apply.

### Strengths — steal these
- The split into reusable **Workspace** (repos, MCP, skills, setup `goal`) + **Gateway** (network) + **Task** (binding), with multiple workspaces per task (`pkg/apis/v1alpha1/ax.proto`, `docs/concepts.md`).
- The in-sandbox **runner contract**: readyz gate, prepare-once, process group, stay-up-after-exit, SIGTERM grace, debug-gated exec (`docs/runner.md`, `runner/runner.go`).
- Suspend as a desired-state field reconciled by the controller (`internal/server/server.go`, `reconciler.go`).
- Sticky `WorkspaceReady` versus flapping `Ready` conditions (`reconciler.go` `setNotReady`).
- Spec-hash-named templates plus pattern-based cleanup on delete (`taskTemplateName`, `deleteTaskTemplates`).
- Two-phase delete with a visible `Terminating` phase (`store.go` `MarkTaskDeleting`).

### Weaknesses & tradeoffs
- The queue claims "at-least-once" (`internal/store/store.go` comments), but:
  - events are **acked even when reconcile fails** (`worker.go:101`);
  - nothing `XAUTOCLAIM`s pending entries from dead consumers, and consumer names are random per boot;
  - there is no periodic resync, so a task left "Initializing" after the 15 s poll is never re-reconciled.
- `UpdateTaskStatus` is a read-modify-write with no CAS or resourceVersion (`store/redis/store.go` ~L270), so updates can be lost.
- Policy fails open, secrets are in plaintext template env, resources are unenforced, and the suspend marker bug above.
- Heavy dependency chain: K8s, Substrate and GCS.

### Implications for a Rust framework
- Model a `WorkspaceTemplate`: repos, toolchain setup, MCP, skills and an optional natural-language setup goal. Bind it per attempt, and compose several templates per workspace (for example code plus shared tools).
- Ship a small in-sandbox daemon, `forgeline-agentd`, implementing AX's runner contract plus E2B-style exec/fs RPC. Every non-local backend then gets identical semantics.
- Reconciliation must be level-triggered: periodic resync, retry with backoff (never ack-and-drop), CAS versions on status, and policy application that **fails closed**. The agent must not start until `PolicyEnforced=True`.
- Store the setup marker *inside* whatever the snapshot scope preserves, or in the orchestrator's DB.

---

## 3. Agent Substrate (agent-substrate/substrate)

### What it is
A Go actor runtime for Kubernetes: about 92k non-test, non-generated LOC plus vendor. It maps many snapshot-able "actors" onto fewer warm worker pods. The README says "early development… not ready for production", and `docs/architecture.md` says "much of this architecture is aspirational". Last commit 2026-09-24. It started at Google and is heading for CNCF.

### Architecture
- **`ate-api-server`** (`cmd/ateapi/`) is a gRPC `Control` service (`pkg/proto/ateapipb/ateapi.proto`) backed by PostgreSQL (`cmd/ateapi/internal/store/atepg/`). It has a scheduler (`cmd/ateapi/internal/scheduling/`) and a workflow engine (`controlapi/workflow_{resume,suspend,pause,revert,delete,tag}.go`).
- **`atelet`** runs per node as a DaemonSet. Its RPCs are `Run`, `Checkpoint`, `Restore`, `UploadPausedCheckpoint` and `Terminate` (`internal/proto/ateletpb/atelet.proto`). It streams snapshots to and from GCS/S3.
- **`ateom`** is the herder inside each worker pod. Its RPCs are `RunWorkload`, `CheckpointWorkload` and `RestoreWorkload` (`internal/proto/ateompb/ateom.proto`). There is one per sandbox class: `cmd/ateom-gvisor` (runsc checkpoint/restore) and `cmd/ateom-microvm` (Kata + Cloud Hypervisor, with a memory snapshot and userfaultfd demand paging).
- **Networking:**
  - `atenet-router` is Envoy + ext_proc. It reads `ate-target-actor`, **resumes the actor on demand**, and tunnels over mTLS to `atunnel` (`cmd/atenet`).
  - `atenet-egress` is the egress policy enforcement point (PEP).
  - Credential providers are a gRPC plugin (`pkg/proto/credproviderpb/credprovider.proto`, `cmd/credential-provider/kubernetes-secrets`).
- **K8s owns infrastructure** (`WorkerPool` CRD, `SandboxConfig`). **Postgres owns high-churn records** (Actor, Worker, Tag, ActorTemplate).

### Core abstractions / domain model
- **`ActorTemplate`:** immutable image/env/volumes plus `SnapshotsConfig` (`onPause`/`onCommit` scope `FULL|DATA`, `onResume` source) and `SandboxConfig` (class). It produces a **golden snapshot**.
- **`Actor`:** an instance with `ActorState` ∈ {RESUMING, RUNNING, SUSPENDING, SUSPENDED, PAUSING, PAUSED, CRASHED, DELETING, REVERTING} (`ateapi.proto` L528). It has an optional immutable `source_tag` and a `worker_selector`.
- **`Tag`:** an immutable, atespace-scoped alias that owns **its own copy** of a suspended actor's snapshot. `CreateActor(source_tag=…)` forks from it.
- **`EgressPolicy`:** at most one per actor. Rules are ordered and first-match-wins, matching `hostnames` (exact or leftmost-label `*`), `cidrs` or `all`. The only rule *effect* is `inject_static_headers` from credential-provider URIs.
- **`Worker` / `WorkerPool`:** the warm capacity.

### Orchestration / lifecycle model
From `docs/architecture.md`:
- `CreateActor` starts in SUSPENDED, pointing at the golden snapshot.
- `ResumeActor` claims a warm worker, restores the golden snapshot (first run) or the actor's own, and ends in RUNNING.
- `SuspendActor` checkpoints to object storage, wipes the worker and releases the previous snapshot. An actor owns one snapshot at a time.
- `PauseActor` keeps the checkpoint **node-local**, so resume is pinned to that node.
- `RevertActor` goes from RUNNING/PAUSED/CRASHED back to the last completed snapshot.
- `DeleteActor(any_state)` deletes regardless of state.

Every operation takes a **per-actor distributed lease**. The lease context is cancelled if the lease is lost (`controlapi/workflow.go:216` `acquireActorLease`).

The router **parks** requests while the pool is saturated: it retries `ResourceExhausted` with backoff up to a 5 s budget and never cancels an in-flight restore (`docs/request-parking.md`). Worker changes go through a Postgres transactional **outbox** with trim high-water marks, so lagging watchers detect loss and resync (`store/atepg/outbox.go`). The store has a 3.3k-line **contract test suite** that every impl must pass (`store/storecontract/contract.go`).

### State & persistence
- `FULL` scope captures memory + root-fs changes + durable data. `DATA` scope captures durable data only (`ateapi.proto` L202–207).
- Snapshots are pinned to the template and runtime version so restores are reproducible.
- The gVisor class is limited to one `DurableDir`; the microVM class lifts that (`architecture.md`).

### Isolation, network & secrets
- The sandbox is gVisor or a microVM.
- **Egress** (`docs/network-egress.md`, `docs/egress-traffic.md`):
  - nftables redirects all actor TCP (except port 53) into `atunnel`.
  - `atunnel` opens an mTLS CONNECT to the PEP with a **per-actor SPIFFE certificate**.
  - The PEP re-verifies with the API that the actor exists, the UID matches and it is running.
  - The PEP must not trust actor-supplied SNI/Host as proof of destination.
  - UDP and other protocols are dropped. **WebSockets and SSH are blocked**, which matters for `git@` remotes and some agent CLIs. DNS on 53 bypasses the PEP, which is an exfiltration channel.
- **Secrets:** the credential provider resolves `ate-secret://…` URIs at the PEP, keyed by the actor's SPIFFE ID, so secrets **never enter the sandbox**. Hostname rules can MITM (`docs/egress-trust-bundle.md`).
- The API authenticates callers but has **no authorization yet** (`architecture.md`). `docs/threat-model.md` is candid, e.g. T-11 on secrets handling.

### Agent integration
Framework-agnostic OCI. The **Claude Code multiplex demo** (`demos/claude-code-multiplex/`) is weaker than its README suggests:

- `workload/run.sh` just loops `claude --print "$TASK"` every `INTERVAL_SECONDS` with a fixed env-var task.
- `ANTHROPIC_API_KEY` is substituted as a **plain env value** into the ActorTemplate (`agent-*-template.yaml.tmpl`) with `FULL` snapshots, so the key is also persisted in memory snapshots in the bucket. It does not use the credential-injection path.
- The dashboard's "Give a task" **never sends the task to an agent**. `giveTask` picks a random agent and task, and `computeState` (`ui/server.go:255`) advances queued → running → completed on client-side timers with random durations. The comment itself says "the substrate side has no concept of these per-task states".
- The README says "Substrate notices the inactivity and suspends the agent after a short idle window". No idle auto-suspend exists in the code: suspension is an explicit `SuspendActor`, and idle GC is on `docs/roadmap.md` (and AX's roadmap).
- The demo shows actor/worker juggling; it does not show agent work orchestration.

### Verification, budgets & safety rails
There are no agent-level budgets. The rails are infrastructural: the park budget, lease fencing, `RevertActor` for crashed actors, and delete-state guards.

### Observability & UI
- OTel metrics and traces (`docs/observability.md`), resume-latency metrics (`controlapi/metrics.go`), `kubectl-ate`.
- The demo UI reads actor/worker state through the API and pod logs through client-go.

### Config & extensibility
- `SandboxConfig` is cluster-scoped and pins the runtime binaries into each snapshot manifest.
- The egress PEP is swappable: Envoy or agentgateway (`demos/egress`).
- Credential providers are a plugin API.
- There is **no exec/fs API in the core Control service**. Exec comes from separate in-guest "guest services" (`agent-substrate/env`, used by AX in `internal/guest/client.go`).

### Strengths — steal these
- An explicit transitional state machine including CRASHED and REVERTING, plus a Revert-to-last-good op (`ateapi.proto` `ActorState`, `workflow_revert.go`).
- Snapshot **scope** as a first-class enum (FULL vs DATA), plus golden snapshots per template (`ateapi.proto` `SnapshotContentScope`, `SnapshotsConfig`).
- Tags as immutable, retention-owning fork points; fork = `CreateActor(source_tag)` (`ateapi.proto` `CreateTag`, `Actor.source_tag`).
- Egress PEP with first-match rules and **credential header injection** by a pluggable provider; the sandbox never holds the secret (`EgressPolicy`, `credprovider.proto`, `docs/network-egress.md`).
- Per-resource leases whose context is cancelled on lease loss (`controlapi/workflow.go`).
- A store **contract test suite** shared by all implementations (`store/storecontract/contract.go`). Do the same for workspace backends.
- Request parking and resume-on-traffic (`docs/request-parking.md`).

### Weaknesses & tradeoffs
- Operationally heavy: K8s, Postgres, object storage, a runsc build that has `--allow-connected-on-save` (`docs/architecture.md`) and Kata.
- No authorization, a DNS bypass, and WebSocket/SSH blocked.
- No exec in core.
- The Claude Code demo overclaims.
- APIs "almost guaranteed to change".

### Implications for a Rust framework
- Don't build this layer. Target it as a backend later.
- Adopt its vocabulary:
  - states: `Suspending/Suspended/Resuming/Crashed/Reverting`
  - `SnapshotScope::{Full, FsOnly}`
  - `Tag`-like named snapshots for fork points
  - an egress rule model with first-match semantics and `inject` effects
  - secrets as provider URIs resolved outside the sandbox
- Require a per-workspace lease (single writer) in the orchestrator.

---

## 4. container-use (dagger/container-use)

### What it is
A Go MCP server and CLI (`container-use`/`cu`, about 7.9k non-test LOC) that gives each coding agent a Dagger container and a git branch. Marked "experimental". Last commit 2026-08-12; the pace is slowing.

### Architecture
- `mcpserver/tools.go` exposes the MCP tools: `environment_create`, `_open`, `_config`, `_run_cmd`, `_file_{read,list,edit,write,delete}`, `_add_service`, `_checkpoint`, `_log`, `_diff`, `_list`.
- `environment/` handles the Dagger container state, services and fs ops.
- `repository/` handles git: a bare "fork" repo, worktrees, notes and `flock` locks (`repository/flock.go`).
- `cmd/container-use/` is the human side: `log`, `diff`, `checkout`, `merge`, `apply`, `terminal`, `watch`, `prune`, `config`, `agent configure`.

### Core abstractions / domain model
- **`Environment`** (`environment/environment.go`): a petname ID, plus `State{Container (Dagger container ID), Config, Title, timestamps}`, `Services` and `Notes`.
- **`EnvironmentConfig`** (`environment/config.go`, stored in `.container-use/environment.json`): `BaseImage` (default `ubuntu:24.04`), `Workdir`, `SetupCommands` (run before the source is copied in, for caching), `InstallCommands` (after), `Env`, `Secrets` (references) and `Services` (sidecar containers with ports).
- **`Repository`**: the user repo, a bare fork at `~/.config/container-use/repos/<path>`, and worktrees at `~/.config/container-use/worktrees/<id>`.

### Orchestration / lifecycle model
**Create** (`repository/repository.go` `Create`, `repository/git.go` `initializeWorktree`):
1. Resolve the git ref.
2. Push it to the `container-use` remote as branch `<id>`.
3. `git worktree add` in the fork, then `fetch` it into the user repo.
4. Make an empty initial commit.
5. Load the tree into Dagger (`Host().Directory(fork).AsGit().Ref(head)`).
6. `buildBase`: image → env/secrets → setup commands → services → copy source → install commands.
7. Export to the worktree, commit, and save state in git notes.

**Each `environment_run_cmd`** runs in a **new container derived from the previous state** (`WithExec`). The new immutable container ID becomes the state. The whole workdir is then exported to the worktree (`Export(... Wipe: true)`), and all changes are committed **with the agent's `explanation` as the commit message**, even when the command fails.

**Background commands** become Dagger services with tunneled host ports.

**Other operations:**
- `environment_checkpoint` publishes the container as an OCI image.
- `cu merge` / `cu apply` bring work into the user's branch.
- Forking = `environment_create` with `from_git_ref` pointing at another env's branch. The ref is validated by `validateGitRefComponent`. Only files fork; the container state is rebuilt from config.

### State & persistence
Git is the database:
- a branch per environment
- `refs/notes/container-use-state` holds the JSON state (container ID, config)
- `refs/notes/container-use` holds the command log (command, exit code, stdout, stderr)

Concurrency is serialized by file locks per lock type (`LockTypeForkRepo`, `LockTypeUserRepo`, `LockTypeNotes`).

### Isolation, network & secrets
- Isolation comes from BuildKit containers. There is **no network policy**: egress is fully open.
- Every exec passes `ExperimentalPrivilegedNesting: true` (`environment/environment.go:273`, `:375`), which hands the container access to the Dagger API.
- There is **no exec timeout**. The tool description tells the model to set `background` or "the tool [will be] stuck".
- **Secrets** are *references* (`op://`, `env://`, `vault://`, `file://`) resolved by Dagger into `WithSecretVariable`. The agent-facing `environment_config` tool cannot set secrets; it only takes `base_image`, `setup_commands` and `envs`. Agent config changes stay local until a human runs `cu config import` (`mcpserver/tools.go`).
- `docs/secrets.mdx` claims "the AI model never sees actual secret values" because Dagger scrubs raw values from output. An agent can defeat that by encoding the value (`echo $K | base64`), so the claim is too strong.

### Agent integration
- MCP stdio works with any MCP client.
- `cu agent configure` writes the MCP config for Claude Code, Codex, Copilot, Cursor, Goose and Q. It **allowlists** `mcp__container-use__*` in `.claude/settings.local.json` (`cmd/container-use/agent/configure_claude.go`) but **does not deny** the agent's native Bash/Edit tools. Isolation therefore depends on the model obeying `rules/agent.md` ("ALWAYS use ONLY Environments… NO EXCEPTIONS").
- Single-tenant mode makes `environment_id` implicit per chat session (`mcpserver/singletenant.go`).

### Verification, budgets & safety rails
None beyond the per-command log with exit codes. There is no budget, no timeout and no egress control. The service start timeout is 30 s (`environment/service.go`).

### Observability & UI
- `cu log` (git log plus notes), `cu diff` and `cu terminal`, which drops a human into the live container.
- `cu watch` is literally `git log --remotes=container-use --graph` re-run every second (`cmd/container-use/watch_unix.go`).
- Humans review with plain `git checkout <env>`.

### Config & extensibility
- Per-repo `environment.json`, agent-proposable, human-promoted.
- Services as sidecars.
- Anything Dagger can express.

### Strengths — steal these
- **Branch per workspace, commit per step.** Each tool call becomes a commit whose message is the agent's stated intent. That gives free time-travel, bisect and review with standard git (`repository/git.go` `propagateToGit`, `commitWorktreeChanges`).
- A **bare "fork" repo plus a remote added to the user repo**, so agent branches never touch the user's working tree until merge (`repository/repository.go` `ensureFork`, `ensureUserRemote`).
- Immutable, content-addressed environment states. Every step is a snapshot, which makes rollback and fork trivial (`environment/environment.go` `apply`).
- Setup vs install command split for cache reuse (`environment/environment.go` `buildBase`).
- Secret *references* resolved by the runtime. The agent cannot add secrets, and config changes need human promotion (`environment/config.go`, `mcpserver/tools.go` `environment_config`).
- `hooks disabled` (`core.hooksPath=/dev/null`) on every factory git operation (`repository/git.go:30`).
- Validation of env IDs/refs and export paths against traversal (`repository/git.go` `validateGitRefComponent`, `validateExportFilePath`).

### Weaknesses & tradeoffs
- **Exec model:** each command runs in a *new* container, so processes, `cd` and exported shell state do not persist. `UpdateConfig` rebuilds and discards installed packages.
- **Cost:** full-workdir export plus `git status` plus commit on *every* tool call is O(repo size).
- **Silent data loss:** `shouldSkipFile` (under the comment "AI slop below!", `repository/git.go:587`) never commits anything under `build/`, `dist/`, `target/`, `env/`, `.env/` or `venv/`, or files like `*.png`, `*.svg`, `*.pdf` or `*.log`. Legitimate source edits in such paths silently never reach the branch.
- Opt-in isolation, open network, privileged nesting and no timeouts.
- Tight coupling to the Dagger engine.

### Implications for a Rust framework
- Adopt the **code-sync contract**: every backend, local or remote, must materialize work as commits on a factory-owned branch, via a bare mirror plus remote, with one commit per agent step. Use explicit include/exclude rules, and *report* excluded files rather than dropping them silently.
- Keep workspace state in our own DB (SQLite/Postgres). Git notes make a poor database under concurrency.
- Run the agent CLI **inside** the workspace sandbox rather than relying on MCP tool discipline. Offer an MCP facade only for agents that must stay on the host.
- Separate agent-proposed config from human-approved config.

---

## 5. E2B (e2b-dev/E2B)

### What it is
The SDK monorepo for E2B cloud sandboxes (Firecracker microVMs):
- JS/TS SDK: about 24k LOC in `packages/js-sdk/src`, including generated code
- Python SDK: about 47k LOC, sync and async
- CLI, code-interpreter and desktop variants

The infrastructure lives elsewhere (`e2b-dev/runtime`/`belt`). The OpenAPI and envd protos are copied in via Copybara (`spec/README.md`, `CLAUDE.md`). It is a mature commercial product. Last commit 2026-09-18 (release).

### Architecture
The SDK talks to two planes:
1. **Control-plane REST** (`spec/openapi.yml`):
   - `/sandboxes`, `/v2/sandboxes`
   - `/sandboxes/{id}/{pause,resume,connect,fork,timeout,network,refreshes,snapshots,logs,metrics}`
   - `/snapshots`, `/templates` (builds, tags, aliases), `/volumes`, `/secrets`
   - `/events/sandboxes` and `/events/webhooks`
2. **`envd` in-VM daemon** over Connect-RPC (`spec/envd/process/process.proto`, `spec/envd/filesystem/filesystem.proto`), authenticated with a per-sandbox `envdAccessToken`.

Higher-level features are built *on top of exec*:
- The `git` module shells out through `Commands` (`packages/js-sdk/src/sandbox/git/index.ts`).
- The MCP gateway is started as a process during `create()` (`packages/js-sdk/src/sandbox/index.ts`).

### Core abstractions / domain model
- **`Template`:** a builder (`packages/js-sdk/src/template/index.ts`) with `fromImage`, `fromDockerfile`, `runCmd`, `aptInstall`, `gitClone`, `setStartCmd` and `setReadyCmd`. The VM is snapshotted *with the start command already running and ready*, the same idea as Substrate's golden snapshot.
- **`Sandbox`:** id, `state: 'running'|'paused'`, metadata, `endAt` lease, and the `files`, `commands`, `pty` and `git` modules.
- **`Snapshot`:** persistent and named. It survives sandbox deletion, and `Sandbox.create(snapshotId)` starts a sandbox from it.
- **`Volume`:** persistent and mountable (`volumeMounts`).
- **`Secret`:** write-only, versioned, referenced as `${e2b.secrets.<name>}` (`packages/js-sdk/src/secret.ts`).
- **IAM tokens:** JWT-SVID workload identity minted per request.

### Orchestration / lifecycle model
All in `packages/js-sdk/src/sandbox/index.ts` and `sandboxApi.ts`:

- `create(template, {timeoutMs, metadata, envs, network, volumeMounts, lifecycle:{onTimeout, autoResume}, iam, mcp})`. Default TTL is 300 s (`connectionConfig.ts:11`). Max is 24 h on Pro, 1 h on Hobby.
- `setTimeout(ms)` / refreshes: the TTL is a **lease**.
- `onTimeout: 'kill' | 'pause' | {action:'pause', keepMemory}`, plus `autoResume` on inbound traffic.
- `pause({keepMemory})`: a full memory snapshot, or filesystem-only.
- `connect(id, {onResume: 'restore'|'reboot'})`: auto-resumes a paused sandbox. `reboot` is the rescue path for a wedged memory image.
- **`fork({count})`:** checkpoint in place once, boot N sandboxes, and return a `Promise.allSettled`-style array of sandbox-or-error.
- `createSnapshot({name})`, `listSnapshots`, `kill`, `list({query: metadata})`, `getMetrics`.

**Exec:** `commands.run(cmd, {background, cwd, user, envs, onStdout, onStderr, stdin, timeoutMs})`. The command timeout defaults to **60 s** (`commands/index.ts`). It returns a `CommandHandle`: `pid`, `wait`, `kill`, `sendStdin`, `closeStdin`, `disconnect`.
- `commands.connect(pid)` re-attaches, and processes can carry a `tag`.
- The envd wire model is a stream of `ProcessEvent{Start{pid} | Data{stdout|stderr|pty} | End{exit_code, exited, status, error} | KeepAlive}`.
- `ConnectRequest` carries only a selector, with **no offset**, so output produced while detached is not replayable.

**Filesystem:** `read`/`write` (text, bytes, stream), `writeFiles`, `list(depth)`, `makeDir`, `rename`, `remove`, `exists`, `getInfo`, streaming `watchDir` plus a polling watcher API, and signed `uploadUrl`/`downloadUrl`.

### State & persistence
- Memory and filesystem snapshots on pause; named persistent snapshots; volumes independent of sandboxes.
- Metadata on sandboxes lets an orchestrator re-find its sandboxes after a restart (`list({query})`).

### Isolation, network & secrets
The sandbox is a Firecracker microVM. `SandboxNetworkOpts` (`sandboxApi.ts` L215–331) offers:

| Option | What it does |
|---|---|
| `allowOut` / `denyOut` | CIDR/IP/hostname lists or callback form. **Default: all outbound allowed.** |
| `rules[host]` with `transform.headers` | Header injection per host, static or placeholder-based (`${e2b.identity.tokens.*}`, secrets). The egress proxy resolves them, so the sandbox never sees the value. This implies TLS interception. |
| `egressProxy` | SOCKS5 "bring your own proxy". Fails closed. UDP (DNS/QUIC) is **not** tunneled. **Unsupported on the OSS runtime.** |
| `allowPublicTraffic`, `maskRequestHost`, `httpsPorts` | Ingress controls |

`updateNetwork` changes egress at runtime.

`dangerouslyAuthenticate` in the git module stores git credentials inside the sandbox. Its name signals the tradeoff.

### Agent integration
Agent-agnostic: run Claude Code or Codex as a process. The `mcp` option starts an in-sandbox MCP gateway, and `getMcpUrl()`/`getMcpToken()` let an agent *outside* use tools *inside*.

### Verification, budgets & safety rails
- TTL lease, per-command timeout, rate-limit errors and CPU/memory metrics.
- No token or cost budget for the workload.
- A version-skew hazard is documented honestly: an older control plane **silently ignores** `onResume` "while answering as if the request had succeeded" (`sandboxApi.ts` `SandboxOnResume` doc).

### Observability & UI
Sandbox logs (v1/v2), metrics, lifecycle events and **webhooks** with delivery stats (`spec/openapi.yml` `/events/webhooks/*`).

### Config & extensibility
- Templates as code.
- `CLAUDE.md` requires JS↔Python sync/async parity and changesets. `TASTE.md` points to external SDK design principles (`e2b-dev/sdk-harness`).
- Self-hosting via the runtime repo, with feature lag.

### Strengths — steal these
- The **lifecycle surface** is close to what we need: create/connect/pause{keepMemory}/resume{restore|reboot}/fork{count}/snapshot/kill/setTimeout/updateNetwork (`sandbox/index.ts`, `sandboxApi.ts`).
- **`fork({count})` with per-fork results**, for best-of-N attempts from one checkpoint (`sandbox/index.ts` `fork`).
- `onTimeout: pause` + `autoResume`: idle cost control without losing state (`sandboxApi.ts` `SandboxLifecycle`).
- Exec as a typed event stream with pid/tag and re-attach, plus stdin, signals and PTY resize (`spec/envd/process/process.proto`).
- Egress rules with per-host credential-injection transforms, write-only versioned secrets and workload identity (`sandboxApi.ts`, `secret.ts`).
- Metadata labels plus list/query for orphan recovery; lifecycle webhooks.
- Git, MCP and file helpers layered on generic exec rather than per-backend features (`sandbox/git/index.ts`).

### Weaknesses & tradeoffs
- Egress is fail-open by default, and DNS/UDP leave outside the SOCKS tunnel.
- A 24 h max lifetime: long attempts must snapshot or rotate.
- The 60 s default command timeout surprises agent workloads, and the 5-minute default TTL means the orchestrator must renew the lease.
- The best features (egressProxy, `onResume`) lag or are missing on self-host.
- The protocol has no output replay on reattach.
- The SDK is TS/Python only. We would write our own Rust client for REST plus Connect-RPC (`prost` on the synced protos).

### Implications for a Rust framework
- Model our `Workspace` trait on this API.
- Keep an **orchestrator-side output journal** with sequence numbers so the UI can replay regardless of backend.
- Treat the TTL as a lease that a heartbeat task renews.
- Fail closed: our default egress policy is deny, with explicit allow rules.

---

## Proposed workspace/sandbox abstraction

### Design principles
1. **Two layers.**
   - `WorkspaceBackend` provisions workspaces and lists existing ones.
   - `Workspace` is a live handle: lifecycle, exec, fs and code sync.
   - A `Capabilities` struct lets the orchestrator plan around a backend's limits instead of discovering them at runtime. For example, fork on a worktree means *fs-only*.
2. **Policy is declarative and fails closed.** A backend returns a `PolicyReport` saying which rules are *enforced*, *advisory* or *unsupported*. The attempt does not start if a required rule is not enforced. This avoids AX's `(continuing)` and container-use's opt-in isolation.
3. **Secrets are references resolved as late and as far out as the backend allows.** In order of preference: egress header injection (Substrate/E2B) > per-exec env > file. Never put secrets in templates or snapshots (unlike AX and the Substrate demo).
4. **Code leaves only through `sync_out`.** The result is commits on a factory-owned branch in a bare mirror of the user's repo (the container-use pattern), with an explicit exclude list and a report of anything excluded.
5. **Events have a sequence number and are journaled by the orchestrator.** The UI replays from any `EventSeq`, independent of backend reattach semantics.
6. **Lease plus level-triggered reconcile.**
   - Every workspace has a TTL lease renewed by a heartbeat.
   - A reconciler compares desired and observed state periodically (not only on events), with CAS on the status row.
   - Operations on one workspace are serialized by a per-workspace lease (Substrate).
7. **One conformance suite** (Substrate's `storecontract` idea) runs against every backend.

### Rust sketch

```rust
// crate: forgeline-workspace  (async-trait for dyn-compat; futures BoxStream; bytes::Bytes)
use forgeline_core::ids::{AttemptId, WorkspaceId};

#[async_trait]
pub trait WorkspaceBackend: Send + Sync + 'static {
    fn kind(&self) -> BackendKind;                       // LocalWorktree | Docker | E2b | Substrate | Ax
    fn capabilities(&self) -> Capabilities;
    async fn provision(&self, spec: &WorkspaceSpec) -> Result<Box<dyn Workspace>, WsError>;
    async fn attach(&self, id: WorkspaceId) -> Result<Box<dyn Workspace>, WsError>;  // after orchestrator restart
    async fn restore(&self, snap: &SnapshotRef, spec: &WorkspaceSpec) -> Result<Box<dyn Workspace>, WsError>;
    async fn list(&self, labels: &LabelSelector) -> Result<Vec<WorkspaceStatus>, WsError>; // orphan GC
}

#[async_trait]
pub trait Workspace: Send + Sync {
    fn id(&self) -> WorkspaceId;
    async fn status(&self) -> Result<WorkspaceStatus, WsError>;
    fn events(&self, since: Option<EventSeq>) -> BoxStream<'static, (EventSeq, WorkspaceEvent)>;

    // lifecycle
    async fn fork(&self, count: u32, opts: ForkOpts) -> Result<Vec<Result<Box<dyn Workspace>, WsError>>, WsError>;
    async fn snapshot(&self, opts: SnapshotOpts) -> Result<SnapshotRef, WsError>;   // named, immutable (Tag)
    async fn suspend(&self, scope: SnapshotScope) -> Result<(), WsError>;
    async fn resume(&self, mode: ResumeMode) -> Result<(), WsError>;                // Restore | Reboot
    async fn revert(&self) -> Result<(), WsError>;                                   // to last good snapshot
    async fn renew_lease(&self, ttl: Duration) -> Result<SystemTime, WsError>;
    async fn apply_policy(&self, p: &SandboxPolicy) -> Result<PolicyReport, WsError>;
    async fn destroy(self: Box<Self>, opts: DestroyOpts) -> Result<(), WsError>;

    // execution
    async fn exec(&self, req: ExecRequest) -> Result<ExecHandle, WsError>;
    async fn reattach(&self, sel: ProcessSelector) -> Result<ExecHandle, WsError>;  // Pid(u32) | Tag(String)
    async fn processes(&self) -> Result<Vec<ProcessInfo>, WsError>;

    // files, ports, code egress
    fn fs(&self) -> &dyn WorkspaceFs;   // read/write/list/stat/remove/rename/watch(-> BoxStream<FsEvent>)
    async fn expose(&self, port: u16, vis: Visibility) -> Result<Endpoint, WsError>;
    async fn sync_out(&self, req: SyncOut) -> Result<ChangeSet, WsError>;  // commit(s) on factory branch + diff stats
}

pub struct ExecHandle {
    pub pid: u32,
    pub tag: Option<String>,
    pub events: BoxStream<'static, ExecEvent>,              // Started | Stdout(Bytes) | Stderr(Bytes) | Pty(Bytes) | Exited{..}
    pub stdin: Option<Pin<Box<dyn AsyncWrite + Send>>>,
    pub control: Arc<dyn ExecControl>,                      // signal(Signal), resize(PtySize), wait() -> ExitStatus
}

pub struct ExecRequest {
    pub argv: Vec<String>, pub cwd: Option<PathBuf>, pub env: BTreeMap<String, String>,
    pub secrets: Vec<SecretBinding>,            // resolved by backend, never logged
    pub user: Option<String>, pub stdin: bool, pub pty: Option<PtySize>, pub tag: Option<String>,
    pub timeout: Option<Duration>, pub idle_timeout: Option<Duration>, pub output_limit: usize,
    pub kill_process_group: bool,               // default true
}
pub enum ExitReason { Exited(i32), Signaled(i32), Timeout, IdleTimeout, OutputLimit, BudgetExceeded, Lost }
```

```rust
pub struct Capabilities {
    pub isolation: Isolation,              // Host | OsSandbox (landlock/bwrap/seatbelt) | Container | Gvisor | MicroVm
    pub fork: ForkSupport,                 // None | FsOnly | FsAndMemory
    pub snapshot_scopes: Vec<SnapshotScope>, // Full (mem+fs) | FsOnly
    pub suspend: bool, pub resume_on_traffic: bool,
    pub egress: Enforcement,               // None | Advisory (proxy env vars) | Enforced
    pub credential_injection: bool,        // secrets resolved outside the sandbox
    pub ports: bool, pub pty: bool, pub reattach: bool, pub max_lifetime: Option<Duration>,
}

pub struct WorkspaceSpec {
    pub id: WorkspaceId, pub attempt: AttemptId, pub labels: BTreeMap<String, String>,
    pub source: SourceSpec,                // repo mirror, base ref, factory branch name, depth, submodules
    pub template: TemplateRef,             // image | devcontainer | local toolchain | E2B template | ActorTemplate
    pub setup: SetupSpec,                  // setup_cmds (cached, pre-source), install_cmds (post-source), goal, ready_check
    pub mounts: Vec<Mount>,                // extra read-only workspaces (AX multi-workspace)
    pub policy: SandboxPolicy,
    pub lifecycle: LifecyclePolicy,        // ttl, on_idle{after, action: Suspend(scope)|Destroy}, on_expire, keep_after_done
}
```

### Policy types

```rust
pub struct SandboxPolicy {
    pub egress: EgressPolicy,
    pub secrets: Vec<SecretBinding>,
    pub fs: FsPolicy,                      // writable roots, read-only paths, denied paths
    pub resources: Resources,              // cpus, memory_mb, disk_mb, pids
    pub budget: Budget,                    // wall_clock, per_exec_default, output_bytes, tokens, cost_usd (shared Arc across sub-agents)
    pub interactive_access: bool,          // human shell / guest exec (AX `debug`)
    pub required: Vec<PolicyFacet>,        // facets that MUST be Enforced or provisioning fails
}
pub struct EgressPolicy {
    pub default: Verdict,                  // Deny by default
    pub rules: Vec<EgressRule>,            // first match wins (Substrate semantics)
    pub dns: DnsPolicy,                    // SystemResolver | Pinned(Vec<IpAddr>) | ViaProxyOnly
    pub block_private_ranges: bool,        // axe hostcheck list
}
pub struct EgressRule { pub matcher: Matcher, pub ports: Option<Vec<u16>>, pub inject: Vec<HeaderInjection> }
pub enum Matcher { Host(String /* exact or "*.leftmost" */), Cidr(ipnet::IpNet), All }
pub struct HeaderInjection { pub header: String, pub prefix: String, pub secret: SecretRef }
pub struct SecretBinding { pub name: String, pub source: SecretRef, pub exposure: Exposure, pub redact: bool }
pub enum SecretRef { Env(String), File(PathBuf), Keychain(String), OnePassword(String), Vault(String), Provider { scheme: String, uri: String } }
pub enum Exposure { EgressHeader { host: String, header: String, prefix: String }, EnvVar(String), File(PathBuf) }
pub struct PolicyReport { pub facets: Vec<(PolicyFacet, Enforcement)> }   // orchestrator refuses to start agent if a `required` facet < Enforced
```

### Lifecycle states

```
Pending ─▶ Provisioning ─▶ Preparing (clone / setup / install / goal bootstrap) ─▶ Ready ⇄ Busy (exec running)
Ready|Busy ─▶ Snapshotting ─▶ Ready                    (brief pause; E2B createSnapshot, Substrate Tag)
Ready|Busy ─▶ Suspending ─▶ Suspended{scope} ─▶ Resuming ─▶ Ready   (manual, on_idle, or on_expire)
Busy ─▶ Crashed ─▶ (revert) ─▶ Suspended | Terminating
any ─▶ Failed{reason}   ·   any ─▶ Terminating ─▶ Terminated
```

Desired state is one of `{Running, Suspended, Terminated}` (AX `spec.suspend`). Conditions, following AX:
- `SourceReady`
- `SetupComplete`: sticky, and its marker lives inside the preserved scope or in the DB
- `PolicyEnforced`: must be True before the agent process starts
- `LeaseValid`
- `Ready`

### How the five backends map

| Op | LocalWorktree (+OS sandbox) | Docker/Podman | E2B | Substrate | AX |
|---|---|---|---|---|---|
| provision | bare mirror → branch → `git worktree add` (container-use) | container from image + bind/volume of worktree or copy-in | `Sandbox.create(template, {metadata})` | ActorTemplate (golden) + `CreateActor` + `ResumeActor` | apply Task/Workspace/Gateway |
| exec/stream | `tokio::process` in new process group, env scrub, bwrap/landlock (Linux) or seatbelt (macOS) | `docker exec` attach (bollard) | envd `Process.Start` stream | needs in-guest agentd (guest services) | `ax ssh`/guest services (debug-only) |
| fork | new branch from checkpoint commit (FsOnly) | `docker commit` → N containers (FsOnly) | `fork({count})` (FsAndMemory) | suspend → `CreateTag` → N× `CreateActor(source_tag)` | roadmap only |
| snapshot | checkpoint commit + tag | `docker commit` / image push | `createSnapshot` | `CreateTag` | n/a |
| suspend/resume | kill processes; files persist (FsOnly) | `docker pause`/`stop`+`start` | `pause{keepMemory}` / `connect{onResume}` | `Suspend/Pause/ResumeActor` (FULL/DATA) | `spec.suspend` (DATA only) |
| egress | netns + local CONNECT proxy (Enforced on Linux, else Advisory) | `--network none` + proxy sidecar | `allowOut/denyOut/rules` | `EgressPolicy` via PEP | Gateway (ports dropped today) |
| secrets | env per exec; header injection only via local MITM proxy (later) | env / secret files | `rules.transform`, `${e2b.secrets.*}` | credential provider at PEP | K8s secret → template env (avoid) |
| sync_out | commit in worktree | commit from bind mount, or `git bundle` via fs | in-sandbox commit → push to factory remote with injected token, or bundle via fs | same as E2B | same |
| teardown | `worktree remove` + branch GC | `rm -f` | `kill` | `DeleteActor(any_state)` | `ax delete` |

### Which backend first
**Build `LocalWorktree` first, with an `OsSandbox` exec layer. Build `Docker` second, `E2B` third, and `Substrate` behind a `forgeline-agentd` in-guest daemon later. Skip AX as a backend.**

Why worktree first:
- It needs no infrastructure and matches how Claude Code and Codex are run today.
- Its git plumbing becomes the shared `sync_out` implementation for every other backend: bare mirror, factory branch, per-step commits and no hooks.
- It exercises the whole orchestrator and UI loop early.

Its `Capabilities` report `fork: FsOnly`, `snapshot_scopes: [FsOnly]` and `egress: Enforced` only where a network namespace plus proxy is available. The `required`-facet check keeps policy honest.

Docker adds real fs and network isolation cheaply. E2B maps almost 1:1 onto the trait and adds memory fork. Substrate brings density and credential injection, but it has no exec API in its core, so it needs our own in-guest daemon: AX's runner contract plus envd-style Process/FS RPC. That daemon also gives uniform semantics on any plain VM. AX is at the same layer as our orchestrator, so its value is in the ideas (Workspace/Gateway split, runner contract, desired-state suspend), not as a backend.

Ship `forgeline-workspace-conformance` from day one: one test suite parameterized by backend, covering lifecycle transitions, exec streaming and timeouts, process-group kill, fs round-trip, fork isolation, policy fail-closed and `sync_out` completeness.
