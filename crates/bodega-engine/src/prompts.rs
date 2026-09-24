//! Prompts given to agents.
//!
//! Prompts carry context and intent. They are not where rules are enforced:
//! checks, permissions and integration are enforced by the engine whatever
//! the agent does.

use bodega_config::Check;
use bodega_core::{CheckResult, RunSpec, TaskSpec};

/// Instructions appended to the runtime's own system prompt for every
/// implementer session.
pub const IMPLEMENTER_SYSTEM: &str = "You are one agent in a Bodega software factory. \
Several agents work on different tasks of the same change at the same time, each in its own git \
worktree. Stay inside your task's scope, make the change completely, and leave the working tree in \
a state where the project builds and its tests pass. Bodega commits your work, verifies it and \
integrates it; do not push, create branches, or open pull requests.";

/// The first message of an attempt.
pub fn task_prompt(
    run: &RunSpec,
    task: &TaskSpec,
    checks: &[Check],
    feedback: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Task: {} — {}\n\n", task.key, task.title));
    if !task.description.trim().is_empty() {
        out.push_str(task.description.trim());
        out.push_str("\n\n");
    }
    if !task.acceptance.is_empty() {
        out.push_str("## Acceptance criteria\n\n");
        for criterion in &task.acceptance {
            out.push_str(&format!("- {criterion}\n"));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "## The overall request this task belongs to\n\n**{}**\n\n{}\n\n",
        run.title,
        run.request.trim()
    ));
    if !checks.is_empty() {
        out.push_str(
            "## How your work is verified\n\nWhen you finish, Bodega runs these commands in your \
             worktree. The task is only done when all of them pass, so run them yourself first:\n\n",
        );
        for check in checks {
            out.push_str(&format!("- `{}`\n", check.command));
        }
        out.push('\n');
    }
    if let Some(feedback) = feedback {
        out.push_str("## What happened on the previous attempt\n\n");
        out.push_str(feedback.trim());
        out.push_str("\n\n");
    }
    out.push_str(
        "## Rules\n\n\
         - Work only in the current directory; it is a dedicated git worktree for this task.\n\
         - Other tasks are handled by other agents in parallel. Do not do their work.\n\
         - You do not need to commit. Do not push, create branches or open pull requests.\n\
         - When you are done, reply with a short summary of what you changed.\n",
    );
    out
}

/// The follow-up sent when checks fail after the agent's turn.
pub fn check_failure_feedback(results: &[CheckResult], round: u32, max_rounds: u32) -> String {
    let mut out = format!(
        "Verification failed (fix round {round} of {max_rounds}). Fix the problems below, then \
         finish again with a short summary.\n\n"
    );
    for result in results.iter().filter(|r| !r.passed) {
        out.push_str(&format!(
            "### `{}` exited with {}\n\n```\n{}\n```\n\n",
            result.command,
            result
                .exit_code
                .map_or_else(|| "no exit code (timed out)".to_owned(), |c| c.to_string()),
            result.output_tail
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bodega_core::WorkSource;

    #[test]
    fn task_prompt_includes_scope_checks_and_feedback() {
        let run = RunSpec {
            title: "Dark mode".into(),
            request: "Users want a dark theme.".into(),
            source: WorkSource::Manual,
            base_ref: "main".into(),
            pipeline: "default".into(),
            budget: Default::default(),
            config: None,
        };
        let task = TaskSpec {
            key: "T2".into(),
            title: "Theme toggle".into(),
            description: "Add a toggle to the header.".into(),
            role: "implementer".into(),
            depends_on: vec!["T1".into()],
            acceptance: vec!["The choice persists across reloads".into()],
        };
        let checks = [Check {
            name: "test".into(),
            command: "npm test".into(),
            timeout_secs: 60,
        }];
        let prompt = task_prompt(&run, &task, &checks, Some("tests failed"));
        assert!(prompt.starts_with("# Task: T2 — Theme toggle"));
        assert!(prompt.contains("- The choice persists across reloads"));
        assert!(prompt.contains("Users want a dark theme."));
        assert!(prompt.contains("- `npm test`"));
        assert!(prompt.contains("## What happened on the previous attempt\n\ntests failed"));
    }
}
