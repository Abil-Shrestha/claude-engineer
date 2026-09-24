# Forgeline

**An open-source software factory: orchestrate swarms of coding agents from
work item to verified, integrated code.**

Give Forgeline a request and a plan. It runs a swarm of coding agents in
parallel, each in its own git worktree, verifies every change with your own
checks, merges the verified work one task at a time, retries what fails with
feedback, asks a human only when policy says so, and records every step in an
event log you can watch live, replay, and build a UI on.

```
$ forgeline run "Add dark mode" --plan plan.toml --parallel 2 --agent mock
started run_01a0d48f111d747688bc3d7cfd14e3c2
run running
▶ tokens attempt 1 (mock)
▶ toggle attempt 1 (mock)                       ← independent tasks run in parallel
  tokens check notes-exist passed (0.0s)
  tokens verified ($0.01 · 1.2k tokens)
  toggle check notes-exist passed (0.0s)
  toggle verified ($0.01 · 1.2k tokens)
  tokens check integration:notes-exist passed (0.0s)   ← re-checked on the merged code
  tokens merged into the integration branch
✓ tokens done
  toggle check integration:notes-exist passed (0.0s)
  toggle merged into the integration branch
✓ toggle done
▶ persist attempt 1 (mock)                      ← waited for `toggle` to land
  …
✓ docs done
run succeeded
```

(A real transcript with the built-in mock agent, which spends no tokens. With
`--agent claude-code` the same events come from Claude Code; failed checks go
back to the agent as feedback before a task is retried.)

> **Status: early.** The engine, CLI and API work end to end and are covered
> by tests, including the Claude Code adapter against a protocol-faithful fake.
> Planning and review agents, pull requests, sandboxing and the web UI are
> next. See the [roadmap](ROADMAP.md).

## Why another agent orchestrator?

We read the code of 30+ projects in this space (Symphony, Open SWE,
OpenHands, Gas Town, vibe-kanban, AX, Agent Substrate and many more) before
writing a line. The [synthesis](research/SYNTHESIS.md) is worth reading on its
own. Forgeline is built on what that research says works:

- **Deterministic core, LLMs at the edges.** Scheduling, retries, merging and
  budgets are tested Rust state machines, not prompts.
- **Verification is code.** Checks come from the base branch, run under
  Forgeline's control, and gate every merge. Agents can't grade their own work.
- **One worktree per attempt, one merge at a time.** Agents never share a
  working copy, and verified work lands through a serial merge queue that
  re-runs checks on the combined code.
- **Everything is an event.** A durable, gapless, sequenced log powers crash
  recovery, the live UI, replay and audit.
- **Fail closed.** Permission requests are never auto-approved; anything the
  policy doesn't cover waits for a human.
- **Agent-agnostic.** Claude Code today; Codex, ACP agents (Gemini CLI,
  Goose, OpenCode…) and a built-in loop next.

## Quick start

```sh
cargo install --path crates/forgeline-cli
cd your-project
forgeline init                                   # forgeline.toml + .gitignore
forgeline run "Say hello" --agent mock           # try it, no tokens spent
forgeline run "Fix the flaky retry test"         # for real, with Claude Code
forgeline serve                                  # API + live stream on :7777
```

Full guide: [GETTING_STARTED.md](GETTING_STARTED.md).

## Documentation

| | |
|---|---|
| [Getting started](GETTING_STARTED.md) | Install, configure, run, approve, resume |
| [API](API.md) | REST + live event stream; how to build a UI on it |
| [Architecture](ARCHITECTURE.md) | How it works and why |
| [Roadmap](ROADMAP.md) | What's built, what's next, how to help |
| [Research](research/SYNTHESIS.md) | What we learned from 30+ projects |

## Code map

| Crate | What it does |
|---|---|
| `forgeline-core` | Ids, domain model, events, `RunState` fold, task graph, budgets (no I/O) |
| `forgeline-store` | SQLite event log: validated appends, idempotency, live subscriptions |
| `forgeline-workspace` | Git worktrees, commits, diffs, merges |
| `forgeline-agents` | Agent runtimes: Claude Code (stream-json), scripted mock |
| `forgeline-config` | `forgeline.toml`, plans, permission policy |
| `forgeline-engine` | Scheduling, attempts, checks, fix rounds, merge queue, retries, approvals, recovery |
| `forgeline-server` | HTTP API, SSE stream, generated TypeScript types |
| `forgeline-cli` | The `forgeline` binary |

## Contributing

`cargo fmt --all && cargo clippy --workspace --all-targets && cargo test
--workspace` must pass (CI runs exactly that). Every behavior change comes with
a test; scheduling changes come with a test of the pure functions in
`forgeline-engine/src/schedule.rs`. Pick an item from the
[roadmap](ROADMAP.md) and open an issue that names it.

## License

Dual-licensed under [MIT](../LICENSE-MIT) or [Apache-2.0](../LICENSE-APACHE),
at your option.
