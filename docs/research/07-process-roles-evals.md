# 07 — Process, roles, durability & evaluation

Research group: *Process, roles, durability & evaluation*. Input for the design of a Rust
software-factory framework that takes work items through specs and plans, isolated
workspaces, implementation, and review/verification to PRs, with a real-time UI.

Sources were cloned shallowly (September 2026) and read directly. Paths below are relative to each repo root.

| Repo | Revision read | Notes |
|---|---|---|
| `github/spec-kit` | `adbd62a` (2026-09-24, `specify-cli` 1.0.12.dev0) | now ships a YAML workflow engine + extensions/presets/bundles |
| `bmad-code-org/BMAD-METHOD` | `1b59caa` (v6.12.0, 2026-09-03) | skills-based; SM/QA personas removed in v6.3 |
| `FoundationAgents/MetaGPT` | `11cdf46` (2026-01-21, v1.0.0) | default team now TeamLeader + tool-using Engineer2 |
| `OpenBMB/ChatDev` `chatdev1.0` | `31fd994` (2025-09-23) | legacy "virtual software company" |
| `OpenBMB/ChatDev` `main` | `4fb2db0` (2026-07-24) | "ChatDev 2.0 / DevAll" graph platform |
| `humanlayer/12-factor-agents` | `d20c728` (2025-09-21) | essay, no code of substance |
| `SWE-bench/SWE-bench` | `02e7a74` (2026-09-02, v5.0.2) | task-repo format, infra-failure classification |
| `temporalio/sdk-core` | `0eb03f2` (2026-09-18) | Core + Rust SDK `temporalio-sdk` 1.0.0 |

---

## 1. github/spec-kit — Spec-Driven Development toolkit

### What it is
A Python CLI (`specify`) that installs **prompt templates ("commands"/skills), document
templates and helper scripts** into a repo so that *any* coding agent (≈45 integrations
under `src/specify_cli/integrations/*`: Claude, Copilot, Codex, Gemini, Cursor, …) can follow the
same phased process. The agent does the work. Spec-kit supplies the process, the artifact shapes and the file conventions.
Since 2026 it also has a **workflow engine** (`src/specify_cli/workflows/`) that runs the commands
headlessly (`claude -p … --output-format json`) with gates, loops and fan-out.

### Architecture / process model
- **Phases (core, `templates/commands/*.md`)**: `constitution` (once per project) and then, per feature,
  `specify → [clarify] → plan → [checklist] → tasks → [analyze] → implement → converge`.
  `taskstoissues` exports tasks to GitHub issues. Optional extensions add other processes:
  `bug` (assess → fix → test) and `assess` (intake → research → define → shape → decide).
- **Everything is a Markdown prompt with YAML frontmatter.** Frontmatter declares the helper
  script to run first (`scripts: sh: scripts/bash/check-prerequisites.sh --json --require-tasks`) and
  `handoffs:` (the suggested next command, e.g. specify → plan / clarify). The scripts return JSON
  (`FEATURE_DIR`, `AVAILABLE_DOCS`), so the paths the prompts use are deterministic.
- **State lives in files.** `specs/NNN-feature/{spec,plan,research,data-model,quickstart,tasks}.md`,
  `contracts/`, `checklists/`, and `.specify/feature.json` (the current feature directory, decoupled from the
  git branch name). The constitution is at `.specify/memory/constitution.md`.
- **Hooks**: every command begins and ends with `hooks.before_<cmd>` / `hooks.after_<cmd>` read
  from `.specify/extensions.yml`. A mandatory hook must actually be invoked by the agent. Each command
  file carries about 40 lines of boilerplate that re-explains hook semantics to the LLM
  (`templates/commands/converge.md` lines 15–50).
- **Workflow engine** (`workflows/ARCHITECTURE.md`, `src/specify_cli/workflows/engine.py`): 12
  step types (`command`, `prompt`, `shell`, `gate`, `if`, `switch`, `while`, `do-while`,
  `fan-out`, `fan-in`, `slot`, `init`), a Jinja-like expression language over `inputs.*`/`steps.*`,
  and run state saved after every step to `.specify/workflows/runs/{run_id}/state.json` plus an
  append-only `log.jsonl`. `gate` pauses the run; `specify workflow resume <run_id>` continues it.
  Known limitation, stated in the doc: *"Resume tracking is at the top-level step index only… A nested
  step-path stack for exact resume is a planned enhancement."*
- **Customization stack**: presets (override templates/commands), extensions (add commands +
  hooks), bundles (role-based packaging) and catalogs, each resolved through a priority stack
  (project → user → built-in).

### Artifacts & handoffs
The spec is **technology-agnostic** and made of **independently testable, prioritized user stories**
(`templates/spec-template.md`):
```markdown
### User Story 1 - [Brief Title] (Priority: P1)
**Why this priority**: [Explain the value and why it has this priority level]
**Independent Test**: [Describe how this can be tested independently ...]
**Acceptance Scenarios**:
1. **Given** [initial state], **When** [action], **Then** [expected outcome]
...
- **FR-006**: System MUST authenticate users via [NEEDS CLARIFICATION: auth method not specified - email/password, SSO, OAuth?]
## Success Criteria *(mandatory)*
- **SC-001**: [Measurable metric, e.g., "Users can complete account creation in under 2 minutes"]
```
Tasks carry IDs, parallelism markers, story traceability and exact paths (`templates/tasks-template.md`):
```markdown
## Format: `[ID] [P?] [Story] Description`
- **[P]**: Can run in parallel (different files, no dependencies)
- [ ] T012 [P] [US1] Create [Entity1] model in src/models/[entity1].py
- [ ] T014 [US1] Implement [Service] in src/services/[service].py (depends on T012, T013)
**Checkpoint**: At this point, User Story 1 should be fully functional and testable independently
```
`converge` (`templates/commands/converge.md`) re-audits the code against spec/plan/tasks, treating
*"completion claims are not evidence"*, and **appends** new tasks tagged with source and gap type:
```markdown
- [ ] T042 <imperative description> per <source-ref> (<gap-type>)
# <source-ref>: FR-003 | SC-002 | US1/AC2 | plan: storage decision | Constitution II
# <gap-type>: missing | partial | contradicts | unrequested
```
Gates in the workflow engine (`workflows/speckit/workflow.yml`):
```yaml
  - id: review-spec
    type: gate
    message: "Review the generated spec before planning."
    options: [approve, reject]
    on_reject: abort
```
Other handoff mechanics worth noting:
- `specify` caps `[NEEDS CLARIFICATION]` markers at **3**, generates `checklists/requirements.md`,
  and self-validates for up to 3 iterations.
