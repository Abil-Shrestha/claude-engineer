//! Pure scheduling decisions.
//!
//! Everything here is a function of the run's state (plus limits), with no
//! I/O, so the policy — what runs next, what happens after a failure, when a
//! run is finished — is unit-tested without agents, git or a database.

use bodega_core::{AttemptOutcome, EventKind, RunState, RunStatus, TaskId, TaskStatus};

/// Tasks to start now: ready tasks (dependencies done, not running or
/// finished), in plan order, up to the free agent slots.
pub fn tasks_to_start(state: &RunState, running: usize, max_parallel: usize) -> Vec<TaskId> {
    let free = max_parallel.saturating_sub(running);
    state.ready_tasks().into_iter().take(free).collect()
}

/// Events to append after a task's latest attempt failed (or its verified
/// work could not be integrated). Retries while attempts remain; otherwise
/// fails the task and skips everything downstream of it.
pub fn after_failure(
    state: &RunState,
    task_id: TaskId,
    reason: &str,
    retryable: bool,
    max_attempts: u32,
) -> Vec<EventKind> {
    let Some(task) = state.tasks.get(&task_id) else {
        return Vec::new();
    };
    let used = task.attempts.len() as u32;
    if retryable && used < max_attempts {
        return vec![EventKind::TaskStatusChanged {
            task_id,
            status: TaskStatus::Ready,
            reason: Some(format!("retrying after attempt {used}: {reason}")),
        }];
    }
    let mut events = vec![EventKind::TaskStatusChanged {
        task_id,
        status: TaskStatus::Failed,
        reason: Some(if retryable {
            format!("gave up after {used} attempts: {reason}")
        } else {
            reason.to_owned()
        }),
    }];
    for dependent in state.graph().transitive_dependents(&task_id) {
        let status = state.tasks[&dependent].status;
        if !status.is_terminal() {
            events.push(EventKind::TaskStatusChanged {
                task_id: dependent,
                status: TaskStatus::Skipped,
                reason: Some(format!("depends on {}, which failed", task.spec.key)),
            });
        }
    }
    events
}

/// The run's final status once every task is terminal, or `None` while work
/// remains.
pub fn run_outcome(state: &RunState) -> Option<(RunStatus, Option<String>)> {
    if state.tasks.values().any(|t| !t.status.is_terminal()) {
        return None;
    }
    let unfinished: Vec<String> = state
        .tasks
        .values()
        .filter(|t| t.status != TaskStatus::Done)
        .map(|t| format!("{} ({})", t.spec.key, status_word(t.status)))
        .collect();
    if unfinished.is_empty() {
        Some((RunStatus::Succeeded, None))
    } else {
        Some((
            RunStatus::Failed,
            Some(format!(
                "{} of {} tasks did not complete: {}",
                unfinished.len(),
                state.tasks.len(),
                unfinished.join(", ")
            )),
        ))
    }
}

