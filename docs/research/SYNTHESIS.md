# What we learned from 30+ agent-orchestration projects

This is the cross-project synthesis behind Forgeline's design. The per-project
notes (with file-level citations into each codebase) are in this folder:

| Notes | Projects |
|---|---|
| [01 — Tracker-driven daemons](01-tracker-driven-daemons.md) | openai/symphony, gannonh/kata-symphony, kunalm2345/openfactory |
| [02 — Platform agents](02-platform-agents.md) | langchain-ai/open-swe, OpenHands (canvas, software-agent-sdk, automation), anthropics/claude-code-action |
| [03 — Swarm coordination](03-swarm-coordination.md) | steveyegge/gastown, steveyegge/beads |
| [04 — Execution & sandboxes](04-execution-and-sandboxes.md) | jrswab/axe, google/ax, agent-substrate/substrate, dagger/container-use, e2b-dev/E2B |
| [05 — Workbenches & UI](05-workbenches-and-ui.md) | BloopAI/vibe-kanban, smtg-ai/claude-squad, imbue-ai/sculptor |
| [06 — Agent harnesses & protocols](06-agent-harnesses-and-protocols.md) | Claude Agent SDK, openai/codex (codex-rs), block/goose, Agent Client Protocol, mini-swe-agent |
| [07 — Process, roles, durability, evals](07-process-roles-evals.md) | github/spec-kit, BMAD-METHOD, MetaGPT, ChatDev, 12-factor-agents, SWE-bench, Temporal |
| [08 — The new wave of small factories](08-new-wave-factories.md) | vanguard, claude-factory, HAR, agentfactory, ready-for-agent, johnplanow/substrate |

All repositories were read at their `HEAD` on 2026-09-24. Nothing was run.

---

## 1. The landscape: five layers, nobody owns all of them

Every project sits in one or two of these layers. The interesting failures come
from projects that stretch one layer to cover another (an LLM checklist
standing in for a scheduler, a prompt standing in for a permission system).

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ UI / operator surface     vibe-kanban, Sculptor, claude-squad, Kata TUI   │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Work ledger               beads, Linear/GitHub (Symphony), spec-kit docs  │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Orchestrator / factory    Symphony, Kata, OpenFactory, Open SWE,          │
 │                           Gas Town, new-wave factories   ← Forgeline      │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Agent harness             Claude Code, Codex, Goose, OpenHands SDK, axe,  │
 │                           mini-swe-agent  (reached via stream-json/ACP/…) │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Sandbox / infrastructure  worktrees, container-use, E2B, AX, Substrate    │
 └──────────────────────────────────────────────────────────────────────────┘
