# Bodega roadmap

Each milestone ends with something you can run end to end. Exit criteria are
written as tests or demos, not feelings.

## M0 — Foundations ✅

- Research of 30+ projects ([`research/`](research/SYNTHESIS.md)) and the
  architecture ([`ARCHITECTURE.md`](ARCHITECTURE.md)).
- `bodega-core`: ids, domain model, events, `RunState` fold, dependency
  graph with cycle detection, budgets.
- `bodega-store`: SQLite event log with gapless `seq`, validation inside the
  write transaction, idempotency keys, run summaries, live subscriptions.
- `bodega-workspace`: git layer (worktrees, commits, diffs, merges with
  conflict reporting).

## M1 — First vertical slice: plan → parallel agents → verified integration ✅

Built as described below, plus a server (from M2): `bodega serve` exposes
REST snapshots, a resumable SSE stream, approvals and token auth, with
TypeScript types generated from the Rust types. 65 tests; the exit test lives
in `crates/bodega-engine/tests/engine.rs`. Not yet verified: a real Claude
Code run end to end (the adapter is tested against a protocol-faithful fake).

- `bodega-agents`: `AgentRuntime`/`AgentSession` traits; `mock` adapter
  (scripted file edits, for tests and demos); `claude-code` adapter
  (stream-json).
- `bodega-config`: `bodega.toml` (agents per role, checks, limits).
- `bodega-engine`: per-run actor; plan from a file (or a single task from
  the request); tasks dispatched in dependency order under concurrency limits;
  one worktree and branch per attempt; checks run after each attempt;
  failures retried with the check output as feedback; verified branches merged
  one at a time into the run's integration branch; conflicts retried on the new
  head; budgets enforced.
- `bodega-cli`: `init`, `run`, `runs`, `show`, `events --follow`, `doctor`.

**Exit**: an integration test runs a 4-task plan with a diamond dependency
using the mock agent against a real git repo, including one task whose first
attempt fails its checks and succeeds on retry, and ends with an integration
branch containing all four changes. The same flow works with `--agent
claude-code` on a real repository.

## M2 — API, planning, review, durability

- ~~`bodega-server`: REST snapshots + SSE stream with resume by `seq`,
  approvals endpoint, generated TypeScript types with a CI drift check.~~ (done)
- Planner stage: an agent writes `plan.json` (validated as a DAG); optional
  human plan approval.
- Reviewer stage: fresh-context review (optionally another runtime/model),
  typed findings, capped fix rounds.
- Leases, durable timers and crash recovery (resume agent sessions, restart
  attempts from the last commit).
- Publisher + outbox: push the integration branch and open a PR (`gh`), keyed
  idempotently by branch.
- `codex` adapter (`exec --json`).

**Exit**: kill -9 the server mid-run; on restart the run finishes. A request
typed into the API ends as a PR whose body includes the plan, the check results
and the review log.

## M3 — The UI and real isolation

- Web UI (React or Svelte, served by the binary): attention inbox, run DAG
  view, live agent transcripts, diff review with inline comments sent back to
  the agent, approvals, swarm timeline, budget gauges.
- OS sandbox for worktrees (Landlock/bubblewrap on Linux, Seatbelt on macOS)
  with fail-closed policy reports; Docker/devcontainer backend; secrets injected
  outside the sandbox.
- `acp` adapter (Gemini CLI, Goose, OpenCode, …).
- Work sources: GitHub issues (label-driven), then Linear and beads.
- Notifications (desktop, Slack webhook) for approvals and failures.

**Exit**: supervise 10+ concurrent agents across 2 runs from the UI; an agent
cannot read files outside its workspace or reach hosts outside its allowlist.

## M4 — Trustworthy at scale

- Proof records bound to tree hashes; anti-gaming checks (edited tests,
  forced passes, tampered acceptance specs); checks read from the base ref.
- Review lenses in parallel; converge stage mapping acceptance criteria to
  evidence.
- Footprint prediction and file reservations; conflict prediction against
  open PRs; best-of-N attempts with side-by-side comparison.
- `bodega bench`: mine merged PRs from your repos into SWE-bench-style
  tasks and measure resolve rate, regressions and cost per change.
- Postgres storage backend; OpenTelemetry export; E2B backend.

**Exit**: a public dashboard of bench results across agents and models on a
set of open-source repositories.

## M5 — Everywhere

- Bodega as an ACP agent and an MCP server, so editors and other agents can
  start and steer runs.
- Distributed workers (Agent Substrate / AX / Kubernetes) for large swarms.
- Multi-repo runs and stacked PRs.

---

## How to contribute to a milestone

Open an issue that names the milestone and the exit criterion it moves
forward. Every behavior change comes with a test; engine scheduling changes
come with a test of the pure `decide` function.