- `clarify` (`templates/commands/clarify.md` l.73–200) scans against a fixed ambiguity **taxonomy**
  (scope, data model, UX, NFRs, integrations, edge cases, terminology, completion signals). It asks
  **≤5 questions, one at a time**, each multiple-choice with a **recommended option**, and writes each
  answer into the spec immediately under `## Clarifications / ### Session YYYY-MM-DD` (atomic overwrite
  after each answer).
- `plan` has a **Constitution Check gate** and a "Complexity Tracking" table that any violation must justify.
- `analyze` is **strictly read-only**. It reports duplication, ambiguity, underspecification, coverage gaps and
  inconsistency, and treats constitution conflicts as automatically CRITICAL.
- `implement` treats checklists as a gate: any unchecked item means STOP and ask. It marks tasks `[X]` and halts on a
  failing non-parallel task.
- The `bug` extension ends in a verdict of `verified | partial | failed`: *"Never mark a fix as `verified`
  based on tests alone if the original assessment listed a reproduction that you did not actually
  exercise"* (`extensions/bug/commands/speckit.bug.test.md` l.117).

### Strengths — steal these
1. **Stable IDs for everything** (FR-###, SC-###, US#/AC#, T###) plus traceability tags on tasks. This
   is what lets `analyze` and `converge` do mechanical coverage checks. Make IDs first-class in our schema.
2. **Converge loop.** Implement, then audit against intent with append-only remediation tasks, until
   "Converged". This is a clean definition of done that doesn't trust the implementer's checkmarks.
3. **Bounded clarification**: at most 3 open markers, at most 5 questions, multiple choice with a recommended default,
   answers recorded in the artifact. Humans answer fast and the answer lands in the spec for later stages.
4. **Constitution** = repo-level invariants checked at plan/analyze/converge. Constitution violations are CRITICAL.
5. **Agent-agnostic by design**: same templates, per-agent registrar (Markdown/TOML/YAML/skills formats)
   and a `build_exec_args` per CLI (`src/specify_cli/integrations/base.py` l.1089–1108).
6. **Deterministic helper scripts** for path and prerequisite resolution, returning JSON, so the LLM doesn't guess paths.
7. The workflow YAML (steps, gates, loops, fan-out, `{{ steps.x.output }}`) is a reasonable **user-facing
   pipeline DSL** to copy.

### Weaknesses & tradeoffs
- **The process is enforced by prose, not code.** Hooks, gates inside commands and "MUST NOT modify
  spec.md" are all instructions to the LLM. Nothing checks them except the next LLM call. Each command
  carries boilerplate re-explaining hook semantics, which costs tokens and invites drift.
- **Heavy ceremony for small changes.** The full cycle produces 6–8 documents per feature. The README now
  frames SDD as one of three entry points and adds a `lean` preset, which admits this.
- **Mostly single-agent and sequential.** `[P]` markers exist but `implement` runs in one agent session.
  Parallel work across agents or worktrees is not modelled (no workspace isolation, no merge step).
- **Weak durability.** Run state is a JSON file per run and resume is at top-level step granularity.
  Concurrent features collide on `.specify/feature.json`.
- **Verification is LLM-judged** (converge/analyze read code). No built-in test execution gate beyond
  what the agent chooses to run.

### Implications for a Rust framework
- Model **Spec / Plan / Task / Finding** as typed records with stable IDs, rendered to Markdown for
  humans and agents but *owned* by the orchestrator's DB. Markdown is the view, not the source of truth.
- Enforce gates, hooks and write-permissions **in the engine** (e.g. the analyze stage runs with a read-only
  workspace mount; converge may only append tasks through a tool). Don't re-explain them in every prompt.
- Ship a **pipeline DSL** close to spec-kit's workflow.yml (steps, gate, if/switch, loops with caps,
  fan-out/fan-in) with **nested-path resume from day one**.
- Provide an **agent adapter trait** (`build_invocation(prompt, model, output=json) -> Command`) plus
  per-agent command/skill installers. Spec-kit's 45 integrations show that demand is broad.

---

## 2. bmad-code-org/BMAD-METHOD — agent personas, stories, context

### What it is
A large prompt library ("skills", `skills/*/SKILL.md` + `workflow.md` + step files + `customize.toml`)
for "agile AI-driven development". Historically (v4–v6.2) it was organised around **named personas**:
Analyst *Mary*, PM, Architect, UX, Scrum Master *Bob*, Dev *Amelia*, QA *Quinn*, Tech Writer *Paige*,
and a **document pipeline**: brief → PRD → architecture → epics/stories. Large docs were **sharded**, the
SM drafted **story files** carrying the needed context, dev implemented, and QA reviewed.

### Architecture / process model
The design has **evolved away from role-play**, and this is the most informative finding:
- v6.3.0 (`CHANGELOG.md` l.369): *"Consolidate three agent personas into Developer agent (Amelia):
  remove Barry quick-flow-solo-dev, Quinn QA agent, and Bob Scrum Master agent."* (Also `removals.txt`.)
- v6.11.0 (l.26–38): *"Quick Dev becomes Build, the one official way BMad implements code…
  the `bmad-create-story` → `bmad-dev-story` pair is deprecated, and Phase 4 is a single chain:
  `bmad-sprint-planning → bmad-build → bmad-code-review`."* `bmad-shard-doc` and `bmad-index-docs` were
  *"removed outright"*. Tech-writer persona retired. Review skills merged into one `bmad-review` with
  "lenses".
- v6.12.0: *"Build decides how much ceremony a change needs after investigating it, not before."*

The current process:
- **Planning (optional, right-sized)**: `bmad-product-brief`, `bmad-prd`, `bmad-ux`, `bmad-architecture`,
  `bmad-create-epics-and-stories`. `bmad-sprint-planning` opens with a **readiness gate**
  (PASS/CONCERNS/FAIL, `skills/bmad-sprint-planning/references/readiness-gate.md`): *"could a developer
  implement these epics without inventing decisions nothing records?"*
