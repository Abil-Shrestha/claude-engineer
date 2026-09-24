# 06 — Agent harnesses & protocols: how a Rust orchestrator should drive agents

Research group: "Agent harnesses & protocols". Goal: decide what our Rust `AgentRuntime` / adapter
layer looks like so one orchestrator can drive Claude Code, Codex, Gemini CLI, Goose, OpenCode, etc.
uniformly (spawn → stream normalized events → follow-up input → interrupt → approve/deny tools →
resume → usage/cost), and which wire protocols to implement first.

Sources were read at these commits (shallow clones in the session scratchpad, `research/<repo>`):

| Repo | Commit / version | Notes |
|---|---|---|
| `anthropics/claude-agent-sdk-python` | `2b87034` (bundles CLI `2.1.281`, `src/claude_agent_sdk/_cli_version.py`) | Full source, the clearest protocol reference |
| `anthropics/claude-agent-sdk-typescript` | `fb04f66` (GitHub repo) + npm `@anthropic-ai/claude-agent-sdk@0.3.281` | GitHub repo ships **no source** (README, CHANGELOG, `examples/session-stores/*`). Types read from the npm tarball's `sdk.d.ts` (489 KB) and flags from the minified `sdk.mjs` |
| `openai/codex` | `3d64285` | `codex-rs/` Rust workspace, plus `sdk/typescript` and `sdk/python` |
| `block/goose` (now `aaif-goose/goose`) | `6eb8710`, workspace version `1.52.0` | Rust workspace `crates/*` |
| `agentclientprotocol/agent-client-protocol` | `03f9574` | `zed-industries/agent-client-protocol` redirects here (same `HEAD` from `git ls-remote`) |
| `agentclientprotocol/rust-sdk` | shallow `HEAD` | Rust runtime crate `agent-client-protocol` 2.2.0 |
| `zed-industries/claude-agent-acp` | shallow `HEAD` | Claude Code → ACP adapter (TypeScript) |
| `SWE-agent/mini-swe-agent` | `04d809c` | |

All paths below are relative to the repo root unless stated.

---

## 1. Claude Agent SDK (TypeScript + Python) — the `claude` CLI subprocess protocol

### What it is

The Claude Agent SDKs are thin clients around the **Claude Code CLI run as a child process**. Neither SDK
contains an agent loop. They spawn `claude`, speak **newline-delimited JSON (NDJSON) over stdin/stdout**, and
add a bidirectional **control protocol** on the same pipes for permissions, hooks, in-process MCP
servers, interrupts and runtime reconfiguration. The TypeScript package bundles a native `claude` binary per
platform (optionalDependencies `@anthropic-ai/claude-agent-sdk-{linux,darwin,win32}-{x64,arm64}[-musl]`,
`package/manifest.json` pins CLI `2.1.281` with checksums). The Python wheel bundles it at
`_bundled/claude` (`_internal/transport/subprocess_cli.py:337-350`).

### Architecture

- **Transport** (`_internal/transport/subprocess_cli.py`, class `SubprocessCLITransport`): finds the CLI
  (bundled, then `which claude`, then well-known paths, `:251-335`), builds argv (`_build_command`,
  `:566-791`), spawns with `anyio.open_process` (`:859-867`), and frames stdout into lines with `_LineFramer`
  (`:159-192`). It skips non-JSON lines such as `[SandboxDebug]` (`:195-217`), enforces a 1 MB max
  message size (`_DEFAULT_MAX_BUFFER_SIZE`, `:36`), and shuts down in stages: close stdin, wait 5 s, SIGTERM,
  wait 5 s, SIGKILL (`:948-1047`). An `atexit` reaper kills orphaned children (`:54-64`).
- **Query / control router** (`_internal/query.py`, class `Query`): one reader task demultiplexes stdout
  into (a) `control_response` messages matched to pending requests by `request_id`, (b) `control_request`
  messages from the CLI (permission prompts, hook callbacks, MCP messages), each handled in its own task,
  (c) `control_cancel_request`, which cancels an in-flight handler, (d) `transcript_mirror` frames, and
  (e) everything else, which is yielded to the user as SDK messages (`:333-424`).
- **Client** (`client.py`, `ClaudeSDKClient`): `connect()` starts the reader, sends `initialize`, then writes
  user messages (`:232-245`). `query()` writes further user turns on the same process (`:262-294`).
- The TS `Query` interface (`sdk.d.ts:2705`) is an `AsyncGenerator<SDKMessage>` with control methods:
  `interrupt, setPermissionMode, setModel, setMaxThinkingTokens, applyFlagSettings, supportedCommands/Models/Agents,
  mcpServerStatus, getContextUsage, rewindFiles, reconnectMcpServer, toggleMcpServer, setMcpServers,
  streamInput, stopTask, backgroundTasks, close`.

### Protocol / invocation details

**Argv.** Both SDKs always run in streaming mode. The TS bundle's builder starts with
`["--output-format","stream-json","--verbose","--input-format","stream-json"]` (in minified `sdk.mjs`).
Python places `--input-format stream-json` last (`subprocess_cli.py:570, 789`). The canonical invocation
for an orchestrator:

```bash
claude --output-format stream-json --verbose --input-format stream-json \
  [--append-system-prompt "<extra>" | --system-prompt "<full>" | --system-prompt-file F] \
  --model <m> [--fallback-model <m2>] [--effort low|medium|high|xhigh|max] \
  --permission-mode default|acceptEdits|plan|bypassPermissions|dontAsk|auto \
  --permission-prompt-tool stdio            # route permission prompts to us over the control protocol
  [--allowedTools "Read,Grep,Bash(git:*)"] [--disallowedTools ...] [--tools "Read,Edit,Bash"|""|default] \
  [--mcp-config '{"mcpServers":{...}}' --strict-mcp-config] \
  [--setting-sources=user,project,local] [--settings '<json or path>'] [--add-dir DIR]... \
  [--max-turns N] [--max-budget-usd X] [--task-budget TOKENS] [--json-schema '<schema>'] \
  [--include-partial-messages] [--include-hook-events] \
  [--session-id=<uuid> | --resume=<id-or-title> [--fork-session] [--resume-session-at=<msg-uuid>] | --continue] \
  [--no-session-persistence] [--plugin-dir DIR] [--agent NAME]
```

Key facts, with sources:
- `--permission-prompt-tool stdio` is what the SDK passes when a `canUseTool` callback is set. The TS bundle
  throws if you set both `canUseTool` and `permissionPromptToolName` (minified `sdk.mjs`). Python does the
  same by copying options with `permission_prompt_tool_name="stdio"` (`types.py:1924-1940`).
- Values for `--resume`, `--session-id`, `--resume-session-at` and `--resume-drops-turn` are passed in
  `--flag=value` form so an untrusted value that begins with `-` cannot inject flags
  (`subprocess_cli.py:638-651, 702-717`). Our adapter should do the same.
- `system_prompt=None` in Python becomes `--system-prompt ""`, an **empty** system prompt that is not Claude
  Code's default (`:572-573`). Only the `preset: "claude_code"` form, with no flag or with `--append-system-prompt`,
  keeps Claude Code's own prompt (`:582-585`). An orchestrator that wants normal Claude Code behavior should use
  `--append-system-prompt`.
- Hooks, subagent definitions (`agents`), skills filters and similar settings are sent in the
  `initialize` control request, not as flags (`query.py:254-308`, `subprocess_cli.py:722-723`).
- Environment: `CLAUDE_CODE_ENTRYPOINT=sdk-py|sdk-ts`, `CLAUDE_AGENT_SDK_VERSION`. The SDK **removes
  `CLAUDECODE`** from the inherited environment so a CLI spawned from inside a Claude Code session does not
  think it is nested (`subprocess_cli.py:815-820`). Goose does the same (`crates/goose/src/providers/claude_code.rs:337`).
  It also propagates OTEL `TRACEPARENT`/`TRACESTATE` (`:823-847`), sets `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING`
  when `rewindFiles` is wanted, and `CLAUDE_CONFIG_DIR` relocates all state.
- TS `Options.spawnClaudeCodeProcess?: (SpawnOptions) => SpawnedProcess` (`sdk.d.ts:2355-2377, 9076-9150`)
  lets the caller run the CLI "in VMs, containers, or remote environments". The SDK only needs
  `stdin/stdout/kill/exit`. This is the launcher abstraction we want.

**Stdin messages (SDK → CLI).** Each is one JSON object per line.

```jsonc
// user turn (client.py:237-245; TS SDKUserMessage sdk.d.ts:5996)
{"type":"user","message":{"role":"user","content":"Fix the failing test"},
 "parent_tool_use_id":null,"session_id":"default",
 "priority":"now|next|later",          // TS only: queueing / steering while a turn runs
 "uuid":"<client-uuid>"}               // echoed back as user_message_uuid(s) on replies
// control request (query.py:623-650)
{"type":"control_request","request_id":"req_1_9f3a2b1c",
 "request":{"subtype":"initialize","hooks":{"PreToolUse":[{"matcher":"Bash","hookCallbackIds":["hook_0"]}]},
            "agents":{...},"skills":[...]}}
{"type":"control_request","request_id":"req_2_…","request":{"subtype":"interrupt"}}   // TS adds cancel_queued?
{"type":"control_request","request_id":"req_3_…","request":{"subtype":"set_permission_mode","mode":"acceptEdits"}}
{"type":"control_request","request_id":"req_4_…","request":{"subtype":"set_model","model":"sonnet"}}
// other subtypes: mcp_status, get_context_usage, rewind_files, mcp_reconnect, mcp_toggle, stop_task,
// mcp_set_servers, get_session_cost, reload_plugins, … (TS union SDKControlRequestInner, sdk.d.ts:4795)
// control response to a CLI-originated request (query.py:597-621)
{"type":"control_response","response":{"subtype":"success","request_id":"<id from CLI>","response":{...}}}
{"type":"control_response","response":{"subtype":"error","request_id":"…","error":"message"}}
```

