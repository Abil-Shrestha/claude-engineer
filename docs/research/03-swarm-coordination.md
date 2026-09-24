# 03 — Swarm coordination & work ledger: Gas Town and Beads

**Group overview.** These two projects are a matched pair from the same author, and together they are the most operationally battle-scarred "software factory" in this survey. **Beads (`bd`)** is the *work ledger*: a dependency-graph issue tracker stored in Dolt (a version-controlled SQL database). It provides typed DAG edges, a persisted "ready" projection, compare-and-swap (CAS) claims, leases with heartbeats, and a cross-machine sync story. **Gas Town (`gt`)** is the *factory floor*. It runs one tmux session per agent, uses git worktrees as sandboxes, and stacks LLM-driven supervisors (Mayor, Deacon, Witness, Refinery) on top of a deterministic Go daemon. All coordination (mail, hooks, merge requests, convoys) goes through beads rows. Together they are about 600K lines of non-test Go. The main lesson: **the split between the ledger and the executor is right**, but putting coordination on a shared versioned database, using LLM prose as the control plane, and talking to agents through tmux keystrokes produces a large body of workaround code. The contention bugs referenced in comments (Dolt read-only mode, connection storms, zombie processes, nudge garbling) are exactly the failure modes a Rust factory has to design out from the start. All findings below come from reading the code at `gastown@649b832` (2026-07-23) and `beads@c507f3b` (2026-09-24). Files are cited relative to each repo root.

---

## steveyegge/gastown

### What it is
Go multi-agent workspace manager / orchestrator. About **249K non-test LOC** (+226K test LOC), with 104K in `internal/cmd` alone. Release v1.2.1 (CHANGELOG, 2026-06-06); last commit 2026-07-23. It is mature in the sense of being hardened by production incidents: nearly every non-trivial branch carries a `gt-xxxx`/`GH#` incident comment. It is also still churning, with visible design drift between docs, prompts and code.

### Architecture
Processes on one host:

```
                 ┌──────────────── gt daemon (Go, deterministic) ────────────────┐
                 │ heartbeat every 3m (config/operational.go):                   │
                 │ ensure Dolt → Deacon → Boot → Witnesses → Refineries → Mayor  │
                 │ GUPP check, orphaned work, polecat health, idle reap,         │
                 │ scheduler dispatch (`gt scheduler run`), ConvoyManager poll   │
                 └───────┬───────────────────────────────┬───────────────────────┘
                         │ tmux new-session / respawn    │ MySQL protocol
   tmux server ──────────▼──────────────┐        ┌───────▼────────────────────┐
   hq-mayor   (claude, LLM)             │        │ dolt sql-server :3307      │
   hq-deacon  (LLM patrol)              │ bd/gt  │  hq DB  (town: mail, agents,│
   <rig>-witness  (LLM patrol)          ├───────►│   convoys, sling contexts) │
   <rig>-refinery (LLM merge queue)     │ CLIs   │  <rig> DBs (issues, MRs,    │
   <rig>-<Toast>  polecat (claude/codex)│        │   agent beads, wisps)       │
   crew/<name>    human-driven          │        └────────────────────────────┘
   └── each pane cwd = git worktree of <rig>/mayor/rig (shared object store)
   ~/gt/.runtime/{locks,heartbeats,nudge_queue,pids}  ~/gt/.events.jsonl
```

The agent-facing API is the `gt` and `bd` CLIs themselves. Agents run shell commands such as `gt done`, `gt mail send` and `bd update`, and the orchestrator reacts to the resulting database and filesystem state. Gas Town mostly shells out to `bd` (`internal/beads/beads.go` builds `exec.CommandContext(ctx, "bd", …)`), with an optional in-process `beadsdk.Storage` fast path used by the daemon and mail (`internal/beads/store.go`, `internal/daemon/daemon.go` `openBeadsStores`).

**Work item flow**, traced through `gt sling <bead> <rig>` (`internal/cmd/sling.go` `runSling`):
1. Take a per-bead flock at `.runtime/locks/sling/<id>.flock` (`tryAcquireSlingBeadLock`).
2. Run guards: refuse closed or deferred beads and flag-like titles. If the bead is already hooked but its assignee's session is dead, auto-force a re-sling.
3. `resolveTarget` allocates a themed polecat name from the pool (`internal/polecat/namepool.go`). It creates a worktree on a fresh branch from `<rig>/mayor/rig` (`internal/polecat/manager.go` `AddWithOptions` → `WorktreeAddFromRef`) and runs the rig's setup command (30-minute timeout).
4. Reserve capacity (`internal/cmd/polecat_capacity.go` `acquirePolecatAdmission`, which uses a flock plus reservation files, and only when `scheduler.max_polecats > 0`).
5. Create an auto-convoy. Instantiate the formula `mol-polecat-work` as a root wisp bonded to the bead (`InstantiateFormulaOnBead`).
6. Take a per-assignee flock. Run `bd update --status=hooked --assignee=<agent>` with 10 retries and a read-back verification (`internal/cmd/sling_helpers.go` `hookBeadWithRetryWithTownRoot`).
7. Start the session with `SessionManager.Start` (`internal/polecat/session_manager.go`). This creates a tmux session with `-e` env vars (GT_ROLE, BD_ACTOR, GT_RUN OTEL run-id, BD_DOLT_AUTO_COMMIT=off) and launches the agent CLI with a "beacon" prompt. It then screen-scrapes to accept the trust and bypass dialogs, waits for the `❯ ` prompt, sends a startup nudge and re-verifies delivery.
8. If any step fails, roll back (`rollbackSlingArtifacts`).