- **Build** (`skills/bmad-build*/`): step files `01 clarify-and-route → 02 plan → 03 implement →
  04 review → 05 present`. It routes to `oneshot` or `full`, and review is `none|quick|thorough|auto`. The
  **spec file's frontmatter `status` is the resumable state machine** (`draft → ready-for-dev →
  in-progress → in-review → done | blocked`). Re-invoking with a spec path jumps to the right step
  (`step-01-clarify-and-route.md`).
- **Build Auto** (unattended): no human interaction. Every exit is a HALT with an explicit terminal
  `status` and `blocking condition` string (e.g. `intent gap`, `no subagents`, `review repair loop exceeded
  5 iterations (non-convergence)`).
- **Sprint tracking**: `sprint-status.yaml`, owned by a **tested deterministic script**
  (`skills/bmad-sprint-planning/scripts/sprint_plan.py`, 37 tests). It uses "preserve-never-downgrade" status
  merging: `STORY_RANK = {"backlog": 0, "ready-for-dev": 1, "in-progress": 2, "review": 3, "done": 4}`.
  The changelog's explanation: *"Judgment stays with the LLM"*, and the mechanics moved to code.
- **Context**: instead of sharding, `compile-epic-context.md` has a subagent distil the planning docs
  into `epic-<N>-context.md`, a cache that becomes **invalid when any planning file is newer**. The previous
  done story's Code Map / Implementation Notes / Spec Change Log carry forward.
  `bmad-project-context` keeps one verified block in `AGENTS.md` with an admission test: *"anything
  derivable from source is read live and never stored"*.
- **Personas remain as UX**: the persona SKILL.md files (e.g. `skills/bmad-agent-dev/SKILL.md`) are menus of
  skills with a voice and an icon. `bmad-party-mode` stages multi-persona discussion for ideation
  only.
- **Customization**: layered TOML (`customize.toml` → `_bmad/custom/{skill}.toml` → `.user.toml`);
  arrays of tables merge by `id`/`code`. Rendered skills are published as **content-addressed immutable
  snapshots** with a manifest of source hashes (v6.11 "Inspectable workflow snapshots").

### Artifacts & handoffs
The Build spec (`skills/bmad-build-auto/spec-template.md`) is a compact, machine-readable story file
with a target of 900–1600 tokens (*"above 1600 risks context-rot in implementation agents"*):
```markdown
---
status: 'draft' # draft | ready-for-dev | in-progress | in-review | done | blocked
route: '' # oneshot | full — set by step-02
review: '' # none | quick | thorough — set by step-04
lenses_ran: [] # ids of the lenses launched, set by step-04
review_loop_iteration: 0 # incremented by step-04 before each review loopback
deferred: [] # append-only machine-readable deferred review findings
---
<intent-contract>
## Intent            **Problem:** …  **Approach:** …
## Boundaries & Constraints   **Always:** …  **Never:** …
## I/O & Edge-Case Matrix  | Scenario | Input / State | Expected Output / Behavior | Error Handling |
</intent-contract>
## Code Map / Tasks & Acceptance / Implementation Notes (append-only) / Spec Change Log (append-only) / Review Triage Log (append-only) / Verification
```
Review is **parallel, context-free lenses** (`customize.toml` `[[workflow.thorough_lenses]]`): Blind
Hunter (diff only, a "finding floor" of `min(floor(sqrt(kB)+1),10)`), Edge Case Hunter,
**Verification Gap** (*"if this behavior broke, would any test fail?"*, `review-prompts/verification-gap.md`),
and **Intent Alignment Auditor** (fed the *verbatim* original intent plus the diff). The diff is passed as a
**file path, never pasted**. Triage (`step-04-review.md`) gives *every* finding a verdict and a route:
```markdown
### {date} — Review pass
- verdicts: <total> findings — high <N>, medium <N>, low <N>, false <N>, maybe-false <N>
- findings:
  - `[verdict]` `[intent_gap|bad_spec|patch|defer|reject]` <summary> — <evidence…>