**Stdout messages (CLI → SDK).** TS `StdoutMessage = SDKMessage | SDKActiveGoalMessage | SDKControlResponse | SDKControlRequest | SDKControlCancelRequest | SDKKeepAliveMessage`
(`sdk.d.ts:9176`). `SDKMessage` is a 39-variant union (`sdk.d.ts:5133`). The ones an orchestrator must handle:

| `type` / `subtype` | Meaning | Key fields |
|---|---|---|
| `system`/`init` (`sdk.d.ts:5692`) | Session ready | `session_id, model, cwd, tools[], mcp_servers[{name,status}], permissionMode, slash_commands, skills, plugins, agents, claude_code_version, apiKeySource` |
| `assistant` (`:3462`) | One complete API message | `message: BetaMessage` (content blocks `text`, `thinking`, `tool_use{id,name,input}`), `parent_tool_use_id` (non-null inside subagents), `error?`, `session_id, uuid` |
| `user` | Tool results / replays | content blocks `tool_result{tool_use_id,content,is_error}`, `tool_use_result`, `parent_tool_use_id` |
| `stream_event` (`:5282`) | Token deltas (with `--include-partial-messages`) | `event: BetaRawMessageStreamEvent` (`content_block_delta`, …) |
| `result` (`:5469-5580`) | **End of turn** | `subtype: success | error_during_execution | error_max_turns | error_max_budget_usd | error_max_structured_output_retries`, `is_error, duration_ms, duration_api_ms, num_turns, result, stop_reason, terminal_reason, total_cost_usd, usage, modelUsage{model:{inputTokens,outputTokens,cacheRead/CreationInputTokens,costUSD,contextWindow}}, permission_denials[], structured_output?, deferred_tool_use?, errors[], api_error_status?, session_id` |
| `system`/`session_state_changed` (`:5644`) | Run-state edge | `state: idle | running | requires_action` |
| `system`/`status` (`:5677`) | e.g. compacting | `status: compacting | requesting | null`, `permissionMode` |
| `system`/`task_started|task_progress|task_notification|task_updated` | Subagents and background tasks | `task_id, task_type, description, usage{total_tokens,tool_uses,duration_ms}, status` (`types.py:1195-1282`) |
| `system`/`hook_started|hook_response` | Hook lifecycle (with `--include-hook-events`) | `hook_event` |
| `tool_progress` (`:5891`) | Heartbeat for long tools | `tool_use_id, tool_name, elapsed_time_seconds` |
| `rate_limit_event` | Plan limits | `rate_limit_info{status allowed|allowed_warning|rejected, resetsAt, rateLimitType, utilization}` |
| `conversation_reset` | `/clear`; running totals reset | `new_conversation_id` (`types.py:1438`) |
| `control_request` from CLI | Needs a reply | `can_use_tool`, `hook_callback`, `mcp_message`, `elicitation` |
| `control_cancel_request` | CLI withdraws a pending request | `request_id` (`sdk.d.ts:3736`) |
| `keep_alive` | Heartbeat | — |

`TerminalReason` (`sdk.d.ts:9324`) is an exhaustive list of why a turn ended: `completed, max_turns,
aborted_streaming, aborted_tools, budget_exhausted, prompt_too_long, api_error, model_error, hook_stopped,
tool_deferred, …`. It maps cleanly onto our `StopReason`.

**Permission round trip.** With `--permission-prompt-tool stdio` the CLI writes:

```json
{"type":"control_request","request_id":"…","request":{"subtype":"can_use_tool","tool_name":"Bash",
 "input":{"command":"npm test"},"tool_use_id":"toolu_…","permission_suggestions":[…],
 "blocked_path":null,"decision_reason":"…","decision_reason_type":"rule|mode|classifier|…",
 "title":"…","display_name":"…","agent_id":"…"}}
```

(`types.py:2422-2433`; the richer TS shape is at `sdk.d.ts:4588-4640`). The CLI blocks until it gets
`{"behavior":"allow","updatedInput":{…},"updatedPermissions":[PermissionUpdate…]}` or
`{"behavior":"deny","message":"…","interrupt":true?}` (`query.py:534-551`, TS `PermissionResult`
`sdk.d.ts:2440`). `updatedInput` lets the orchestrator **rewrite the tool call** before it runs, for
example to force a safer command. `updatedPermissions` persists "always allow" rules to a destination
(`session`, `localSettings`, …). `interrupt:true` also stops the turn.

**Hooks** round-trip the same way. `initialize` registers `hookCallbackIds`; the CLI later sends
`{"subtype":"hook_callback","callback_id":"hook_0","input":{…},"tool_use_id":…}`, and the reply is the hook
JSON output (`permissionDecision: allow|deny|ask|defer`, `continue`, `decision:"block"`, …). Hook events are
`PreToolUse, PostToolUse, PostToolUseFailure, UserPromptSubmit, Stop, SubagentStart, SubagentStop, PreCompact,
Notification, PermissionRequest` (`types.py:284-295, 437-585`).

**In-process MCP tools** (`type:"sdk"` servers) are tunneled as `control_request{subtype:"mcp_message",
server_name, message:<JSON-RPC>}`, and the reply is `{"mcp_response":<JSON-RPC>}` (`query.py:573-591, 670-699`).
This lets the host expose tools to the agent **without a separate MCP process**.

### Session, resume & state

- Transcripts are JSONL at `$CLAUDE_CONFIG_DIR|~/.claude/projects/<sanitized-cwd>/<session-id>.jsonl`
  (`_internal/sessions.py:3, 123-145`). Because the project key comes from the **cwd**, resuming in a
  different worktree path will not find the session. The TS `SDKStartupFailureReason` includes
  `worktree_resume_refused` and `cwd_unavailable` (`sdk.d.ts:5673`).
- `--session-id=<uuid>` pre-assigns the id, so the orchestrator owns it. `--resume=<id>` continues,
  `--fork-session` branches, `--resume-session-at=<assistant-msg-uuid>` truncates history, and
  `--no-session-persistence` makes the session ephemeral.
- `--session-mirror` plus a `SessionStore` makes the CLI emit `transcript_mirror` frames. The SDK batches
  them into an external store and flushes before each `result` (`query.py:224-231, 373-393`). Resuming from
  the store rebuilds a temporary `CLAUDE_CONFIG_DIR` (`_internal/session_resume.py:1-12`). The TS repo's only
  code is `examples/session-stores/{redis,postgres,s3}`. **This is a ready-made design for a central
  transcript DB.**
- Session APIs: `listSessions, getSessionMessages, forkSession, renameSession, tagSession, deleteSession,
  listSubagents, getSubagentMessages` (`sdk.d.ts:547-1088`).
