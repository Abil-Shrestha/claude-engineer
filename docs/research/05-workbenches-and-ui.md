# 05 — Workbenches & UI: vibe-kanban, claude-squad, Sculptor

> Research group "Workbenches & UI". Repos were shallow-cloned on 2026-09-24 and read at source level; nothing was built or run.
> File paths are relative to each repo's root. "LOC" means raw line counts, not SLOC.

## Group overview

These three projects are *human-in-the-loop workbenches*, not autonomous factories. In each one, a person starts agents in isolated git working copies, watches them, answers their questions, reviews the diff, and ships. They sit at three levels of integration depth:

- **claude-squad** has no integration at all. It runs any CLI inside tmux and scrapes the screen to guess the agent's state.
- **vibe-kanban (VK)** is the most relevant to us: a **Rust (axum + sqlx/SQLite) backend with a React frontend**. It drives nine agent CLIs through four protocol families (stream-JSON over stdio, JSON-RPC, ACP, and HTTP+SSE) and normalizes all of them into one conversation model. That model streams to the browser as RFC 6902 JSON Patches over WebSockets.
- **Sculptor** is a Python/FastAPI + Electron app. It integrates deeply with two harnesses (Claude Code and Pi), and adds a *push-based* status-signal API so any terminal agent can report busy, idle, or waiting without screen-scraping.

All three bet on git worktrees for isolation. All three run agents with permissions bypassed by default and with no OS-level sandbox. None of them treats automated verification (tests or CI) as a hard gate. The interesting differences are in how each one detects agent state, how it persists and replays history, and how it pulls the human's attention to the right place.

---

## 1. BloopAI/vibe-kanban

### What it is
- **What:** a local web app, launched with `npx vibe-kanban`, for planning work, running coding agents in worktree "workspaces", reviewing diffs, and opening PRs.
- **Stack:** Rust 2024 edition. About 109k Rust LOC across 33 crates, of which roughly 24k is the hosted-cloud crate `crates/remote`. About 108k TS/TSX LOC in `packages/`. Apache-2.0.
- **Version and activity:** v0.1.45; last commit 2026-09-19.
- **Status:** the README headline reads **"Vibe Kanban is sunsetting."** The code shows that the hosted cloud is shutting down: `create_checkout_session` returns HTTP 410 with "Vibe Kanban Cloud is shutting down" (`crates/remote/src/routes/billing.rs`). The kanban project page has been replaced by an export-only `ProjectSunsetPage` (`packages/web-core/src/pages/kanban/ProjectKanban.tsx`, `ProjectSunsetPage.tsx`).
- **Why it is sunsetting:** the stated reason is not in the repo, and the blog post (vibekanban.com/blog/shutdown) was blocked by this sandbox's egress proxy.
- **What the code does show:** the issue and kanban layer had moved to the hosted cloud (Postgres + ElectricSQL, `crates/remote/AGENTS.md`). Local tasks had been decoupled from workspaces (migrations `20260113144821_remove_shared_tasks.sql` and `20260217120312_remove_task_fk_from_workspaces.sql`). So when the cloud shut down, the product's namesake kanban died with it, while local workspaces keep working.
- **Lesson:** never make the planning surface depend on a hosted service.

### Architecture
It is a Cargo workspace (`Cargo.toml`). The key crates:

| Crate | Role |
|---|---|
| `server` | axum 0.8 HTTP/WS API. Embeds the built SPA with `rust-embed` (`crates/server/src/routes/frontend.rs`). Holds the `generate_types` binary. |
| `executors` | Agent drivers and log normalizers. At 26k LOC it is the heart of the system. |
| `services` | `ContainerService` trait (orchestration), `EventService` (DB→UI change feed), diff streaming, approvals, notifications, PR monitor, config with versioned migrations (`services/src/services/config/versions/v1..v8`). |
| `local-deployment` | Concrete `ContainerService`: process spawning, exit monitors, auto-commit, PTY terminals (`portable-pty`). |
| `db` | sqlx 0.8 SQLite models and 76 migrations. |
| `git`, `worktree-manager`, `workspace-manager` | Hybrid `git2` + git CLI. Multi-repo workspaces. |
| `git-host` | GitHub via the `gh` CLI and Azure via the `az` CLI. |
| `mcp` | `rmcp` MCP server exposing VK to agents, with "global" and "orchestrator" modes. |
| `preview-proxy` | Subdomain reverse proxy for dev-server previews that injects devtools scripts. |
| `relay-*`, `embedded-ssh`, `trusted-key-auth`, `tauri-app` | Remote access over WebRTC, yamux, an SSH server (`russh`), SPAKE2 pairing, and a Tauri 2 desktop shell. |
| `review` | A separate CLI that uploads a PR to VK's cloud to get a narrative review (`crates/review/src/main.rs`). |

The `Deployment` trait (`crates/deployment/src/lib.rs`) bundles every service so that the local and cloud builds can share route code.

### Core abstractions / domain model
The DB models live in `crates/db/src/models/`. Migration `20251216142123_refactor_task_attempts_to_workspaces_sessions.sql` records how the model evolved from "task attempts" to workspaces and sessions.

