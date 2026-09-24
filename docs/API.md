# Bodega API

Everything Bodega does is visible, and controllable, through a small HTTP
API. The web UI, bots and scripts are all ordinary clients of it; the engine
never renders anything itself.

Start it with:

```sh
bodega serve                                      # http://127.0.0.1:7777
bodega serve --allow-origin http://localhost:5173 # plus a UI dev server
bodega serve --addr 0.0.0.0:7777 --token "$SECRET"  # remote access needs a token
```

TypeScript types for every request, response and event are generated from the
Rust types into [`ui/src/api/types.ts`](../ui/src/api/types.ts). CI fails if
they drift, so the UI can rely on them.

---

## The model in one minute

- A **run** is one piece of requested work (a feature, a bug). It has a plan
  of **tasks** with dependencies.
- Each task is worked on by **attempts**: one agent session in its own git
  worktree and branch. Failed attempts are retried with feedback.
- After each attempt, **checks** (build, tests…) run. Verified work is merged
  into the run's **integration branch**, one task at a time.
- Anything an agent is not allowed to do on its own becomes an **approval**
  waiting for a human.
- Every change is an **event** in an append-only log with a global, gapless
  sequence number `seq`. Snapshots (`RunState`, `RunSummary`) are folds of
  that log.

---

## Endpoints

| Method & path | Returns |
|---|---|
| `GET /api/health` | `{ name, version }` |
| `GET /api/runs` | `RunSummary[]`, most recently updated first |
| `POST /api/runs` | `201 { run_id }`: starts a run (body: `CreateRun`) |
| `GET /api/runs/{run_id}` | `RunState`: everything about one run |
| `GET /api/runs/{run_id}/events?after=0&limit=500` | `Event[]`: the run's history, paged by `seq` |
| `POST /api/runs/{run_id}/resume` | `202`: continue an unfinished run |
| `GET /api/approvals` | `PendingApproval[]` across all runs |
| `POST /api/approvals/{approval_id}` | `204`: resolve (body: `Resolve`) |
| `GET /api/stream?after=&run=` | Server-sent events: every event after `seq`, then live |

Errors are JSON: `{ "error": "…" }` with `400` (bad input, e.g. a plan with a
cycle), `401` (token), `404`, `409` (conflict, e.g. an approval that was
already resolved) or `415` (mutations must send `Content-Type:
application/json`).

**Auth.** With `--token`, send `Authorization: Bearer <token>`. `EventSource`
cannot set headers, so every endpoint also accepts `?token=<token>`. Without a
token, the server only answers requests addressed to `localhost` or a loopback
IP (`Host` header), which blocks DNS-rebinding attacks from web pages; it
refuses to listen on a non-loopback address without a token.

### Start a run

```http
POST /api/runs
Content-Type: application/json

{
  "title": "Add dark mode",
  "request": "Users want a dark theme that follows the OS setting.",
  "plan": {
    "summary": "Theme tokens, toggle, persistence, docs",
    "tasks": [
      { "key": "tokens",  "title": "Add color tokens for both themes" },
      { "key": "toggle",  "title": "Add a theme toggle to the header" },
      { "key": "persist", "title": "Persist the chosen theme", "depends_on": ["toggle"],
        "acceptance": ["The choice survives a reload"] },
      { "key": "docs",    "title": "Document theming", "depends_on": ["tokens", "persist"] }
    ]
  },
  "max_cost_usd": 10
}
```

`request`, `plan`, `base_ref` and `max_cost_usd` are optional. Without a plan
the whole request is a single task. The plan is validated (unique keys, known
dependencies, no cycles) before anything runs.

### Resolve an approval

```http
POST /api/approvals/apr_0192…
Content-Type: application/json

{ "decision": "approved", "comment": "fine for this repo", "by": "abil" }
```

`decision` is `"approved"` or `"rejected"`. A rejection's `comment` is passed
to the agent as the reason.

---

## The live stream