```
The routes, in cascade order:
- **intent_gap**: save the patch, revert, HALT `blocked`.
- **bad_spec**: extract KEEP notes, revert code, amend the spec outside `<intent-contract>`, re-implement.
  The loop count is capped at 5.
- **patch**: the *same* implementation subagent is re-engaged to fix.
- **defer**: pre-existing issues are recorded in frontmatter.

### Strengths — steal these
1. **Right-size the process after investigation** (oneshot vs full, quick vs thorough review), with the
   decision recorded as `route_source: pinned|auto`.
2. **Finding triage taxonomy**: `intent_gap / bad_spec / patch / defer / reject`, with evidence required
   for `false`. Fix the *spec* (and re-derive code) rather than patch symptoms when the spec was wrong.
   Loop caps turn non-convergence into an explicit `blocked` state.
3. **Independent reviewers with deliberately restricted context** (blind diff, intent-only auditor,
   verification-gap). Diversity comes from what each reviewer is allowed to see, not from persona costume.
4. **Status-in-artifact state machine + explicit HALT strings.** This makes unattended runs resumable and
   machine-parsable.
5. **Deterministic scripts own bookkeeping** (sprint status, config resolution, rendering). LLMs keep judgment.
6. **Context budget discipline**: 900–1600-token specs, a cached epic context invalidated by mtime, and
   carry-forward of the previous story's notes.
7. **Content-addressed prompt snapshots** give reproducibility of exactly what ran.

### Weaknesses & tradeoffs
- The persona-heavy v4–v6.2 design (SM → story → Dev → QA handoffs, sharded docs) produced ceremony and
  duplicated context. The maintainers removed most of it. Treat that as evidence, not a feature list.
- Everything is still **prompt-enforced**: "NEVER skip steps", "READ COMPLETELY", Jinja-templated
  markdown steering an agent through micro-files. Orchestration lives inside one agent session and depends
  on the host's subagent support (`HALT … no subagents`).
- Heavy churn: many breaking renames per minor release, plus a shim layer to keep old IDs alive.
- Planning docs (PRD/architecture/UX) are LLM-authored prose with limited mechanical validation beyond
  the readiness gate.

### Implications for a Rust framework
- **Roles = (prompt template, toolset, context policy, model, permissions)**, not personas. Personas are
  optional presentation.
- Implement the **review-lens fan-out and triage router in the engine**: parallel reviewer jobs, each
  with an explicit *context allowlist* (diff only / diff + intent / diff + tests). A typed `Finding
  {verdict, route, evidence, location}`, and routes mapped to engine transitions (revert + re-plan,
  re-engage implementer, halt with `blocked(reason)`, defer to backlog).
- Keep a **work-item status lattice with monotonic merge** (never downgrade except via an explicit repair
  command) and **typed halt reasons**.
- Store a **rendered-prompt snapshot hash** on every step event, for reproducibility and for the eval harness.

---

## 3. MetaGPT and ChatDev — "Code = SOP(Team)" and the virtual software company

### What it is
- **MetaGPT** (`metagpt/`): a Python multi-agent framework in which **Roles** (ProductManager, Architect,
  ProjectManager, Engineer, QaEngineer) run **Actions** (WritePRD, WriteDesign, WriteTasks, WriteCode,
  WriteCodeReview, WriteTest…) and communicate through an **Environment** message pool. The standard operating procedure (SOP) is encoded
  as "which role watches which action's output".
- **ChatDev v1** (`chatdev1.0` branch): a "virtual software company". A **ChatChain** of phases, each a
  two-agent role-play chat (CAMEL-style "instructor ↔ assistant"), e.g. CEO↔CPO for DemandAnalysis,
  CTO↔Programmer for Coding, Reviewer↔Programmer for CodeReview.
- **ChatDev v2 "DevAll"** (`main`): a general, zero-code **graph workflow platform** (YAML nodes/edges, Vue
  UI). v1 is kept only as `yaml_instance/ChatDev_v1.yaml`.

### Architecture / process model
**MetaGPT pub/sub.** `Message` (`metagpt/schema.py`) carries `content`, typed `instruct_content`
(pydantic), `cause_by` (the Action type that produced it), `sent_from`, `send_to`. Roles subscribe by action
type:
```python
# metagpt/roles/architect.py / project_manager.py / engineer.py / qa_engineer.py
self.set_actions([WriteDesign]);  self._watch({WritePRD})
self.set_actions([WriteTasks]);   self._watch([WriteDesign])
self.set_actions([WriteCode]);    self._watch([WriteTasks, SummarizeCode, WriteCode, WriteCodeReview, FixBug, ...])
self.set_actions([WriteTest]);    self._watch([SummarizeCode, WriteTest, RunCode, DebugError])
```
`Environment.publish_message` delivers to every role whose address set matches
(`metagpt/environment/base_env.py` l.175–195). `Role._observe` filters its buffer by
`cause_by in self.rc.watch or self.name in n.send_to` (`metagpt/roles/role.py` l.410). `Team.run`
loops `n_round` times until all roles are idle, raising `NoMoneyException` when `cost_manager.total_cost
>= max_budget` (`metagpt/team.py` l.92–100). `Team.serialize/deserialize` checkpoint to JSON for
`--recover-path`. Outputs are schema'd with **ActionNode** (`metagpt/actions/design_api_an.py`: keys
like "Implementation approach", "File list", "Data structures and interfaces" (mermaid classDiagram),
"Program call flow" (sequenceDiagram)).

**The pivot is already in MetaGPT itself.** `metagpt/software_company.py` now hires
`TeamLeader, ProductManager, Architect, Engineer2, DataAnalyst`, and the classic
`ProjectManager`, `Engineer(n_borg=5)` and `QaEngineer` are **commented out**. `Engineer2`
(`metagpt/roles/di/engineer2.py`) is a ReAct tool-user with `max_react_loop = 40` and tools
`Editor`, `Terminal:run_command`, `Browser`, `git_create_pull`, `CodeReview`, `Deployer`. `TeamLeader`
routes work dynamically (`publish_team_message`) instead of following a fixed SOP.

**ChatDev v1 chain** (`CompanyConfig/Default/ChatChainConfig.json`): DemandAnalysis → LanguageChoose →
Coding → CodeCompleteAll (≤10 cycles) → CodeReview (≤3 × [Comment, Modification]) → Test (≤3 ×
[ErrorSummary, Modification]) → EnvironmentDoc → Manual. Loops end on the magic string `<INFO> Finished`
(`chatdev/composed_phase.py` l.213–248).

### Artifacts & handoffs
ChatDev composed phase config:
```json
{ "phase": "CodeReview", "phaseType": "ComposedPhase", "cycleNum": 3,
  "Composition": [
    { "phase": "CodeReviewComment", "phaseType": "SimplePhase", "max_turn_step": 1, "need_reflect": "False" },
    { "phase": "CodeReviewModification", "phaseType": "SimplePhase", "max_turn_step": 1, "need_reflect": "False" } ] }
```
The reviewer prompt dumps the **entire codebase** as `{codes}` and asks for *one* highest-priority comment,
or `"<INFO> Finished"`. Code is recovered from chat by a regex that matches a filename line followed by a fenced
code block (`chatdev/codes.py` l.33, `r"(.+?)\n```.*?\n(.*?)```"`), and whole files are rewritten each turn. "Testing" (`chatdev/chat_env.py`
l.107–150) is **running `python3 main.py` for 3 seconds and grepping stderr for "Traceback"**. On
`ModuleNotFoundError` it runs `pip install <module>` with `shell=True` (l.74–80), which is a supply-chain hazard.

In MetaGPT the handoffs are the typed `instruct_content` of PRD → Design → Tasks messages, plus files in a
project repo (`docs/prd`, `docs/system_design`, `docs/task`, source).

ChatDev v2 (`yaml_instance/ChatDev_v1.yaml`) re-expresses the same chain as a graph of `agent`,
`literal`, `loop_counter` and `passthrough` nodes, **but its agent nodes now get function tools**
(`save_file`, `apply_text_edits`, `read_file_segment`, `search_in_files`, `uv_related:All`).
The engine (`docs/user_guide/en/execution_logic.md`) runs DAG layers in parallel and collapses cycles
(Tarjan SCC) into "super nodes".

### Critical assessment: what worked, what didn't, and why tools plus tests replaced role-play
What **worked** and survives:
- **Structured intermediate artifacts** (PRD → design → task list) with schemas. Typed handoffs beat free
  chat, and they are the ancestor of spec-kit/BMAD specs.
- **Routing by message type** (`cause_by` subscription) as a decoupled pub/sub. This is a good fit for an event bus.
- **Separation of author and reviewer**, **bounded loops** (`cycleNum`, `n_round`), and **budget caps** (`invest`).
- Checkpoint/recover of the team state.

What **didn't** work:
- **Grounding.** The bottleneck in software work is contact with the environment: reading the real repo,
  running real tests, editing precisely. Role-play chat carries no information the model didn't already
  have. ChatDev "tests" by running `main.py` for 3 s, and the reviewer sees a pasted blob.
- **Context blow-up and lossy handoffs.** Whole-codebase prompts (`{codes}`), whole-file rewrites and
  regex extraction don't scale beyond toy greenfield apps (Gomoku, 2048). They cannot do brownfield
  edits, which is what SWE-bench-style tasks are.
- **Consensus and sycophancy.** Two instances of one model role-playing CEO/CTO agree quickly. `"<INFO>
  Finished"` is an easy exit. Persona prose adds tokens, not independent judgment.