- Multi-turn: keep stdin open and write more `user` messages. A `result` frame ends one turn, not the run.
  Background subagents can wake the parent for more turns, so stdin must stay open while tasks are in flight
  (`query.py:38-52, 787-842, 856-886`, issue #1088 referenced in comments).

### Permissions, sandboxing & safety

- Permission modes: `default | acceptEdits | plan | bypassPermissions | dontAsk | auto`
  (`types.py:25-27`; TS adds `--allow-dangerously-skip-permissions`).
- Rules: `--allowedTools` / `--disallowedTools` accept patterns such as `Bash(git:*)` and `Skill(name)`.
  Commas and parentheses delimit, so names are validated (`subprocess_cli.py:67-157`).
- Bash sandbox (macOS/Linux) is configured through `--settings '{"sandbox":{…}}'`: `enabled,
  autoAllowBashIfSandboxed, excludedCommands, allowUnsandboxedCommands, network{allowedDomains, deniedDomains,
  httpProxyPort, socksProxyPort, allowUnixSockets…}, enableWeakerNestedSandbox` (`types.py:870-955`;
  merge logic at `subprocess_cli.py:469-521`).
- Budget guards: `--max-turns`, `--max-budget-usd` (result `error_max_budget_usd`), `--task-budget`.
- Windows hardening: refuses to spawn `.cmd`/`.bat` shims (BatBadBut, CVE-2024-27980), `:362-467`.

### Events & observability

- **USD cost is native**: `result.total_cost_usd` and per-model `modelUsage[*].costUSD` (`types.py:1314-1378`).
  Per-message token usage is on `assistant.message.usage`. The `result` also carries latency fields
  (`ttft_ms`, `duration_api_ms`, …, `sdk.d.ts:5530-5560`).
- State edges: `session_state_changed` (`idle/running/requires_action`) is the best UI status primitive.
- Subagent nesting: `parent_tool_use_id` on every message, plus `task_*` system messages.
- Forward compatibility: unknown `type`s are skipped, not fatal (`message_parser.py:390-394`). Our parser
  must do the same.
- Error mapping: a non-zero exit after an `is_error` result is replaced by a `ResultError` that carries the
  result payload (`query.py:430-472`). API failures can arrive as `subtype:"success", is_error:true`
  (`query.py:55-80`).

### Strengths — steal these

1. **One process, many turns, full duplex**: NDJSON stdin/stdout plus a `request_id`-correlated control
   channel in both directions. Simple enough to implement in Rust in a few hundred lines. Goose already has a
   Rust implementation (`crates/goose/src/providers/claude_code.rs:333-398, 957-994`).
2. `can_use_tool` with `updatedInput`/`updatedPermissions`/`interrupt` gives the orchestrator a real
   **policy enforcement point**.
3. Orchestrator-assigned `--session-id`, plus `--fork-session` and `--resume-session-at` for branching and
   retrying from a checkpoint.
4. `session_state_changed`, `terminal_reason` and `total_cost_usd` provide first-class run telemetry.
5. `spawnClaudeCodeProcess` separates **what protocol** from **where the process runs**.
6. Transcript mirroring to an external store (`--session-mirror`).

### Weaknesses & tradeoffs

- The protocol is not formally specified. The source of truth is the SDK's `.d.ts`, which changes weekly
  (39 message types; `SDKControlRequestInner` has 39 subtypes). The SDK is version-paired with the CLI
  (`0.3.281` ↔ `2.1.281`). **Pin the CLI version per adapter release** and parse leniently.
- Stdin lifecycle edge cases: closing stdin too early breaks hooks and permission callbacks for background
  tasks. The SDK's own comments admit the ledger approach is a mitigation (`query.py:796-805`).
- A 1 MB line limit by default. Large tool outputs need a larger buffer.
- Session storage is keyed by cwd, which is awkward for per-task worktrees. Solve this by giving each
  workspace its own `CLAUDE_CONFIG_DIR` or keeping the cwd stable.

### Implications for a Rust framework

- Build `ClaudeCodeAdapter` directly on this protocol, not through ACP. It is the richest surface: USD cost,
  approvals, hooks, subagents, fork/resume.
- Needed: an NDJSON framer with a configurable max line size that skips non-JSON lines; a control-channel
  multiplexer (`HashMap<RequestId, oneshot::Sender>` for our requests, plus a handler registry for CLI
  requests, with cancellation on `control_cancel_request`); lenient serde (`#[serde(other)]`, keep the raw
  `serde_json::Value`).
- Always pass `--permission-prompt-tool stdio` and answer with our policy engine. Pass `--session-id`
  from our DB. Strip `CLAUDECODE`. Set `CLAUDE_CODE_ENTRYPOINT` to our own id. Use equals-form flags for
  untrusted values.

---

## 2. OpenAI Codex (`codex-rs`) — Rust reference implementation

### What it is

Codex CLI is written in Rust: a Cargo workspace of about 100 crates in `codex-rs/` (`codex-rs/Cargo.toml`
`[workspace] members`). It is the most useful **Rust reference** in this set, showing how a production agent
structures protocol crates, transports, sandboxing and persistence.

### Architecture

Crates that matter to us (`codex-rs/`):

| Crate | Role |
|---|---|
| `protocol/` | Core **SQ/EQ protocol**: "Uses a SQ (Submission Queue) / EQ (Event Queue) pattern to asynchronously communicate between user and agent" (`protocol/src/protocol.rs:1-4`). `enum Op` submissions (`:590`: `Interrupt, TurnInput, ExecApproval, …`), `struct Event { id, msg: EventMsg }` (`:1337-1355`), `AskForApproval` (`:983`), `SandboxPolicy` (`:1069`) |
| `core/` | Agent loop (`codex_thread.rs`, `exec.rs`, `apply_patch.rs`, `compact*.rs`, `mcp_tool_call.rs`, `guardian*`) |
| `app-server-protocol/` | Typed JSON-RPC API (v1 and v2) with `ts-rs` and `schemars` derives. Exports TS and JSON Schema (`src/export.rs`, `schema_fixtures.rs`) |
| `app-server/`, `app-server-transport/` | Server over **stdio, unix socket, websocket** (`app-server-transport/src/transport/{stdio,unix_socket,websocket}.rs`), plus a daemon mode |
| `app-server-client/` | `InProcessAppServerClient`, the same API over in-memory channels |
| `exec/` | `codex exec` headless runner, **implemented as an in-process app-server client** (`exec/src/lib.rs` imports `InProcessAppServerClient`, `ThreadStartParams`, `TurnStartParams`, `TurnInterruptParams`, …) |
| `sandboxing/`, `linux-sandbox/`, `bwrap/`, `windows-sandbox-rs/`, `process-hardening/`, `network-proxy/`, `execpolicy/` | Sandboxing and policy |
| `rollout/`, `thread-store/`, `state/` | JSONL rollouts plus SQLite state |
| `rmcp-client/`, `codex-mcp/` | MCP client (on `rmcp`) |
| `worktree/`, `git-utils/` | `--worktree` managed git worktrees |

Core dependencies (`codex-rs/Cargo.toml [workspace.dependencies]`): `tokio 1`, `tokio-util`, `futures`,
`serde`/`serde_json`, `thiserror 2`, `anyhow`, `tracing`, `clap 4`, `axum 0.8`, `reqwest 0.12`,
`tokio-tungstenite 0.28` (forked), `rmcp =3.2.0`, `sqlx =0.9.0`, `ts-rs 11`, `schemars 0.8`,
`landlock 0.4`, `seccompiler 0.5`, `portable-pty 0.9`, `uuid`, `chrono`.

### Protocol / invocation details

Codex has **three machine interfaces**. An MCP-server mode (`codex mcp-server`) no longer appears in
`cli/src/main.rs`'s subcommands; `codex mcp` now manages *external* MCP servers.

**(a) `codex exec --json`: one-shot JSONL** (`--json` has alias `--experimental-json`, `exec/src/cli.rs:58-65`).

```bash
codex exec --json [--model M] [--sandbox read-only|workspace-write|danger-full-access] \
  [--cd DIR] [--add-dir DIR] [--skip-git-repo-check] [--ephemeral] [--output-schema FILE] \
  [-o LAST_MSG_FILE] [-c key=value ...] [--image F] [--worktree] \
  [--dangerously-bypass-approvals-and-sandbox|--yolo] "<prompt or - for stdin>"
codex exec --json resume <SESSION_ID|--last> "<follow-up>"   # continue a thread (cli.rs:149-230)
codex exec --json fork <SESSION_ID> "<prompt>"
codex exec --json review --uncommitted|--base BR|--commit SHA
```

Shared flags live in `utils/cli/src/shared_options.rs`. `codex exec` **forces
`approval_policy = Never`** in headless mode (`exec/src/lib.rs:576`), so approvals are not interactive. Safety
comes from the `--sandbox` choice. The official TS SDK drives this mode (`sdk/typescript/src/exec.ts:92`:
`["exec","--experimental-json", …]`, with `resume <threadId>` for follow-ups at `:166-168`). Event schema
(`exec/src/exec_events.rs`, `#[serde(tag="type")]`):

```jsonc
{"type":"thread.started","thread_id":"…"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"bash -lc 'cargo test'",
  "aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.updated","item":{…}}          // e.g. todo_list progress
{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"src/x.rs","kind":"update"}],"status":"completed"}}
{"type":"turn.completed","usage":{"input_tokens":…,"cached_input_tokens":…,"cache_write_input_tokens":…,
  "output_tokens":…,"reasoning_output_tokens":…}}
{"type":"turn.failed","error":{"message":"…"}}
{"type":"error","message":"…"}
```

Item types (`ThreadItemDetails`, `:105-133`): `agent_message{text}`, `reasoning{text}`,
`command_execution{command,aggregated_output,exit_code,status in_progress|completed|failed|declined}`,
`file_change{changes[{path,kind add|delete|update}],status}`, `mcp_tool_call{server,tool,arguments,result,error,status}`,
`collab_tool_call{tool spawn_agent|send_input|wait|close_agent, receiver_thread_ids, agents_states}`,
`web_search`, `todo_list{items[{text,completed}]}`, `error`.

**(b) `codex app-server`: full bidirectional JSON-RPC** (`cli/src/main.rs:553-576`:
`--listen stdio://` (default) `| unix://[PATH] | ws://IP:PORT | off`, or `--stdio`). Framing is JSON-RPC 2.0
**without the `"jsonrpc":"2.0"` field** ("We do not do true JSON-RPC 2.0", `app-server-protocol/src/rpc.rs:1-2`),
one message per line. The official Python SDK spawns `codex app-server --listen stdio://`
(`sdk/python/src/openai_codex/client.py:256`), then `initialize` → `initialized`
(`client.py` `initialize()`). The method catalog is in `app-server-protocol/src/protocol/common.rs`:

```jsonc
→ {"id":1,"method":"initialize","params":{"clientInfo":{"name":"factory","title":null,"version":"0.1"},
     "capabilities":{"experimentalApi":false}}}
→ {"method":"initialized"}
→ {"id":2,"method":"thread/start","params":{"cwd":"/ws/task-42","model":"…","approvalPolicy":"on-request",
     "sandbox":"workspace-write","developerInstructions":"…","ephemeral":false,"dynamicTools":[…]}}
← {"method":"thread/started","params":{…}}
→ {"id":3,"method":"turn/start","params":{"threadId":"…","input":[{"type":"text","text":"Fix CI"}],
     "approvalPolicy":null,"sandboxPolicy":null,"model":null}}      // per-turn overrides (v2/turn.rs:167-240)
← turn/started · item/started · item/agentMessage/delta · item/reasoning/summaryTextDelta ·
  item/commandExecution/outputDelta · item/fileChange/patchUpdated · turn/diff/updated · turn/plan/updated ·
  thread/tokenUsage/updated · item/completed · hook/started · hook/completed
← {"id":"s1","method":"item/commandExecution/requestApproval","params":{"threadId","turnId","itemId",
     "command","cwd","reason","commandActions","proposedExecpolicyAmendment","availableDecisions":[…]}}
→ {"id":"s1","result":{"decision":"accept"}}   // accept|acceptForSession|acceptWithExecpolicyAmendment|
                                               // applyNetworkPolicyAmendment|decline|cancel
→ {"id":4,"method":"turn/steer","params":{"threadId":"…","input":[…]}}         // inject mid-turn
→ {"id":5,"method":"turn/interrupt","params":{"threadId":"…","turnId":"…"}}
← {"method":"turn/completed","params":{"threadId":"…","turn":{"status":"completed|interrupted|failed",…}}}
```

Server → client requests: `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`
(decisions `accept|acceptForSession|decline|cancel`, `v2/item.rs:115`), `item/permissions/requestApproval`,
`item/tool/requestUserInput`, `mcpServer/elicitation/request`, `item/tool/call` (host-implemented "dynamic
tools" declared in `thread/start.dynamicTools`, like Claude's in-process MCP), and
`account/chatgptAuthTokens/refresh`. The 70+ client methods include `thread/{start,resume,fork,read,list,
archive,revert,compact/start}`, `turn/{start,steer,interrupt}`, `review/start`, `command/exec`,
`fs/*`, `config/*`, `mcpServerStatus/list`, `model/list`. Param structs are camelCase. Policy enums are
kebab-case (`AskForApproval`: `untrusted | on-request | never | granular{…}`, `protocol.rs:983-1006`;
`SandboxMode`: `read-only | workspace-write | danger-full-access`).

Schemas can be generated from the binary: `codex app-server generate-json-schema` / `generate-ts`
(`cli/src/main.rs:617-621`). **We can code-generate Rust or TS client types from the exact binary we pin.**

**(c) In-process**: `codex-app-server-client::InProcessAppServerClient` (used by `exec` and the TUI). This is
only relevant if we linked Codex crates, which we should not (Apache-2.0, but huge and unstable internal APIs).

### Session, resume & state

- Rollouts: JSONL per thread at `~/.codex/sessions/rollout-<ts>-<thread-id>.jsonl` (`rollout/src/lib.rs:86`,
  `rollout/src/recorder.rs:1, 82-102`), plus SQLite metadata (`state/src/sqlite.rs`, `thread-store/`).
- `thread/resume {threadId | history | path}`, `thread/fork`, `thread/revert` (produces a new rollout file),
  `thread/archive|delete`, `exec resume <id>|--last`, `--ephemeral` (no persistence). `CODEX_HOME`
  relocates everything, so use one per workspace for isolation.
- Turn lifecycle (`TurnStatus: Completed | Interrupted | Failed | InProgress`, `v2/turn.rs:33`) is explicit
  and separate from thread lifetime.

### Permissions, sandboxing & safety

- Approval policy (`AskForApproval`) is independent of the sandbox policy (`SandboxPolicy: DangerFullAccess |
  ReadOnly{network_access} | ExternalSandbox{network_access} | WorkspaceWrite{writable_roots, network_access,…}`,
  `protocol.rs:1069-1110`). `ExternalSandbox` exists for exactly our case, where the orchestrator already
  sandboxes (container/VM) and the agent should not nest another sandbox.
- **Linux**: bubblewrap is now the default filesystem sandbox. It uses `--ro-bind / /` and binds writable
  roots, re-applies protected subpaths (`.git`, `.codex`) as read-only, and adds `PR_SET_NO_NEW_PRIVS` plus a
  **seccomp network filter** in-process. Legacy Landlock is rejected for filesystem-restricted policies
  because "it cannot isolate app-server Unix sockets" (`linux-sandbox/README.md`). The sandbox type enum is
  `None | MacosSeatbelt | LinuxSeccomp | WindowsRestrictedToken | WindowsMxc` (`protocol/src/sandbox.rs:10-16`).
- **macOS**: Seatbelt `sandbox-exec` profiles, closed by default (`(deny default)`), in
  `sandboxing/src/seatbelt_base_policy.sbpl` and `seatbelt_network_policy.sbpl`.
- **execpolicy**: Starlark `prefix_rule(pattern=[…], decision=allow|prompt|forbidden, justification,
  match=[…], not_match=[…])`. The examples are validated at load time as unit tests
  (`execpolicy/README.md`). Approval replies can include `acceptWithExecpolicyAmendment`, which learns a
  rule from an approval.
- "Guardian" auto-review (`--approve-for-me`: `approvals_reviewer="auto_review"`, `on-request`,
  `workspace-write`, `utils/cli/src/shared_options.rs`). An LLM reviewer answers approvals, and
  `item/autoApprovalReview/{started,completed}` notifications expose that.
- `codex sandbox -- <cmd>` runs any command in the host sandbox (Seatbelt / Linux / Windows variants,
  `cli/src/main.rs:188-189, 459-467`). This could serve as an **off-the-shelf sandbox for our own fallback
  agent** if we do not build one.

### Events & observability

- The item model (`item.started → item.updated* → item.completed`, stable `item.id`) plus streaming deltas
  is the cleanest UI contract of all the agents studied.
- `turn/diff/updated` carries the **aggregated unified diff for the turn**, which is ideal for a live
  "changes" pane.
- Tokens only, **no USD**: `thread/tokenUsage/updated{total,last,modelContextWindow}`
  (`v2/thread.rs:1895-1925`); exec `turn.completed.usage`. The orchestrator must price tokens itself.
- Rate limits: `account/rateLimits/updated`. OTEL is built in (`otel/`).

### Strengths — steal these

1. **SQ/EQ core** (`Op` in, `Event` out) and **one API served over several transports**: in-process
   channels, stdio, unix socket, websocket. `exec` and the TUI are just clients. Build our orchestrator core
   the same way: the web UI, CLI and tests all talk to one typed API.
2. **Protocol crate with `ts-rs` + `schemars` derives.** The UI gets generated TS types and external
   clients get JSON Schema. Do this for our event enum.
3. Item lifecycle with stable ids, patch-style updates and an aggregated turn diff.
4. Split **approval policy** and **sandbox policy**, and add an `ExternalSandbox` mode.
5. `turn/steer` (inject into a running turn) is distinct from `turn/start` (queue a new turn) and
   `turn/interrupt`.
6. Learned approvals (execpolicy amendments) and an LLM guardian reviewer as a pluggable approval routing
   target.

### Weaknesses & tradeoffs

- `exec --json` is one-shot, with no approvals and no mid-run input except starting `exec resume` again.
  The full app-server API is large (70+ methods), partly `experimental`, and its README is a changelog of
  edge cases (`app-server/README.md`).
- Not true JSON-RPC (no `jsonrpc` field). Generic JSON-RPC crates must tolerate that.
- No USD cost reporting.
- CLI flags drift: goose's Codex provider still passes `--full-auto` (`crates/goose/src/providers/codex.rs:107-110`),
  but that flag no longer exists anywhere in `codex-rs`. Adapters must probe versions and have integration
  tests against pinned binaries.

### Implications for a Rust framework

- Ship two Codex adapters: `CodexExecAdapter` (MVP, one-shot, sandbox-only safety, trivial JSONL) and
  `CodexAppServerAdapter` (full: approvals, steer, interrupt, resume). Generate its types from
  `codex app-server generate-json-schema` of the pinned version.
- When the orchestrator already sandboxes the workspace (container), run Codex with
  `sandbox: danger-full-access` or `ExternalSandbox`. Otherwise the bwrap-in-docker nesting problem applies.
- Copy the Codex workspace shape: a `protocol` crate (types + derives, no deps), a `core`, `transport`
  crates, and thin binaries.

---

## 3. Goose (`block/goose` → `aaif-goose/goose`)

### What it is

Goose is an open-source general-purpose agent with a Rust core, CLI and desktop. Its "goose 2.0"
direction is to **unify every client on ACP**: "We needed a single protocol that any client could speak to
reach the same agent core … we have chosen ACP … as our new default interface to goose"
(`documentation/blog/2026-04-08-goose-acp-and-new-tui/index.md`). The old custom REST+SSE server `goosed` is
slated for removal (phase 4 in that post). Goose is both an **ACP agent** (`goose acp` on stdio,
`goose serve` on HTTP+WS, `crates/goose-cli/src/cli.rs:838-866`) and an **ACP client** that wraps other agents
as "providers".

### Architecture

`crates/`: `goose` (core: agents, providers, acp server, session, recipes, security), `goose-agent` ("The
GDK's Agent Loop", a new state machine: `machine.rs`, `operation.rs`, `inference.rs`, `tool.rs`),
`goose-provider-types` (the `Provider` trait), `goose-providers` (HTTP LLM providers), `goose-mcp`
(built-in MCP servers), `goose-cli`, `goose-sdk`/`goose-sdk-types` (GDK with uniffi bindings),
`goose-acp-macros`, `goose-context-management`, `goose-roaming` (iroh P2P). Dependencies:
`agent-client-protocol 2.2.0`, `agent-client-protocol-schema =1.9.1`, `rmcp 3.2.0`, `tokio`, `axum 0.8`,
`sqlx` (sqlite) (`Cargo.toml [workspace.dependencies]`). The agent loop is mid-migration: legacy
`crates/goose/src/agents/agent.rs` versus the state machine behind `GOOSE_STATE_MACHINE=1` (`AGENTS.md`).

- **Provider abstraction** (`crates/goose-provider-types/src/base.rs:474-540`): `stream(model_config, system,
  messages, tools) -> MessageStream` where `MessageStream = Stream<Item=Result<(Option<Message>,
  Option<ProviderUsage>)>>` (`:330`), plus `provider_session_id()` and `resume(session_id)`.
- **Coding agents wrapped as providers** (`crates/goose/src/providers/`): `claude_code.rs` (native stream-json +
  control protocol), `codex.rs` (`codex exec --json`), `gemini_cli.rs` (`gemini --output-format stream-json`),
  `cursor_agent.rs`, and ACP-based `claude_acp.rs` (spawns `claude-agent-acp`), `codex_acp.rs` (`codex-acp`),
  `copilot_acp.rs`, `amp_acp.rs`, `pi_acp.rs`. All ACP ones share `AcpProvider` (`crates/goose/src/acp/provider.rs`).
- **Extensions = MCP servers**: `ExtensionConfig` variants `stdio | builtin | platform | streamable_http`
  (`crates/goose/src/agents/extension.rs:157-208`). "Platform" extensions are in-process
  (`agents/platform_extensions/`: `developer`, `todo`, `summon`, `orchestrator`, `scheduler`,
  `code_execution`, …).
- **ACP server** (`crates/goose/src/acp/server/dispatch.rs`) handles `initialize`, `authenticate`,
  `session/new`, and so on, plus ~80 vendor extension methods under `_goose/unstable/...` (for example
  `_goose/unstable/config/extensions/add`, `SteerSessionRequest`) via `#[custom_methods]`
  (`crates/goose/src/acp/server/custom_dispatch.rs`, `crates/goose-sdk-types/src/custom_requests.rs`).

### Protocol / invocation details

How goose drives other agents (verified argv):
- **Claude Code** (`providers/claude_code.rs:333-398`): `claude --input-format stream-json --output-format
  stream-json --verbose [--mcp-config F --strict-mcp-config] --include-partial-messages --system-prompt-file F
  --model M` plus mode flags: Auto → `--dangerously-skip-permissions`; Approve/SmartApprove →
  `--permission-prompt-tool stdio`, with `can_use_tool` answered in Rust (`:957-994`). It sends an
  `initialize` control request (`:442`) and `set_model` via control request (tests at `:1515-1537`).
- **Codex** (`providers/codex.rs:97-197`): `codex exec [-m M] [-c model_reasoning_effort="…"] --json
  [--skip-git-repo-check] [-i image] -` with the prompt on stdin. Modes: `--yolo` / `--full-auto` (stale) /
  `--sandbox read-only`.
- **Gemini CLI** (`providers/gemini_cli.rs:97-113`): `gemini -m M [-r <session_id>] --output-format stream-json
  --yolo`, prompt on stdin. It parses `{"type":"init","session_id"}`, `{"type":"message","role":"assistant","content"}`,
  `{"type":"result","stats":{…}}` and `{"type":"error"}` (`:230-290`). The provider is marked deprecated in
  favor of API providers.
- **ACP agents** (`providers/claude_acp.rs`): spawn the adapter binary, `initialize`, `session/new` with
  goose's extensions converted to ACP `mcpServers`, and map goose modes to the agent's session modes
  (`Auto→bypassPermissions`, `Approve→default`, `SmartApprove→acceptEdits`, `Chat→plan`).

### Session, resume & state

SQLite `sessions.db` (`crates/goose/src/session/session_manager.rs:29`, schema `:1025-1060`). The `Session`
row stores `working_dir, provider_name, model_config_json, goose_mode, recipe_json, accumulated_*_tokens,
accumulated_cost, parent_session_id, project_id, schedule_id` (`:62-110`). Conversation messages live
alongside. Import from Claude Code / Codex / Pi `.jsonl` transcripts is supported (`goose-cli/src/cli.rs:594`).
Provider-side sessions (Claude's or Gemini's `session_id`) are tracked via `provider_session_id()`/`resume()`.

### Permissions, sandboxing & safety

- `GooseMode`: `auto | approve | smart_approve | chat` (`crates/goose-provider-types/src/goose_mode.rs:22-34`).
- A **ToolInspector pipeline** (`crates/goose/src/tool_inspection.rs`): each inspector returns
  `InspectionAction::{Allow, Deny, RequireApproval(Option<String>)}` with `confidence`, `reason`,
  `inspector_name`. Implementations: `security/{security_inspector, adversary_inspector, egress_inspector,
  scanner, patterns}.rs` and `permission/{permission_inspector, permission_judge, permission_store}.rs`.
- Malware check on extension launch commands (`agents/extension_malware_check.rs`).
- No OS sandbox of its own. It relies on the host or container.

### Events & observability

Core event enum (`crates/goose-agent/src/events.rs`):
`AgentEvent::{Message(Message), Usage(ProviderUsage), MessageUsage{message_id,usage},
McpNotification((String, ServerNotification)), HistoryReplaced(Conversation)}`. The ACP layer turns these
into `session/update` notifications. OTEL tracing (`crates/goose/src/otel/`), `gen_ai_telemetry.rs`.

### Strengths — steal these

1. **ACP as the single client protocol** for TUI, desktop and IDEs, with vendor extensions in the `_goose/`
   namespace. Our orchestrator could do the same: expose ACP to editors and a richer native WS API to our UI.
2. **An orchestrator exposed to agents as an MCP extension**: `platform_extensions/orchestrator.rs` gives the
   model tools `list_sessions, view_session, start_agent, send_message, interrupt_agent`, and `summon.rs`
   gives `delegate` (instructions or a recipe `source`, with `parameters` and `extensions`). This is the right
   shape for letting a planner agent drive a swarm through our control plane.
3. **Recipes** (`crates/goose/src/recipe/mod.rs:43-83`): versioned YAML with `instructions, prompt,
   extensions, settings, parameters, response (JSON schema), sub_recipes`. This is a portable "task template"
   format.
4. The ToolInspector chain as a composable approval policy.
5. A Rust implementation of the Claude Code control protocol exists, and we can learn from its edge cases
   (`claude_code.rs`).

### Weaknesses & tradeoffs

- **Coding agents modeled as LLM "providers"** (`stream(system, messages, tools)`) is an impedance mismatch.
  These agents own their context, tools and approvals (`manages_own_context() -> true` in
  `gemini_cli.rs`), so goose has to flatten history into a prompt and loses tool-level events. **Our
  abstraction must be "agent session", not "model".**
- Two agent loops mid-migration, and an architecture in flux (goosed → ACP, Electron → Tauri).
- Adapters bit-rot with upstream CLIs (`--full-auto`).

### Implications for a Rust framework

- The `agent-client-protocol` crate is production-used in Rust by goose (client and server). That
  de-risks our ACP adapter.
- Offer an MCP server from the orchestrator (tools such as `start_agent/send_message/view_session/interrupt`)
  that we inject into every agent via `--mcp-config` / ACP `mcpServers` / Codex `dynamicTools`.
- Keep per-session rows with `parent_session_id`, accumulated tokens and cost, and the provider session id,
  as goose does.

---

## 4. Agent Client Protocol (ACP) — Zed

### What it is

ACP "standardizes communication between code editors … and coding agents" (`README.md`). It is **JSON-RPC 2.0
over stdio**, newline-delimited, with no embedded newlines (`docs/protocol/v1/transports.mdx:17-24`). The
agent usually runs as a subprocess of the client. Current stable protocol version: **1**. **v2 is a draft**
with breaking changes (`docs/protocol/v2/migration.mdx`). The repo holds the spec docs, JSON Schemas
(`schema/v1`, `schema/v2`, each with `schema.unstable.json`) and the Rust **schema** crate
`agent-client-protocol-schema` 1.9.1. The runtime crate `agent-client-protocol` 2.2.0 lives in
`agentclientprotocol/rust-sdk`, alongside `-http` (HTTP/SSE + WebSocket), `-rmcp`, `-conductor`, `-polyfill`
and `-trace-viewer`.

### Architecture

Roles: **Client** (editor or orchestrator) and **Agent**. The Rust SDK adds **Proxy** and **Conductor**
roles, for chains of message-rewriting proxies between client and agent (`docs/rfds/proxy-chains.mdx`;
`rust-sdk/src/agent-client-protocol-conductor`). Capabilities are negotiated at `initialize`
(`AgentCapabilities{loadSession, promptCapabilities, mcpCapabilities, sessionCapabilities{list,delete,resume,
close,additionalDirectories}, auth}`, `ClientCapabilities{fs, terminal, session, auth, elicitation}`, from
`schema/v1/schema.json`). Extensibility comes through `_meta` fields and `_`-prefixed methods
(`docs/protocol/v1/extensibility.mdx`).

### Protocol / invocation details

Method catalog (`schema/v1/meta.json`):
- Agent methods (client→agent): `initialize`, `authenticate`, `session/new {cwd, mcpServers[], additionalDirectories?}`,
  `session/load {sessionId, cwd, mcpServers}` (replays history as updates), `session/resume`, `session/list`,
  `session/delete`, `session/close`, `session/set_mode`, `session/set_config_option`,
  **`session/prompt {sessionId, prompt: ContentBlock[]}` → `{stopReason}`**, plus the notification `session/cancel`.
  `$/cancel_request` is available for any request.
- Client methods (agent→client): **`session/update`** (notification), **`session/request_permission
  {sessionId, toolCall, options[]}`** → `{outcome: selected{optionId} | cancelled}`, `fs/read_text_file`,
  `fs/write_text_file`, `terminal/{create,output,release,wait_for_exit,kill}`, `elicitation/{create,complete}`.
- `session/update` variants (v1 stable): `user_message_chunk, agent_message_chunk, agent_thought_chunk,
  tool_call, tool_call_update, plan, available_commands_update, current_mode_update, config_option_update,
  session_info_update, usage_update`. Unstable adds `plan_update, plan_removed, notice, compaction_update,
  compaction_summary_chunk`.
- `ToolCall {toolCallId, title, kind: read|edit|delete|move|search|execute|think|fetch|switch_mode|other,
  status: pending|in_progress|completed|failed, content: [content|diff|terminal], locations, rawInput, rawOutput}`.
- `StopReason: end_turn | max_tokens | max_turn_requests | refusal | cancelled`.
- `PermissionOptionKind: allow_once | allow_always | reject_once | reject_always`.
- Usage: `usage_update {used, size, cost?{amount,currency}}` (context-window occupancy). Per-turn token
  `usage` on `PromptResponse` exists **only in the unstable schema** (`schema/v1/schema.unstable.json`,
  RFD `docs/rfds/end-turn-token-usage.mdx`).

Minimal Rust client (from `rust-sdk/src/agent-client-protocol/examples/yolo_one_shot_client.rs`):
`AcpAgent::from_str(cmd)` → `Client.builder().on_receive_notification(|n: SessionNotification| …)
.on_receive_request(|r: RequestPermissionRequest, responder| …).connect_with(agent, |conn| { conn.send_request(
InitializeRequest::new(ProtocolVersion::V1)); conn.send_request(NewSessionRequest::new(cwd));
conn.send_request(PromptRequest::new(session_id, blocks)) })`. The whole client is about 60 lines on tokio.

**v2 draft changes** (`docs/protocol/v2/migration.mdx`): the `session/prompt` response only acknowledges
acceptance, and completion plus the stop reason arrive as a `state_update` (`running | idle | requires_action`).
Updates become **upserts** (omitted = unchanged, `null` = cleared, chunks append). `tool_call` is folded into
`tool_call_update`. Client **fs and terminal APIs are removed**. `session/load` becomes
`session/resume{replayFrom}`. `session/list|resume|close` become required.

### Session, resume & state

State lives in the agent. ACP only offers `session/load` (with history replay via `session/update`),
`session/resume`, `session/list`, `session/close`, `session/delete`, and fork as an RFD/unstable feature. How
well resume works depends on each agent's implementation.

### Permissions, sandboxing & safety

Only `session/request_permission` with option kinds, plus `session/set_mode` or config options (for example
Claude's `default/acceptEdits/plan/bypassPermissions` exposed as modes, as goose maps them). No sandbox
concept at all. That is left to the agent or host.

### Events & observability

`session/update` is a decent, editor-oriented event stream: text and thought chunks, tool calls with kind and
status, diffs and terminal content, plan, mode changes, context usage. Missing from stable v1: per-turn tokens,
cost (optional), subagent structure (goes into `_meta`), rate limits, turn ids, and an explicit run-state
signal (v2 adds `state_update`).

### Who speaks it

`docs/get-started/agents.mdx` lists about 40 agents. Natively: **Gemini CLI, OpenCode, Goose, Cursor, GitHub
Copilot (preview), Kiro CLI, Qwen Code, Kimi CLI, Cline, Junie, Augment, Factory Droid, OpenHands, Mistral
Vibe, Docker cagent, Poolside**, and more. Via adapters: **Claude Code** (`zed-industries/claude-agent-acp`,
built on the Claude Agent SDK), **Codex** (`agentclientprotocol/codex-acp`), **Pi** (`pi-acp`).

### Strengths — steal these

1. **One adapter reaches the long tail** of agents (roughly 40), with a stable v1 and a maintained Rust crate
   (`agent-client-protocol`, tokio-based, used by goose).
2. A good event vocabulary: `ToolKind`, tool-call status lifecycle, `diff` content, `plan` entries, and
   permission options with allow/reject once/always. **Adopt these names in our normalized model** so the ACP
   mapping is close to identity.
3. v2's **upsert semantics** and `state_update(idle|running|requires_action)` are the right shape for a
   real-time UI, and match Claude's `session_state_changed`.
4. The Proxy/Conductor chain is a standard way to insert middleware (context injection, tool filtering,
   logging) between orchestrator and agent.
5. Exposing **our orchestrator as an ACP agent** would let Zed, JetBrains and other editors attach to a
   factory run for free.

### Weaknesses & tradeoffs (as a universal adapter layer)

- **Lowest common denominator.** Cost is optional, token usage is unstable, and there is no turn id,
  subagent tree, rate-limit, budget or structured-output concept. Anything richer goes into `_meta`, which
  means per-agent code again.
- **Adapters add a hop and lag.** `claude-agent-acp` is about 19,300 lines of TypeScript (`src/*.ts`;
  `acp-agent.ts` alone is 10,810 lines) and needs Node. It tunnels Claude-specific options through
  `_meta.claudeCode` (`src/acp-agent.ts:1355-1451`), and its `usage_update` sends `{used,size}` without cost
  (`:4446-4449`). We would lose `total_cost_usd`, `terminal_reason`, hooks and `updatedInput`.
- **Editor-centric.** v1's `fs/*` and `terminal/*` client methods assume the client owns the buffers and
  terminals. A headless orchestrator should **not** advertise those capabilities (agents then use their own
  tools). v2 removes them anyway.
- **Version churn.** v2 changes turn semantics (the prompt response no longer ends the turn), so we must
  negotiate per connection and support both for a while.

**Verdict:** ACP is the right **universal fallback** and the right **northbound protocol for editors**, but
not the primary integration for the agents we care most about. Implement Claude Code and Codex natively for
fidelity (cost, approvals with input rewriting, steer, fork). Implement one generic **ACP v1 client**
adapter (v2 behind a feature flag) for everything else: Gemini CLI, OpenCode, Goose, Cursor, Copilot,
Qwen, Kiro. Shape our normalized event model as a **superset of ACP `session/update`** so that adapter stays
thin.

### Implications for a Rust framework

- Depend on `agent-client-protocol` (runtime) and `agent-client-protocol-schema` (types) for the ACP adapter.
  Advertise `fs: none, terminal: none` and send our orchestrator MCP server in `session/new.mcpServers`.
- Use the ACP names (`ToolKind`, `ToolStatus`, `PermissionOptionKind`, `StopReason`, plan entry status) in our
  own enums.
- Later, a `factory acp` subcommand can expose each work item as an ACP session for editors.

---

## 5. mini-swe-agent — the minimal viable loop

### What it is

A deliberately tiny software-engineering agent from the SWE-agent team: "Just some 100 lines of python for the
agent class" (`README.md:26`). It is used for the SWE-bench bash-only leaderboard. Design claims
(`README.md:42-49`): **bash is the only tool** (it does not even need the tool-calling interface), history is
**completely linear**, and **every action runs independently via `subprocess.run`**, which makes sandboxing as
simple as swapping in `docker exec`.

### Architecture

Three protocols (`src/minisweagent/__init__.py:43-80`):
`Model.query(messages) -> dict`, `Environment.execute(action, cwd) -> {output, returncode, exception_info}`,
`Agent.run(task) -> dict`. The loop (`agents/default.py:88-157`):

```text
messages = [system(template), user(instance_template(task))]
loop:
  check limits (step_limit, cost_limit, wall_time_limit)  -> exit LimitsExceeded/TimeExceeded
  msg = model.query(messages); cost += msg.extra.cost      -> append
  outputs = [env.execute(a) for a in msg.extra.actions]    -> append observation messages
  on FormatError: append error-feedback message (max N consecutive)
  save trajectory JSON after every step
  stop when last message role == "exit"
```

Completion is signaled by the model running a command whose output's first line is
`COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT` (`environments/local.py:45-56`). The tool-call variant exposes a single
`bash{command}` function (`models/utils/actions_toolcall.py`, `BASH_TOOL`).

### Protocol / invocation details

`mini` CLI and batch runners (`run/mini.py`, `run/benchmarks/swebench.py`). Environments: `local`,
`docker` (`docker exec -w cwd …`, `environments/docker.py:101-110`), `singularity`, `bubblewrap`, `swerex_modal`,
`contree`. Models go through litellm, openrouter, portkey and requesty.

### Session, resume & state

The only state is the message list, serialized after every step as `trajectory_format: "mini-swe-agent-1.1"`
(`agents/default.py:159-190`). Resuming means reloading messages. There is no shell state to restore by design.

### Permissions, sandboxing & safety

None in the loop. Safety is fully delegated to the Environment (container or bwrap). Timeouts kill the
**whole process group** (`environments/local.py:72-92`).

### Events & observability

Trajectory JSON plus logs, with cost and API-call counters. It is simple and complete for offline analysis,
but has no streaming.

### Strengths — steal these

1. Model/Environment/Agent separation, with the environment as the only safety boundary.
2. Stateless per-action execution, which makes containers, remote hosts and replay trivial.
3. Hard limits (steps, cost, wall time) enforced **before** each model call, and format-error feedback with a
   consecutive-error cap.
4. Persist the trajectory after every step.

### Weaknesses & tradeoffs

No streaming, interrupts or approvals. `cd`/env changes do not persist between actions. Viewing and editing
files goes through `sed`/`cat`, which is token-inefficient on large files. There is no context management
(linear history grows until limits hit).

### Implications for a Rust framework

A native **fallback agent** (`NativeAgentAdapter`) is cheap to build: about 300 lines of Rust on
`reqwest` against the Anthropic, OpenAI or OpenAI-compatible APIs, with one `bash` tool (optionally
`str_replace_edit`), executed through the same `Launcher` as everything else (docker exec / bwrap / `codex
sandbox`). It emits our normalized events natively and serves as (a) the hermetic test double for the
orchestrator, (b) a low-cost worker for trivial tasks and reviewers, and (c) a fallback when a vendor CLI
breaks. Keep it minimal. Its value is being fully under our control, not matching Claude Code or Codex.

---

## Cross-agent comparison

| Capability | Claude Code (stream-json) | Codex app-server | Codex exec --json | ACP v1 (generic) | Native (mini-style) |
|---|---|---|---|---|---|
| Transport | NDJSON stdio + control msgs | JSON-RPC-ish stdio/unix/ws | JSONL stdout | JSON-RPC 2.0 stdio (HTTP/WS RFD) | in-process |
| Follow-up input on live process | yes (`user` msgs, `priority`) | yes (`turn/start`, `turn/steer`) | no (`exec resume`) | yes (`session/prompt`) | yes |
| Interrupt | `control_request interrupt` | `turn/interrupt` | kill process | `session/cancel` | cancel token |
| Tool approval | `can_use_tool` (+rewrite input, persist rules) | `*/requestApproval` (accept/forSession/decline/cancel, amendments) | none (`never`) | `session/request_permission` (options) | ours |
| Resume / fork | `--resume`, `--fork-session`, `--session-id`, `--resume-session-at` | `thread/resume`, `thread/fork`, `thread/revert` | `exec resume/fork` | `session/load|resume` (+fork unstable) | ours |
| Usage | tokens per message + **USD** per turn and per model | tokens (total/last, context window) | tokens per turn | context `used/size`, optional cost | ours |
| Subagents | `parent_tool_use_id`, `task_*` | collab items, child threads | `collab_tool_call` | `_meta` only | — |
| Turn end signal | `result` | `turn/completed` | `turn.completed/failed` | prompt response `stopReason` (v2: `state_update`) | loop exit |
| Run state | `session_state_changed` | `thread/status/changed` | — | v2 `state_update` | ours |
| Structured output | `--json-schema` → `structured_output` | `--output-schema` / turn param | `--output-schema` | — | ours |
| Host-provided tools | in-process MCP via `mcp_message`, `--mcp-config` | `dynamicTools` + `item/tool/call`, MCP config | `-c mcp_servers…` | `mcpServers` in `session/new` | native |

---

## Normalized event model

Design rules:
1. **Superset of ACP `session/update`**, reusing its vocabulary (`ToolKind`, `ToolStatus`, plan entries,
   permission option kinds, stop reasons), with **Codex-style item lifecycle** (stable ids, start/update/complete)
   and **ACP v2 upsert semantics** (updates patch by id).
2. Every event goes in an envelope with the orchestrator's ids, a monotonic `seq` for UI replay and resync,
   the optional subagent parent, and the **raw native payload** for audit and debugging.
3. The enum is `#[non_exhaustive]`, uses `serde(tag = "type")`, and derives `ts_rs::TS` +
   `schemars::JsonSchema` so the web UI gets generated types (the Codex pattern).

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema)]
pub struct EventEnvelope {
    pub run_id: RunId,                  // orchestrator id for this agent run (work item attempt)
    pub seq: u64,                       // monotonic per run; UI resumes from last seq
    pub at: chrono::DateTime<chrono::Utc>,
    pub agent: AgentKind,               // ClaudeCode | Codex | Acp(name) | Native
    pub native_session_id: Option<String>,
    pub turn_id: Option<TurnId>,        // ours; Codex turn id / Claude user uuid mapped in
    pub parent_call_id: Option<CallId>, // subagent nesting (Claude parent_tool_use_id, Codex collab)
    pub event: AgentEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(type = "unknown")]
    pub raw: Option<serde_json::Value>,  // original native line/notification (audit, debugging)
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEvent {
    SessionStarted { model: Option<String>, cwd: PathBuf, tools: Vec<String>,
                     mcp_servers: Vec<McpServerStatus>, agent_version: Option<String> },
    Status { state: RunState, detail: Option<StatusDetail> },      // Idle|Running|RequiresAction ; Compacting|Retrying{attempt,delay_ms}
    TurnStarted { input_ref: Option<String> },
    MessageDelta { message_id: MsgId, role: Role, channel: Channel, text: String }, // Channel: Text|Thought
    Message { message_id: MsgId, role: Role, content: Vec<ContentPart> },          // full upsert
    ToolCall { call_id: CallId, name: String, kind: ToolKind, title: String,
               status: ToolStatus, input: Value, locations: Vec<PathBuf> },         // upsert by call_id
    ToolOutputDelta { call_id: CallId, stream: OutStream, chunk: String },          // stdout/stderr
    ToolResult { call_id: CallId, status: ToolStatus, output: Value, exit_code: Option<i32> },
    FileChanges { call_id: Option<CallId>, changes: Vec<FileChange> },              // {path, kind, unified_diff?}
    TurnDiff { unified_diff: String },                                              // Codex turn/diff/updated; else computed by us from git
    Plan { entries: Vec<PlanEntry> },                                               // {content, status, priority}
    PermissionRequested { request_id: ReqId, call_id: Option<CallId>, tool: String, input: Value,
                          reason: Option<String>, options: Vec<PermissionOption> },
    PermissionResolved { request_id: ReqId, decision: PermissionDecision, decided_by: Decider },
    Task { task_id: String, phase: TaskPhase, description: String, usage: Option<Usage> }, // subagents / bg tasks
    Usage(UsageSnapshot),     // cumulative tokens; cost_usd: Option<f64>; context_used/size; cost_source: Native|Priced
    RateLimit { status: RateLimitStatus, resets_at: Option<i64>, utilization: Option<f32> },
    Notice { level: Level, message: String },
    TurnCompleted { stop: StopReason, result_text: Option<String>, structured: Option<Value>,
                    usage: Option<UsageSnapshot>, duration_ms: Option<u64>, num_steps: Option<u32> },
    Error { kind: ErrorKind, message: String, retryable: bool },
    SessionEnded { exit: ExitStatus },
}

pub enum StopReason { EndTurn, MaxTokens, MaxTurns, Budget, Refusal, Cancelled, ToolDeferred, Error }
pub enum ToolKind { Read, Edit, Delete, Move, Search, Execute, Think, Fetch, Mcp, Subagent, Other } // ACP + Mcp/Subagent
pub enum ToolStatus { Pending, InProgress, Completed, Failed, Declined }
```

How native events map (T = TurnCompleted):

| Normalized | Claude Code | Codex app-server v2 (exec --json) | ACP v1 | Gemini stream-json (if not via ACP) |
|---|---|---|---|---|
| `SessionStarted` | `system/init` | `thread/started` + `thread/start` response (`thread.started`) | `session/new` response | `init` |
| `Status` | `system/session_state_changed`, `system/status` (compacting), `system/api_retry` (attempt, retry_delay_ms) | `thread/status/changed` | derived (prompt in flight / pending permission); v2 `state_update` | derived |
| `TurnStarted` | synthesized when we write a `user` message (bind via `uuid` echo) | `turn/started` (`turn.started`) | synthesized on `session/prompt` | synthesized |
| `MessageDelta` | `stream_event` `content_block_delta` (text/thinking) | `item/agentMessage/delta`, `item/reasoning/*Delta` | `agent_message_chunk`, `agent_thought_chunk` | `message` (delta) |
| `Message` | `assistant` text/thinking blocks | `item/completed` AgentMessage/Reasoning (`agent_message`, `reasoning`) | v1: aggregate chunks; v2 `agent_message` | aggregate |
| `ToolCall` | `assistant` `tool_use` block; kind from tool name (Bash→Execute, Edit/Write→Edit, Read→Read, Grep/Glob→Search, Task→Subagent, `mcp__*`→Mcp) | `item/started` CommandExecution/FileChange/McpToolCall/WebSearch | `tool_call` / first `tool_call_update` | `tool_use` |
| `ToolOutputDelta` | `tool_progress` (heartbeat only) | `item/commandExecution/outputDelta` | `tool_call_update` content (terminal) | — |
| `ToolResult` | `user` `tool_result` block (`is_error`) | `item/completed` (status, exit_code) | `tool_call_update{status}` | `tool_result` |
| `FileChanges` | derive from Edit/Write/MultiEdit input (+ git diff) | FileChange item, `item/fileChange/patchUpdated` | `diff` content in tool call | derive from git |
| `Plan` | `TodoWrite` tool input | `turn/plan/updated` (`todo_list`) | `plan` | — |
| `PermissionRequested/Resolved` | `control_request can_use_tool` / our `control_response` | `item/*/requestApproval` / our response + `serverRequest/resolved` | `session/request_permission` / our outcome | n/a (`--yolo`) |
| `Task` | `system/task_started|progress|notification|updated` | collab items, child thread notifications | `_meta` | — |
| `Usage` | `assistant.message.usage`; `result.usage/modelUsage/total_cost_usd` (cost_source=Native) | `thread/tokenUsage/updated` (priced by us) | `usage_update` (+unstable `PromptResponse.usage`) | `result.stats` |
| `RateLimit` | `rate_limit_event` | `account/rateLimits/updated` | — | — |
| `TurnCompleted` | `result` (`subtype`, `terminal_reason`→StopReason, `result`, `structured_output`) | `turn/completed{turn.status}` (`turn.completed`/`turn.failed`) | `session/prompt` response `stopReason` | `result` |
| `Error` | `result.is_error`, `assistant.error`, non-zero exit | `error` notification, JSON-RPC errors | JSON-RPC error | `error` |
| `SessionEnded` | process exit | `thread/closed` / exit | connection closed | exit |

---

## Proposed AgentRuntime trait

Separate three concerns: **where** a process runs (`Launcher`), **how** we talk to it (`AgentRuntime`
adapters), and **who decides** approvals (`ApprovalBroker`). Adapters never talk to the UI. They emit
envelopes and await the broker.

```rust
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Static description of an adapter; lets the scheduler pick agents and the UI grey out actions.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub live_followups: bool,          // send() on a running process (Claude, Codex app-server, ACP)
    pub steer_mid_turn: bool,          // Codex turn/steer, Claude priority "now"
    pub interrupt: bool,
    pub approvals: ApprovalSupport,    // None | PerToolCall | PerCommandAndPatch
    pub approval_rewrites_input: bool, // Claude updatedInput
    pub resume: bool, pub fork: bool,
    pub cost: CostReporting,           // NativeUsd | TokensOnly | ContextOnly | None
    pub structured_output: bool,
    pub host_tools: HostTools,         // Mcp | InProcessMcp | DynamicTools | None
    pub own_sandbox: bool,             // agent can sandbox itself (Codex, Claude sandbox settings)
}

/// What to run. Adapter-neutral; adapters translate to flags / RPC params.
pub struct SessionSpec {
    pub workspace: PathBuf,                    // worktree root = cwd
    pub extra_dirs: Vec<PathBuf>,
    pub initial_input: Vec<InputPart>,         // text, images, file refs
    pub instructions_append: Option<String>,   // appended to the agent's own system prompt
    pub model: Option<String>,
    pub policy: PermissionPolicy,              // ReadOnly | Plan | Ask | AutoEdit | Autonomous{sandboxed: bool}
    pub mcp_servers: Vec<McpServerSpec>,       // incl. the orchestrator's control-plane MCP server
    pub resume: Option<ResumeFrom>,            // {native_session_id, fork: bool, at_message: Option<String>}
    pub session_id_hint: Option<String>,       // Claude --session-id; ignored by others
    pub limits: Limits,                        // max_turns, max_cost_usd, wall_clock, max_output_bytes
    pub output_schema: Option<serde_json::Value>,
    pub env: Vec<(String, String)>,            // incl. isolated CLAUDE_CONFIG_DIR / CODEX_HOME
    pub extra_args: Vec<String>,               // escape hatch, equals-form enforced
}

/// Where processes run: local, docker exec, ssh, k8s exec, VM. (Claude SDK's spawnClaudeCodeProcess.)
#[async_trait]
pub trait Launcher: Send + Sync {
    async fn spawn(&self, cmd: CommandSpec) -> anyhow::Result<ChildIo>; // stdin/stdout/stderr + kill + wait
}

/// Decides tool permissions. Orchestrator impl = policy pipeline (goose ToolInspector-style:
/// static rules -> execpolicy prefix rules -> LLM reviewer -> human in UI), with timeout -> deny.
#[async_trait]
pub trait ApprovalBroker: Send + Sync {
    async fn decide(&self, run: &RunId, req: &PermissionRequest) -> PermissionDecision;
}
pub enum PermissionDecision {
    Allow { scope: Scope /* Once|Session|Always */, rewritten_input: Option<serde_json::Value> },
    Deny { message: String, interrupt_turn: bool },
}

pub struct RuntimeCtx {
    pub run_id: RunId,
    pub launcher: Arc<dyn Launcher>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub cancel: CancellationToken,
}

/// Factory for sessions of one agent type (ClaudeCode, CodexAppServer, CodexExec, Acp{cmd}, Native).
#[async_trait]
pub trait AgentRuntime: Send + Sync + 'static {
    fn kind(&self) -> AgentKind;
    fn capabilities(&self) -> &Capabilities;
    /// Version + auth + flag probe against the pinned binary; fail fast on drift.
    async fn probe(&self, launcher: &dyn Launcher) -> anyhow::Result<ProbeReport>;
    async fn start(&self, spec: SessionSpec, ctx: RuntimeCtx) -> anyhow::Result<AgentSession>;
}

