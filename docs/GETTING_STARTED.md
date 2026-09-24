# Getting started with Forgeline

## Install

Forgeline is a single Rust binary. With a Rust toolchain (1.85+):

```sh
git clone https://github.com/Abil-Shrestha/claude-engineer forgeline
cd forgeline
cargo install --path crates/forgeline-cli     # installs `forgeline`
```

To run real agents you also need at least one agent CLI installed and logged
in. Today that is [Claude Code](https://docs.claude.com/en/docs/claude-code)
(`claude`). A built-in `mock` agent lets you try everything without one.

## Set up a repository

```sh
cd your-project
forgeline init          # writes forgeline.toml, adds .forgeline/ to .gitignore
```

Edit `forgeline.toml`. The most important part is **checks**: the commands
that decide whether an agent's work is done. Without them, work is integrated
unverified.

```toml
[agents.default]
runtime = "claude-code"
# model = "claude-opus-5"
permission_mode = "accept_edits"
allowed_tools = ["Read", "Glob", "Grep", "Edit", "Write", "MultiEdit", "TodoWrite"]

[[checks]]
name = "test"
command = "cargo test --workspace"

[limits]
max_parallel_agents = 4
max_attempts_per_task = 3
max_fix_rounds = 2
max_cost_usd = 25.0

[permissions]
allow = ["Bash(cargo:*)", "Bash(git status:*)", "Bash(git diff:*)"]
deny = ["WebFetch", "Bash(git push:*)"]
on_unknown = "escalate"     # ask a human
```

Then commit it. Forgeline reads `forgeline.toml` **from the base branch**, so
an agent can never loosen its own checks or permissions by editing the file.

```sh
git add forgeline.toml .gitignore && git commit -m "Add Forgeline"
forgeline doctor        # checks git, config, agents
```

## Your first run (no tokens spent)

```sh
forgeline run "Say hello" --agent mock
```

You will see the task start, the checks run, and the work merge into the run's
integration branch. `forgeline show <id>` prints the result; `git log
main..forgeline/<id>/integration` shows the commits.

## Real runs

A one-task run:

```sh
forgeline run "Fix the flaky retry test in src/net.rs"
```

A multi-task run with a plan, so agents work in parallel where the dependency
graph allows:

```toml
# plan.toml
summary = "Dark mode"

[[tasks]]
key = "tokens"
title = "Add color tokens for light and dark themes"
description = "Define the palette in src/theme.ts; no component changes."

[[tasks]]
key = "toggle"
title = "Add a theme toggle to the header"

[[tasks]]
key = "persist"
title = "Persist the chosen theme"
depends_on = ["toggle"]
acceptance = ["The choice survives a reload", "It defaults to the OS setting"]

[[tasks]]
key = "docs"
title = "Document theming"
depends_on = ["tokens", "persist"]
```

```sh
forgeline run "Add dark mode" --plan plan.toml --max-cost 10 --verbose
```

What happens:

1. `tokens` and `toggle` start at once, each in its own worktree under
   `.forgeline/worktrees/`, on its own branch.
2. When an agent finishes, Forgeline commits its work and runs your checks.
   If they fail, the output goes back to the same agent to fix (up to
   `max_fix_rounds`), then to a fresh attempt with the failure as feedback
   (up to `max_attempts_per_task`).
3. Verified work is merged into `forgeline/<id>/integration` one task at a
   time, and the checks run again on the combined code. A conflict or a
   failure sends the task back for another attempt on top of the new code.
4. `persist` starts only after `toggle` has landed, so it builds on it.
5. When every task is done, review and merge the integration branch like any
   other branch.

If an agent asks to run something your policy doesn't cover, the run pauses
on that attempt and prints an approval command:

```sh
forgeline approvals                          # what is waiting
forgeline approve apr_0192…                  # allow it
forgeline approve apr_0192… --deny --comment "use the test fixture instead"
```

## Everyday commands

| Command | What it does |
|---|---|
| `forgeline runs` | List runs with status, progress and spend |
| `forgeline show <run>` | Tasks, attempts, checks and spend for one run |
| `forgeline events <run> [--follow] [--json]` | The run's event log |
| `forgeline resume <run>` | Continue after Ctrl-C, a crash or a reboot |
| `forgeline serve` | HTTP API + live stream for the web UI ([API.md](API.md)) |
| `forgeline doctor` | Check the setup |

Run ids can be shortened to the 8 characters `forgeline runs` shows.

## Where things live

```
your-project/
├── forgeline.toml            # committed: agents, checks, limits, permissions
└── .forgeline/               # ignored: local state
    ├── forgeline.sqlite      # the event log (everything that happened)
    └── worktrees/<run>/      # agent worktrees and the integration worktree
```

Worktrees of successful attempts are removed after they merge (their branches
remain). Failed attempts' worktrees are kept so you can inspect them.