- **Repo:** a registered local repository with scripts attached: `setup_script`, `cleanup_script`, `archive_script`, `dev_server_script`, `copy_files` globs, `parallel_setup_script`, `default_target_branch`, and `default_working_dir` (`repo.rs`).
- **Workspace:** a branch plus a `container_ref` (the directory on disk), with `archived`, `pinned`, and `worktree_deleted` flags. It contains one worktree per repo (`workspace_repo.rs`).
- **Session:** one agent conversation inside a workspace (`executor`, `agent_working_dir`). A workspace can hold several sessions.
- **ExecutionProcess:** any spawned process. `run_reason` is one of setupscript, codingagent, devserver, cleanupscript, or archive. It also carries `status`, `exit_code`, and a `dropped` soft-delete flag. It stores the serialized `executor_action` as JSON.
- **ExecutionProcessRepoState:** `before_head_commit` and `after_head_commit` per repo per process. Rewind and diff-per-turn are built on this.
- **CodingAgentTurn:** `agent_session_id` (the CLI's own session id, used for resume), `prompt`, `summary`, and `seen` (unread tracking).
- **Merge:** a direct merge or a PR (`merge.rs`). **Scratch:** typed JSON blobs for drafts, queued messages, notes, and UI preferences, all persisted server-side (`scratch.rs`).
- **Executor side** (`crates/executors/src/executors/mod.rs`):
  - `CodingAgent` is an `enum_dispatch` enum over the nine agents.
  - The `StandardCodingAgentExecutor` trait has `spawn`, `spawn_follow_up(session_id, reset_to_message_id)`, `spawn_review`, `normalize_logs(msg_store, worktree)`, `discover_options` (streams JSON patches), and `get_availability_info`.
  - `BaseAgentCapability` covers SessionFork, SetupHelper, and ContextUsage.
  - `SpawnedChild` wraps a process-group child plus an optional executor→container `exit_signal` and a container→executor `CancellationToken`.

### Orchestration model
- **Action chains:** an `ExecutorAction { typ, next_action: Option<Box<ExecutorAction>> }` is a linked list (`crates/executors/src/actions/mod.rs`). `start_workspace` builds the chain setup scripts → coding agent → cleanup script. Setup scripts run sequentially, or all in parallel when every repo sets `parallel_setup_script` (`crates/services/src/services/container.rs::start_workspace`).
- **Exit handling:** `spawn_exit_monitor` (`crates/local-deployment/src/container.rs`) races the OS exit against the executor's exit signal. This matters because some agents, such as Codex's app-server, never exit on their own.
- **After a run exits**, the monitor:
  1. records completion;
  2. auto-commits leftover changes (`try_commit_changes`);
  3. **skips the cleanup script if the agent produced no commits**;
  4. starts the next action;
  5. otherwise drains a **queued follow-up message** (`queued_message_service`) or finalizes with a notification.
- **Follow-ups** resume the agent's own session id (`CodingAgentFollowUpRequest.session_id`).
- **Rewind:** `reset_session_to_process` resets every repo to the target process's `before_head_commit`, soft-drops later processes, and resumes Claude with `--resume-session-at <message uuid>`. The result is "edit an earlier message and time-travel".
- **Agent-driven fan-out:** there is no scheduler or DAG, but the `mcp` crate's orchestrator mode lets an agent create workspaces and sessions and run turns in *other* sessions (`crates/mcp/src/task_server/tools/sessions.rs`). It refuses to run a turn in the orchestrator's own session.

### State & persistence
- **Database:** SQLite at `asset_dir/db.v2.sqlite`, with `journal_mode=DELETE` rather than WAL (`crates/db/src/lib.rs`). Migrations are embedded with `sqlx::migrate!`. There is a Windows-only checksum self-heal for line-ending mismatches. Queries are compile-time checked (`pnpm run prepare-db` generates the offline data).
- **Raw logs:** every process's `LogMsg` stream is appended as JSONL to `asset_dir/sessions/<session>/processes/<exec>.jsonl` (`crates/utils/src/execution_logs.rs`), with a fallback to the legacy `execution_process_logs` table.
- **Live logs:** held in an in-memory `MsgStore` per process. It is a `broadcast` channel with capacity 100k plus a history capped at 100 MB (`crates/utils/src/msg_store.rs`). Late subscribers get `history_plus_stream()`.
- **Replay:** normalized logs are **not persisted as a canonical form**. For a finished process, `stream_normalized_logs` loads the raw JSONL, **recreates the worktree if it was deleted** (so that relative paths resolve), and re-runs the executor's normalizer (`container.rs::stream_normalized_logs`).
- **Config:** a JSON file with versioned struct migrations (v1→v8).

### Isolation & workspaces
- **Layout:** worktrees live under `<vk temp dir>/worktrees` or a user override. The override always gets an app-owned subfolder `.vibe-kanban-workspaces`, so orphan cleanup never touches user folders (`crates/worktree-manager/src/worktree_manager.rs::get_worktree_base_dir`). A workspace directory holds `<workspace>/<repo_name>` worktrees, which allows multi-repo workspaces. A legacy single-worktree layout is migrated automatically (`crates/workspace-manager/src/workspace_manager.rs::migrate_legacy_worktree`).
- **Git operations:** `git worktree add`, `remove`, `move`, and `prune` use the **git CLI**, because "the Git CLI is more robust than libgit2 for mutable worktree operations" and it honors sparse-checkout. Reads and diffs use `git2`. Creation is serialized by per-path async mutexes (`WORKTREE_CREATION_LOCKS`) and retried after metadata cleanup. Failed creation of a multi-repo workspace rolls back any worktrees already created.
- **Other lifecycle:** gitignored files such as `.env` are copied in via the `copy_files` globs (`crates/local-deployment/src/copy.rs`). Orphaned and expired worktrees are garbage-collected. `ensure_container_exists` recreates a worktree on demand when it is needed again (cold restart, log replay).
- **No sandboxing:** agents run on the host with the user's credentials.

### Agent integration

| Agent | Exact invocation (from code) | Protocol | Normalizer |
|---|---|---|---|
| Claude Code (`executors/claude.rs`) | `npx -y @anthropic-ai/claude-code@2.1.119 -p --verbose --output-format=stream-json --input-format=stream-json --include-partial-messages --replay-user-messages` plus `[--model] [--effort] [--agent]`. With plan or approvals enabled: `--permission-prompt-tool=stdio --permission-mode=bypassPermissions`; otherwise `--disallowedTools=AskUserQuestion`. The default profile adds `--dangerously-skip-permissions`. Follow-up: `--resume <id> [--resume-session-at <uuid>]`. The "CCR" agent is this same executor with `npx -y @musistudio/claude-code-router@1.0.66 code`. | Claude SDK **control protocol** over stdio (`claude/protocol.rs`, `claude/client.rs`): `initialize` with hook callbacks, `set_permission_mode`, the user message, then `can_use_tool` and `hook_callback` requests | `ClaudeLogProcessor` (3.3k LOC) |
| Amp (`amp.rs`) | `npx -y @sourcegraph/amp@latest --execute --stream-json [--dangerously-allow-all]`. The prompt goes on stdin, then EOF. Follow-up: `threads continue <id>`. Note the unpinned `@latest`. | Claude-compatible stream-JSON | Reuses `ClaudeLogProcessor` with `HistoryStrategy::AmpResume` |
| Codex (`codex.rs`) | `npx -y @openai/codex@0.124.0 app-server [--oss]` | JSON-RPC using OpenAI's own `codex-app-server-protocol` crate, pinned by git tag `rust-v0.124.0` (`crates/executors/Cargo.toml`). Calls `thread_start` or `thread_fork` then `turn_start`; also has a native review, `start_review` (`codex/review.rs`). Sandbox and approval policy are mapped onto `ThreadStartParams`. | `codex/normalize_logs.rs` (2.9k LOC) |
| Gemini CLI (`gemini.rs`) | `npx -y @google/gemini-cli@0.29.3 [--model] [--yolo --allowed-tools run_shell_command] --experimental-acp` | **ACP** via the `agent-client-protocol` 0.8 crate (`acp/harness.rs`) | Shared `acp/normalize_logs.rs` |
| Qwen Code (`qwen.rs`) | `npx -y @qwen-code/qwen-code@0.9.1 [--model] [--yolo] --acp` | ACP | Shared |
| GitHub Copilot (`copilot.rs`) | `npx -y @github/copilot@1.0.83 [--allow-all-tools] [--allow-tool/--deny-tool X] [--add-dir] [--disable-mcp-server] --acp` | ACP | Shared |
| Cursor (`cursor.rs`) | `cursor-agent -p --output-format=stream-json [--force] [--trust] [--model]`. Follow-up: `--resume <id>`. | stream-JSON | In `cursor.rs` |
| OpenCode (`opencode.rs`, `opencode/sdk.rs`) | `npx -y opencode-ai@1.4.7 serve --hostname 127.0.0.1 --port 0` | **HTTP + SSE**: `POST /session`, `/session/{id}/message`, `/fork`, `/abort`, `/command`. Events arrive over `eventsource-stream`; the driver waits for `session.idle` before it finishes. | `opencode/normalize_logs.rs` |
| Droid (`droid.rs`) | `droid exec --output-format stream-json (--auto low\|medium\|high \| --skip-permissions-unsafe) [--model] [--reasoning-effort]`. Follow-up: `--session-id <id>`. | stream-JSON | `droid/normalize_logs.rs` |

Docs and code disagree on the count. The README advertises "10+ agents" including CCR, but the `CodingAgent` enum has nine variants (plus `QaMock` behind the `qa-mode` feature), and CCR is a flag on the Claude executor.

**How normalization works.** This is the best design in the repo.
- **One byte stream per agent.** Whatever the wire protocol (JSON-RPC, ACP, or SSE), the driver re-serializes every event and approval as JSON lines into a *fresh stdout pipe* that it installs on the child (`create_stdout_pipe_writer`, `crates/executors/src/stdout_dup.rs`). Downstream there is therefore always one uniform byte stream in the `MsgStore`, and the raw JSONL log is a faithful, replayable record of the whole protocol exchange, including approvals (`ClaudeJson::ApprovalRequested` / `ApprovalResponse`).
- **Normalizers emit JSON Patches.** Each executor's `normalize_logs` spawns tasks that read `stdout_lines_stream()` and push `LogMsg::JsonPatch` operations that add or replace `/entries/{i}` (`crates/executors/src/logs/utils/patch.rs`). A monotonic index comes from `EntryIndexProvider::start_from(msg_store)`.
- **The common model** (`crates/executors/src/logs/mod.rs`):
  - `NormalizedEntryType` is one of UserMessage, UserFeedback, AssistantMessage, ToolUse{tool_name, action_type, status}, SystemMessage, ErrorMessage, Thinking, Loading, NextAction, TokenUsageInfo{total_tokens, model_context_window}, or UserAnsweredQuestions.
  - `ActionType` is a UI-oriented taxonomy: FileRead, FileEdit{changes: Write/Delete/Rename/Edit{unified_diff}}, CommandRun{result, category}, Search, WebFetch, Tool{args, result}, TaskCreate (subagent), PlanPresentation, TodoManagement, AskUserQuestion, or Other.
  - `ToolStatus` includes `PendingApproval{approval_id}`, Denied, and TimedOut, so an approval request renders *inline* on the tool card.
  - Shell commands are classified into Read, Search, Edit, or Fetch by `CommandCategory::from_command`, which catches redirections, `sed -i`, and similar edits (`logs/utils/shell_command_parsing.rs`).

### Verification & quality gates
The only gate that exists is *git hygiene*:
- **Stop hook for uncommitted work.** For Claude, a `Stop` hook callback (`STOP_GIT_CHECK_CALLBACK_ID`) returns `{"decision":"block","reason": commit_reminder_prompt + git status}` when any repo is dirty, which forces the agent to commit before it may stop (`claude/client.rs::on_hook_callback`, `crates/executors/src/env.rs::check_uncommitted_changes`).
- **Automatic commit.** Codex gets similar handling (`commit_reminder` is passed to its client), and `try_commit_changes` auto-commits anything left over after a run.

Everything else is optional scaffolding:
- **Review requests.** `ReviewRequest` starts an agent in reviewer mode with a prompt of the form "Use `git diff <base>..HEAD`" (`crates/executors/src/executors/mod.rs::build_review_prompt`). Codex uses its native review target instead.
- **Human review.** A person reviews the diff and adds inline comments, which are compiled into a "## Review Comments (N)" markdown follow-up (`packages/web-core/src/shared/hooks/ReviewProvider.tsx::generateReviewMarkdown`). GitHub PR comments can be pulled into the diff view (`routes/workspaces/pr.rs::get_pr_comments`).
- **Rebase conflicts** are surfaced with abort/continue actions, or handed to the agent (`crates/git/src/cli.rs`).
- **Cleanup scripts** (for example lint or format) run after a successful agent turn.

There is no "tests must pass" state, and no CI awareness beyond PR status polling (`services/src/services/pr_monitor.rs`).

### Observability & UI
**Transport.** axum WebSockets carry `{"JsonPatch":[...]}`, `{"Ready":true}`, and `{"finished":true}` frames (`crates/utils/src/log_msg.rs::to_ws_message_unchecked`). An SSE encoding exists too (`to_sse_event`, `routes/events.rs`). Streams are per resource:
- `.../raw-logs/ws` and `.../normalized-logs/ws` (`routes/execution_processes.rs`)
- `/stream/session/ws` for the process list
- `/workspaces/streams/ws`
- `/diff/ws` (`routes/workspaces/git.rs`)
- `/approvals/stream/ws`
- `/scratch/{type}/{id}/stream/ws`
- `/terminal/ws` (a PTY)
- `/agents/discovered-options/ws`

**DB→UI change feed.** `EventService::create_hook` installs SQLite `update_hook` and `preupdate_hook` on every pool connection (`services/src/services/events.rs`). On insert or update it re-queries the row asynchronously and pushes `add`/`replace` patches at `/workspaces/{id}`, `/execution_processes/{id}`, or `/scratch/...`. Deletes are captured in the pre-update hook, because the row is gone afterwards. Each subscriber receives a snapshot (`replace` at the collection root) and then a *filtered* view of one global broadcast (`events/streams.rs`).

**Client side.** `useJsonPatchWsStream` applies patches with `immer` for structural sharing and reconnects with backoff from 1 s to 8 s (`packages/web-core/src/shared/hooks/useJsonPatchWsStream.ts`).

**Views.** The workspace sidebar shows a server-computed `WorkspaceSummary` for each workspace (`routes/workspaces/workspace_summary.rs`):
- `has_pending_approval` and `has_unseen_turns`
- files changed and ±lines
- latest process status
- whether a dev server is running
- PR status, number, and URL

The workspace page has panels for the conversation (a virtualized `@virtuoso.dev/message-list`), changes (`@pierre/diffs` / `@git-diff-view`, with inline comments), logs, terminal (xterm.js), and git operations. It also has a command bar (`cmdk`), a Lexical rich prompt editor with file mentions and slash commands, and a token-usage display.

**Preview browser.** `preview-proxy` routes `{port}.localhost:{proxy}` to the dev server and injects Eruda devtools, a bippy React-DevTools hook, and a **click-to-component** script, so the user can click a UI element and send its component context to the agent (`crates/preview-proxy/src/lib.rs`).

**Attention.** OS notifications (osascript, notify-send, or PowerShell, plus a sound) fire on completion and on "Approval Needed" or "Question Asked" (`services/src/services/notification.rs`, `approvals/executor_approvals.rs`). Approvals carry a `timeout_at` and are backed by an in-memory `DashMap` (`services/src/services/approvals.rs`).

### Config & extensibility
- **Executor profiles.** `default_profiles.json` defines executor × variant profiles. Every DEFAULT variant is fully permissive: `dangerously_skip_permissions`, `yolo`, `danger-full-access`, `force`, and so on. Each executor config struct derives `schemars::JsonSchema`, and `generate_types` writes `shared/schemas/<agent>.json`, which the UI renders as settings forms with `@rjsf/shadcn`.
- **Overrides and MCP.** `CmdOverrides {base_command_override, additional_params, env}` applies to every agent (`crates/executors/src/command.rs`). MCP servers are written into each agent's native config file at the right key path (`CodingAgent::get_mcp_config`, `mcp_config.rs`). The commit-reminder and PR-description prompts are editable templates (`services/src/services/config/mod.rs`).
- **PR creation.** PRs are created with `gh pr create`, using a temporary body file (`crates/git-host/src/github/cli.rs`). An optional **follow-up turn asks the agent itself to rewrite the PR title and body with `gh pr edit`** (`routes/workspaces/pr.rs::trigger_pr_description_follow_up`).

### Strengths — steal these
- **One normalized conversation model across nine CLIs**, including approvals as tool status (`crates/executors/src/logs/mod.rs`). Also steal the trick of funnelling every protocol through a synthetic stdout pipe, so raw logs and live streams share one format (`stdout_dup.rs`).
- **Speak each agent's native structured protocol** rather than scraping: Claude's control protocol, Codex's app-server via OpenAI's own Rust protocol crate, ACP, and OpenCode's HTTP/SSE (`crates/executors/Cargo.toml`, `acp/harness.rs`).
- **Pin agent versions** in the command line (`@anthropic-ai/claude-code@2.1.119`, `@openai/codex@0.124.0`). Normalizers are only valid for a known wire format.
- **Use the Stop hook to block the agent from stopping while work is uncommitted**, and auto-commit as a backstop (`claude/client.rs`, `local-deployment/src/container.rs::try_commit_changes`).
- **Rewind** by combining per-process `before_head_commit` with a soft-drop of later processes and `--resume-session-at` (`container.rs::reset_session_to_process`).
- **Snapshot-then-JSON-Patch streaming with immer on the client** is simple and generic (`events/streams.rs`, `useJsonPatchWsStream.ts`).
- **A server-computed attention summary per workspace**, covering pending approval, unseen turns, diff stats, and PR state (`workspace_summary.rs`).
- **Persist drafts, queued messages, and UI state server-side ("scratch") and stream them**, so they survive reloads and devices (`scratch.rs`).
- **Script hooks per repo** (setup, cleanup, archive, dev server, copy-files), with the cleanup skipped when nothing changed (`repo.rs`, `spawn_exit_monitor`).
- **A preview proxy with click-to-component** for UI work (`crates/preview-proxy`).
- **ts-rs plus schemars generation with a `--check` mode in CI** (`crates/server/src/bin/generate_types.rs`).
- **A fake executor behind a cargo feature** for deterministic end-to-end tests (`executors/qa_mock.rs`, feature `qa-mode`).

### Weaknesses & tradeoffs
- **Permissive by default, isolated only by worktree.** Every default profile bypasses permissions, and nothing is sandboxed.
- **Replay depends on today's normalizer and on a live worktree.** History can silently change when normalizer code changes, and viewing old logs can resurrect a deleted worktree (`stream_normalized_logs`).
- **Fragile stream semantics:**
  - broadcast lag *drops* messages with only a log line (`msg_store.rs`);
  - there are no sequence numbers or resume-from-offset;
  - patches address entries by array index;
  - remove patches are broadcast to every subscriber unfiltered ("we can't verify session_id", `events/streams.rs`);
  - every client filter deserializes every patch.
- **SQLite hooks as the change feed** only see writes made through this process's pool. They re-query rows asynchronously (with a race window) and couple the DB layer to UI shapes. DELETE journal mode rather than WAL limits concurrency.
- **Orchestration is a linked list.** There are no DAGs, retries, budgets, or concurrency limits. Only a human or an MCP-driving agent fans out work.
- **Inline review comments live only in React state** (`ReviewProvider.tsx`), so they are lost on reload and never persisted.
- **Scope sprawl** (relay, WebRTC, SSH, Tauri, and a 24k-LOC cloud) diluted a small team, and the flagship kanban died with the cloud.
- **Fragile dependencies:** ts-rs comes from a personal fork branch (`xazukx/ts-rs`, `use-ts-enum`), and `npx` at spawn time needs the network (plus Amp is unpinned).

### Implications for a Rust framework
- **Keep the shape** `trait AgentDriver` (with enum dispatch) → `SpawnedChild{process group, exit_signal, cancel}` → a normalizer that emits a typed event stream. Separate three layers: *protocol drivers* (stdio stream-JSON, JSON-RPC, ACP, HTTP+SSE), *per-agent adapters*, and *normalizers*.
- **Persist the normalized events as the canonical history**, with a schema version and a monotonically increasing sequence number per stream. Keep the raw JSONL for forensics and re-normalization, but not as the only truth.
- **Make streams resumable:** `subscribe(topic, from_seq)` returns a snapshot plus deltas. On lag, send a fresh snapshot instead of dropping. Use per-topic channels instead of one global broadcast filtered per client.
- **Replace SQLite hooks with an explicit outbox** (an events table written in the same transaction), and use WAL mode.
- **Model workflows as a DAG of typed steps** (setup, agent turn, verification, review, PR) with retry and budget policies, instead of `next_action` chains.
- **Make "verification passed" a first-class state**, not a convention in the prompt.

---

## 2. smtg-ai/claude-squad

### What it is
- **What:** a Go 1.23 terminal UI (Bubble Tea and lipgloss) that manages many agent CLIs in tmux sessions, each on its own git worktree.
- **Size and maturity:** about 7.3k non-test LOC (9.4k with tests). v1.0.20, AGPL-3.0, last commit 2026-08-20. Mature and small; it installs as `cs`.

### Architecture
- `main.go` uses cobra and is the entry point.
- `app/app.go` holds the Bubble Tea model `home` (1,075 LOC).
- `session/instance.go` defines the `Instance` lifecycle.
- `session/tmux/tmux.go` wraps tmux through a PTY (`creack/pty`).
- `session/git/*` handles worktrees and diffs.
- `config/` holds `config.json` and `state.json` under `~/.claude-squad`.
- `daemon/daemon.go` is a background auto-yes poller.
- `ui/` provides the list, the Preview/Diff/Terminal tabs, and overlays for the prompt, branch picker, and profile picker.

### Core abstractions / domain model
- **Instance** (`session/instance.go`) has `Title`, `Path`, `Branch`, `Status`, `Program`, `AutoYes`, `Prompt`, a `gitWorktree`, and `diffStats`. `Status` is one of Running, Ready ("waiting for user input"), Loading, or Paused ("worktree removed but branch preserved").
- **Profile** is `{name, program}`, a named shell command (`config/config.go`).
- **Storage:** `InstanceData` is serialized to JSON (`session/storage.go`). It includes worktree metadata and even the diff *content*.

### Orchestration model
There is none: the human is the orchestrator.
- **Creating an instance** (`n` or `N` with a prompt) creates a worktree, then runs `tmux new-session -d -s claudesquad_<title> -c <worktree> <program>`. It also sets `history-limit 10000` and `mouse on` (`tmux.go::Start`).
- **Prompts are delivered by typing:** `SendKeys(prompt)`, a 100 ms sleep, then Enter (`instance.go::SendPrompt`).
- **Attaching** pipes the PTY to the real terminal until Ctrl-Q. It drops stdin bytes received in the first 50 ms, because those are terminal control-sequence responses (`tmux.go::Attach`).

### State & persistence
- **Storage:** plain JSON state. There is no conversation history beyond tmux scrollback, which `PreviewFullHistory` reads with `capture-pane -S - -E -`.
- **Pause** (`c`, "checkout"):
  1. commits dirty work locally as `[claudesquad] update from '<title>' on <date> (paused)`;
  2. detaches;
  3. removes the worktree but keeps the branch;
  4. prunes.
- **Resume** re-adds the worktree and restarts tmux (`instance.go::Pause/Resume`).
- **Recovery:** if the tmux server died (reboot or crash), `Restore` returns `ErrSessionNotFound` and the instance is parked as Paused instead of failing the whole load (`instance.go::Start`).

### Isolation & workspaces
- **Worktrees:** one per instance, at `<configDir>/worktrees/<sanitized-branch>`, on branch `<BranchPrefix><title>`. `BranchPrefix` defaults to `<username>/` (`session/git/worktree.go`, `config/config.go`).
- **Existing branches:** an instance can start on an existing branch, which is then never deleted on cleanup (`NewGitWorktreeFromBranch`, `isExistingBranch`).
- **Limits:** one repo per instance, and no sandbox.

### Agent integration
- **Any CLI is supported**, via `-p`/`--program` or a profile. Nothing is parsed.
- **State detection is screen-scraping on a 500 ms self-rescheduling tick** (`app.go::tickUpdateMetadataCmd`):
  - It runs `tmux capture-pane -p -e -J` and SHA-256 hashes the content. If the hash changed, the instance is **Running**; if not, it is **Ready** (`tmux.go::HasUpdated`).
  - It detects a pending permission prompt with hard-coded strings: Claude's "No, and tell Claude what to do differently", Aider's "(Y)es/(N)o/(D)on't ask again", and Gemini's "Yes, allow once".
- **Auto-yes** (`-y`) simply presses Enter when a prompt appears (`Instance.TapEnter`). `daemon/daemon.go` keeps doing this every `DaemonPollInterval` (default 1000 ms) after the TUI exits.
- **Trust dialogs** ("Do you trust the files in this folder?", "new MCP server") are dismissed automatically (`CheckAndHandleTrustPrompt`).

### Verification & quality gates
None. Review means reading the Diff tab. `s` commits and pushes via `gh repo sync --source -b <branch>`, falling back to `git push -u origin`, then offers `gh browse --branch` (`session/git/worktree_git.go::PushChanges`). There is no PR creation, no test run, and no CI.

### Observability & UI
- **List:** a spinner for Running or Loading, `●` for Ready, `⏸` for Paused, plus +/- line counts (`ui/list.go`).
- **Tabs** (`ui/tabbed_window.go`): **Preview** is a live capture of the pane. **Diff** runs `git add -N .` then `git diff <baseCommitSHA>` (`session/git/diff.go`). **Terminal** is a shell tmux session in the worktree, cached per instance (`ui/terminal.go`).
- **Memory control:** only the *selected* instance computes a full diff; the others compute `--numstat` only.
- **Timing:** the transport is polling. Preview refreshes every 100 ms and metadata every 500 ms. There are no notifications.

### Config & extensibility
`~/.claude-squad/config.json` holds `default_program`, `profiles[]`, `auto_yes`, `daemon_poll_interval`, and `branch_prefix`. Extending agent-state detection means adding string literals in `tmux.go`.

### Strengths — steal these
- **Zero-integration universality.** Anything that runs in a terminal can be supervised. Keep this as the fallback tier (`tmux.go`).
- **Durable sessions via tmux.** Agents outlive the UI process, and the UI reattaches (`Restore`).
- **Pause/resume that frees disk** (worktree removed, branch kept, work auto-committed), with graceful recovery when tmux dies (`instance.go::Pause/Resume/Start`).
- **Bounded-cost polling.** Full diff for the focused item and numstat for the rest; the tick self-reschedules so ticks never overlap (`app.go::tickUpdateMetadataCmd`).
- **A per-agent shell tab** in the same worktree (`ui/terminal.go`).
- **Keyboard-first flows:** new, new-with-prompt, attach, pause, resume, push.

### Weaknesses & tradeoffs
- **State detection is heuristic and brittle:**
  - "Ready" just means "the screen didn't change for 500 ms", which is wrong during long silent tool calls or when an animated spinner keeps redrawing.
  - The prompt strings break whenever a CLI's copy changes.
  - Detection uses `t.program == ProgramClaude` (exact match), while the trust-prompt check uses `HasSuffix`, so `claude --model x` loses prompt detection.
- **Auto-yes blindly approves every permission prompt**, including from a background daemon.
- **No structured history, cost, or context data.** There are no notifications, no PRs, and no multi-repo support. Diff content is persisted in `state.json`.
- **Prompting by keystroke injection** (with sleeps) races against the agent's TUI.

### Implications for a Rust framework
- **Offer a "terminal agent" tier** (a PTY in tmux or held by the daemon) for unsupported CLIs. Detect state through **push signals** (Sculptor-style hooks calling our CLI or API), with screen-hashing only as a last resort.
- **Keep agent processes independent of the UI process**, using a daemon or supervisor that the UI attaches to.
- **Adopt pause/resume with worktree GC**, and bounded diff computation.

---

## 3. imbue-ai/sculptor

### What it is
- **What:** a desktop app (Electron) for running coding agents in parallel.
- **Stack and size:** a Python 3 FastAPI backend with about 82k non-test LOC plus about 117k LOC of tests, and a React 19 frontend with about 155k TS/TSX LOC. MIT.
- **Maturity:** v0.49.0.dev0, described as an "experimental research preview" (`README.md`). Last commit 2026-09-18. It is the successor to Imbue's earlier Docker-per-agent design (`docs/history.md`).

### Architecture
The backend (`sculptor/sculptor/`):
- **`web/`** holds the FastAPI app (`app.py`), the scoped WebSocket fan-out (`streams.py`), derived views (`derived.py`), and PR polling.
- **`services/`** holds `workspace_service` (environment manager, setup-command runner, branch poller), `task_service`, `data_model_service`, `ci_babysitter_service`, `btw_service`, `git_repo_service`, and `terminal_agent_registry`.
- **`agents/`** holds `harness_registry.py` and one package per harness: `default/claude_code_sdk`, `pi_agent`, `terminal_agent`, and `hello_agent`.
- **`interfaces/`** holds the `Harness` ABC and the `AgentExecutionEnvironment` protocol.
- **`database/`** uses SQLAlchemy with Alembic.
- **`state/`** holds the chat, Claude-stream, and workflow models.

The frontend (`sculptor/frontend`) uses Electron Forge, Vite, Jotai with TanStack Query, Radix Themes, xterm 6, and a TS client generated by `@hey-api/openapi-ts`. Skills ship as Claude Code plugins (`sculptor/sculptor-workflow`, `sculptor-experimental`, `sculptor-plugin`). A `sculpt` CLI with a generated API client (`tools/sculpt`) lets agents and humans script the app. A "custom backend command" can run the backend in Docker, over SSH, or in a VM (`docs/help/experimental/container_backend.md`).

### Core abstractions / domain model
- **Project → Workspace → Agents.** A workspace mode is WORKTREE, CLONE, or IN_PLACE (`database/workspace_enums.py`). Agents are stored as Tasks (`AgentTaskInputsV2` / `AgentTaskStateV2`), and **several agents share one workspace's working copy** (`docs/help/agents.md`).
- **Harness** (`interfaces/agents/harness.py`) is the harness-agnostic seam. It has a `name`, and `capabilities()` returns `HarnessCapabilities` with 15 booleans: chat interface, interactive backchannel, skills, sub-agents, image input, fast mode, context reset, compaction, background tasks, session resume, tool-use rendering, attachments, interruption, file references, and model selection. The UI hides affordances whose capability is false. The harness also provides methods for model catalogs, ask-question and plan-tool classification, and on-disk session paths.
- **Harness registry** (`agents/harness_registry.py`) is "the one module that names every concrete Harness and Agent". Adding a harness means adding one `case` per function.
- **Agent contract** (`interfaces/agents/README.md`): any program that consumes and emits `Message`s. It must support resume after interruption, emit `PersistentRequestCompleteAgentMessage` so the controller can snapshot, and track *blocked* and *complete* states.
- **Notification** uses Apple HIG importance levels: PASSIVE, ACTIVE, TIME_SENSITIVE, CRITICAL (`database/models.py`).

### Orchestration model
It is still human-driven, but with three higher-level mechanisms.

1. **A skills pipeline** (`docs/help/skills.md`): **spec → mock → architect → plan → build → review**.
   - Each stage runs as its own agent tab, named after the stage.
   - Each stage writes a durable artifact on disk (spec, `mocks.html`, `architecture.md`, `plan/` task files, commits, `review.md`) that the next stage reads.
   - `build` re-reads its per-task file every time "so it doesn't drift" and commits after each task.
   - `setup-repo` writes `.sculptor/code.md`, `testing.md`, and `docs.md` for the other skills to follow.
2. **Handoff and stack** (`sculptor-experimental`). These spawn a fresh agent in the same workspace, or in a new workspace whose branch is based on *and targets* the current branch (stacked PRs), seeded with a summary.
3. **A CI babysitter** (`services/ci_babysitter_service/`), an edge-triggered state machine:
   - It classifies transitions into PIPELINE_FAILED, MERGE_CONFLICT, PIPELINE_PASSED, MR_MERGED, and MR_CLOSED (`transitions.py::classify_transitions`).
   - It is idempotent per `pipeline_id`, and it never fires a failure on the first poll after a restart.
   - It enforces a retry cap that resets on green.
   - It waits until the workspace is idle, then reuses one "CI Babysitter" agent tab (`docs/help/ci_babysitter.md`, `state.py`).

### State & persistence
- **Storage model:** SQLite, where **every table is an immutable event log plus a materialized `<table>_latest` view** (`docs/development/database.md`, `database/automanaged.py`).
- **Migrations:** Alembic runs automatically at startup (32 versions so far). A test fails if SQL *or* pydantic JSON-field schemas change without a migration, using a frozen `frozen_pydantic_schemas.json`. Every migration needs a `seed`/`verify` test fixture.
- **Durability claims:** the docs say pending questions and the message queue survive app reloads and harness restarts, and that "an interrupted turn survives a quit, or a crash of Sculptor itself" (`docs/help/integrated_harnesses.md`).

### Isolation & workspaces
- **Modes:** worktree is the default, with a `<user>/<slug>` branch pattern and a configurable delete-branch policy. Clone and in-place are experimental (`docs/help/workspaces.md`). Workspaces live at `~/.sculptor/workspaces/<id>/code`. There is a setup command per repo and env vars from `~/.sculptor/.env` and `.sculptor/.env`.
- **Execution environment:** the `AgentExecutionEnvironment` protocol abstracts file I/O, process spawning, and host↔environment path translation (`to_host_path` / `to_environment_path`), which is what lets the whole backend move into a container.
- **Why they dropped per-agent Docker** (`docs/history.md`):
  - it prevented agents from inspecting each other's work;
  - users found it confusing;
  - containerizing the whole app is equivalent;
  - dependence on Docker Desktop hurt performance.

### Agent integration
**Claude Code** is built in `agents/default/claude_code_sdk/process_manager_utils.py::get_claude_command` and run as `bash -c "exec ..."` so that signals reach Claude directly:
```
exec env IS_SANDBOX=1 claude --dangerously-skip-permissions --permission-prompt-tool stdio
  --output-format=stream-json --verbose --input-format stream-json --include-hook-events
  --mcp-config '{"mcpServers":{"sculptor":{"type":"sdk","name":"sculptor"}}}'
  --disallowed-tools AskUserQuestion,ExitPlanMode --include-partial-messages
  [--resume <sid>] [--append-system-prompt …] [--model …] [--plugin-dir …]…
  [--settings '{"fastMode":true}'] [--effort …]
```
- The prompt is sent as a stream-JSON message on stdin, and each user message is a new CLI invocation resumed by session id.
- The **in-process SDK MCP server** (`mcp_server.py`) replaces `AskUserQuestion` and `ExitPlanMode` with `mcp__sculptor__*` tools. Claude blocks on the tool call while Sculptor renders a native panel and answers for it. That makes questions persistent, and notifications from them "more likely to reach you".
- A pre-compaction hook drives a "Compacting…" indicator. A per-turn context query drives a "% context" chip. Claude Code *Workflow*-tool progress (phases and subagents) is mirrored into a progress tree (`state/workflow_state.py`).

**`/btw` side questions** fork the main session without polluting it (`btw_process_manager.py`):
```
claude --resume <main> --fork-session --no-session-persistence -p <q> --model <haiku>
  --tools '' --strict-mcp-config --disable-slash-commands --append-system-prompt …
```

**Pi** runs as a long-lived `pi --mode rpc --session-dir <d> --session-id <id> --no-extensions --append-system-prompt …`, speaking JSON RPC (`get_state`, `get_available_models`, `set_model`, `abort`), with a curated, pinned extension set that adds the backchannel tools (`agents/pi_agent/agent_wrapper.py`).

**Terminal agents** can be any CLI in a PTY. All capabilities are false and output is never parsed (`agents/terminal_agent/harness.py`). Status arrives through a **push signal API**: hooks call `sculpt signal busy|idle|waiting|files-changed|session-id`, which becomes synthetic, run-scoped status messages that survive a frontend reload. A broken hook degrades to a neutral status (`agent_docs/terminal-agents/architecture.md` §4–5). The diff is refreshed periodically.

**Normalization:** `output_processor.py` (2.1k LOC) turns Claude stream events into chat `ContentBlock`s. Synthetic unified diffs are generated from Edit and Write tool inputs, so tool cards render inline diffs (`process_manager_utils.py::_create_synthetic_edit_diff`).

**Testing:** `agents/testing/fake_claude.py` is a fake Claude binary for deterministic tests.

### Verification & quality gates
Verification is prompt- and skill-driven, not enforced by code:
- `fix-bug` runs strict TDD: reproduce, write a failing test, fix, verify. In autonomous mode it classifies the bug as REPRODUCED, STALE, ALREADY-FIXED, or UNREPRODUCIBLE, and only a proven fix may open a PR (`sculptor-workflow/skills/fix-bug/SKILL.md`).
- `build` runs the checks from `.sculptor/code.md` for each task.
- `review` re-runs the suite, calls the repo's configured review skill, and writes `review.md` without fixing anything.
- The CI babysitter closes the loop on red pipelines and merge conflicts.
- Committing is delegated to the agent: the Commit button asks it to write the message (`docs/help/changes.md`).

### Observability & UI
**Transport.** One WebSocket per client, `stream_everything`, takes `?scope=all|project:<id>|workspace:<id>|agent:<id>`. The scope is authorized per entity and yields `StreamingUpdate` deltas keyed by id: task updates and views, branch info, PR status, setup output, `/btw` updates, and UI actions such as "open file" (`web/streams.py`).

**Client state.** TanStack Query serves as a **push-fed store**, not a fetch cache (`queryFn: skipToken`, `gcTime: Infinity`). Mutations are optimistic, with rollback keyed on a per-agent sync version. The "state ownership" rule is that there is one written store per server fact (`docs/development/style/frontend.md`).

**Attention model** (`frontend/src/common/utils/statusDot.ts`, `common/state/atoms/workspaces.ts`):
- Each agent's dot is one of `running | waiting | error | unread | read`.
- Unread is derived from `lastReadAt` versus `updatedAt`, email-style, with an explicit "mark unread".
- Request errors such as a 429 clear once the user has seen them.
- Each workspace row aggregates `hasError / hasWaiting / hasRunning / hasUnread` and re-renders only when a flag flips.

**Other surfaces:**
- workspace tabs and agent tabs;
- a Files panel with Browse, Changes (a cumulative uncommitted diff), and Commits;
- typed, collapsible tool cards with inline diffs;
- plans open in the editor pane;
- a WYSIWYG prompt editor;
- a visible, editable message queue;
- a terminal;
- a Cmd+K palette;
- a PR button with polled status;
- desktop notifications by importance.

### Config & extensibility
- Settings sections for Git, Repositories, Environment variables, CI, and Experimental.
- Per-repo `.sculptor/*.md` configs that the skills read.
- Bundled plugin skills, plus the user's own `~/.claude/skills` and `.claude/`.
- Extensions (`docs/help/extensions.md`).
- New harnesses via `harness_registry.py`, and terminal agents via `terminal_agent_registry`.
- A custom backend command for running the backend remotely or in a container.

### Strengths — steal these
- **Capability flags per harness** that gate UI affordances, so there is one UI for rich and poor agents (`interfaces/agents/harness.py::HarnessCapabilities`).
- **A push status-signal protocol for arbitrary terminal agents** (`sculpt signal …`), run-scoped so stale "waiting" states never resurrect (`agent_docs/terminal-agents/architecture.md`).
- **Replacing an agent's blocking UX tools with MCP tools the app controls**, which makes questions and plan approvals first-class, persistent, and notifiable (`mcp_server.py`, `get_claude_command`).
- **`/btw` forked side questions** on a cheap model with no tools (`btw_process_manager.py`).
- **An edge-triggered, idempotent CI babysitter** with retry caps and a wait-for-idle rule (`ci_babysitter_service/transitions.py`, `state.py`).
- **An artifact-passing skill pipeline** (spec → … → review) with resumable stages and stacked-branch handoff (`docs/help/skills.md`).
- **The unread/waiting/error attention model with manual mark-unread** (`statusDot.ts`).
- **An event-log-plus-latest-view DB schema, with migration tests that also cover JSON-in-column schemas** (`docs/development/database.md`).
- **Scoped WebSocket subscriptions** with authorization at upgrade time (`web/streams.py::resolve_scope`).
- **An environment protocol with path translation**, which allows running the whole backend elsewhere (`interfaces/environments/agent_execution_environment.py`).

### Weaknesses & tradeoffs
- **Integration is deep but narrow.** Only Claude Code and Pi are "integrated"; Codex, Gemini, and others get the terminal tier. The per-harness cost is admitted in the docs.
- **Agents share one working copy**, and the docs concede that concurrent edits conflict (`docs/help/agents.md`).
- **Agents run with `--dangerously-skip-permissions` and `IS_SANDBOX=1`**, while the sandbox is left to the user: "run the entire application in a container".
- **Quality gates are prompts.** Nothing stops a PR when tests fail.
- **The stack is heavy:** Python + PyInstaller sidecar + Electron, and about 355k LOC including tests. The code carries legacy architecture ("design choices that aren't perfectly aligned", `docs/history.md`), and the project is not taking large outside contributions (`README.md`).
- **One process spawn per message** adds latency and depends on `--resume` session files existing on the host (`is_session_id_valid`).

### Implications for a Rust framework
- **Define a `Harness` trait with a `Capabilities` struct**, and render the UI from capabilities.
- **Provide an agent-facing CLI and API** (`factory signal`, `factory handoff`, `factory ask`) plus an MCP server, so agents and hooks can report state and request human input through *our* channels.
- **Persist entities as an event log plus latest views.** Make CI, PR state, and merge conflicts inputs to an edge-triggered policy engine.

---

## UX patterns for supervising agent swarms

What to build, with the evidence from these repos:

1. **An attention inbox, not a list.** Each agent has exactly one attention state: `waiting-on-you` (approval, question, or plan), `error`, `running`, `unread-result`, or `read`. Unread is tracked email-style with mark-unread (Sculptor `statusDot.ts`; VK `has_pending_approval` and `has_unseen_turns`). Default sort: waiting → error → unread → running. Make one keyboard shortcut jump to the next agent that needs you.
2. **Blocking requests are durable, typed objects.** Approvals, questions, and plan approvals need an id, a timeout, persistence across reloads, an OS or push notification with a deep link, and inline rendering on the tool card (VK `ToolStatus::PendingApproval` and `approvals.rs`; Sculptor's MCP replacement tools). Batch-approve across agents.
3. **Diff review that talks back.** Inline comments on the diff are compiled into one structured follow-up (VK `generateReviewMarkdown`). *Persist* the comments, thread the agent's reply against each one, and import GitHub PR review comments into the same view (VK `get_pr_comments`).
4. **Time travel.** Allow editing any earlier message to rewind both the git state and the agent's session (VK `reset_session_to_process` with `--resume-session-at`). Show per-turn diffs from `before`/`after` commits.
5. **A visible, editable message queue** per agent while it runs (VK `queued_message`; Sculptor's message queue). Side questions (`/btw`) must not pollute the agent's context.
6. **Live context gauges:** tokens and % of context, a compaction marker, model and effort, and cost per agent and per swarm (VK `TokenUsageInfo`; Sculptor's "% context" chip).
7. **A workspace cockpit:** conversation | diff | terminal | dev-server logs | preview browser with click-to-component that sends to the agent (VK `preview-proxy`).
8. **Verification badges** per workspace (setup ✓, tests ✓/✗, lint, CI pipeline, merge conflicts), with auto-remediation (Sculptor's CI babysitter). No product here shows *test status* as a first-class badge. **Build it.**
9. **Pipeline and graph views** for multi-stage work: spec → plan → build → review as columns, and a dependency DAG of tasks (Sculptor's workflow tree and dependency graph; VK's kanban).
10. **Graceful degradation tiers:** fully integrated harness → ACP or JSON-RPC harness → terminal agent with signal hooks → raw PTY with screen-hash heuristics (claude-squad).

**What is missing in all three:**
- a **swarm timeline** or Gantt view across agents;
- **best-of-N comparison** (several agents on one task, a side-by-side diff of their diffs, pick a winner);
- **budget and policy controls** (max parallel agents, max spend, stop conditions);
- **audit and replay** of a whole run;
- **cross-agent conflict prediction** (two workspaces touching the same files);
- **stacked-PR visualization**.

---

## Rust + UI stack lessons

**VK's choices, and whether to copy them:**

| Concern | VK choice (evidence) | Recommendation |
|---|---|---|
| HTTP/WS | `axum` 0.8 with `ws`, `tower-http`, `tokio` (`Cargo.toml`) | **Copy.** |
| Single binary | `rust-embed` of the Vite `dist` (`routes/frontend.rs`), an `npx` launcher (`npx-cli/`), optional Tauri 2 shell (`crates/tauri-app`) | **Copy.** Also offer `cargo install` and plain release binaries. |
| Processes | `command-group` (process groups and kill-tree), `os_pipe` stdout re-injection, `portable-pty` terminals, `tokio_util::CancellationToken`, `oneshot` exit signals | **Copy.** Add a supervisor so agents survive a UI or server restart (the Sculptor and claude-squad lesson). |
| Agent protocols | `agent-client-protocol` 0.8 (ACP); `codex-app-server-protocol` git-pinned; a hand-rolled Claude control protocol (`claude/protocol.rs`); `eventsource-stream` for OpenCode; `rmcp` for an MCP server | **Copy.** Put ACP first for new agents and wrap others in adapters. Pin agent binary versions per adapter. |
| DB | `sqlx` 0.8 SQLite, compile-time checked queries (offline `prepare`), embedded migrations, `sqlite-preupdate-hook` for the change feed | **Copy sqlx and SQLite**, but use **WAL**, and use an **outbox/event table with a monotonically increasing seq** instead of update hooks. Consider Sculptor's event-log-plus-`_latest` pattern for auditability. |
| Streaming | RFC 6902 JSON Patch (`json-patch` crate) over WebSocket, snapshot then deltas; client uses `rfc6902` and `immer` | **Copy the patch semantics.** Add `seq` and resume-from-seq, snapshot-on-lag (never drop silently), per-topic channels with server-side scoping (Sculptor's `?scope=`), and stable entry ids instead of array indices. Use SSE for read-only streams and WS for PTY and bidirectional traffic. |
| TS types | `ts-rs` (**from a fork branch**) with a `generate_types --check` in CI, plus `schemars` → JSON Schema → RJSF settings forms | **Copy the pattern.** Use upstream `ts-rs` or `specta` (plus `tauri-specta` if Tauri); keep the `--check` CI gate and the schemars-driven config forms. Sculptor's alternative (OpenAPI → `@hey-api/openapi-ts`) is also sound; `utoipa` would be the Rust equivalent. |
| Git | `git2` for reads and diffs; **git CLI** for worktree add/remove/move, rebase, and push; per-path async locks; `gh` and `az` CLIs for PRs | **Copy.** Evaluate `gix` for reads. Keep the CLI for mutations. |
| FS/diff | `notify` + `notify-debouncer-full` + `ignore` (gitignore-aware) feeding live diff patches | **Copy.** |
| Frontend | React, TanStack Router and Query, virtualized message list, `@pierre/diffs` / `@git-diff-view`, `xterm.js`, `cmdk`, Lexical, Radix | Similar stack is fine. Adopt Sculptor's **state-ownership rule** (push-fed query cache, a single writer, optimistic mutations with rollback). |
| Testing | A `qa-mode` feature with `QaMockExecutor`; Sculptor's `fake_claude.py` | **Build a fake-agent binary that replays recorded protocol transcripts**, so the orchestrator, normalizers, and UI can be tested without real agent CLIs. |

**Bottom line.** VK shows a Rust backend is fully viable for this product, and its executor and normalizer layer is the most reusable design in this group. Where we should do better:
- **durable, versioned normalized events** instead of re-normalizing raw logs on every replay;
- **resumable, sequenced streams** instead of a lossy global broadcast;
- **an explicit workflow DAG with verification gates** instead of `next_action` chains;
- **a sandbox story** instead of "yolo in a worktree".