```

Forgeline's job is the orchestrator layer, done properly, with first-class
seams to every other layer: a ledger adapter below the UI, an agent-adapter
layer, and a workspace-backend layer.

---

## 2. Comparison of the most relevant projects

| | Orchestration | Durable state | Isolation | Agent integration | Verification | Live UI feed |
|---|---|---|---|---|---|---|
| **Symphony** | One GenServer owns claims; polls tracker; reconcile before dispatch | None (restart = re-poll tracker) | Per-issue dir on host | Codex app-server JSON-RPC | Prompt-only | Snapshot API, no replay |
| **Kata-Symphony** (Rust) | Actor + durable "factory" stages | SQLite: runs, leased attempts, artifacts, publication intents | Worktree / Docker, workers get no creds | Codex, Pi, Claude | Code-computed gate pinned to reviewed commit | WS: subscribe → snapshot → deltas |
| **OpenFactory** | CAS state machine over SQLite + reconciler | SQLite | Fresh VM per run; second VM for review | Claude Code / Codex CLI | Reviewer in fresh VM, other model family | Dashboard polling |
| **Open SWE** | One `dispatch()` entry, interrupt/queue semantics, reconcile sweep | Postgres transcript + LangGraph checkpoints | Persistent sandbox per thread | Deep Agents (LangGraph) | Review graphs, CI babysitting | SSE, resume by version |
| **OpenHands** | Agent server inside sandbox; event tree | JSON file per event | Local/Docker/VM/cloud via one API | Own loop + ACP to Claude/Codex/Gemini | — | WS, resume by timestamp |
| **Gas Town** | LLM "Mayor"/"Witness" follow markdown checklists | Dolt rows | Worktree per polecat, perms bypassed | Interactive CLIs in tmux, keystroke injection | LLM-run merge ("refinery") | tmux panes |
| **beads** | Ledger only: stored `is_blocked`, CAS claims, leases | Dolt | — | CLI/JSON for any agent | — | SSE events watch with cursor |
| **vibe-kanban** (Rust) | Linked action chain per workspace | sqlx/SQLite | Worktree only, perms bypassed | 9 CLIs → one normalized conversation | — | WS snapshot + JSON Patch (lossy on lag) |
| **Sculptor** | Workspaces + skills pipeline | Event log + `_latest` tables | Shared copy per workspace | Claude Code, Pi; capability flags | CI babysitter | Push-fed query cache |
| **AX / Substrate** | K8s-style reconcilers; actors | Redis / Postgres | gVisor/microVM, snapshot suspend/resume, egress proxy | Runner contract (any image) | — | Watch streams |
| **claude-factory** | Pure `ready_set` scheduler with file footprints | Files | Worktrees | Claude Code subagents | 2 reviewers, capped fix rounds | Kanban file |
| **new-wave (substrate/vanguard/HAR)** | Pipelines of varying rigor | Mixed | Docker / cgroups | Claude Code, Codex | Anti-gaming gates, proof records | Mixed |

---

## 3. Twelve lessons the evidence converges on

**1. Deterministic core, LLMs at the edges.** The strongest projects keep
scheduling, retries, merging and recovery in tested code and call models only
for judgment (planning, implementing, reviewing). Gas Town is the cautionary
tale: its merge queue and supervisors are LLM checklists, while the
deterministic Go versions sit unused (`internal/refinery/batch.go` is only
called from tests). MetaGPT and ChatDev both abandoned role-play chat for
tool-using workers (07). → Forgeline's engine is a Rust state machine; agents
are workers it dispatches.

**2. One owner of state, reconcile before acting.** Symphony (one GenServer),
Kata (one actor), OpenFactory (one CAS state machine) and Open SWE (one
`dispatch()` function plus `reconcile.py`) all converge on a single authority
that re-checks reality (tracker, git, processes) before every dispatch. The
beads/Gas Town incident trail shows what happens with many writers: contention,
read-only mode, zombie re-queues (03).

**3. The event log is the product's spine.** Durable state, crash recovery,
the live UI and evals all want the same thing: an append-only, gapless,
sequenced log with idempotent writes (Open SWE transcripts, beads
`events:watch`, Kata's envelope, 12-factor factors 5 and 12). Clients resume
with "everything after seq N". Lossy broadcasts (vibe-kanban) and timestamp
resumes (OpenHands) are the anti-patterns.

**4. Build our own durability; don't depend on a workflow platform.**
Pipelines are data (YAML), not user code, so Temporal's replay model buys
little and costs a cluster; Restate is BSL; Open SWE's dependence on LangGraph
Platform makes self-hosting a paid feature (02, 07). SQLite (WAL, single
writer) plus leases, timers and idempotency keys covers thousands of runs and
ships in one binary.

**5. Verification must be deterministic, host-run and ungameable.** Agents'
self-reported "tests pass" is unreliable everywhere it was measured (08).
Substrate reads gate config from the *trusted base tree* and flags `|| true`,
edited tests and tampered acceptance specs; Kata computes the gate in code and
pins it to the reviewed commit; vanguard puts a hash of host-run output in the
PR. → A `ProofRecord { tree_hash, command, exit_code, output_hash, commit }` is
the only thing that can mark work done.

**6. Review in a fresh context, with typed triage and capped loops.** Reviewers
that see the implementer's reasoning rubber-stamp it. OpenFactory reviews in a
second VM with a different model family; BMAD runs parallel restricted-context
lenses (blind diff, edge cases, verification gap, intent vs diff) and triages
each finding into `patch / bad_spec / intent_gap / defer / reject`;
claude-factory unions two reviewers' blocking findings and counts *fix rounds,
not findings*. Every loop is capped and ends in a typed state.

**7. Agents produce artifacts; a trusted publisher applies effects.** Workers
should hold no forge or tracker credentials (Kata `build_isolated_env`,
Symphony's host-side tools). Agents emit validated results; a credentialed
publisher applies pushes, PRs and comments idempotently from an outbox (Kata
ADR-0004/0005). Prompts saying "do not push" while the token sits in the
sandbox (OpenFactory) are not a control.

**8. One workspace per unit of parallel work, and code leaves only through
git.** Parallel sub-agents sharing a working tree (Open SWE `task`, OpenHands
delegation, Sculptor) cause the collisions everyone reports. container-use
commits every step to a factory-owned branch in a bare mirror; OpenFactory
moves work between VMs as git bundles pinned to a SHA (04).

**9. Structured agent I/O, not terminal scraping.** Gas Town's 4.4k-line tmux
wrapper and claude-squad's screen hashing are what you get from driving
interactive TUIs. vibe-kanban shows the better path: protocol drivers
(stream-json, JSON-RPC, ACP, HTTP+SSE) → per-agent adapters → one normalized,
typed event model, plus a per-harness *capabilities* struct (Sculptor) so the
UI shows only what an agent supports.

**10. Sandboxes fail closed, secrets stay outside.** Almost every project
defaults to `--dangerously-skip-permissions` in a bare worktree. The good
ideas: a backend reports which policy rules it actually *enforces* and the
attempt refuses to start otherwise (04); secrets as references injected by an
egress proxy (Substrate, E2B, vanguard's nonce proxy); a cgroup per agent turn
(ready-for-agent); dropping agent writes to `.github/workflows`.

**11. The scheduler must keep agents off each other.** Dependency DAG plus
predicted file footprints ("unknown footprint ⇒ run alone", claude-factory
`ready_set.py`), leased file reservations, conflict prediction against open
PRs (agentfactory), a serial rebase → re-test → merge queue, and re-checking
verdicts made stale by a concurrent merge (substrate).

**12. Humans are reached through durable, typed requests.** Approvals and
questions are first-class objects with ids, timeouts, notifications and deep
links (vibe-kanban approvals, Sculptor's MCP question tools, 12-factor factor
7). The operator UI is an *attention inbox* (waiting → error → unread →
running), not a list of terminals.

---

## 4. Pitfalls we saw repeatedly

- **Workflow correctness living in a prompt** (Symphony's 300-line workflow
  prompt, OpenFactory's "Do NOT push", spec-kit's prompt-enforced gates).
- **Blocking I/O or unbounded buffers in the orchestrator loop** (Symphony's
  inline tracker HTTP; Kata's `block_in_place`, unbounded mpsc and ever-growing
  event vector; OpenFactory's sequential engine).
- **Durability bolted on later as a second plane** (Kata's legacy in-memory
  loop next to its SQLite factory).
- **Auto-approving permission prompts** (OpenHands ACP, claude-squad
  "auto-yes", most new-wave factories).
- **Status that depends on the model calling a reporting tool** or on
  in-process `finally` blocks (claude-code-action's spinner that never stops).
- **Docs that over-claim**: unwired bandit routing, merge queues that moved to
  closed binaries, "self-improving" meaning advisory text (08); doc/code drift
  in Symphony, Kata and Gas Town. → Capabilities must be introspectable and
  tested end to end.
- **Tool and version drift**: stale agent CLIs inside sandboxes, silently
  ignored flags. → Pin agent versions per adapter and run a `doctor` preflight.

---

## 5. Decisions this leads to

| Question | Decision | Why (lessons) |
|---|---|---|
| Language/runtime | Rust, tokio, single binary | Kata and vibe-kanban prove the stack; embeddable, fast, safe concurrency |
| Durability | Own event-sourced engine on SQLite (WAL), storage trait for Postgres later | 3, 4 |
| Scheduling | Pure `decide(state) → commands` core; DAG + footprints; bounded concurrency | 1, 2, 11 |
| Agents | Layered adapters → normalized `AgentEvent`; Claude Code stream-json and ACP first | 9 |
| Workspaces | `WorkspaceBackend`/`Workspace` traits with capabilities + fail-closed policy; git worktree first | 8, 10 |
| Verification | Deterministic checks from the trusted base config, `ProofRecord`s, fresh-context reviewers with typed triage, capped loops | 5, 6 |
| Side effects | Agents never hold forge credentials; publisher applies effects from an outbox with idempotency keys | 7 |
| UI contract | Sequenced event stream (SSE/WS) + REST snapshots + generated TS types; attention inbox | 3, 12 |
| Process | Default pipeline triage → spec → plan → implement → verify → review → converge → PR; all configurable, with invariants | 6, 07 |
| Measurement | Built-in "factory bench" replaying merged PRs from the user's own repos | 07 |

The full design is in [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md) and the
build order in [`docs/ROADMAP.md`](../ROADMAP.md).