/// A live session: an event stream plus a control handle (cloneable, usable from any task).
pub struct AgentSession {
    pub events: BoxStream<'static, EventEnvelope>,   // ends after SessionEnded
    pub control: Arc<dyn AgentControl>,
}

#[async_trait]
pub trait AgentControl: Send + Sync {
    fn native_session_id(&self) -> Option<String>;
    /// Queue a new turn (Delivery::NextTurn) or inject into the running one (Delivery::Steer).
    async fn send(&self, input: Vec<InputPart>, delivery: Delivery) -> anyhow::Result<TurnId>;
    async fn interrupt(&self) -> anyhow::Result<()>;
    async fn set_policy(&self, policy: PermissionPolicy) -> anyhow::Result<()>;
    async fn set_model(&self, model: &str) -> anyhow::Result<()>;
    /// Close stdin / end session gracefully, escalate SIGTERM -> SIGKILL after `grace`.
    async fn shutdown(&self, grace: Duration) -> anyhow::Result<ExitReport>;
}
```

Shared building blocks beneath the adapters:
- `transport::JsonLines`: `tokio::io::BufReader` + line framer with a max line size (default 16 MB, not
  1 MB), skipping non-JSON lines (Claude `[SandboxDebug]`), lenient decode into `enum Known | Unknown(Value)`.
- `transport::JsonRpcPeer`: bidirectional request/response correlation, server→client requests,
  cancellation (`$/cancel_request`, Claude `control_cancel_request`), tolerant of Codex's missing `jsonrpc`
  field. Used by the Codex app-server and ACP adapters. Claude's control channel is a small variant
  (`control_request`/`control_response` with `request_id`).
- `pricing`: token→USD tables for agents that report only tokens (Codex, most ACP agents). Mark
  `cost_source = Priced`.
- `recorder`: persist every envelope (raw included) to the run log (SQLite/Postgres + object storage), so the
  UI can replay by `seq`. Plus a `ReplayRuntime` that re-emits recorded runs for tests and UI development.
- Process hygiene: kill the process group on shutdown, reap on orchestrator exit, strip `CLAUDECODE`,
  isolate `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `GEMINI_*` per workspace, pass untrusted values in
  `--flag=value` form.

