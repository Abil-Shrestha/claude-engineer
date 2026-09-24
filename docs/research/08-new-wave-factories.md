# 08 — New wave of small factories (2026)

Repos studied (shallow clones read September 2026): `SebaBoler/vanguard`, `jchandra74/claude-factory`, `os-factory/har`, `supaku/agentfactory` (now branded "Donmai"), `berenddeboer/ready-for-agent`, `johnplanow/substrate`. File paths below are relative to each repo root.

## Group overview

These six 2026 projects all use the same basic pipeline: a tracker label or state marks work as ready, the tool claims it, creates one git worktree per item, runs a headless CLI agent (almost always `claude -p --output-format stream-json --verbose`), reviews the result and opens a PR. Where they differ is **what they trust**. Vanguard trusts a Docker sandbox, a host that owns every file sync, and a hash of the verification output that the host computes. Claude-factory trusts a Python scheduler and Claude Code hooks, and everything runs inside one Claude Code session. HAR doesn't orchestrate agents at all. It gives them a "jig" (an isolated slot with its own ports and database), records each validation under a hash of the exact file tree, and enforces that with a git commit gate. Agentfactory is a Redis-backed fleet runtime driven by Linear status changes. It has completion contracts and deterministic backstops, and it has bandit routing, but the routing is only half wired. Ready-for-agent is the most rigorously engineered of the group: its lifecycle state machine is generated from an ontology, each agent turn runs in its own cgroup, it has a three-state merge policy, and it has more than 70 ADRs. Substrate has the most gates. It hardens verification against reward hacking and adds an automated "experimenter" for self-improvement. Several ideas show up across the group. A deterministic host makes the decisions and the LLM only proposes. Agents report outcomes as machine-readable result lines. Every loop is bounded and ends in `needs_human`/`parked`. And the "tests pass" claims agents make about their own work are routinely distrusted. Almost all of them over-claim in their READMEs. Agentfactory's bandit routing is not wired into dispatch, and its merge queue was moved out of the repo. Vanguard's "self-improving" means an advisory digest plus eval drafts that a human curates. Substrate's experimenter works by appending HTML-comment directives to prompt files.

---

## 1. SebaBoler/vanguard: sandboxed Claude Code with host-owned proof

**What it is.** TypeScript (Node 24, pnpm, strict ESM) with about 31k lines of source and 25k lines of tests. It also includes a Tauri desktop app: about 3k lines of Rust in `apps/desktop/src-tauri/src/*.rs` and about 6k lines of TSX. It is very active: the last commit was 2026-09-23 and the latest PR is #396. MIT. Its stated philosophy is "harness over code": every agent failure counts as a harness failure, so you fix the prompt, skill, tool or sandbox limit rather than the agent's output.

**Architecture & pipeline.** A `TaskFetcher` pulls from GitHub Issues/Projects v2, GitLab or Linear (Linear goes through the `linear` CLI, see `src/tasks/linear-cli.ts`). The `watch` loop (`src/runners/watch.ts`) runs two passes, keyed on labels (GitHub) or states (Linear):
1. **Spec pass.** A deterministic triage runs first. If the ticket passes, `techSpecStage` runs read-only, posts a `<tech_spec>` plus a `<spec_manifest>` JSON block as a comment, and relabels the ticket `ready for agent`. The human gets one poll interval to review the spec.
2. **Agent pass.** Triage runs again, then Implementer → Reviewer → (optional) Simplifier → (optional) Conformance → gate/repair loop → secret scan → commit → draft PR.

Stages are `PipelineStage` records (`src/pipeline/pipeline.ts`). Each one sets model, effort, maxTurns, provider, a fallback provider, `copyBack` (false for read-only stages), `resumeUntilComplete`, `stageCostFraction`/`stageCostFloorUsd`, `onStageBudgetExceeded` and `timeoutMs`. You can also declare flows as HCL (`src/flows/parse.ts`, `src/flows/flow-b.hcl`, for example plan on opus → implement on sonnet → adversary on opus → repair on sonnet) and edit them in the desktop app's visual editor (`apps/desktop/src/features/workflow/FlowCanvas.tsx`). PRs have their own triggers: `ready for vanguard review` starts a read-only adversarial review, and `needs revision` makes vanguard fix its own draft based on your comments.

**Agent integration.** `src/agents/claude-code.ts` builds the Claude invocation. It runs *inside the sandbox* with the prompt on stdin:
`claude --print --output-format stream-json --verbose --permission-mode bypassPermissions [--effort E] [--max-turns N] [--max-budget-usd X] [--resume ID] [--fork-session] [--append-system-prompt S] [--mcp-config F --strict-mcp-config] [--allowed-tools …] [--model M]`.
Codex is invoked as `codex exec --json --sandbox danger-full-access -m M` (`src/agents/codex.ts`). Cursor, z.ai, OpenRouter, Meridian and custom providers are also supported (`src/agents/registry.ts`). Stages signal completion with `<promise>COMPLETE</promise>`. The stream parser in `src/agents/claude-stream.ts` handles two cases well:
- **Synthetic-message detection.** If every assistant message carries the model `<synthetic>`, the CLI never reached the model and invented the text itself (an API or gateway error). The parser fails loudly instead of spending resume budget replaying the same error.
- **Truncated-stream salvage.** If there is no `result` event but the exit code was 0 and there were real turns and a session id, the stage becomes `incomplete` and resumable instead of crashing.

Session JSONL files are copied out of the sandbox to the host and restored later so runs can resume or fork with a warm cache (`src/agents/session-store.ts`). The README claims 97–99% cache efficiency, and `cacheEfficiency` is recorded on every `RunResult`.

**State, isolation & merging.**
- **Worktree and sandbox.** There is one host git worktree per task (`src/worktree/manager.ts`). The Docker sandbox (Firecracker on KVM hosts) receives a *copy* of it (`copyIn`). `seedSandboxGit` in `src/core/vanguard.ts` builds a throwaway git repo inside the sandbox so the agent's own `git diff` works. Without it, weaker models "confabulate completion".
- **Copy-back.** The host copies changes back through a skip regex, and it **hard-drops any write under `.github/workflows/`**. That closes the path where a prompt-injected agent writes a workflow that later runs with repo secrets (`WORKFLOW_PATH` in `src/core/vanguard.ts`).
- **Secrets.** Secrets reach the sandbox through a tmpfs file, never env or argv (`src/sandbox/docker.ts`). With `--llm-proxy`, the sandbox only gets a per-run nonce. A host sidecar holds the real Anthropic, OpenAI or z.ai key and swaps it in (`src/sandbox/llm-proxy-server.mjs`), and the provider hosts are dropped from the egress allowlist (`src/sandbox/egress-proxy.ts`).
- **Concurrency.** The sandbox cap is derived from RAM: `totalmem / 2 GB / 2` (`src/core/concurrency.ts`).
- **Claims and merging.** A claim is a label swap (`vanguard:running`) or a Linear state change. Vanguard never merges; every run ends as a draft PR.