- **Waterfall.** PRDs with competitive-analysis quadrants and full class diagrams are generated before any
  code touches reality, and nothing validates them against the codebase.
- **No real verification signal.** Nothing like FAIL_TO_PASS tests. Quality was judged by LLM-graded
  "executability/completeness" metrics.

Why modern agents replaced it: a single **tool-using agent loop** (read, grep, edit, run tests, observe
errors) with **executable feedback** outperformed multi-persona chat on brownfield benchmarks. The
evidence is in the repos themselves: MetaGPT's default team moved to a ReAct `Engineer2` with
Terminal/Editor/git tools and a dynamic TeamLeader, ChatDev moved v1 to a legacy branch and gave its
agents file tools, and BMAD removed its SM/QA personas. The multi-agent structure that remains is
**orchestration of independent tool-using workers** (parallel tasks, fresh-context reviewers), not
conversation between characters.

### Strengths — steal these
1. **Typed pub/sub by artifact type**: stages subscribe to event kinds (`SpecApproved`, `PlanReady`,
   `VerifyFailed`), which decouples stages and makes the pipeline extensible.
2. **Budget as a first-class stop condition** (`NoMoneyException`), with **cycle caps** on every loop.
3. **Structured output nodes** (ActionNode with key/type/instruction/example) for stage outputs.
4. **Graph engine with cycle handling** (DevAll's SCC super-nodes) is a reasonable execution model for user-defined pipelines.

### Weaknesses & tradeoffs
- In-memory message pool with "for debug" history. It is not durable and not replayable. Recovery is a JSON dump
  of pydantic models.
- The SOP is baked into code (`_watch` lists), and dynamic routing (TeamLeader) gives up predictability.
- Persona prompts ("You are Chief Executive Officer…") are pure overhead for coding tasks.
- DevAll is a general no-code agent-graph tool, broad and not opinionated about software delivery (no
  workspaces, PRs or tests as first-class concepts).

### Implications for a Rust framework
- Use an **internal typed event bus** (tokio broadcast/mpsc fed from the durable event log) where stages
  subscribe to event kinds. Do not model agents chatting to each other. Model **artifacts flowing between
  tool-using workers**.
- Every loop needs `max_iterations` and every run needs `budget_usd`/`budget_tokens`. Exhaustion →
  `Blocked(reason)`, never silent success.
- Replace "reviewer chat" with **verification first (deterministic), review second (independent LLM
  lenses)**.

---

## 4. humanlayer/12-factor-agents

### What it is
An essay (`content/factor-*.md`) arguing that production agents are **mostly deterministic software
with LLM steps**, and listing 12 (+1) factors. It has no framework. The examples are pseudo-Python.

### Architecture / process model
The core loop is a **thread of typed events**: the LLM picks the next step, deterministic code executes
it, and the result is appended to the thread (`factor-08-own-your-control-flow.md`):
```python
def handle_next_step(thread: Thread):
  while True:
    next_step = await determine_next_step(thread_to_prompt(thread))
    if next_step.intent == 'request_clarification':
      thread.events.append({ type: 'request_clarification', data: nextStep })
      await send_message_to_human(next_step)
      await db.save_thread(thread)
      # async step - break the loop, we'll get a webhook later
      break
    elif next_step.intent == 'fetch_open_issues':
      ...
      continue
```

### Artifacts & handoffs — which factors apply to an orchestrator
| Factor | Applies to our orchestrator? | How |
|---|---|---|
| 1 NL → tool calls | Yes, at intake | Work-item text → typed `Intent{kind, scope, route}` via structured output |
| 2 Own your prompts | **Yes** | Templates versioned in repo, overridable (org → repo → user), rendered snapshot hash per step |
| 3 Own your context window | **Yes** | Per-stage context policy (what artifacts, what diff, what repo map); never whole transcripts |
| 4 Tools = structured outputs | Yes | Stage outputs are schema-validated (spec/plan/findings JSON + MD render) |
| 5 Unify execution + business state | **Yes** | One append-only event log is both. Projections are derived |
| 6 Launch/pause/resume APIs | **Yes** | `start_run`, `signal(run, approve)`, `resume` via CLI/HTTP/webhook |
| 7 Contact humans with tool calls | **Yes** | Expose a `request_human_input` MCP tool to every agent backend; it becomes a gate event in the UI/Slack |
| 8 Own your control flow | **Yes (critical)** | The engine, not the LLM, decides stage transitions. LLMs decide within a stage |
| 9 Compact errors | Yes | Verification failures are summarised (failing test names + trimmed traces) back to the implementer, with retry caps |
| 10 Small, focused agents | **Yes** | One task per agent session, fresh-context reviewers, specs ≤ ~1.5k tokens |
| 11 Trigger from anywhere | Yes | GitHub/Linear/Slack/cron/CLI → work items |
| 12 Stateless reducer | **Yes** | `state = events.fold(apply)`, a pure transition function that is unit-testable without LLMs |
| 13 Pre-fetch context | Yes | Precompute repo map, failing test output and related files before the agent starts |

### Strengths — steal these
1. **Factor 5 + 12 = event sourcing.** The thread *is* the state, trivially serialisable, forkable,
   renderable to UI. This matches our real-time UI requirement exactly.
2. **Factor 7**: humans as a tool with a typed request (`urgency`, `format: yes_no|multiple_choice`,
   `choices`) → pause → webhook resume.
3. **Factor 8**: break the loop on high-stakes actions (deploy, merge) for approval. This is the model for gates.

### Weaknesses & tradeoffs
- Written for teams that **own the inner agent loop** (direct API calls). A software factory mostly
  drives *third-party* coding agents (Claude Code, Codex, etc.) whose inner loop we don't own. The
  factors therefore apply at the **outer loop** (stage boundaries), plus hooks/MCP tools to reach inside.
- Factor 5's "infer execution state from the context window" doesn't hold for a multi-stage,
  multi-agent system. We need explicit run/step state, derived from events rather than prompts.
- No guidance on concurrency, isolation, retries of side effects, or evaluation.

### Implications for a Rust framework
- Engine core = `fn apply(state: &RunState, ev: &Event) -> RunState` + `fn decide(state) -> Vec<Command>`,
  pure and property-tested. Effects (spawn agent, run tests, open PR) are executed by workers and
  their results appended as events.
- Ship a **factory MCP server** that every agent session gets: `request_human_input`,
  `report_progress`, `record_finding`, `append_task`, `read_artifact`. This gives structured outputs and
  human contact regardless of the agent vendor.

---

## 5. SWE-bench/SWE-bench — evaluation harness design

### What it is
The standard benchmark for "given a real GitHub issue and repo at a base commit, produce a patch that
makes the hidden tests pass". The harness (`swebench/harness/`) evaluates predictions (patches) in
per-instance Docker containers.

### Architecture / process model
- **Instance** (`swebench/types.py`):
```python
class SWEbenchInstance(TypedDict):
    repo: str
    instance_id: str
    base_commit: str
    patch: str            # gold fix (non-test hunks of the merged PR)
    test_patch: str       # test hunks of the merged PR (hidden from the model)
    problem_statement: str
    hints_text: str
    created_at: str
    version: str
    FAIL_TO_PASS: str     # tests failing before, passing after the gold patch
    PASS_TO_PASS: str     # tests passing before and after (regression guard)
    environment_setup_commit: str
```
- **Collection** (`swebench/collect/`): list the PRs of a repo, keep merged PRs that reference an issue,
  and split the diff into test and non-test hunks (`collect/utils.py::extract_patches`). Candidates are
  validated by running tests before and after the gold patch to derive F2P/P2P.
- **Task repo** (new source of truth, `swebench/task/repo.py`), one directory per instance:
  `task.yaml, tests.json, problem_statement.md, hints.md, gold.patch, test.patch, eval.sh, Dockerfile,
  test_assets/`. `swebench/task/checks.py` lints it (required files, markers, image naming).
- **Images**: one image per instance, `sweb.eval.<arch>.<instance_id>:<tag>`
  (`swebench/image_builder/image_spec.py`). `_build_before_eval` (`run_evaluation.py` l.643) builds
  locally so that *"a stale published image can [not] report a clean pass"*.
- **Run** (`run_evaluation.py::run_instance`): start container → copy the prediction patch → try
  `git apply --verbose`, then `--3way`, then `--reject`, then `patch --batch --forward --fuzz=5` (l.54–59) →
  run `/eval.sh` with a timeout. The eval script checks out test files at the base commit, applies
  `test.patch`, runs the listed tests between `>>>>> Start/End Test Output` markers and records the exit
  code → parse with a per-language log parser (`harness/log_parsers/{python,rust,go,java,js,…}.py`) →
  `report.json`.
- **Grading** (`harness/grading.py` l.309):
```python
def get_resolution_status(report):
    f2p = compute_fail_to_pass(report); p2p = compute_pass_to_pass(report)
    if f2p == 1 and p2p == 1:       return ResolvedStatus.FULL.value
    elif 0 < f2p < 1 and p2p == 1:  return ResolvedStatus.PARTIAL.value
    else:                           return ResolvedStatus.NO.value
```
- **Robustness details worth copying**:
  - `SUITE_RAN` regex (l.26–46) guards against "runner never started, empty status map scored as pass".
  - `infra_failure.py` classifies environment failures (OOM, docker daemon, DNS, display) as
    `environment` vs `ambiguous` *post hoc*, advisory only, without changing the denominator.
  - The report separates `resolved / unresolved / error / empty_patch / incomplete / infra_failure / ambiguous`
    (`harness/reporting.py`).

### Artifacts & handoffs
Prediction in (`instance_id`, `model_name_or_path`, `model_patch`), `report.json` + logs per instance
out, and an aggregate run report. The task repo tree is the reproducible artifact.

### Strengths — steal these
1. **F2P / P2P grading**: objective, cheap and hard to game if tests are hidden. P2P catches regressions.
2. **Hidden tests + base commit + pinned environment image** make runs reproducible and comparable.
3. **Task-repo layout** is a plain-files dataset that can be version-controlled, diffed and linted.
4. **Patch-apply fallback ladder**, **timeouts**, **"did the suite actually run" checks**, and
   **infra vs model failure separation**.
5. **Multi-language log parsers** behind a registry.

### Weaknesses & tradeoffs
- F2P tests can encode details not stated in the issue (exact names/APIs). SWE-bench *Verified* needed
  human screening for underspecified issues and over-specific tests. Expect the same on user repos.
- **Contamination**: public repos/PRs are in training data. A time split (tasks after the model cutoff)
  matters.
- Flaky tests poison P2P. Validation has to run tests multiple times.
- Per-instance images are large (disk/cache cost). Environment setup is the hardest part of adding a repo.
- Grades only test outcomes, not code quality, PR size, spec adherence or cost.

### Implications for a Rust framework
- Reuse the **same sandbox/workspace abstraction** for production runs and evals (container + base
  commit + env image keyed by `(repo, env_setup_commit, lockfile hash)`).
- Adopt **F2P as a production gate** for bug work items (a reproduction test must fail before and pass after)
  and **P2P as the regression gate** (the baseline test set passing before must still pass).
- See "Built-in eval harness" below.

---

## 6. Durable execution (Temporal) — concepts and Rust status

### What it is
Temporal runs **workflows** (deterministic orchestration code) whose every decision and result is
recorded in an **event history** on the server. After a crash, a worker **replays** the workflow code
against the history to rebuild in-memory state, then continues. Side effects happen only in
**activities**, which are retried under policy. `temporalio/sdk-core` is the Rust core shared by the TS,
Python, .NET and Ruby SDKs, and it now ships a **Rust SDK `temporalio-sdk` 1.0.0** (`crates/sdk`) plus
`temporalio-workflow` (`crates/workflow`, native + WASM components).

### Architecture / process model
- **Workflow task loop** (`ARCHITECTURE.md`): the server sends `HistoryEvent`s. Core's per-command
  **state machines** (`crates/sdk-core/src/worker/workflow/machines/`: `activity_state_machine.rs`,
  `timer_state_machine.rs`, `signal_external_…`, `child_workflow_…`, `update_state_machine.rs`,
  `patch_state_machine.rs`, …) turn them into `WorkflowActivation` jobs for the language layer. User code
  runs until it blocks and returns `WorkflowCommand`s. During replay, commands are **matched against
  history** and a mismatch is a nondeterminism error (`internal_flags.rs`:
  `IdAndTypeDeterminismChecks`).
- **Determinism rules**: no wall clock, randomness, I/O or threads in workflow code. Use SDK
  timers, side-effects and version markers (`patch_state_machine.rs`) when code changes under running
  workflows.
- **Activities**: retry policy (initial interval, backoff coefficient, max interval, max attempts,
  non-retryable error types). Timeouts: schedule-to-start, start-to-close, schedule-to-close and
  **heartbeat** (for long-running work, with progress details returned on retry).
- **Signals** (async input: approve, cancel, new info), **queries** (read-only state), **updates**
  (validated synchronous mutation), **timers/durable sleep**, **child workflows**, **continue-as-new**
  (to truncate history). Core tracks history size and `suggest_continue_as_new_reasons`
  (`machines/workflow_machines.rs`). Temporal documents hard limits of roughly 51,200 events / 50 MB per
  history and ~2 MB per payload.
- **Task queues + sticky caching** (`arch_docs/sticky_queues.md`): workers poll named queues. Cached
  workflow state avoids full replays.

### Artifacts & handoffs
Rust SDK workflow (`crates/sdk/README.md`):
```rust
#[workflow_methods]
impl GreetingWorkflow {
    #[run]
    async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<String> {
        let name = ctx.state(|s| s.name.clone());
        let greeting = ctx.execute_activity(
            MyActivities::greet, name,
            ActivityOptions::start_to_close_timeout(Duration::from_secs(10))
        )?.await?;
        Ok(greeting)
    }
}
```

### Strengths — steal these (concepts, not necessarily the dependency)
1. **Workflow/activity split**: pure orchestration decisions vs retried side effects, each with explicit
   timeouts and a **heartbeat** (ideal for multi-hour agent sessions).
2. **Signals / queries / updates** map directly onto *human gate approvals*, *UI reads* and *operator
   edits* (e.g. change budget mid-run).
3. **Event history as the source of truth**, with nondeterminism detection and **versioning markers** for
   evolving pipelines under in-flight runs.
4. **Task queues with capability routing** (e.g. `gpu`, `macos`, `docker`) for worker pools.

### Weaknesses & tradeoffs
- **Operational weight**: a Temporal cluster (server + persistence + visibility) is a big ask for a
  local-first dev tool, and single-binary deployment is not possible.
- **The determinism tax**: every pipeline change needs patch markers or worker versioning. That is hard for a
  framework whose users edit pipelines in YAML.
- **History/payload limits**: agent transcripts, diffs and logs cannot live in history. They go to blob
  storage anyway, and streaming token output is out of scope.
- The value of *code replay* is low here: our orchestration is a **data-defined graph** (pipeline YAML),
  not arbitrary user code, so we can persist the interpreter's state directly.

Alternatives checked:
- **Restate** (Rust, single binary, journal-based durable execution, Rust SDK) is licensed **BSL 1.1**,
  with an additional use grant that forbids offering a "Public Restate Platform Service". That is a
  licensing concern for an open-source framework dependency and for hosted offerings.
- Embedded Rust options exist but are young: **duroxide** (Microsoft, in-process on Tokio, pluggable
  provider with built-in SQLite) and **obelisk** (WASM workflows, SQLite, pre-release).

### Implications for a Rust framework
Copy Temporal's *semantics* (activities with retry/timeouts/heartbeats, signals, queries, timers,
idempotency, versioned pipelines) into a **small embedded event-sourced engine on SQLite**. Do not
depend on Temporal or Restate. Leave an adapter seam in case an enterprise user wants Temporal later.
Details are in "Durability recommendation".