---

## Adapter priority

1. **Claude Code, native stream-json + control protocol.** Build first. It is the most likely default
   worker, has the richest surface (USD cost, `can_use_tool` with input rewriting and persistent rules,
   hooks, subagent tasks, `--session-id`/`--fork-session`/`--resume-session-at`, `session_state_changed`), a
   precise reference in `claude-agent-sdk-python` (`_internal/query.py`, `transport/subprocess_cli.py`), and an
   existing Rust implementation to crib from (`goose/crates/goose/src/providers/claude_code.rs`). Pin the CLI
   version, because the SDK/CLI pair moves weekly.
2. **Codex.** Start with `codex exec --json` (a day of work: one-shot JSONL, `--sandbox`, `exec resume` for
   follow-ups, `--output-schema`). Then do **`codex app-server --listen stdio://`** for approvals, `turn/steer`,
   `turn/interrupt`, fork and resume, with types generated from `codex app-server generate-json-schema` of the
   pinned binary. Price tokens ourselves.
3. **Generic ACP v1 client** on the `agent-client-protocol` crate (v2 behind a feature flag). One adapter
   covers Gemini CLI, OpenCode, Goose (`goose acp`), Cursor, Copilot CLI, Qwen, Kiro and ~30 more, and serves
   as a fallback path for Claude (`claude-agent-acp`) and Codex (`codex-acp`). Do not advertise fs/terminal
   client capabilities. Inject our control-plane MCP server via `session/new.mcpServers`. Accept
   lowest-common-denominator telemetry.
4. **Native fallback agent** (mini-swe-agent style) on model APIs, plus a **ReplayRuntime** for tests. It
   gives hermetic CI for the orchestrator, cheap reviewer/triage workers, and resilience when vendor CLIs
   break. Execute its bash tool through the same `Launcher` and sandbox.
5. **Later / northbound**: expose the orchestrator as an **ACP agent** (editors attach to runs) and as an
   **MCP server** of control-plane tools (`start_agent, send_message, view_session, interrupt_agent`, following
   goose's `platform_extensions/orchestrator.rs`) so planner agents can drive the swarm. Add one-shot headless
   adapters (for example Gemini `--output-format stream-json`) only for agents without ACP.