Inside the session, the agent's SessionStart hook runs `gt prime --hook`. That renders the role template, operator directives, the hooked bead, the formula checklist inline, and any mail. The agent works, then runs `gt done` (`internal/cmd/done.go`, 2.7K lines). `gt done` pushes the branch, creates an ephemeral **MR bead** (`gt:merge-request`) and notifies the Witness. It then kills its own tmux session, excluding its own PID so the command itself survives. The Witness (an LLM) runs `gt patrol scan`, which calls Go zombie and completion detection (`internal/witness/handlers.go` `DetectZombiePolecats`). It sends `MERGE_READY` to the Refinery (an LLM), which merges, pushes, and mails back `MERGED`.

### Core abstractions / domain model
- **Town → Rig → Agents.** A rig holds a bare `.repo.git`, a canonical clone `mayor/rig` where its `.beads` lives, and `witness/`, `refinery/`, `crew/` and `polecats/` directories (`docs/design/architecture.md`).
- **Everything is a bead.** Agents are agent beads (`<prefix>-<rig>-polecat-<name>` with `hook_bead`, `agent_state` and `role_bead`). Mail messages are beads labelled `gt:message` with assignee = recipient. MRs, convoys, sling contexts, cleanup wisps and merge slots are all beads too.
- **Hook** = bead `status=hooked` + assignee, mirrored in the agent bead's `hook_bead`. The "Propulsion Principle" (GUPP) says: if something is on your hook, you run it.
- **Polecat has three layers** (`docs/concepts/polecat-lifecycle.md`, `internal/polecat/types.go`): *Identity* (permanent, with CV and history), *Sandbox* (worktree per assignment), and *Session* (ephemeral tmux/Claude, cycled on handoff or compaction). States: `working, idle, done, review-needed, stuck, stalled, zombie`. The last three are *derived* by cross-checking tmux against beads ("discover, don't track"). Reuse goes through one fail-closed decision function, `DecideSlotReuse`/`DecideWorkstate` (`internal/polecat/reuse.go`, `workstate.go`).
- **Formula → protomolecule → molecule/wisp** (47 embedded TOML formulas in `internal/formula/formulas/`). Root-only wisps render their steps into the prompt. `pour = true` materializes the steps as sub-wisps so completed steps survive a session crash (`docs/concepts/molecules.md`).
- **Convoy.** A tracking bead with `tracks` edges to the work items; it "lands" when everything it tracks is closed. It carries a merge strategy: `direct`, `mr` or `local`.
- **Sling context.** An ephemeral scheduler bead holding a JSON description and a `tracks` edge to the work bead. Scheduling state therefore never mutates the work item. The state machine is OPEN → CLOSED(dispatched | circuit-broken after 3 failures | cleared) (`docs/design/scheduler.md`).
- **Protocol messages.** Typed only by subject-line regex (`internal/witness/protocol.go`): `POLECAT_DONE`, `MERGE_READY`, `MERGED`, `MERGE_FAILED`, `HELP:`, `LIFECYCLE:Shutdown`, `DISPATCH_*`.

### Orchestration model
- **Sourcing.** A human or the Mayor creates beads and runs `gt sling` on a bead, a batch, an epic or a convoy. When an issue closes, `ConvoyManager` (`internal/daemon/convoy_manager.go`) polls beads events and feeds the next ready issue to its convoy (`feedFirstReady`). It also runs a periodic scan for stranded convoys.
- **Scheduling.** The default is *direct dispatch with no cap*: `scheduler.max_polecats = -1`. In deferred mode (`> 0`), heartbeat step 14 shells out to `gt scheduler run`. That command takes a flock, counts active polecats, joins the sling contexts with `bd ready`, and runs `DispatchCycle` (capacity, batch size, spawn delay) (`internal/cmd/capacity_dispatch.go`, `internal/scheduler/capacity/`). Pressure gating by load average, free memory or session count exists (`internal/daemon/pressure.go`) but is *disabled by default*. The README's claim to "scale comfortably to 20-30 agents" therefore depends on the operator configuring these limits.
- **Concurrency control.** Filesystem flocks per bead, per assignee and for admission. A Dolt connection-capacity admission check fails closed (`manager.go` `CheckDoltServerCapacity`, added after "read-only mode under load", gt-lfc0d). Transient Dolt errors are retried with exponential backoff and jitter. They are classified by **substring matching** on error text (`isDoltOptimisticLockError`: "optimistic lock", "database is read only", "cannot update manifest").
- **Supervision chain.** Daemon (Go) → Boot (ephemeral LLM triage) → Deacon (LLM patrol) → Witness per rig (LLM) → polecats. The daemon's restart tracker has 30s→10m exponential backoff, puts an agent into crash-loop state after 5 restarts in 15 minutes, and uses a separate fixed "pause" backoff for usage-limit stops (`internal/daemon/restart_tracker.go`). Polecats write heartbeat files v2 with self-reported state, stale after 3 minutes (`internal/polecat/heartbeat.go`). A session with no output for 30 minutes counts as hung (`constants.HungSessionThreshold`). The Witness follows a **restart-first** policy and never nukes a polecat automatically (`mol-witness-patrol.formula.toml`).
- **Completion** is self-managed (`gt done`). The Witness observes but does not gate it, so it cannot become a bottleneck.
- **Merge landing** (the "refinery"). **Critical finding:** the Go merge engine exists. `internal/refinery/engineer.go` has `doMerge` with gate phases and `acquireMainPushSlot`, and `batch.go` has Bors-style `ProcessBatch` batch-then-bisect. But neither `ProcessMRInfo` nor `ProcessBatch` has a non-test caller. `gt refinery` only uses `Engineer` for claim, release and list. The live merge queue is the **Refinery LLM** following `mol-refinery-patrol.formula.toml`, one MR at a time, running prose instructions like `git checkout -b temp origin/<branch>; git merge --no-ff origin/<target>`. It then runs the configured gates, `git merge --no-ff` + `git push`, and compares local and remote SHAs "to detect silent push failure". On a conflict it creates a conflict-resolution task bead. `docs/design/architecture.md` marks batch-then-bisect "Blocked by Phase 1".