**Verification & gates.**
- **Triage** (`src/tasks/triage.ts`). A deterministic gate that spends no model budget. The description must be at least 30 characters, and agent mode also needs either real Acceptance-Criteria bullets (the issue template's placeholder bullets are explicitly rejected) or a spec comment. Otherwise the ticket goes to `needs info`.
- **Conformance gate** (`src/pipeline/conformance-gate.ts`). The planner's `spec_manifest` declares required files, tests, acceptance artifacts and producer→consumer dependencies. The host checks the diff against it with no LLM. It detects new test content by regex for JS, Python, Go and Rust, and flags dangling consumers.
- **Proof of Work** (`src/pipeline/verify.ts`). The *host* runs the verify command inside the sandbox and computes SHA-256 over stdout and stderr. It stamps a PASS or FAIL block (command, exit code, hash, output tail) into the PR body. If no command resolves, it renders a visible **SKIPPED** block instead of dropping it silently. The command is auto-detected from `package.json` or pytest markers. Visual Proof (`src/pipeline/visual-proof.ts`) hashes a manifest of screenshots and other artifacts.
- **Gate loop** (`src/runners/source-adapter.ts`, around line 390). The gate passes only if conformance passes, verification passes, *and* the implementer reported completion. On failure, the loop resumes the implementer session with a minimal "failing witness" (the code comments call it CEGIS-style), up to N times. If the gate still fails, the PR is **downgraded to `Part of #N`** with a delivered-vs-deferred checklist (`src/runners/review-body.ts`), so an unverifiable "done" never auto-closes the issue.
- **Guardrails.** A secret scan blocks publishing (`src/core/secret-scan.ts`). `runJudgedRepair` freezes the run to `needs_human` after 3 judge rejections and leaves the sandbox alive with a `shellCommand` for the operator (`src/pipeline/judged-repair.ts`). Budget exhaustion freezes to `budget_exceeded`, and the run can resume with a higher cap. Per-stage caps are stored as *fractions*, so they re-derive automatically after a raise.

**UI/observability.** The Tauri app (`apps/desktop`) has a board, fleet, dashboard, run inspector (stages, diff, proof gate, transcript) and flow editor. Its Rust side (`apps/desktop/src-tauri/src/sidecar.rs`) talks NDJSON JSON-RPC to a `vanguard __sidecar` child over **two pipes**: a run pipe held for the whole run and a query pipe for short calls. It buffers the last 4 runs' events (2,000 max each) so the UI can re-attach. Metrics are `run_complete` lines in `.vanguard/runs/metrics.jsonl`. The Claude adapter parses output only after `sandbox.exec` resolves, so the UI gets stage-level events, **not live turns**.

**Novel ideas — steal these.**
- **Host-attested proof blocks, including SKIPPED** (`src/pipeline/verify.ts`).
- **A planner-emitted, machine-checkable spec manifest checked deterministically against the diff**, with a `Part of #N` downgrade when the gate fails (`conformance-gate.ts`, `review-body.ts`).
- **Stream-parse robustness:** synthetic-message detection and truncated-stream salvage (`claude-stream.ts`).
- **Nonce LLM proxy plus workflow-write drop** as containment for a hijacked agent.
- **Deterministic retrospective memory.** A redacted digest of failed runs, failed proofs and reviewer notes is injected as advisory context (`src/core/retrospective-memory.ts`). `vanguard eval-suggest` drafts eval cases from those failures but *refuses to write* the eval corpus, so a human curates them (`src/cli/eval-suggest.ts`). In code, this pair is what "fix the harness" means.
- **Fork-and-select:** N implementer variants forked from the same session, with the diffs scored by a one-shot judge run in `/tmp` so it can't touch the worktree (`src/pipeline/fork-select.ts`, `sandboxComplete`).

**Weaknesses & tradeoffs.**
- "Self-improving" is advisory text plus drafts a human curates. Nothing modifies the harness automatically.
- Label-swap claims can race if two watchers run.
- Copying the whole tree in and out costs time and disk on big repos, and the reseeded git in the sandbox hides history from the agent.
- The proof hash is posted by the same host that computes it, and it isn't bound to a tree or commit SHA. Compare HAR.
- Judges and fork scoring reuse the same provider, with a single sample (n=1).
- Stage models, efforts and budgets are hand-tuned presets (`fastStages`, `planImplementReviewStages`); there is no data-driven routing.

**Implications for a Rust framework.**
- Build a `Sandbox` trait with `copy_in`, `copy_out`, a *streaming* `exec` (tokio process plus line codec) and `shell_command()` for frozen runs.
- Define a `ProofRecord { cmd, exit, sha256_output, tree_hash, commit }`.
- Model the gate loop as an explicit state machine: `Gate{conformance, verify, completed}` → `Repair(n)` → `PartialScope`.
- The nonce proxy is a small axum/hyper sidecar.
- Size the semaphore from RAM.
- Put synthetic-message and truncation detection into the Claude stream parser from day one.

---

## 2. jchandra74/claude-factory: the whole factory as a Claude Code plugin

**What it is.** A Claude Code plugin with about 970 lines of code: seven Python scripts (stdlib only) plus bash hooks. The bulk is about 2.3k lines of Markdown skills and agent definitions. It is at v0.2.0, and the last commit was 2026-07-11. The factory built its own 0.2 release in one unattended "shift" (6 of 6 issues done, tag `shift-1`).

**Architecture & pipeline.** The orchestrator is a Claude Code session running `skills/factory-run/SKILL.md`, which is PM, model router and scheduler in one and "never codes". Tickets are Markdown files in `docs/issues/` with YAML frontmatter: `status, blocked_by, footprint (file globs), complexity (mechanical|standard|architectural), lane, agent, tier, fix_rounds, escalated, worktree, branch, parked_reason` (for example `docs/issues/002-tactical-delegation.md`). The loop:
1. `ready_set.py` computes which tickets can be dispatched.
2. All ready implementers are dispatched as parallel Task subagents in a single message.
3. Each implementer returns a compact **result contract**.
4. Two independent reviewers run.
5. Their blocking findings are unioned.
6. Fix rounds run, with an escalation ladder.
7. The merge queue is serialized.
8. The worktree is torn down.
9. The ready set is recomputed.

Planning happens upstream with mattpocock/skills (`/grill-with-docs` → `/to-spec`), then `factory-to-issues` adds the footprint and complexity fields.

**Agent integration.** Implementers are Claude Code subagents in `agents/`: `factory-implementer-light` (haiku), `factory-implementer` (sonnet) and `-heavy` (opus). `factory-reviewer` runs on opus with fresh context. A Codex lane goes through the `codex-plugin-cc` "codex-delegate" skill, with a one-time probe at line start and a run-wide fallback to Claude. Workers may spawn depth-1 helpers (`factory-explore-helper` read-only, `factory-grunt-helper`), and depth is limited *mechanically*: the helpers' tool allowlists exclude `Agent`. Every worker returns a 7-line contract, `TICKET/STATUS/FILES_TOUCHED/TESTS/FINDINGS/DELEGATIONS/NOTE`, so diffs and logs never enter the hub's context.

**State, isolation & merging.**
- **State.** The ticket files are the database. The orchestrator is the *single writer*, and it changes state only through `skills/factory-run/scripts/ticket_update.py`, which also re-renders the board.
- **Scheduling.** `skills/factory-run/scripts/ready_set.py` is a pure function. It takes the `blocked_by` DAG, checks **footprint overlap** (`footprints_overlap` in `ticket_store.py`: glob prefixes compared per path component, so `src/Api` ≠ `src/ApiClient`, and an *empty footprint conflicts with everything*) and applies `max_parallel`. It emits `terminal`, `all_done`, `stalled` (a dependency cycle or a missing blocker id) and deadlock status (a parked transitive blocker).
- **Isolation.** Each ticket gets a worktree on `factory/tk-<id>`, branched off a `working_branch` that is never main.
- **Merging.** Merges are strictly serial: rebase → rerun verify → `merge --no-ff` → push the working branch → remove the worktree. A rebase conflict or a post-rebase test failure costs one fix round.

**Verification & gates.**
- **Review gauntlet.** Pass A is a fresh-context reviewer that **re-runs the verify commands itself**. Pass B is an adversarial second model: Codex, or a second Claude with an "assume the first reviewer missed something" charter. Findings are labelled `blocking:` or `note:`. The merge gates on the **union** of blocking findings, and any number of findings costs exactly **one** fix round, after which the *full* gauntlet re-runs.
- **Escalation ladder.** After 2 fix rounds the ticket moves up one tier and gets one more round. If that fails it is parked, keeping its worktree for a human.
- **Guardrails** (`hooks/git-guardrails.sh`, a PreToolUse hook). It blocks force-push, `reset --hard`, `clean -f`, `branch -D` and `worktree remove --force` unless the command is prefixed `FACTORY_ORCHESTRATOR=1`. It also blocks every push *except* exactly `git push origin <working_branch>` as named in the active run marker.

**UI/observability.**
- **Board.** `.factory/board.html` is a static kanban with a 10-second meta refresh. `docs/issues/BOARD.md` is committed on every transition (`chore(board): #id from -> to`), so remote viewers can watch progress on GitHub.
- **Loop pump.** The Stop hook (`hooks/factory_stop_hook.py`) returns exit 2 to *block the session from stopping* while `.factory/run.json` says `running`, re-running `ready_set.py` and feeding a flag line back to the model. When the board is terminal, stalled or at the iteration cap, it blocks once more to demand the shutdown ritual (standup report, push notification), then sets `closing`. It fails open on any error, and deleting the marker is the kill switch.

**Novel ideas — steal these.**
- **Predicted footprints as the parallelism signal**, with "unknown = serial" (`ready_set.py`).
- **Termination computed, never reasoned.** The scheduler, not the LLM, decides the line is done, and it detects cycles and deadlocks.
- **Counting fix rounds, not findings**, a union of blocking findings across two reviewers, and a full re-review after every fix.
- **A tier-escalation ladder before parking.**
- **A board as a git-committed projection**, so the remote kanban costs nothing to host.
- **Push permission scoped to a run marker** instead of an allow/deny toggle.
- **A thin-hub contract** so orchestrator context survives long runs and `--resume`.

**Weaknesses & tradeoffs.**
- Everything lives in one Claude Code session: there's no process supervision, no cost tracking (on the roadmap), and compaction is a risk.
- Most of the protocol is prose the model is asked to obey.
- Regex guardrails are easy to bypass (`git -c`, aliases, scripts).
- The footprint heuristic misses semantic conflicts.
- The tracker is local files only, and one commit per transition makes history noisy.
- Docs and code disagree. The README says the reviewer always runs a higher tier, but the heavy implementer and the reviewer are both opus. `config.py` routes mechanical tickets to the light agent with a `sonnet` model override, while that agent's frontmatter and the 0.1.0 changelog say haiku.

**Implications for a Rust framework.**
- Implement `ready_set` as a pure, property-tested function over `(DAG, footprints, in_flight, capacity)` that returns `Ready/Deferred/Blocked/Terminal/Stalled`.
- Require a typed worker result contract.
- Make escalation ladders and review-union policies configuration.
- Offer a "git-committed board projection" as one UI sink.
- Keep the kill-switch-by-marker pattern for unattended mode.

---

## 3. os-factory/har: the harness as a jig, with content-addressed proof

**What it is.** A TypeScript CLI and MCP server (`@osfactory/har`) with about 65k lines of source, including "Mission Control" in `control/` (Next.js, Prisma and SQLite), and about 24k lines of tests. Apache-2.0, sponsored by Kerno. It is very active: release 1.14.3 went out on 2026-09-24. It is **not an orchestrator**. It is the contract and environment that agents, or other orchestrators, call into.

**Architecture & pipeline.**
- **The `.har/` contract.** The repo carries `harness.env` (schema-validated), `stages.json` plus `.har/stages/*.sh`, hooks and plugins. Stage kinds are `setup | launch | verify | test | inspect | reset | teardown | custom`.
- **Lifecycle.** Discover → `har env launch <slot>` → the agent builds → `har env verify <slot> [--full]` → `har env complete <slot>`, which reuses the last passing full validation, tears the slot down and *keeps the branch*.
- **Plugins** register verification stages (Playwright, RocketSim, Kerno).
- **Factory lines** (`docs/.../guides/factory-lines.md`, `src/core/lines.ts`, `src/harness/lines.ts`) are installable manifests of *stations*, each with skills, required MCP servers and a gate. They include a `traveler` card, and `handoff.autonomousShip` must be false.

**Agent integration.** Agents call `har` through the CLI or through MCP tools (`src/mcp/server.ts`: `har_launch_environment`, `har_run_verification`, `har_complete_environment`, `har_run_line_gate`, `har_get_logs`, `har_list_artifacts`, and others). `har agents install` writes workflows (`/har-wt`, `/setup-har`, `/har-maintain`, `/factory-line`) into `.claude/skills/`, `.cursor/commands/` and `~/.codex/prompts/`. An optional Claude **worktree guard**, a PreToolUse hook on `Edit|Write|MultiEdit|NotebookEdit`, blocks edits in the main checkout (`src/core/claude-hooks.ts`). `--no-worktree` records `mode: external`, so HAR can run inside worktrees that Conductor or any other orchestrator created.

**State, isolation & merging.**
- **Slots.** A slot is a numbered, reusable lane, and "**occupied slots always block**" (`src/core/slot-launch-guard-occupied.ts`). Each launch creates a fresh branch and worktree, `.env.agent.<id>` and `.har/slots/agent-<id>.json`.
- **Per-slot ports** are allocated in steps (`HARNESS_PORT_STEP`, `src/core/slot-ports.ts`).
- **Per-slot database.** With shared Postgres, a template database is seeded once and *cloned* per slot. File-backed databases migrate per slot (`src/runtime/slot-database.ts`).
- **Evidence** lives under `.har/runs/…`, `.har/validations/<treeHash>.json`, work-units, work-attempts and validation-bindings, written atomically via temp file and rename (`src/core/work-units.ts`).
- **No merging.** Agents hand off a branch plus evidence, and a human ships. The blog calls this the "andon cord".

**Verification & gates.**
- **Content-addressed validation.** `computeWorktreeSnapshot` (`src/core/change-batch.ts`) points a temporary `GIT_INDEX_FILE` at `read-tree HEAD`, runs `add -A`, then `write-tree`. That hashes the *entire working state*, including untracked files and deletions, without touching the real index, and the hash is byte-comparable with the staged tree at commit time.
- **Commit gate** (`src/core/hooks.ts`). Managed pre-commit and post-commit blocks compare the staged tree hash against a passing full validation (`block` or `warn` mode; `worktrees` or `all` scope). Post-commit binds the commit SHA to that validation (`src/core/commit-bindings.ts`). `har env complete` refuses if the tree changed after the last full verify.
- **Cumulative line gate.** `cumulativeGateStages` treats a stage tagged `fromStation: X` as required from X onward, so the gate is a **ratchet** and QA checks are never removed.
- **Poka-yoke.** A line can never add stages to routine `verificationStages`, and `doctor` fails if one does.
- **Drift.** `src/harness/drift.ts` compares the installed harness against template checksums to catch harness rot.

**UI/observability.**
- **Mission Control** (`control/`) is backed by SQLite through Prisma. Its models include `AgentSlot`, `AgentSessionEvent/Span/Usage`, `AgentTrajectoryRecord`, `Run`, `WorkUnit`, `WorkAttempt`, `ValidationBinding` and `ChangeBatch`, and its views include a trajectory viewer, slot timeline, worktree grid and change-batch diff.
- **Telemetry.** Agent hooks export OTEL through a pinned `@osfactory/otel-hook` to Mission Control's ingest (`control/src/server/otel-ingest.ts`). The hooks run under strict **budgets**: 10-second hook timeout, 1.5-second export and 0.5-second flush, "so telemetry never costs a turn" (`src/core/otel-hooks.ts`). Content capture can be omitted for privacy.
- **Usage** is harvested from `~/.claude/projects` and the Codex session files (`src/core/usage-harvest/`).

**Novel ideas — steal these.**
- **Validation keyed by tree hash, plus a commit gate and a commit binding.** This is real proof-of-work tied to exact bytes.
- **Slots with a port block and a cloned database per agent**, which is what lets N agents run a full stack at once.
- **Cumulative ratchet gates**, grown from the questions previous gates missed. The blog post `docs/src/content/blog/the-factory-line.md` explains this candidly.
- **Hook budget discipline** for telemetry.
- **An external-worktree mode**, so a harness composes with other orchestrators.
- **Harness drift detection.**

**Weaknesses & tradeoffs.**
- No scheduling, spawning or agent supervision. Correct behaviour depends on the agent following the skill.
- Evidence is local JSON under the same user, so an agent could forge it. The gate is a git hook, bypassable with `HAR_SKIP_GATE=1` or `--no-verify`.
- `RemoteExecutor` is a stub (`src/core/cloud-executor.ts`).
- There is heavy migration and profile churn (`src/harness/migrations.ts` is 727 lines).
- The blog's "exit 86 re-entry guard" can no longer be found in `src/`; the shims it protected were retired (#314).

**Implications for a Rust framework.**
- Implement the tree snapshot with `gix`/`git2` and a temporary index. `Validation{tree_hash, stage_results, ts}` should be the unit that gates merges.
- Build a slot allocator for ports and databases (Postgres `CREATE DATABASE … TEMPLATE`).
- Store gate ratchets as data.
- Expose an MCP server so agents can self-serve launch and verify.
- Ingest OTEL (tonic/axum with opentelemetry-proto), and publish a hook budget contract.

---

## 4. supaku/agentfactory (Donmai): a Linear-driven fleet runtime

**What it is.** A pnpm/turbo TypeScript monorepo with about 119k lines of source and 89k lines of tests across 13 packages: `core` 48k, `server` 18k, `cli` 10k, `linear` 10k, `nextjs` 8k, `code-intelligence` 6.6k, `architectural-intelligence` 7k and a small `dashboard`. The last commit was 2026-09-11 (#213). MIT. It is **mid-migration**: the Linear CLI, the daemon, `arch` and the merge queue are moving to a Go `af`/`rensei` binary that is *not in this repo* (`docs/migration-from-legacy-cli.md`, CHANGELOG deprecations).

**Architecture & pipeline.**
- **Linear status drives the work type:** Backlog→`development`, Started→`inflight`, Finished→`qa`, Delivered→`acceptance`, Rejected→`refinement`. There are also `research`, `backlog-creation`, `merge` and `security`, plus `*-coordination` variants for parent issues with sub-issues.
- **Webhooks and queue.** A Next.js webhook server (`packages/nextjs`) receives Linear events and enqueues work in Redis. Workers claim it (`packages/server/src/work-queue.ts`, `issue-lock.ts`, `session-heartbeat.ts`, `patrol-loop.ts`, `orphan-cleanup.ts`).
- **Governor.** A pure **decision engine** decides actions per issue (`packages/core/src/governor/decision-engine.ts`): terminal statuses, holds, cooldowns, a circuit breaker at `MAX_SESSION_ATTEMPTS = 3`, and top-of-funnel gating (research and backlog creation must finish before development).
- **Orchestrator size cap.** `orchestrator.ts` has been decomposed to 1.9k lines, and a test **pins it at 2,000 lines or fewer** (`orchestrator-line-count.test.ts`).

**Agent integration.**
- **Provider interface.** `AgentProvider {spawn, resume}` returns an `AgentHandle {stream, injectMessage, stop}`.
- **Claude** uses the Agent SDK `query()` (`packages/core/src/providers/claude-provider.ts`) with `permissionMode: 'acceptEdits'`. In autonomous mode it adds a `canUseTool` deny-list, blocks `AskUserQuestion` and every Linear MCP tool via `disallowedTools` (forcing the CLI), sets `settingSources: []` and uses a custom autonomous system prompt. Code-intelligence MCP tools run in-process (BM25, PageRank repo map, duplicate detection), and the SDK sandbox is optional. `createAutonomousCanUseTool` also *shapes* tool use. It can deny Grep and Glob until the agent has tried an `af_code_*` tool, and it strips `run_in_background` from `Agent` calls so coordinator sessions can't exit and orphan their sub-agents. Bash is allowed unless shared safety rules deny it (`safety-rules.ts`).
- **Other providers:** Codex app-server with an approval bridge, Amp, Spring AI, and A2A as both client and server (`providers/a2a-provider.ts`, `server/src/a2a-server.ts`).
- **Provider selection** goes through a 9-tier cascade (`providers/index.ts: resolveProviderWithSource`). An issue label `provider:codex` wins, then a mention ("use codex"), then config per work type, then config per project, then env per work type, then env per project, then the config default, then `AGENT_PROVIDER`, then `claude`. The resolver returns a *source string* so every choice can be audited.

**State, isolation & merging.**
- **Worktrees** live in `../{repo}.wt/`.
- **Warm worktree pool** (`packages/core/src/workarea/local-pool.ts`). Members are keyed by (repo, toolchainKey). On reuse the pool runs a scoped clean and checks the lockfile for drift, which is watched with `fs.watch` and invalidates members. A `cleanStateChecksum` is computed over the lockfile and config files. node_modules are linked in (`dep-linker.ts`).
- **File reservations** (`packages/server/src/file-reservation.ts`). Each file gets a Redis `SET NX` mutex with a 1-hour TTL, plus a per-session index, exposed to agents as tools.
- **Conflict prediction** (`orchestrator/conflict-predictor.ts`). Before dispatch, the orchestrator checks which files open PRs touch and **injects a warning** into the prompt.
- **Session rows** are updated through a Lua compare-and-set (CHANGELOG, "lossless session metadata writes").
- **Merge queue.** The README describes a local rebase→test→merge queue with mergiraf and a parallel `MergePool` that uses greedy graph colouring. **None of that code is in this repo.** It is now `af admin merge-queue`.

**Verification & gates.**
- **Completion contracts** per work type (`orchestrator/completion-contracts.ts`), with fields such as `pr_url`, `branch_pushed`, `commits_present`, `work_result` and `pr_merged_or_enqueued`, each flagged `backstopCapable`.
- **Steer, then backstop.** If a session ends without meeting its contract, **session steering** resumes it with a focused prompt so the agent writes its own commit (`session-steering.ts`). After that the deterministic **backstop** commits (excluding build artifacts by enumerating exact paths, not globs), pushes and creates the PR (`session-backstop.ts`).
- **Outcome markers.** `<!-- WORK_RESULT:passed|failed -->` drives Linear transitions. An `unknown` result makes *no* transition and posts a diagnostic comment (`docs/WORK_RESULT_MARKER.md`).
- **Quality ratchet.** A quality **baseline** is captured from main before the agent starts (`quality-baseline.ts`). A committed `.donmai/quality-ratchet.json` holds minimum test count and maximum failures, typecheck errors and lint errors, and **only tightens**. It is enforced at merge time and in CI (`quality-ratchet.ts`).
- **Human override directives** in issue comments: `hold`, `resume`, `skip-qa`, `decompose`, `reassign`, `priority` (`governor/override-parser.ts`).
- **Stuck-worker decision tree:** nudge (2) → restart (3) → reassign (1) → escalate, with a 45-minute guard (`server/src/stuck-decision-tree.ts`).

**Routing (docs vs code).** `packages/core/src/routing/routing-engine.ts` implements Thompson sampling over Beta posteriors per (provider, workType). The reward is 0.5 for completion + 0.2 for a PR + 0.3 for a QA pass − 0.1 × cost/$5 (`reward.ts`). The **write side** is wired: `packages/cli/src/lib/routing-recorder.ts` records outcomes to Redis streams. The **read side is not**: `resolveProviderWithSourceAsync`, the only path that calls `selectProvider`, has no callers, and the orchestrator uses the synchronous cascade (`orchestrator.ts:977`). So the README's "intelligent routing" isn't live in the open-source code.

**UI/observability.** Dashboard components (fleet, routing metrics), Next.js route handlers, and an MCP server (`submit-task`, `list-fleet`, `get-cost-report`, `forward-prompt`, `fleet://logs/{id}`). An instrumented provider plus observability hooks (`packages/core/src/observability/`).

**Novel ideas — steal these.**
- **Separate work-type sessions (dev, QA, acceptance) driven by tracker status.**
- **Completion contracts: steer the agent first, then a deterministic backstop.**
- **A warm worktree pool with lockfile-drift invalidation.**
- **File-lease reservations plus open-PR conflict prediction injected into the prompt.**
- **A baseline-vs-ratchet quality gate.**
- **A pure stuck-remediation decision tree.**
- **Comment directives as a human control plane.**
- **Provider choices recorded with their provenance.**
- **Tool-use shaping in the permission callback**: force code-intel before grep, and force sub-agents into the foreground.
- **A test that caps orchestrator size**, as a guard against agents bloating the codebase.

**Weaknesses & tradeoffs.**
- Tight coupling to Linear. Redis is required for the queue, reservations and routing.
- The README over-claims: the bandit isn't wired, the merge queue is elsewhere, and the TUI was removed.
- The open-source repo is turning into a shell around a Go binary.
- The CHANGELOG is dominated by distributed-state race fixes (duplicate dequeue, stranded sessions, CAS writes, zombie re-queues), which shows what a multi-process Redis design costs.
- Reservations are advisory, because the agent has to call the tool.

**Implications for a Rust framework.**
- Make tracker-status→work-type a configuration table feeding a pure governor, with `decide(ctx) -> Action` and a reason.
- Implement `CompletionContract` plus a `Backstop` trait.
- Build the bandit (`rand_distr::Beta`), but wire both the observe and select sides and log `source`.
- Prefer single-node SQLite leases with TTLs over Redis until distribution is actually needed.
- Keep reconciliation (orphans, zombies) as pure functions over snapshots.

---

## 5. berenddeboer/ready-for-agent: a formally modelled work-item lifecycle

**What it is.** A Bun plus Effect-TS monorepo with about 112k lines of source and **about 176k lines of tests** (more tests than code), in 30+ packages. It uses SQLite (Turso local), a TanStack Start SPA and GraphQL Yoga, and ships binaries per platform. It is very active (PR #1345, 2026-09-23) and has 72 ADRs in `docs/adr/`. It runs on the developer's own laptop and existing subscription. The "150+ PRs a week" headline is the author's own claim and couldn't be verified.

**Architecture & pipeline.**
- **One loopback app server** (port 6056) serves the SPA and `/graphql`. Job workers are Effect fibers that claim from a SQLite queue with two lanes (`jobs`, `issue-refresh`), 5-minute visibility and about 1.5-second jittered polling (`ARCHITECTURE.md`).
- **Issue Reconciler.** The sole writer of the Issue store polls forges for the `ready-for-agent` label.
- **Work Item lifecycle.** States are *generated* from an ontology (`packages/lifecycle-model/src/generated/work-item-state.ts`). The main path is `create_worktree → install_dependencies → implement → pre_commit → review → assess_changes → commit → create_pr → watch_pr_status_checks ⇄ investigate_pr_status_checks → mark_pr_ready_for_review → decide_pr_merge → merge_pr / resolve_pr_merge_conflict → close_issue → local_cleanup`. Terminal states are `complete | failed | needs_human | abandoned`.
- **A human starts every item:** "Implement now", "Implement locally" (stops before any commit or PR), or, on a parent issue, "Implement All with Auto-merge". There is no automatic pickup.

**Agent integration.**
- **Backends:** OpenCode (the default), Codex, Grok (via ACP) and Claude Code.
- **Claude** (`packages/claude/src/lib/build-args.ts`): `claude -p --output-format stream-json --verbose --dangerously-skip-permissions --model M [--effort L] [--resume <id> | --session-id <uuid>] -- <prompt>`. It never uses `--continue`, `--fork-session` or `--bare` (ADR 0047). **Every lifecycle step continues the same Session**, so pre-commit repair, review apply and merge risk all see the implementation context.
- **Outcomes** come back as the final line, `READY_FOR_AGENT_RESULT: <TOKEN>[: arg]`. Parsing is strict: exactly one such line, and it must be last (`result-line.ts`, `decide-pr-merge.ts`). **Docs vs code:** ADR 0044 says this contract would become "one JSON outcome object per step", but nine lifecycle modules still parse sentinel lines.

**State, isolation & merging.**
- **Worktrees:** one per Work Item, with a bare clone recommended.
- **Process containment.** **Every agent turn and native repo command runs in its own transient, delegated systemd cgroup v2**. Kill and release use `cgroup.kill`, and execution is refused outright on macOS and Windows (`docs/process-ownership.md`). This handles process lifecycle, and the docs say plainly it is not a security sandbox.
- **GitHub Operation Coordinator** (ADR 0050). Exactly one GitHub operation runs at a time. Admission is ordered by origin (Operator > Lifecycle > Polling > Background), with a 60-second anti-starvation override and throttle deadlines.
- **Fair-share agent-turn admission** across repositories uses least-recently-granted ordering (ADR 0065). Defaults are `maxConcurrentAgentTurns` 2 and `maxConcurrentWorkItems` 5.
- **Merge Policy** is `off | classify | always`, pinned per Work Item (ADR 0059). Under `classify`, **Decide PR Merge** asks the implementing session for a risk verdict, `CLANKER_MERGE` or `NEEDS_HUMAN: reason` (`decide-pr-merge.ts`).
- **Merge guards.** Draft PRs stay draft until checks are green (ADR 0060). An autonomous merge requires successful checks (ADR 0055).
- **Competing PRs.** If another PR would also close the issue, the Work Item is quarantined to `needs_human` (ADR 0053).
- **CI circuit breaker.** A red default branch latches a Closed gate that holds new admissions until a newer green run (ADR 0068).
- **Stale binaries.** A binary older than the DB's applied migrations fails fast (ADR 0062).

**Verification & gates.**
- **Pre-Commit** runs `git hook run --ignore-missing pre-commit` after `git add -A`. Hook output goes to a log file, and a **sub-agent summarises it** so the main session's context stays small (ADR 0010).
- **Review** (`review.ts`):
  - An impact-based severity rubric (low, medium, high).
  - Validity rules: a finding must be a regression or an unmet in-scope requirement, with cited evidence. Out-of-scope issues become non-blocking "follow-up observations".
  - Apply outcomes: `FIXED`, `DEFERRED` (low or medium only), `CLEARED` (needs evidence).
  - **Risk-based reruns:** medium or high findings trigger a full re-review, while low findings get a short Review Rerun Assessment (ADR 0034).
  - A cap of 6 fix rounds, and a **no-progress timeout** that resets only at verified checkpoints (ADR 0069).
- **Automated-reviewer reruns** are bounded by **durable permits reserved before the external call**, so a crash can't unlock an extra rerun (ADR 0027).
- **Scope anchor.** `.ready-for-agent/scope.md` is reloaded on every turn, so the agreed scope survives compaction and retries (`scope-handoff.ts`).

**UI/observability.** Routes for kanban, repos, completed items and per-session telemetry (`apps/harness/src/routes/`). GraphQL **subscriptions act as invalidation signals** (for example `repositoriesChanged`), and clients refetch. Agent Turn Tail shows a bounded, non-persisted excerpt of the latest turn. `ready-for-agent jump <sessionId>` continues the agent's session interactively in a terminal or tmux.

**Novel ideas — steal these.**
- **An ontology-derived lifecycle.** OWL, SKOS and SHACL in `ontology/rfa.ttl` and `shapes.ttl` generate the state enum, the transition relation and the reason codes. CI validates fixture graphs, and one runtime transition point checks declared pairs (`transition-relation-check.ts`; strict in tests, "observe" only in production).
- **A cgroup per agent turn.**
- **A three-state merge policy with a per-item pin**, plus a CI circuit breaker, plus draft-until-green.
- **A forge-operation coordinator and fair-share admission.**
- **A strict last-line result token.**
- **A severity rubric with risk-scaled reruns.**
- **scope.md as a scope anchor.**
- **Durable permits.**
- **Jump into a session.**

**Weaknesses & tradeoffs.**
- Local execution is Linux-only, on a single node.
- No autonomous intake, so throughput depends on a human clicking.
- **Merge risk is judged by the same session that wrote the code** (self-assessment bias), and review uses the same backend.
- `--dangerously-skip-permissions` runs on the host.
- `work-item-lifecycle.ts` is a single 9.9k-line file.
- SQLite has a single writer.

**Implications for a Rust framework.**
- Declare the lifecycle once (a Rust `enum` plus a transition table generated from a spec file; the ontology is optional) and apply every transition through one function that carries a reason code.
- Run each agent in a cgroup, either via systemd transient units over `zbus` or directly through cgroupfs.
- Build a forge coordinator as a priority semaphore with aging.
- Keep durable budget permits in SQLite.
- Push invalidation events over WebSocket or SSE.

---

## 6. johnplanow/substrate: gate-heavy pipeline with an experimenter

**What it is.** `substrate-ai` v0.21.19, a TypeScript monorepo (CLI plus `packages/{core,sdlc,factory}`) with about 139k lines of source, about 243k lines of tests and about 113k lines of Markdown (BMAD planning artifacts and story files). The last commit was 2026-07-10. It develops itself: substrate dispatches substrate's own stories. State lives in SQLite, or in Dolt for versioned state.

**Architecture & pipeline.**
- **Operated by *your* assistant.** Claude Code runs `substrate run --events` and parses NDJSON events. `--help-agent` prints a machine prompt of under 2,000 tokens generated from the same TypeScript event types. `init` injects a marker-wrapped `CLAUDE.md` section and slash commands.
- **Phases** follow BMAD: research → analysis → planning → solutioning → implementation → cross-story contract verification.
- **Per story:** `create-story → test-plan → dev-story → build-fix → code-review (SHIP_IT | NEEDS_MINOR_FIXES | NEEDS_MAJOR_REWORK) → fix/rework → verification → finalize`.
- **DOT graph engine** (`packages/factory/src/graph/`) with conditional edges, LLM-evaluated conditions and fan-out/fan-in.
- **Autonomy modes.** `--halt-on all|critical|none` selects attended, supervised or autonomous, with exit codes 0, 1 (some stories escalated) and 2 (run failure).
- **Recovery Engine** tiers: A retries with added context, B drafts a re-scope proposal, C halts. Dispatch **pauses under back-pressure** once 2 or more proposals are pending.

**Agent integration.**
- **Workers are child CLIs** (`packages/core/src/adapters/claude-adapter.ts`): `claude -p --model M --dangerously-skip-permissions --output-format stream-json --verbose --system-prompt <base + optimization directives>`. A "scoped" profile instead writes a per-worktree settings file and passes `--permission-mode acceptEdits --settings <file>`, but it is **off by default**.
- **`--system-prompt` replaces CLAUDE.md**, so workers don't read the orchestrator's instructions.
- **`--max-turns` was removed**, after an audit concluded it was accepted but "not honored" in 2.1.15x. Vanguard, by contrast, relies on it.
- **Output contract:** a YAML block extracted from the final `result` text.
- **Routing policy YAML** is **subscription-first with rate-limit windows**, for example 220k tokens per 18,000 seconds for Claude, before falling back to API billing (`routing/routing-engine-impl.ts`). `RoutingTuner` applies *at most one* conservative model downgrade per invocation, based on per-phase token telemetry (`routing/routing-tuner.ts`).

**State, isolation & merging.**
- **Worktrees:** per story, under `~/.substrate/worktrees/<project>-<hash8>/<key>/` on `substrate/story-<key>`.
- **Commit-first.** The agent's output is auto-committed at dev-story completion, *before* review, so the branch is always the durable copy. Failure paths add `wip(...)` checkpoints.
- **Finalization:** `merge` (ff-only by default; three-way is required for concurrent runs), `branch` or `pr`, plus an optional `epic_gate_command` that must pass before the last story of an epic integrates.
- **Cross-story race recovery** re-verifies stories whose verdict went stale because a concurrent story committed afterwards (`packages/sdlc/src/verification/cross-story-race-recovery.ts`). `reconcile-from-disk` fixes false failures.
- **Dolt** adds `substrate diff <story>` and `history`.

**Verification & gates** (`packages/sdlc/src/verification/checks/`). The "Tier A" checks use no LLM:
- `phantom-review`: the review returned no real verdict.
- `trivial-output`: fewer than 100 output tokens.
- AC evidence, and `build`.
- `test-suite`: runs the real suite using the test command from the **trusted main-tree profile**. It **rejects exit-code laundering** such as `|| true`, `; exit 0`, `|| :` and brace-group masks (`detectsExitCodeLaundering`). Its motivating field finding: a story passed all six checks while pytest was red, because the "tests pass" signal was self-reported.
- `test-mutation`: a WARN when pre-existing test files, `conftest.py` or fixture directories are modified, the reward-hack pattern.
- `acceptance-spec`: a FAIL if the worktree's copy of the acceptance registry, deferrals or contract differs from the trusted copy.
- `contamination`: foreign toolchains or languages.
- `net-new-implementation`: the change is only stubs or whitespace.
- `runtime-probes`: probes derived from AC text by a "probe-author" phase, including detection of error envelopes.
- `source-ac-fidelity`.

Beyond Tier A, there is a scenario store with SHA-256 manifests, satisfaction scoring, and convergence loops with plateau detection (`packages/factory/src/{scenarios,convergence}`), plus Docker Compose "digital twins".

**Learning / self-improvement.**
- **Learning loop** (`packages/sdlc/src/learning/`). A deterministic rule chain classifies failures (`failure-classifier.ts`: namespace-collision, dependency-ordering, resource-exhaustion, build or test failure, and so on). Findings are stored in the state database and injected into later prompts when relevant enough, scored as `0.5·Jaccard(file overlap) + 0.3·package + 0.2·root-cause` with a threshold of 0.3 and a 2,000-character budget (`relevance-scorer.ts`, `findings-injector.ts`). Operators can annotate findings as confirmed or false positives.
- **Supervisor.** Multi-signal stall detection needs two of timer, output growth and CPU (`src/modules/supervisor/multi-signal-stall.ts`). It then kills the process tree and resumes.
- **Experimenter** (`packages/core/src/supervisor/experimenter.ts`). It creates a worktree and branch, **appends an HTML-comment directive to a phase prompt file**, re-runs the affected stories and compares metrics. IMPROVED or MIXED results get `gh pr create`; REGRESSED results delete the branch. Some directives weaken the gates outright, for example `review_cycles` → "accept passing implementations with minor style issues, reduce strictness".

**UI/observability.** A TUI (`--tui`), the NDJSON event protocol (`pipeline:*`, `story:*`, `verification:*`, `supervisor:experiment:*`), `report` (with `--verify-ac` for an AC→test traceability matrix), `metrics --compare`, `cost`, and OTEL telemetry optionally stored in Dolt.

**Novel ideas — steal these.**
- **Trusted-tree reads for gate configuration, plus anti-reward-hacking tripwires** (laundering, test mutation, acceptance-spec tampering, phantom or trivial output).
- **Commit-first durability.**
- **Stale-verification detection under concurrency.**
- **A CLI that agents operate, with `--help-agent` generated from the types.**
- **Autonomy levels mapped to halt severities and exit codes.**
- **Recovery tiers with back-pressure on proposals.**
- **Stall detection that needs two signals.**
- **Routing aware of subscription windows.**
- **Relevance-scored lesson injection.**

**Weaknesses & tradeoffs.**
- The surface is enormous for one maintainer, and a half-finished migration duplicates modules (`src/modules/routing` alongside `packages/core/src/routing`).
- Markdown far outweighs code.
- The experimenter makes crude, n=1 edits, and some of them relax the gates (a Goodhart risk).
- The BMAD artifact layout is baked in (`_bmad-output/`).
- Permissions are skipped by default.
- Many checks are heuristics (word overlap between AC text and test names) that need operator annotations to correct them.

**Implications for a Rust framework.**
- Implement verification as a chain of `trait Check { tier, needs_llm, run(ctx) -> Findings }` objects.
- *Always* read gate configuration and test commands from the base commit, never from the agent's worktree.
- Ship laundering and test-mutation tripwires.
- Commit before review.
- Store `verified_at_base` on each verdict so it can be invalidated when the base moves.
- Emit a versioned NDJSON event schema with JSON Schema (`schemars`) so agents can operate the framework themselves.
- Track subscription windows per provider.

---

## Cross-repo takeaways

### Novel ideas ranked by value for our Rust framework
1. **Make verification something the agent can't game.** Read gate config and test commands from the trusted base tree, detect exit-code laundering, trip on modified pre-existing tests and acceptance specs, and flag phantom or trivial output (substrate `verification/checks/*`). Host-run verification should replace self-reported "tests pass" everywhere.
2. **Proof bound to exact bytes.** Combine HAR's tree hash from a temporary index and its commit binding with vanguard's host-computed, hashed output blocks. The resulting `ProofRecord{tree_hash, cmd, exit, sha256, commit}` should be the only thing that can authorise "done" or a merge.
3. **A deterministic scheduler that keeps agents off each other.** DAG plus predicted footprints ("unknown ⇒ serial") plus terminal and stall detection (claude-factory `ready_set.py`). Add leased file reservations and open-PR conflict prediction (agentfactory), a serialized rebase→retest→merge queue, and re-verification of stale verdicts when the base moves (substrate).
4. **One declared lifecycle with a single transition function and reason codes.** From ready-for-agent's ontology: generate the Rust enum and transition table, and fail on any undeclared transition. Turn "observe" into "enforce".
5. **Completion contracts: steer, then backstop, then downgrade.** Resume the agent with the specific missing obligation (agentfactory steering), then have deterministic code finish it. If the spec manifest is still unmet, publish as `Part of #N` rather than closing the issue (vanguard).
6. **Every loop bounded, with durable budgets and ladders.** Count fix rounds, not findings, and union the blocking findings. Escalate a tier before parking. Reserve permits durably before external calls. Use no-progress timeouts instead of wall-clock ones. Freeze runs with a live shell for a human.
7. **Process containment and credential hygiene.** A cgroup v2 per agent turn (ready-for-agent). A sandbox with a nonce LLM proxy, tmpfs secrets, an egress allowlist and a dropped `.github/workflows` write-back (vanguard).
8. **Merge safety policy:** off, classify or always, pinned per item; draft until green; a CI-red circuit breaker; quarantine of competing PRs (ready-for-agent); a quality ratchet against a baseline (agentfactory); cumulative gates (HAR).
9. **Learning that actually closes the loop.** A deterministic failure taxonomy plus relevance-scored lesson injection (substrate), a retrospective digest, failures drafted into eval cases (vanguard), and an outcome-observation store from day one so bandit routing can later be wired *properly* (agentfactory shows how easy it is to leave the select side dark).
10. **Fairness and rate control.** A forge-operation coordinator with origin priority and aging, fair-share turn admission across repos, and subscription-window-aware routing.
11. **Agent-facing robustness.** Strict last-line result tokens, detection of `<synthetic>` messages, salvage of truncated streams as resumable, scope.md anchors, and a sub-agent that summarises noisy hook logs.
12. **Observability plumbing.** OTEL hooks with hard time budgets, invalidation-style subscriptions for the UI, per-run event backlogs so the UI can re-attach, and "jump into session" handoff to a terminal or tmux.

### Common failure modes (evidence from code comments, ADRs and changelogs)
- **Agents' self-reported success is unreliable.** They claim tests pass, stop early (turn caps, GLM prose-stops), or leave residue that typechecks. Several projects added host-side verification only after being burned (substrate field finding #11, vanguard dogfood #352).
- **Reward hacking:** editing or deleting tests, `|| true` wrappers, editing acceptance fixtures (substrate H1.7/H7).
- **Concurrency races:** concurrent commits invalidate earlier verdicts, and semantic conflicts appear only after rebase. Distributed-state races include duplicate dequeue, stranded sessions and zombie re-queues (agentfactory CHANGELOG). Label-swap claims aren't atomic.
- **Tool and version drift:** stale CLIs inside sandboxes fail mid-run (vanguard), a stale global binary caused a fork-bomb loop (HAR #291), a stale binary met a newer DB (ready-for-agent ADR 0062), and CLI flags are silently ignored (`--max-turns`, per substrate). Pin versions, run preflight `doctor` checks and fail fast.
- **Unbounded retries on flaky externals** such as automated-review reruns and CI. Durable permits and circuit breakers fix this.
- **Permissive defaults and weak guardrails:** Of the four projects that spawn agents themselves, three default to `--dangerously-skip-permissions` or `bypassPermissions` (vanguard, ready-for-agent, substrate), and agentfactory pairs `acceptEdits` with a permissive `canUseTool`. Regex git guardrails are easy to bypass. Real isolation (a container or a cgroup plus credential proxying) matters more than deny-lists.
- **Docs over-claim, and the gap is growing.** Unwired bandit routing, a merge queue that moved to a closed binary, "self-improving" that means advisory text, and ADRs describing contracts the code hasn't adopted. For our framework, capabilities should be introspectable (a capabilities manifest à la `--help-agent`) and tested end to end rather than asserted in the README.