---

## Recommended default pipeline

Principles, from the evidence above:
- Orchestrate **independent tool-using workers over typed artifacts**, not role-play.
- **Right-size after investigation.**
- **Deterministic verification before LLM review.**
- **Every loop capped. Every exit typed.**
- Humans are reached through tools and gates.

| # | Stage | Worker / role | Output artifact (typed, stable IDs) | Gate (default) |
|---|---|---|---|---|
| 0 | **Intake & triage** | Triage agent (read-only repo access) | `Intent{problem, approach, kind: feature/bug/refactor/chore, size: trivial/standard/epic, route}` | auto |
| 1 | **Specify + Clarify** | Planner (read-only) | `spec.md`: intent contract (Problem/Approach, Always/Never, I/O & edge matrix), FR-###, AC as Given/When/Then with IDs, ≤3 `NEEDS_CLARIFICATION`, ≤5 structured questions via `request_human_input` | **human approve** for `standard`/`epic` when open questions or high risk. Auto for trivial |
| 2 | **Plan + Analyze** | Planner; Analyzer (read-only, fresh context) | `plan.md`: code map, tasks `T###` with file paths, deps DAG, `[P]` parallel flags, verification commands; constitution check; coverage matrix AC→tasks | auto if analyze has no CRITICAL. Human for epics |
| 3 | **Workspace provisioning** | Engine (no LLM) | worktree/container per task or task group, `baseline_commit`, env image hash, baseline test snapshot (for P2P) | auto |
| 4 | **Implement** | Implementer agent per task (fresh session, ≤ ~1.5k-token task spec + pre-fetched context) | commits on the task branch; append-only implementation notes; for bugs a **reproduction test that fails first** (F2P) | loop until verify passes, cap N |
| 5 | **Verify** | Engine (deterministic) | build/lint/typecheck/tests report; F2P (new/target tests pass), P2P (baseline tests still pass); flake re-runs | **hard gate**, never LLM-judged |
| 6 | **Review** | Parallel reviewer lenses with restricted context: blind-diff, edge-case, verification-gap, intent-alignment (verbatim intent + diff) | `Finding{verdict, route: patch/bad_spec/intent_gap/defer/reject, evidence, location}` + triage log | patch → back to 4 (same session). bad_spec → amend plan, revert, back to 4. intent_gap → **human**. Cap 3–5 loops → `Blocked` |
| 7 | **Converge** | Auditor (read-only) | coverage report: every FR/AC → evidence (test/file). Gaps → appended tasks | auto loop back to 4, cap |
| 8 | **Integrate & PR** | Integrator (engine + small LLM step) | rebased branch, PR body generated from spec + verification evidence + triage log + deferred list | **human merge** (no auto-merge by default) |
| 9 | **Learn** | Retro agent (post-merge / post-block) | proposed `AGENTS.md`/constitution edits (human-approved); the finished item is added to the eval task pool | human |