/// What the next attempt at a task should know about the previous one,
/// derived from the log so it survives restarts.
pub fn feedback_for(state: &RunState, task_id: TaskId) -> Option<String> {
    let task = state.tasks.get(&task_id)?;
    let last = state.attempts.get(task.attempts.last()?)?;
    let mut parts = Vec::new();
    if let Some(error) = &task.last_integration_error {
        parts.push(format!(
            "Your previous attempt passed its checks, but its changes could not be integrated with \
             the work other agents finished in the meantime:\n{error}\n\
             You are now starting from the latest integrated code. Redo the task on top of it."
        ));
    } else if let Some(AttemptOutcome::Failed { kind, message }) = &last.outcome {
        parts.push(format!(
            "Your previous attempt (attempt {}) failed ({kind:?}): {message}",
            last.number
        ));
    }
    let failed_checks: Vec<String> = last
        .checks
        .iter()
        .rev()
        .filter(|c| !c.passed)
        .take(3)
        .map(|c| {
            format!(
                "`{}` ({}) exited with {}:\n```\n{}\n```",
                c.name,
                c.command,
                c.exit_code
                    .map_or_else(|| "no exit code (timed out)".to_owned(), |c| c.to_string()),
                c.output_tail
            )
        })
        .collect();
    if !failed_checks.is_empty() && task.last_integration_error.is_none() {
        parts.push(format!("Failing checks:\n\n{}", failed_checks.join("\n\n")));
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn status_word(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Ready => "ready",
        TaskStatus::Running => "running",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Skipped => "skipped",
        TaskStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bodega_core::{
        AgentRef, AttemptId, CheckResult, Event, FailureKind, RunId, RunSpec, TaskSpec, Usage,
        WorkSource, WorkspaceId,
    };

    struct Log {
        state: RunState,
        seq: u64,
    }

    impl Log {
        fn new(tasks: &[(&str, &[&str])]) -> (Self, Vec<TaskId>) {
            let run_id = RunId::new();
            let mut log = Self {
                state: RunState::new(&Event {
                    seq: 1,
                    run_id,
                    at_ms: 0,
                    kind: EventKind::RunCreated {
                        spec: RunSpec {
                            title: "t".into(),
                            request: "r".into(),
                            source: WorkSource::Manual,
                            base_ref: "main".into(),
                            pipeline: "default".into(),
                            budget: Default::default(),
                        },
                    },
                })
                .unwrap(),
                seq: 1,
            };
            let ids = tasks
                .iter()
                .map(|(key, deps)| {
                    let task_id = TaskId::new();
                    log.push(EventKind::TaskCreated {
                        task_id,
                        spec: TaskSpec {
                            key: key.to_string(),
                            title: key.to_string(),
                            description: String::new(),
                            role: "implementer".into(),
                            depends_on: deps.iter().map(|d| d.to_string()).collect(),
                            acceptance: Vec::new(),
                        },
                    });
                    task_id
                })
                .collect();
            (log, ids)
        }

        fn push(&mut self, kind: EventKind) {
            self.seq += 1;
            let event = Event {
                seq: self.seq,
                run_id: self.state.id,
                at_ms: 0,
                kind,
            };
            self.state.apply(&event).unwrap();
        }

        fn status(&mut self, task_id: TaskId, status: TaskStatus) {
            self.push(EventKind::TaskStatusChanged {
                task_id,
                status,
                reason: None,
            });
        }

        fn failed_attempt(&mut self, task_id: TaskId, check_output: &str) {
            let attempt_id = AttemptId::new();
            self.push(EventKind::AttemptStarted {
                attempt_id,
                task_id,
                number: self.state.tasks[&task_id].attempts.len() as u32 + 1,
                agent: AgentRef {
                    runtime: "mock".into(),
                    model: None,
                },
                workspace_id: WorkspaceId::new(),
                branch: "b".into(),
            });
            self.push(EventKind::CheckCompleted {
                attempt_id,
                result: CheckResult {
                    name: "test".into(),
                    command: "cargo test".into(),
                    passed: false,
                    exit_code: Some(101),
                    duration_ms: 5,
                    output_tail: check_output.into(),
                },
            });
            self.push(EventKind::AttemptFinished {
                attempt_id,
                outcome: AttemptOutcome::Failed {
                    kind: FailureKind::Verification,
                    message: "checks failed".into(),
                },
                usage: Usage::default(),
            });
        }
    }

    #[test]
    fn starts_ready_tasks_up_to_the_free_slots() {
        let (mut log, ids) = Log::new(&[("A", &[]), ("B", &[]), ("C", &[]), ("D", &["A"])]);
        assert_eq!(tasks_to_start(&log.state, 0, 2), &ids[..2]);
        assert_eq!(tasks_to_start(&log.state, 2, 2), Vec::<TaskId>::new());
        log.status(ids[0], TaskStatus::Running);
        assert_eq!(tasks_to_start(&log.state, 1, 4), &ids[1..3]);
        log.status(ids[0], TaskStatus::Done);
        assert_eq!(
            tasks_to_start(&log.state, 0, 4),
            vec![ids[1], ids[2], ids[3]]
        );
    }

    #[test]
    fn retries_then_fails_and_skips_dependents() {
        let (mut log, ids) = Log::new(&[("A", &[]), ("B", &["A"]), ("C", &["B"]), ("X", &[])]);
        log.failed_attempt(ids[0], "boom");
        let retry = after_failure(&log.state, ids[0], "checks failed", true, 2);
        assert!(matches!(
            retry.as_slice(),
            [EventKind::TaskStatusChanged {
                status: TaskStatus::Ready,
                ..
            }]
        ));

        log.failed_attempt(ids[0], "boom again");
        let give_up = after_failure(&log.state, ids[0], "checks failed", true, 2);
        let statuses: Vec<_> = give_up
            .iter()
            .map(|e| match e {
                EventKind::TaskStatusChanged {
                    task_id, status, ..
                } => (*task_id, *status),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            statuses,
            vec![
                (ids[0], TaskStatus::Failed),
                (ids[1], TaskStatus::Skipped),
                (ids[2], TaskStatus::Skipped)
            ]
        );
    }

    #[test]
    fn non_retryable_failures_fail_immediately() {
        let (mut log, ids) = Log::new(&[("A", &[])]);
        log.failed_attempt(ids[0], "x");
        let events = after_failure(&log.state, ids[0], "budget", false, 5);
        assert!(matches!(
            events.as_slice(),
            [EventKind::TaskStatusChanged {
                status: TaskStatus::Failed,
                ..
            }]
        ));
    }

    #[test]
    fn run_outcome_waits_for_all_tasks() {
        let (mut log, ids) = Log::new(&[("A", &[]), ("B", &[])]);
        assert_eq!(run_outcome(&log.state), None);
        log.status(ids[0], TaskStatus::Done);
        assert_eq!(run_outcome(&log.state), None);
        log.status(ids[1], TaskStatus::Done);
        assert_eq!(run_outcome(&log.state), Some((RunStatus::Succeeded, None)));
        log.status(ids[1], TaskStatus::Failed);
        let (status, reason) = run_outcome(&log.state).unwrap();
        assert_eq!(status, RunStatus::Failed);
        assert_eq!(
            reason.as_deref(),
            Some("1 of 2 tasks did not complete: B (failed)")
        );
    }

    #[test]
    fn feedback_comes_from_the_last_attempt() {
        let (mut log, ids) = Log::new(&[("A", &[])]);
        assert_eq!(feedback_for(&log.state, ids[0]), None);
        log.failed_attempt(ids[0], "assertion failed: left == right");
        let feedback = feedback_for(&log.state, ids[0]).unwrap();
        assert!(feedback.contains("attempt 1"), "{feedback}");
        assert!(feedback.contains("assertion failed"), "{feedback}");

        log.push(EventKind::IntegrationFailed {
            task_id: ids[0],
            attempt_id: *log.state.tasks[&ids[0]].attempts.last().unwrap(),
            reason: "merge conflict in src/lib.rs".into(),
        });
        let feedback = feedback_for(&log.state, ids[0]).unwrap();
        assert!(feedback.contains("could not be integrated"), "{feedback}");
        assert!(feedback.contains("src/lib.rs"), "{feedback}");
        assert!(!feedback.contains("assertion failed"), "{feedback}");
    }
}
