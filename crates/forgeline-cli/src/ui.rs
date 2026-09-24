//! Terminal output: colors (when the terminal supports them) and one-line
//! renderings of events for live progress.

use std::io::IsTerminal;
use std::sync::OnceLock;

use forgeline_core::{
    AgentEvent, AttemptOutcome, Event, EventKind, RunState, RunStatus, TaskStatus, Usage,
};

fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

fn paint(code: &str, text: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

pub fn bold(text: &str) -> String {
    paint("1", text)
}
pub fn dim(text: &str) -> String {
    paint("2", text)
}
pub fn green(text: &str) -> String {
    paint("32", text)
}
pub fn red(text: &str) -> String {
    paint("31", text)
}
pub fn yellow(text: &str) -> String {
    paint("33", text)
}
pub fn cyan(text: &str) -> String {
    paint("36", text)
}

pub fn run_status(status: RunStatus) -> String {
    match status {
        RunStatus::Pending => dim("pending"),
        RunStatus::Running => cyan("running"),
        RunStatus::WaitingForApproval => yellow("waiting for approval"),
        RunStatus::Succeeded => green("succeeded"),
        RunStatus::Failed => red("failed"),
        RunStatus::Cancelled => dim("cancelled"),
    }
}

pub fn task_status(status: TaskStatus) -> String {
    match status {
        TaskStatus::Pending => dim("pending"),
        TaskStatus::Ready => dim("ready"),
        TaskStatus::Running => cyan("running"),
        TaskStatus::Done => green("done"),
        TaskStatus::Failed => red("failed"),
        TaskStatus::Skipped => yellow("skipped"),
        TaskStatus::Cancelled => dim("cancelled"),
    }
}

pub fn usage(usage: &Usage) -> String {
    let tokens = usage.total_tokens();
    let tokens = if tokens >= 1_000_000 {
        format!("{:.1}M tokens", tokens as f64 / 1e6)
    } else if tokens >= 1_000 {
        format!("{:.1}k tokens", tokens as f64 / 1e3)
    } else {
        format!("{tokens} tokens")
    };
    format!("${:.2} · {tokens}", usage.cost_usd)
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

/// One line describing an event for live progress, or `None` for events
/// that are only shown in verbose mode (agent chatter) or not at all.
pub fn describe(event: &Event, state: &RunState, verbose: bool) -> Option<String> {
    let key = |task_id| {
        state
            .tasks
            .get(task_id)
            .map_or_else(|| "?".to_owned(), |t| t.spec.key.clone())
    };
    let attempt_key = |attempt_id| {
        state
            .attempts
            .get(attempt_id)
            .map_or_else(|| "?".to_owned(), |a| key(&a.task_id))
    };
    let line = match &event.kind {
        EventKind::RunCreated { spec } => format!("{} {}", bold("run"), spec.title),
        EventKind::RunStatusChanged { status, reason, .. } => match reason {
            Some(reason) => format!("{} {} — {reason}", bold("run"), run_status(*status)),
            None => format!("{} {}", bold("run"), run_status(*status)),
        },
        EventKind::PlanProposed { tasks, .. } => format!("plan: {} task(s)", tasks.len()),
        EventKind::TaskCreated { .. } => return None,
        EventKind::TaskStatusChanged {
            task_id,
            status,
            reason,
        } => {
            let task = key(task_id);
            match (status, reason) {
                (TaskStatus::Running, _) => return None,
                (TaskStatus::Ready, Some(reason)) => {
                    format!("{} {task} {}", yellow("↻"), dim(&one_line(reason, 160)))
                }
                (TaskStatus::Ready | TaskStatus::Pending, None) => return None,
                (TaskStatus::Done, _) => format!("{} {task} done", green("✓")),
                (status, reason) => format!(
                    "{} {task} {}{}",
                    red("✗"),
                    task_status(*status),
                    reason
                        .as_deref()
                        .map(|r| format!(" — {}", one_line(r, 160)))
                        .unwrap_or_default()
                ),
            }
        }
        EventKind::AttemptStarted {
            task_id,
            number,
            agent,
            ..
        } => format!(
            "{} {} attempt {number} {}",
            cyan("▶"),
            key(task_id),
            dim(&format!(
                "({}{})",
                agent.runtime,
                agent
                    .model
                    .as_deref()
                    .map(|m| format!(", {m}"))
                    .unwrap_or_default()
            ))
        ),
        EventKind::Agent { attempt_id, event } => {
            if !verbose {
                return None;
            }
            let task = attempt_key(attempt_id);
            match event {
                AgentEvent::Message { text } => format!("  {task} {}", dim(&one_line(text, 140))),
                AgentEvent::ToolCall { tool, input, .. } => {
                    let detail = ["command", "file_path", "path", "pattern"]
                        .iter()
                        .find_map(|k| input.get(k).and_then(|v| v.as_str()))
                        .unwrap_or("");
                    format!("  {task} · {tool} {}", dim(&one_line(detail, 100)))
                }
                AgentEvent::Log { message, .. } => {
                    format!("  {task} {}", dim(&one_line(message, 140)))
                }
                _ => return None,
            }
        }
        EventKind::CheckCompleted { attempt_id, result } => format!(
            "  {} check {} {} {}",
            attempt_key(attempt_id),
            result.name,
            if result.passed {
                green("passed")
            } else {
                red("failed")
            },
            dim(&format!("({:.1}s)", result.duration_ms as f64 / 1000.0))
        ),
        EventKind::ReviewCompleted {
            attempt_id,
            verdict,
            ..
        } => format!("  {} review: {verdict:?}", attempt_key(attempt_id)),
        EventKind::AttemptFinished {
            attempt_id,
            outcome,
            usage: spent,
        } => {
            let task = attempt_key(attempt_id);
            match outcome {
                AttemptOutcome::Succeeded { .. } => {
                    format!("  {task} verified {}", dim(&format!("({})", usage(spent))))
                }
                AttemptOutcome::Failed { kind, message } => format!(
                    "  {task} attempt failed ({kind:?}): {}",
                    one_line(message, 160)
                ),
                AttemptOutcome::Cancelled => format!("  {task} attempt cancelled"),
            }
        }
        EventKind::ApprovalRequested {
            approval_id, title, ..
        } => format!(
            "{} {title}\n    approve: {}\n    deny:    {}",
            yellow("⏸ approval needed:"),
            bold(&format!("forgeline approve {approval_id}")),
            dim(&format!("forgeline approve {approval_id} --deny")),
        ),
        EventKind::ApprovalResolved { decision, by, .. } => {
            format!("  approval {decision:?} by {by}")
        }
        EventKind::BranchIntegrated { task_id, .. } => {
            format!("  {} merged into the integration branch", key(task_id))
        }
        EventKind::IntegrationFailed {
            task_id, reason, ..
        } => format!(
            "  {} could not be integrated: {}",
            key(task_id),
            one_line(reason, 160)
        ),
        EventKind::PullRequestOpened { url, .. } => format!("pull request: {url}"),
        EventKind::BudgetExceeded { exceeded } => format!("{} {exceeded}", red("budget:")),
    };
    Some(line)
}