### State & persistence
- **Durable.** One Dolt server per town, with the `hq` database plus one database per rig under `~/gt/.dolt-data/`. `routes.jsonl` maps ID prefixes to rigs; worktrees carry a `.beads/redirect` file (max depth 3).
- **Ephemeral.** Wisps (in beads' dolt_ignored `wisps` table), the nudge queue, heartbeats, flocks, pidfiles, and tmux itself, which is the liveness source of truth.
- **Crash recovery.**
  - `gt done` writes **checkpoints as labels** on the agent bead (`done-cp:<stage>:<value>:<ts>`, stages pushed/mr-created/witness-notified). `--resume` replays from them and discards stale checkpoints whose branch does not match.
  - An MR is idempotent by branch+commit. The MR bead is read back to verify it exists *before* the worktree is torn down (GH#1945).
  - `gt handoff` persists a self-mail to Dolt *before* running `tmux respawn-pane -k`, and fails loudly if Dolt is down (`internal/cmd/handoff.go`).
- **Dolt pollution control.** "Dogs" reap closed wisps, flatten history, kill zombie Dolt servers ("45 zombies (7GB RAM)"), and back up JSONL. There is also an explicit per-role *mail budget*: prefer `gt nudge`, which writes zero commits (`docs/design/dolt-storage.md`).
- **Doc/code drift.**
  - Auto-commit: `dolt-storage.md` says "auto-commit is on by default… all agents write to main". But `internal/config/env.go` sets `BD_DOLT_AUTO_COMMIT=off` for polecats. Its comment cites a branch-per-polecat/DOLT_MERGE design the same doc says was removed, and blames "manifest contention leading to Dolt read-only mode (gt-5cc2p)".
  - Hook storage: the README says work persists in "git-backed hooks", but hooks are Dolt rows.

### Isolation & workspaces
- Polecats and the refinery work in **git worktrees** off `mayor/rig`, sharing its objects. Crew get full clones. Branch names are load-bearing metadata: `gt done` infers the issue from the branch (`internal/polecat/branch_name.go`). A cross-rig guard rejects slinging a bead to another rig's polecat.
- There is **no process or network isolation by default**. Agents run under the user's UID with `claude --dangerously-skip-permissions` or `codex --dangerously-bypass-approvals-and-sandbox` (`internal/config/agents.go`).
- The guardrails are PreToolUse "tap guards" on command patterns (`gh pr create*`, `git checkout -b*`, `sudo *`, `apt install*`) in `internal/hooks/templates/claude/settings-autonomous.json` and `internal/cmd/tap_guard*.go`.
- An optional `ExecWrapper` (e.g. `exitbox run --profile=…`, `internal/config/types.go`) and an mTLS `gt-proxy-server`/`gt-proxy-client` pair let containerized polecats call `gt`/`bd` and push over git smart-HTTP (`docs/proxy-server.md`). The full sandboxing design (`docs/design/sandboxed-polecat-execution.md`) is still marked "Proposal".

### Agent integration
- **Presets.** claude, gemini, codex, kiro, cursor, auggie, amp, opencode, copilot, pi, omp, vibe and groq-compound. They live in one registry struct, `AgentPresetInfo` (`internal/config/agents.go`), which records per agent:
  - command and args;
  - `ProcessNames` for liveness checks;
  - resume flag and resume style;
  - hooks provider and directory;
  - `ReadyPromptPrefix` (`"❯ "` for Claude, `"› "` for Codex) and `ReadyDelayMs`;
  - `EscapeCancelsRequest`;
  - `HasTurnBoundaryDrain`;
  - ACP mode.

  Users can extend the registry via JSON.
- **Invocation.** The *interactive TUI runs inside tmux*, not headless. Delivering a message (`internal/tmux/tmux.go` `NudgeSessionWithOpts`, part of a 4.4K-line file) takes these steps:
  1. Take a cross-process flock and an in-process semaphore.
  2. Find the agent's pane.
  3. Dismiss the "Rewind" menu if it is open, and exit copy mode.
  4. Sanitize control characters.
  5. `send-keys -l` in 512-byte chunks, then wait an adaptive delay.
  6. Send Escape, then wait 600ms, which must exceed readline's `keyseq-timeout`.
  7. Re-check for Rewind mode.
  8. Submit Enter and verify it was accepted.
  9. Send SIGWINCH to wake detached panes.
- **Output parsing.** Essentially none. State changes arrive through the agent running CLIs. Liveness comes from pane process names. Readiness comes from scraping the prompt prefix. Humans read output with `gt peek` (capture-pane). Cost comes from a Stop hook, `gt costs record`.
- **Hooks installed.** SessionStart/PreCompact → `gt prime --hook`. UserPromptSubmit → `gt mail check --inject`, which drains the **nudge queue** at the turn boundary so an in-flight tool call is never interrupted (`internal/nudge/queue.go`: per-session JSON files, TTL 30m normal / 2h urgent, depth cap 50, `.claimed` files for crash-safe drain). Agents without turn-boundary hooks get a background nudge-poller process instead.
- **Messaging.** Durable *mail* (`internal/mail/router.go`) creates a bead and a Dolt commit. It supports addresses for agents, lists, claimable queues, announce channels and groups. Ephemeral *nudges* are delivered either immediately through tmux or queued. `gt handoff` means auto-hooked self-mail, then a handoff marker, then `respawn-pane`. `gt seance` resumes a predecessor Claude session via `--fork-session` so you can ask it questions.

### Verification & quality gates
- The polecat formula `mol-polecat-work` runs load-context → branch-setup → implement → commit → self-review → build-check → **pre-verify** → submit. Pre-verify means rebasing onto the target and running all configured gates, then `gt done --pre-verified`, which records `pre_verified_base` on the MR so the refinery can fast-path it.
- `gt done` guards:
  - refuse to submit with zero commits;
  - require fresh review-evidence comments tied to the HEAD SHA for review-only work (`hasFreshReviewReportEvidence`);
  - strip the overlay CLAUDE.md;
  - verify the pushed commit;
  - read back the MR bead.
- Refinery gates are per-rig `setup/typecheck/lint/build/test` commands injected as formula variables. They are run and interpreted by the LLM. There is an optional LLM `quality-review` step (off by default) and a `merge_strategy=pr` option with `require_review`. A pre-push hook forbids landing integration branches except via `gt mq integration land`.

### Observability & UI
- `.events.jsonl` is the raw audit log. The feed curator dedups and aggregates it into `.feed.jsonl` (`internal/events`, `internal/feed/curator.go`).
- `gt feed` is a Bubble Tea TUI with a "problems" view for stuck or zombie agents. `gt dashboard` is a web UI with SSE at `/api/events` and endpoints for convoys, mail, issues and allowlisted `/api/run` commands (`internal/web`).
- Other commands: `gt vitals`, `gt costs`, `gt trail`, `gt peek`. Each rig gets its own tmux theme.
- OTEL logs and metrics are keyed by a per-spawn `run.id` propagated via `GT_RUN` to every `bd` subprocess (`docs/otel-data-model.md`).
- `gt doctor` runs 130+ checks (`internal/doctor`, 20K LOC).
- The primary "UI" is still attaching to an agent's tmux session.

### Config & extensibility
- Config files:
  - `settings/config.json`: agents, plus the "ZFC" operational thresholds, which are compiled defaults you can override.
  - `mayor/daemon.json`: per-patrol enablement, auto-populated by `EnsureLifecycleDefaults`.
  - `config/messaging.json`, `settings/escalation.json`, `mayor/accounts.json` (Claude account rotation, plus a quota dog).
- **Role directives** (markdown injected at prime time) and **formula overlays** (`replace`/`append`/`skip` per step) layer town → rig (`internal/config/directives.go`, `internal/formula/overlay.go`). `gt doctor` validates the overlay step IDs.
- You can also add custom formulas, Deacon plugins, custom agent presets and hook overrides (`gt hooks`).

### Strengths — steal these
- **Identity / Sandbox / Session separation**, with reuse decided by one pure, fail-closed function (`internal/polecat/reuse.go`, `workstate.go`). Unknown git or MR state means "needs recovery", never "reusable".
- **Idempotent, checkpointed completion.** The flow is push → MR → notify, with checkpoints, an idempotency key of branch+commit, and a read-back before any destructive cleanup (`internal/cmd/done.go`).
- **Durable mail vs ephemeral nudge**, with nudges drained at turn boundaries through the agent's UserPromptSubmit hook: TTL, depth cap, crash-safe claim files (`internal/nudge/queue.go`). There is also an explicit per-role message budget to control write amplification.
- **Agent preset registry** as the single source of truth for runtime quirks, with capability flags instead of `switch agent` scattered around (`internal/config/agents.go`).
- **Restart tracker** with a crash-loop budget and a distinct non-escalating "paused for usage limit" backoff (`internal/daemon/restart_tracker.go`). Also mass-death detection (`recordSessionDeath`/`emitMassDeathEvent`) and a global E-stop (`internal/estop`).
- **Scheduler state kept off the work item** (sling-context beads), plus a circuit breaker after N dispatch failures (`docs/design/scheduler.md`).
- **Context re-hydration**: `prime` on SessionStart/PreCompact, and handoff = persist first, then respawn (`internal/cmd/handoff.go`).
- **Directives and formula overlays** for operator policy without forking prompts.
- **Idle effort tuning**: supervisor patrols switch to an "abbreviated" mode when no events arrived, aiming for about 10% of a full patrol's tokens (patrol formulas).
- **Per-spawn run ID propagated to all subprocess telemetry** (`GT_RUN`).

### Weaknesses & tradeoffs
- **tmux keystroke IPC is inherently fragile.** It depends on prompt glyphs, the Rewind UI, Escape timing and vim mode. Every agent CLI release can break it, and there is no structured result channel.
- **The control plane is LLM prose.** The Refinery, Witness and Deacon are markdown checklists. Merges are raw git commands chosen by an LLM, while the deterministic batch-then-bisect code is dead in production. The result is nondeterministic, token-expensive and hard to test, and it needs supervisors for the supervisors (Boot watching Deacon watching Witness).
- **A versioned DB as a hot coordination bus.** Hooks, mail, heartbeats-as-state and MR updates are all SQL writes, and possibly Dolt commits. At 20+ agents the failure modes named in the code are manifest contention → **read-only mode**, connection storms, optimistic-lock errors and zombie `dolt sql-server` processes. The mitigations are client-side retries (10× with backoff up to 30s), admission checks, and background Dogs that compact and flatten.
- **Single-host assumptions.** Locking uses flocks under `~/gt/.runtime`. `sling` uses a blind `bd update --status=hooked` guarded by those local flocks, not beads' CAS claim.
- **Structured data lives in free text.** MR fields are `key: value` lines in the description, checkpoints are labels, and protocol messages are typed by subject regex.
- **Unsafe defaults.** Capacity is uncapped, pressure gating is off, permissions are bypassed and there is no sandbox. The daemon's recovery loop runs every 3 minutes, so detecting a dead agent can take minutes.
- **Large, drifting surface.** For example, `mol-witness-patrol` still describes a "persistent model: polecats go idle, sandbox preserved for reuse", while `docs/concepts/polecat-lifecycle.md` describes the "retired completion model".

### Implications for a Rust framework
- Use **structured agent protocols as the primary channel**: Claude Code stream-JSON/SDK, `codex exec --json`, ACP. Keep a PTY attach view only for humans. Model adapters as a trait with capability flags mirroring `AgentPresetInfo` (hooks, resume, turn-boundary injection, escape-cancels, ready detection).
- Keep a **deterministic core in Rust**: merge queue (batch-then-bisect, done properly), liveness, reuse and recovery decisions, retries — all as explicit `enum` state machines. Spawn LLMs only for judgment tasks (conflict resolution, review, triage), and spawn them *as work items*.
- The orchestrator should **own the agent processes**. Use tokio child processes in their own process groups (pidfd or cgroups), so liveness is known directly rather than inferred from `pgrep` or pane names. That removes the need for Witness/Deacon/Boot LLM patrols.
- Carry over the ideas that transfer as-is: Identity/Workspace/Run layers, the checkpointed idempotent "submit" transaction, the nudge-vs-mail split with turn-boundary delivery via agent hooks, the restart tracker's crash-loop budget, and circuit breakers.
- Build sandboxing in from day one (bubblewrap, landlock or containers). Make permissions an explicit policy, not a regex guard hook.

---

## steveyegge/beads

### What it is
`bd`, a Go dependency-graph issue tracker for agents, stored in Dolt. About **359K non-test LOC** (+501K test LOC): `cmd/bd` about 115K, storage about 70K, plus 67 up-migrations. Last commit 2026-09-24, so very active. It ships via brew, npm and PyPI (`beads-mcp`), and a public Go SDK (`beads.go`) is consumed by Gas Town (`github.com/steveyegge/beads v1.0.5`). It is the more *engineered* of the two: conformance suites, parity suites across storage stacks, and build-time invariant tests.

### Architecture
```
 agents / humans / gt ──► bd CLI (cobra, cmd/bd; --json everywhere)
                          │            └── bd serve → HTTP v0 (internal/httpapi, OpenAPI types)
                          ▼                   /issues/{id}:claim  /issues:claimNext  /ready
              issueops (tx-level SQL, shared)   /events:watch (SSE, cursor)
              sqlbuild (shared predicates)
   ┌──────────────┬──────────────────────┬─────────────────────────────┐
   │ embedded     │ server               │ proxied-server              │
   │ in-proc Dolt │ dolt sql-server      │ bd-managed proxy → dolt     │
   │ 1 writer,    │ multi-writer         │ (internal/storage/dbproxy,  │
   │ file lock    │                      │  domain/db + uow)           │
   └──────┬───────┴──────────┬───────────┴──────────────┬──────────────┘
          └── Dolt DB: versioned tables + dolt_ignored (wisps, leases, journal)
                     │ bd dolt push/pull → refs/dolt/data on git origin / DoltHub / S3
                     └ .beads/issues.jsonl = export only (not sync)
```
Almost every command has a parallel `*_proxied_server.go` implementation (59 files in `cmd/bd`). "Seam A" parity suites keep ready semantics identical across the stacks (`internal/storage/sqlbuild/ready.go` header). Tracker adapters cover Linear, Jira, GitHub, GitLab, ADO and Notion (`internal/tracker`, `internal/linear`, …).

### Core abstractions / domain model
- **Issue ("bead")**, defined in `internal/types/types.go` `Issue`:
  - hash ID `prefix-xxxx`, with hierarchical `.1` child IDs;
  - title / description / design / acceptance_criteria / notes;
  - `status`: open, in_progress, blocked, deferred, closed, pinned, hooked, plus custom statuses with a category (active/wip/done/frozen);
  - priority 0–4;
  - `issue_type`: bug, feature, task, epic, chore, decision, message, molecule, gate, spike, story, milestone, event, plus custom types;
  - assignee and owner;
  - started_at, closed_at, close_reason, closed_by_session;
  - defer_until and due_at;
  - `metadata` JSON;
  - messaging fields (sender, ephemeral);
  - gate fields (await_type/id, timeout, waiters);
  - molecule fields (mol_type swarm/patrol/work, bonded_from);
  - `work_type` (mutex / open_competition — *declared, but unused beyond validation*);
  - `RowVersion`, an opaque CAS token.

  This is a god-table: messages, agents, events and gates are all issues.
- **Typed dependency edges** (`types.go` ~L1249). The *blocking* types are `blocks`, `parent-child`, `conditional-blocks` (B runs only if A fails) and `waits-for` (a fan-in gate over a spawner's children, with `metadata.gate = all-children | any-children`). The *non-blocking* types are related, discovered-from, replies-to, relates-to, duplicates, supersedes, tracks, until, caused-by, validates, delegated-from, authored-by/assigned-to/approved-by and attests. Cycles are rejected at write time. `external:<project>:<capability>` dependencies resolve at query time against a `provides:` label.
- **Planes.** Versioned tables, plus dolt_ignored clone-local tables: `wisps`, `leases`, the events journal and `local_metadata` (`internal/storage/schema/schema.go` `doltIgnorePatterns`).
- Other entities: **memories** (`bd remember`, injected by `bd prime`), **gates** (human, timer, gh:run, gh:pr, bead), **merge slot** (a mutex bead with holder and waiters in metadata), and **formulas/molecules** (`bd cook`, `bd mol pour|wisp|bond|squash|burn`).

### Orchestration model (ledger semantics)
- **`bd ready`** is a single SQL predicate over a **persisted `is_blocked` projection** (`internal/storage/sqlbuild/ready.go` `BuildReadyWorkWhere`). An issue is ready when:
  - its status is open, or a custom status in the active category;
  - it is not pinned;
  - `is_blocked = 0`;
  - it is not ephemeral;
  - its type is not an infra type (merge-request, gate, molecule, rig, …);
  - `defer_until` has passed and it is not a child of a deferred parent.

  Filters: labels (all/any/exclude/glob/regex), assignee/unassigned, and recursive parent. Sort policy is `hybrid` (issues from the last 48h by priority, then older issues oldest-first), `priority` or `oldest`.
- **`is_blocked` is maintained on write.** Each write computes the affected set (`AffectedByStatusChangeInTx`, `AffectedByDepChangeInTx`, parent-child descendant expansion), then recomputes mark/unmark passes **to a fixpoint** inside the same transaction (`internal/storage/issueops/blocked_state.go` `RecomputeIsBlockedInTxWithResult`). Reads are cheap; writes pay the cost.
- **Claim = CAS** (`internal/storage/issueops/claim.go` `ClaimIssueInTx`):
  1. Read the pre-image inside the transaction.
  2. Decide claimability in Go: unassigned, the same actor (idempotent re-claim, tolerant of different identity spellings), or a **pool alias** from `claim.pools`.
  3. Run `UPDATE … SET assignee=?, status='in_progress', started_at=…, row_lock=<fresh> WHERE id=? AND row_lock=<old> AND status IN (claimable)`.
  4. Zero rows affected is disambiguated into `ErrAlreadyClaimed` or not-claimable, and a lease row is granted.
- **The `row_lock` trick** (`internal/storage/issueops/lease.go` `freshRowLock`). Dolt has no row locks and merges concurrent commits *cell by cell*. A reclaim writing `status` and a close writing `closed_at` would therefore silently merge. So every status- or ownership-mutating path rewrites a random `row_lock` cell to *force* a 1213/1205 serialization conflict. `withRetryTx` then replays it, with backoff from 25ms up to 5s embedded or 15s server, a circuit breaker, and a rule that **an indeterminate commit is never replayed** (`internal/storage/dolt/store.go`). A build-time test (`TestAllIssueRowWritesStampRowLock`) enforces the invariant.
- **Claim-next** (`bd ready --claim`, HTTP `:claimNext`; `ClaimReadyIssueInTx`) scans the *entire* ready set in sort order and tries the CAS on each until one wins. At N agents, everyone walks the same order and collides on the head of the queue.
- **Leases.** Default TTL is 5 minutes. `bd heartbeat` writes **only** the ephemeral `leases` table: no Dolt commit, no history. `bd reclaim --older-than` is a reaper you run from a supervisor, with a grace window of about 2× TTL. It reverts expired claims to open and records a recovery event. Leases are **replica-scoped**: reclaim skips leases granted by another node unless you pass `--any-replica` (`cmd/bd/heartbeat.go`, `cmd/bd/reclaim.go`, `lease.go` `ReclaimExpiredLeasesInTx`).
- **Generalized CAS guards**: `bd update --if-assignee/--if-status` (`internal/storage/issueops/update_cas.go`), `bd unclaim --if-assignee`, and `revision` exposed in the HTTP detail DTO. These came from a hostile review that found "check-then-act on bd assignee/status" to be the most common bug in fleet scripts (`PROPOSAL-cas-conditional-update.md`).
- **No scheduler and no agent registry.** Assignees are plain strings (`docs/multi-agent/coordination.md`). Gates are evaluated by *polling* `bd gate check`.

### State & persistence
- Dolt is the source of truth. In embedded mode every write is a Dolt commit. In server mode auto-commit defaults to **off**, because "firing DOLT_COMMIT after every write under concurrent load causes 'database is read only' errors" (`docs/architecture/dolt.md`). Gas Town's `dolt-storage.md` says the opposite.
- **Sync** runs over Dolt remotes: `refs/dolt/data` on the git origin, or DoltHub, S3 or GCS. JSONL is export-only, because upsert-only import cannot represent deletes (`docs/core-concepts/sync-concepts.md`). Pull auto-resolves only conflicts that are safe to resolve: machine-local metadata, audit-only edges, schema_migrations rows, memory KV, and the **issues table last-writer-wins by `updated_at`** (GH#4698). Anything else goes to `bd conflicts` ours/theirs. Post-merge FK cascade violations are repaired (`store.go` `tryAutoResolveMergeConflicts`, `internal/storage/versioncontrolops/conflicts.go`).
- A schema-skew guard refuses to run a binary older than the DB schema. A remote-migrate gate ensures exactly one clone migrates.
- **Compaction / "memory decay"** (`cmd/bd/compact.go`, `internal/compact/`): Tier 1 summarizes issues closed for at least 30 days with Claude Haiku (`haiku.go`), or through an agent-driven `--analyze/--apply` flow. The original content is discarded. **Tier 2 is "planned, not yet implemented".** Separately, `bd compact --dolt`, `bd prune/purge` and wisp GC reclaim storage.

### Isolation & workspaces
Beads is not a workspace manager. Its isolation features:
- a per-project `.beads/` directory;
- `BEADS_DIR`;
- `--stealth` (no files committed);
- `--contributor` (planning issues routed to a separate repo);
- `--readonly` for worker sandboxes;
- `routes.jsonl` prefix routing and `.beads/redirect` files, which Gas Town uses so every worktree shares one database.

Embedded mode is single-writer via a file lock ("database is locked" → switch to server mode).

### Agent integration
- **Teaching agents.**
  - `bd init` writes a marker-delimited section into AGENTS.md (`internal/templates/agents/defaults/beads-section.md`): use `bd ready`, `--claim`, `discovered-from`, `bd close`, and no markdown TODO lists.
  - `bd setup claude|codex|cursor|gemini|factory|aider|junie|opencode|mux|copilot` installs hooks, skills and plugins (`cmd/bd/setup/`). For Claude, that is a SessionStart hook `bd prime --hook-json`, which also fires after compaction (`source=compact`), so the older PreCompact hooks are migrated away (`cmd/bd/setup/claude.go`).
  - `bd prime` is token-budgeted: about 50 tokens when MCP is active, about 1–2K otherwise. It can be overridden with `.beads/PRIME.md`, supports policy profiles (conservative / minimal / team-maintainer), and injects memories (`cmd/bd/prime.go`).
- **Interfaces.** The CLI with `--json`/`--brief`, typed exit codes for guard mismatches, an MCP server, the HTTP API (`bd serve`), and the Go SDK.
- There is no invocation of agents at all. Beads is passive, and correctness relies on agents following the prose instructions: claim, heartbeat, close.

### Verification & quality gates
- The ledger gates *workflow*, not code: blocking edges, gates (e.g. `gh:run` CI or `gh:pr` merge), `--validate` (description completeness), `bd lint`, `bd stale`, `bd orphans`, epic-closure checks and `bd human`.
- For its own quality it has storage conformance suites (`backend/conformance`), cross-stack parity suites, row-lock invariant tests, and ready-work benchmarks at 10K and 20K issues (`BENCHMARKS.md`).

### Observability & UI
- Per-issue audit events (`bd show`) and Dolt history (`bd history`).
- `bd graph` (dot, mermaid, D3 HTML) and `bd dep tree`.
- The **events journal**, exposed as a paged read plus `GET /v0/beads/events:watch` SSE (`internal/httpapi/events_watch.go`). The server keeps no per-consumer state. Each record carries `id: <seq>`, so the browser's `Last-Event-ID` *is* the `since` cursor and reconnects resume exactly where they left off. There is a hard cap on concurrent streams.
- OTEL traces and metrics on the Dolt paths (`doltTracer`, `doltMetrics`: serialization errors, retries, circuit trips).
- UIs are left to the community (`docs/community-tools.md`).

### Config & extensibility
`.beads/config.yaml` plus `BD_*`/`BEADS_*` env vars. You can define custom statuses (with categories) and custom types, `claim.pools`, `export.auto`, and `agent.profile`. Storage backends are pluggable with a conformance suite (`backend/`, `PROPOSAL-pluggable-storage-backends.md`). There are also formulas, tracker adapters and git hooks.

### Strengths — steal these
- **A persisted readiness projection with incremental fixpoint recompute** makes "what can run now" a cheap indexed query (`blocked_state.go`, `sqlbuild/ready.go`). The **edge vocabulary** is excellent: `waits-for` with all/any fan-in, `conditional-blocks`, and non-blocking `discovered-from`/`tracks`/`supersedes`.
- **CAS claim semantics**: idempotent re-claim by the same actor, pool aliases for dispatcher pre-assignment, and a generalized `--if-assignee/--if-status` plus an opaque revision token (`claim.go`, `update_cas.go`).
- **Leases separated from history.** Heartbeats are cheap and non-versioned, and the reaper has a grace window and replica provenance (`lease.go`, `reclaim.go`).
- **Explicit data planes.** Versioned records vs clone-local ephemeral tables (wisps, leases, journal), selected per record by `StorageClass`.
- **Retry discipline.** Replay only transactions that were provably rolled back; never replay an indeterminate commit (`store.go` `withRetryTx`).
- **Event journal with the cursor as the contract**, SSE implemented as repeated `since` reads, `Last-Event-ID` resume, and a stream cap. This is ideal for a real-time UI.
- **Hash IDs plus hierarchical child IDs** allow coordination-free creation across agents and branches.
- **Agent onboarding tooling**: an idempotent marker section in AGENTS.md, SessionStart `prime` with a token budget, and memories injected at prime time.

### Weaknesses & tradeoffs
- **Dolt fights the workload.** With no row locks, a `row_lock` hack is needed. Commits per write under concurrency lead to read-only mode. The Dolt version is pinned (2.3.0 broke `DOLT_RESET`). The git-remote cache takes about 20 minutes to rebuild. There is heavy server lifecycle machinery (proxy, pidfiles, force-stop of unverifiable processes).
- **Three storage stacks** (embedded / server / proxied), with 59 duplicated command implementations and parity suites. That multiplies maintenance and bug surface; `store.go` alone is 5.3K lines.
- **Claim-next herd.** Every agent scans the same ordering and CASes the head, so contention and retries grow with N. There is no `SKIP LOCKED`, sharding or randomization.
- **Leases are node-local.** Cross-machine liveness is visible only via committed status/assignee, and only the granting node can reap. Nothing forces an agent to actually heartbeat.
- **Schema absorbed orchestrator concepts**: `hooked`, agent/rig/merge-request types, `gt:*` labels, messages, events. The god-table blurs the ledger boundary. `open_competition` is dead weight.
- **Polling-based**: gates, reclaim and journal consumers all poll. There is no push of wake-ups to agents.
- **Lossy compaction** that depends on an LLM; tier 2 is missing. Docs lag the code: `coordination.md` omits leases, heartbeat and reclaim, and the Gas Town and Beads docs contradict each other on auto-commit.

### Implications for a Rust framework
- **Adopt the ledger model**: a typed DAG, a persisted `is_blocked` projection maintained transactionally, CAS claims with a revision token, and leases in a non-historical table plus a reaper. Keep the full edge-type vocabulary.
- **Don't require Dolt.** Use SQLite in WAL mode behind a single orchestrator writer (actor/`tokio::mpsc`), or Postgres with `FOR UPDATE SKIP LOCKED` for claim-next (which removes the herd). Get history from an append-only event log (event sourcing) rather than a versioned database.
- **Interoperate with beads as a `WorkSource`.** Write an adapter over `bd --json` or `bd serve` (`:claimNext`, `:claim`, `events:watch`) so beads users can plug in. Map `discovered-from` to follow-up items the factory files.
- **Orchestrator-assigned work.** Because the Rust orchestrator owns the agent process, it should claim *on behalf of* the agent and heartbeat the lease *from process liveness*. Don't rely on agents following prompts to claim, heartbeat and close.
- **Copy the journal/SSE cursor contract** for the UI: monotonic `seq`, `Last-Event-ID` resume, bounded streams, and paged catch-up reads.

---

## Cross-repo takeaways
1. **Separate the ledger from the executor.** Beads (what/when/who) and Gas Town (where/how) are cleanly split, and Gas Town consumes beads through a CLI plus an SDK. Our Rust factory should define a `WorkSource`/ledger trait with a native store *and* a beads adapter, independent of the execution engine.
2. **Deterministic core, LLM at the edges.** Gas Town's biggest liability is supervisors and the merge queue implemented as LLM checklists (`mol-refinery-patrol`, `mol-witness-patrol`), while the deterministic Go equivalents rot unused. Rust should own liveness, admission, merge queue, retries and recovery as tested state machines, and spawn LLMs only for judgment work items.
3. **What breaks at 20+ agents** (from the incident comments in both codebases):
   - write contention on a shared versioned DB (manifest contention → read-only mode, serialization storms);
   - DB connection exhaustion;
   - head-of-queue claim contention;
   - garbled or lost tmux input under concurrent nudges;
   - zombie processes and servers;
   - supervisor token burn;
   - uncapped spawn defaults.

   Mitigations worth copying: admission control that fails closed, per-role write budgets (nudge vs mail), ephemeral planes, circuit breakers, crash-loop budgets and idle-effort tuning. Better still, avoid the causes: a single-writer orchestrator, structured agent I/O, and owned processes.
4. **Idempotency everywhere.** Checkpointed `gt done`, MR idempotency by branch+commit, CAS claims, never replaying an indeterminate commit, and read-back verification before destructive steps. Every factory transition (claim → workspace → run → submit → merge → cleanup) should be an idempotent, resumable step with a recorded checkpoint.
5. **Reconciliation over tracking.** Both converge on "derive state from reality, plus persisted projections" (`is_blocked`; zombie and stalled states computed from tmux × beads). A Kubernetes-style reconciler loop in Rust (desired state in the ledger, observed state from owned processes and git) is the clean form of this.
6. **Typed protocols.** Replace subject-regex mail, `key: value` descriptions and labels-as-KV with typed events (serde enums) on one journal. Serve the UI from that journal with a cursor/SSE contract like beads' `events:watch`.