Trivial route: 0 → 4 → 5 → 6 (quick lens) → 8. Bug route: specify = reproduce and assess (F2P gate mandatory).

**Must be configurable** (a pipeline YAML in the style of spec-kit's workflow DSL, with nested resume):
- which stages run and in what order
- gate policy per stage: `auto | human | expr` (e.g. `size == "epic" or risk == "high"`)
- templates per stage, through an override stack: built-in → org → repo → user, with a snapshot hash recorded
- review lens set, including external tools and different models
- loop caps, budgets (tokens/$/wall-clock per run and per stage), and parallelism limits
- agent backend and model per role
- constitution/invariants file, and the verification commands
- routing thresholds, and whether tasks fan out to parallel workspaces

**Non-configurable invariants**:
- verify is deterministic
- every exit is a typed terminal state (`Done | Blocked(reason) | Failed(reason) | Cancelled`)
- every loop has a cap
- all events are persisted

---

## Durability recommendation

**Build our own event-sourced durable state machine on SQLite (embedded, single binary), with
Temporal-like semantics but without deterministic code replay.** Keep a storage trait for Postgres
later. Do not depend on Temporal or Restate.

Reasoning:
1. **The orchestration is data, not code.** Pipelines are YAML graphs interpreted by our engine. We can
   persist the interpreter state (`run → step path → attempt`) directly, so Temporal's
   biggest value (replaying arbitrary user code) and biggest cost (the determinism/versioning tax) both
   disappear. Pipeline versioning becomes "pin the pipeline definition hash per run".
2. **The workload is a few long, non-deterministic, side-effectful activities** (agent sessions of
   minutes to hours, container builds, test runs, PR creation) and a small number of decisions. A simple
   `events` table + projections + leases handles thousands of concurrent runs on one SQLite writer (WAL).
3. **Local-first, single binary.** Temporal needs a cluster. Restate is an extra BSL-licensed process.
   duroxide/obelisk are promising but young, and would still leave us to own the domain model.
4. **The event log doubles as the real-time UI feed** (SSE/WebSocket subscribers tail it) and as audit/eval
   data (12-factor factors 5 and 12).

Design sketch:
- **Tables**:
  - `events(run_id, seq, ts, kind, payload_json, causation_id, idempotency_key)`, append-only, unique `(run_id, seq)`.
  - Projections (`work_items`, `runs`, `steps`, `gates`, `findings`) updated **in the same
    transaction** as the append.
  - `timers(run_id, fire_at, kind)` for durable sleep and timeouts.
  - `leases(step_id, worker_id, lease_until, heartbeat_at)`.
  - Blobs (transcripts, diffs, logs) go on disk/object storage, referenced by content hash, and never inline.
- **Activities** carry `retry_policy` (backoff, max attempts, non-retryable kinds) and timeouts
  (schedule-to-start, start-to-close, **heartbeat**). An agent session heartbeats through the adapter. A lost
  lease leads to re-dispatch: resume the agent session if the backend supports it, else restart the step
  from the last **git checkpoint commit** in the workspace.
- **External side effects** use intent → result event pairs with **idempotency keys** (e.g. PR keyed by
  branch name), plus **reconciliation on recovery** ("does branch/PR X already exist?"). Use an outbox for webhooks.
- **Signals** (gate approve/reject, human answers, PR review comments via webhook, cancel, budget change)
  are appended events that wake the run. **Queries** are projection reads.
- **Pure core**: `apply(state, event)` and `decide(state) -> commands` are LLM-free, property-tested, and can be
  replayed from the log for debugging ("time-travel" in the UI).
- **Escape hatch**: the engine API mirrors workflow/activity/signal/query, so a `TemporalBackend` could be
  added for users who already run Temporal. It is not on the critical path.

---

## Built-in eval harness

A "factory bench" that measures the whole pipeline on **historical issues from the user's own repos**,
built on SWE-bench's design.

1. **Mine tasks** (`factory bench mine <repo>`):
   - Take merged PRs linked to issues (or with good descriptions) that touch tests.
   - Split the diff into `gold.patch` / `test.patch`.
   - Record `base_commit` and an environment spec (Dockerfile or devcontainer + lockfile hash).
   - Prefer PRs **after the model's training cutoff** and keep a time-split holdout.
2. **Validate**:
   - Run the tests at base + `test.patch` (the F2P candidates must fail), then at base + gold + `test.patch`
     (they must pass), **3× each** to drop flaky tests.
   - Derive `FAIL_TO_PASS`/`PASS_TO_PASS`.
   - Have an LLM screen (and, optionally, a human) reject underspecified issues and tests that encode
     unstated details, as SWE-bench *Verified* did.
   - Store the result in the SWE-bench **task-repo layout** (`tasks/<id>/{task.yaml, tests.json,
     problem_statement.md, gold.patch, test.patch, eval.sh, Dockerfile}`), lintable and versioned.
3. **Run**:
   - Feed `problem_statement` as a work item through the *real* pipeline configuration in **eval mode**:
     tests hidden, human gates auto-answered by a scripted policy or "simulated human", and no PR creation.
   - Use the same sandbox and workspace code as production.
4. **Grade** in a fresh container:
   - Apply the candidate diff with test-file hunks stripped, using the patch fallback ladder. Then apply
     `test.patch`, run `eval.sh`, parse the output, and score `FULL/PARTIAL/NO`.
   - Guard against "suite never ran".
   - Classify infra failures separately, without changing the denominator.
5. **Metrics per run**:
   - resolve rate, P2P regressions, $ and tokens, wall-clock
   - human-gate count, review loop count, `Blocked` rate by reason
   - diff size vs gold
   - reviewer precision/recall (did lenses flag defects that the hidden tests later exposed?)
   - spec quality (clarifying questions asked, and whether they were needed)
6. **Use it as CI for the factory itself**:
   - Every change to prompts, templates, models or pipeline config runs the bench (a small smoke subset
     per PR, the full set nightly), recording the snapshot hashes.
   - Report paired comparisons with repeated runs (pass@1 mean ± CI, pass@k), because variance between
     runs is large.
   - Surface the results as a dashboard in the UI.
7. **Shadow mode** (optional): when a human closes a new issue with a PR, replay it through the factory
   and diff the outcome. This continuously adds fresh, uncontaminated tasks to the pool.