`GET /api/stream` is a [server-sent events](https://developer.mozilla.org/docs/Web/API/Server-sent_events)
stream. Each message's `data` is one `Event` as JSON, and its SSE `id` is the
event's `seq`.

- `?after=<seq>` starts after that event (default `0`: the whole history, then live).
- `?run=<run_id>` only sends one run's events.
- Reconnects are automatic and lossless: the browser sends `Last-Event-ID`
  and the server resumes from there. A client that falls behind is caught up
  from the log, never silently dropped.
- A keep-alive comment is sent periodically so proxies keep the connection open.

```ts
import type { Event, RunState } from "./api/types";

let run: RunState = await (await fetch(`/api/runs/${runId}`)).json();

const stream = new EventSource(`/api/stream?run=${runId}&after=${run.last_seq}`);
stream.onmessage = async (message) => {
  const event: Event = JSON.parse(message.data);
  switch (event.type) {
    case "agent":
      // Live transcript: append to the attempt's activity view.
      appendActivity(event.attempt_id, event.event);
      break;
    default:
      // Structural change: refresh the snapshot (cheap, and always correct).
      run = await (await fetch(`/api/runs/${runId}`)).json();
      render(run);
  }
};
```

Refetching the snapshot on structural events is the simplest correct client.
A client that wants zero refetches can fold events itself; the rules are in
`crates/bodega-core/src/state.rs` (`RunState::apply`), and every event
carries enough data to do it.

---

## Events

Every event has `seq`, `run_id`, `at_ms` (Unix milliseconds) and a `type`:

| `type` | Meaning | Key fields |
|---|---|---|
| `run_created` | A run was requested | `spec` (title, request, base_ref, budget) |
| `run_status_changed` | Run lifecycle | `status`: `pending`, `running`, `waiting_for_approval`, `succeeded`, `failed`, `cancelled`; `stage`; `reason` |
| `plan_proposed` | The plan | `summary`, `tasks` |
| `task_created` | A task exists | `task_id`, `spec` (key, title, description, role, depends_on, acceptance) |
| `task_status_changed` | Task lifecycle | `status`: `pending`, `ready`, `running`, `done`, `failed`, `skipped`, `cancelled`; `reason` (e.g. why it is retrying) |
| `attempt_started` | An agent started on a task | `attempt_id`, `task_id`, `number`, `agent` (runtime, model), `branch` |
| `agent` | Something the agent did | `attempt_id`, `event` (see below) |
| `check_completed` | A check ran | `attempt_id`, `result` (name, command, passed, exit_code, duration_ms, output_tail). Names starting with `integration:` ran on the merged code. |
| `attempt_finished` | The attempt ended | `outcome`: `{result: "succeeded", commit}`, `{result: "failed", kind, message}` or `{result: "cancelled"}`; `usage` |
| `integration_failed` | Verified work could not be merged (conflict, or checks failing on the combined code) | `task_id`, `reason` |
| `branch_integrated` | Work landed on the integration branch | `task_id`, `branch`, `commit` |
| `approval_requested` | A human decision is needed | `approval_id`, `kind` (`plan`, `merge`, `question`, `permission`), `title`, `details`, `task_id` |
| `approval_resolved` | Decision made | `approval_id`, `decision`, `by`, `comment` |
| `review_completed` | A reviewer finished (planned) | `verdict`, `findings` |
| `pull_request_opened` | A PR was opened (planned) | `url`, `number` |
| `budget_exceeded` | A spend limit was hit | `exceeded` |

### Agent events

`agent` events wrap a normalized `AgentEvent`, the same whichever agent
(Claude Code, the mock, and later Codex and ACP agents) produced it:

| `event.type` | Fields |
|---|---|
| `session_started` | `session_id`, `model` |
| `message` | `text`: what the agent said |
| `thinking` | `text`: a reasoning summary, when the runtime exposes one |
| `tool_call` | `call_id`, `tool`, `input` (JSON) |
| `tool_result` | `call_id`, `output` (truncated), `is_error` |
| `permission_requested` | `request_id`, `tool`, `input` |
| `usage` | `usage`: **cumulative** tokens and cost for the attempt |
| `log` | `level`, `message` (stderr, warnings) |
| `finished` | `success`, `summary`, `usage`: the agent's turn ended |

---

## Building a great UI on this

The research behind Bodega ([`research/05-workbenches-and-ui.md`](research/05-workbenches-and-ui.md))
found what makes supervising many agents tractable, and what nobody has built
yet. Suggested views, all derivable from the API above:

- **Attention inbox.** One row per run or attempt that needs you, sorted:
  waiting on you (`GET /api/approvals`) → failed → finished and unread →
  running. One keyboard shortcut to jump to the next one.
- **Run graph.** Tasks as a DAG (`RunState.tasks[*].spec.depends_on`), colored
  by status, with attempts stacked on each node.
- **Swarm timeline.** A Gantt of attempts across all runs (`attempt_started` →
  `attempt_finished`), with checks as markers. *Nobody has this.*
- **Attempt cockpit.** Live transcript (`agent` events), the diff of the
  attempt's branch, check output, and approvals rendered inline on the tool
  call that triggered them.
- **Verification badges.** Checks per attempt and `integration:` checks per
  merge, first class. *Nobody shows test status as a badge today.*
- **Replay.** Scrub through a finished run by `seq`: the log is complete, so
  the UI can show exactly what the swarm knew at any moment.
- **Spend.** Cost and tokens per attempt, run and day (`usage`), against the
  run's `budget`.
