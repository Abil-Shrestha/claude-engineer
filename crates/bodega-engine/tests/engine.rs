//! End-to-end engine tests: real git repositories, the real event store and
//! the scripted mock agent.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bodega_agents::{MockAgent, MockTurn};
use bodega_config::{Config, Plan};
use bodega_core::{
    AttemptOutcome, Decision, Event, EventKind, FailureKind, RunId, RunState, RunStatus,
    TaskStatus, WorkSource,
};
use bodega_engine::{Engine, NewRun};
use bodega_store::EventStore;
use bodega_workspace::{GitRepo, Worktree};
use serde_json::json;

const CONFIG: &str = r#"
[limits]
max_parallel_agents = 2
max_attempts_per_task = 2
max_fix_rounds = 1
attempt_timeout_secs = 60

[agents.default]
runtime = "mock"

[[checks]]
name = "no-broken"
command = "sh check.sh"
timeout_secs = 30

[permissions]
allow = ["Read"]
on_unknown = "escalate"
"#;

/// The check fails if any file in the worktree contains the word BROKEN.
const CHECK_SCRIPT: &str = "#!/bin/sh\n\
if grep -rl BROKEN --exclude-dir=.git --exclude=check.sh . ; then\n\
  echo 'found BROKEN'; exit 1\n\
fi\n";

struct Harness {
    _dir: tempfile::TempDir,
    repo: GitRepo,
    engine: Engine,
    state_dir: std::path::PathBuf,
}

impl Harness {
    async fn new(config: &str, agent: MockAgent) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        let status = tokio::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .current_dir(&repo_dir)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        std::fs::write(repo_dir.join("README.md"), "# demo\n").unwrap();
        std::fs::write(repo_dir.join("check.sh"), CHECK_SCRIPT).unwrap();
        Worktree {
            path: repo_dir.clone(),
            branch: "main".into(),
        }
        .commit_all("initial")
        .await
        .unwrap();
        let repo = GitRepo::open(&repo_dir).await.unwrap();
        let store = EventStore::open(dir.path().join("bodega.sqlite")).unwrap();
        let state_dir = dir.path().join("state");
        let engine = Engine::new(
            store,
            repo.clone(),
            Config::parse(config).unwrap(),
            state_dir.clone(),
        )
        .with_runtime(Arc::new(agent));
        Self {
            _dir: dir,
            repo,
            engine,
            state_dir,
        }
    }

    async fn start(&self, plan: &str) -> RunId {
        self.start_with_budget(plan, None).await
    }

    async fn start_with_budget(&self, plan: &str, max_cost_usd: Option<f64>) -> RunId {
        self.engine
            .create_run(NewRun {
                title: "Test run".into(),
                request: "Build the thing".into(),
                source: WorkSource::Manual,
                base_ref: Some("main".into()),
                plan: Plan::parse(plan).unwrap(),
                max_cost_usd,
                max_tokens: None,
            })
            .await
            .unwrap()
    }

    async fn events(&self, run_id: RunId) -> Vec<Event> {
        self.engine
            .store()
            .run_events(run_id, 0, 100_000)
            .await
            .unwrap()
    }

    async fn integrated(&self, run_id: RunId, path: &str) -> Option<String> {
        self.repo
            .show_file(&Engine::integration_branch(run_id), path)
            .await
            .unwrap()
    }
}

/// The task key from the first line of a task prompt (`# Task: KEY — …`).
fn task_key(prompt: &str) -> Option<&str> {
    prompt
        .lines()
        .next()?
        .strip_prefix("# Task: ")?
        .split_whitespace()
        .next()
}

fn seq_of(events: &[Event], pred: impl Fn(&EventKind) -> bool) -> u64 {
    events
        .iter()
        .find(|e| pred(&e.kind))
        .map(|e| e.seq)
        .expect("event not found")
}

fn task_id_by_key(state: &RunState, key: &str) -> bodega_core::TaskId {
    state.task_by_key(key).unwrap().id
}

#[tokio::test]
async fn diamond_plan_runs_in_parallel_with_a_fix_round() {
    // B's first turn leaves a broken file; the check fails, the agent gets
    // the output as feedback and fixes it in a second turn.
    let agent = MockAgent::new(|call| {
        if call.turn > 1 {
            let mut turn = MockTurn::writes([("B.txt", "fixed")]);
            turn.message = "Fixed the broken file.".into();
            return turn;
        }
        let key = task_key(&call.input).unwrap_or("?").to_owned();
        let content = if key == "B" {
            "BROKEN".to_owned()
        } else {
            key.clone()
        };
        let mut turn = MockTurn::writes([(format!("{key}.txt"), content)]);
        turn.delay = Duration::from_millis(150);
        turn
    });
    let h = Harness::new(CONFIG, agent).await;
    let run_id = h
        .start(
            r#"
            summary = "diamond"
            [[tasks]]
            key = "A"
            title = "a"
            [[tasks]]
            key = "B"
            title = "b"
            [[tasks]]
            key = "C"
            title = "c"
            depends_on = ["A", "B"]
            [[tasks]]
            key = "D"
            title = "d"
            depends_on = ["C"]
            "#,
        )
        .await;

    let state = h.engine.drive(run_id).await.unwrap();
    assert_eq!(
        state.status,
        RunStatus::Succeeded,
        "{:?}",
        state.status_reason
    );
    for key in ["A", "B", "C", "D"] {
        let task = state.task_by_key(key).unwrap();
        assert_eq!(task.status, TaskStatus::Done, "{key}");
        assert_eq!(task.attempts.len(), 1, "{key} needed one attempt");
        assert!(task.integrated_commit.is_some());
    }
    assert_eq!(h.integrated(run_id, "A.txt").await.as_deref(), Some("A"));
    assert_eq!(
        h.integrated(run_id, "B.txt").await.as_deref(),
        Some("fixed")
    );
    assert_eq!(h.integrated(run_id, "D.txt").await.as_deref(), Some("D"));

    // B: failed check, fix round, passing check, then integration check.
    let b = &state.attempts[&state.tasks[&task_id_by_key(&state, "B")].attempts[0]];
    let results: Vec<(String, bool)> = b
        .checks
        .iter()
        .map(|c| (c.name.clone(), c.passed))
        .collect();
    assert_eq!(
        results,
        vec![
            ("no-broken".into(), false),
            ("no-broken".into(), true),
            ("integration:no-broken".into(), true)
        ]
    );
    assert!(b.usage.input_tokens >= 2_000, "two turns of usage");

    // A and B ran at the same time; C started only after both landed.
    let events = h.events(run_id).await;
    let (a, bb, c) = (
        task_id_by_key(&state, "A"),
        task_id_by_key(&state, "B"),
        task_id_by_key(&state, "C"),
    );
    let started = |t| {
        seq_of(
            &events,
            |k| matches!(k, EventKind::AttemptStarted { task_id, .. } if *task_id == t),
        )
    };
    let integrated = |t| {
        seq_of(
            &events,
            |k| matches!(k, EventKind::BranchIntegrated { task_id, .. } if *task_id == t),
        )
    };
    let a_finished = seq_of(
        &events,
        |k| matches!(k, EventKind::AttemptFinished { attempt_id, .. } if *attempt_id == state.tasks[&a].attempts[0]),
    );
    assert!(started(bb) < a_finished, "B started before A finished");
    assert!(started(c) > integrated(a) && started(c) > integrated(bb));
    assert!(state.total_usage().cost_usd > 0.0);
}

#[tokio::test]
async fn a_task_that_keeps_failing_skips_its_dependents() {
    let agent = MockAgent::new(|call| {
        let key = task_key(&call.input).unwrap_or("fix").to_owned();
        if key == "A" || call.turn > 1 {
            return MockTurn::writes([("A.txt", "still BROKEN")]);
        }
        MockTurn::writes([(format!("{key}.txt"), key.clone())])
    });
    let h = Harness::new(CONFIG, agent).await;
    let run_id = h
        .start(
            r#"
            [[tasks]]
            key = "A"
            title = "a"
            [[tasks]]
            key = "B"
            title = "b"
            depends_on = ["A"]
            [[tasks]]
            key = "X"
            title = "independent"
            "#,
        )
        .await;
    let state = h.engine.drive(run_id).await.unwrap();
    assert_eq!(state.status, RunStatus::Failed);
    assert_eq!(state.task_by_key("A").unwrap().status, TaskStatus::Failed);
    assert_eq!(state.task_by_key("A").unwrap().attempts.len(), 2);
    assert_eq!(state.task_by_key("B").unwrap().status, TaskStatus::Skipped);
    assert_eq!(state.task_by_key("X").unwrap().status, TaskStatus::Done);
    assert_eq!(
        state.status_reason.as_deref(),
        Some("2 of 3 tasks did not complete: A (failed), B (skipped)")
    );
    // The second attempt was told why the first one failed.
    let second = &state.attempts[&state.task_by_key("A").unwrap().attempts[1]];
    assert!(matches!(
        second.outcome,
        Some(AttemptOutcome::Failed {
            kind: FailureKind::Verification,
            ..
        })
    ));
    assert_eq!(h.integrated(run_id, "X.txt").await.as_deref(), Some("X"));
    assert_eq!(h.integrated(run_id, "A.txt").await, None);
}

#[tokio::test]
async fn merge_conflicts_are_retried_on_top_of_the_new_head() {
    let agent = MockAgent::new(|call| {
        let key = task_key(&call.input).unwrap_or("?").to_owned();
        let mut turn = MockTurn::writes([("shared.txt", format!("written by {key}"))]);
        turn.delay = Duration::from_millis(if key == "P" { 50 } else { 400 });
        turn
    });
    let h = Harness::new(CONFIG, agent).await;
    let run_id = h
        .start(
            r#"
            [[tasks]]
            key = "P"
            title = "p"
            [[tasks]]
            key = "Q"
            title = "q"
            "#,
        )
        .await;
    let state = h.engine.drive(run_id).await.unwrap();
    assert_eq!(
        state.status,
        RunStatus::Succeeded,
        "{:?}",
        state.status_reason
    );
    let q = state.task_by_key("Q").unwrap();
    assert_eq!(q.attempts.len(), 2, "Q conflicted once and was retried");
    let events = h.events(run_id).await;
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::IntegrationFailed { reason, .. } if reason.contains("merge conflict in shared.txt")
    )));
    assert_eq!(
        h.integrated(run_id, "shared.txt").await.as_deref(),
        Some("written by Q")
    );
}

#[tokio::test]
async fn unknown_tool_requests_wait_for_a_human() {
    let agent = MockAgent::new(|_| MockTurn {
        ask_permission: Some((
            "Bash".into(),
            json!({ "command": "curl https://example.com" }),
        )),
        ..MockTurn::writes([("fetched.txt", "data")])
    });
    let h = Harness::new(CONFIG, agent).await;
    let run_id = h
        .start("[[tasks]]\nkey = \"T1\"\ntitle = \"fetch\"\n")
        .await;
    let engine = h.engine.clone();
    let driver = tokio::spawn(async move { engine.drive(run_id).await });

    // Wait for the escalation, then approve it like `bodega approve` would.
    let approval_id = loop {
        let state = h.engine.store().run(run_id).await.unwrap().unwrap();
        if let Some(pending) = state.pending_approvals().next() {
            assert!(pending.title.contains("Bash"), "{}", pending.title);
            assert!(pending.details.contains("curl"));
            break pending.id;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    h.engine
        .resolve_approval(run_id, approval_id, Decision::Approved, "tester", None)
        .await
        .unwrap();

    let state = tokio::time::timeout(Duration::from_secs(30), driver)
        .await
        .expect("run finishes after approval")
        .unwrap()
        .unwrap();
    assert_eq!(state.status, RunStatus::Succeeded);
    assert_eq!(
        h.integrated(run_id, "fetched.txt").await.as_deref(),
        Some("data")
    );
}

#[tokio::test]
async fn denied_requests_fail_the_attempt() {
    let config = CONFIG.replace("max_attempts_per_task = 2", "max_attempts_per_task = 1") + "\n";
    let config = config.replace("on_unknown = \"escalate\"", "on_unknown = \"deny\"");
    let agent = MockAgent::new(|_| MockTurn {
        ask_permission: Some(("Bash".into(), json!({ "command": "rm -rf build" }))),
        ..MockTurn::writes([("x.txt", "x")])
    });
    let h = Harness::new(&config, agent).await;
    let run_id = h.start("[[tasks]]\nkey = \"T1\"\ntitle = \"t\"\n").await;
    let state = h.engine.drive(run_id).await.unwrap();
    assert_eq!(state.status, RunStatus::Failed);
    let attempt = state.attempts.values().next().unwrap();
    assert!(matches!(
        &attempt.outcome,
        Some(AttemptOutcome::Failed { kind: FailureKind::Agent, message }) if message.contains("permission denied")
    ));
}

#[tokio::test]
async fn attempts_interrupted_by_a_crash_are_retried() {
    let agent = MockAgent::new(|_| MockTurn::writes([("done.txt", "ok")]));
    let h = Harness::new(CONFIG, agent).await;
    let run_id = h.start("[[tasks]]\nkey = \"T1\"\ntitle = \"t\"\n").await;
    // Simulate a process that died mid-attempt.
    let state = h.engine.store().run(run_id).await.unwrap().unwrap();
    let task_id = task_id_by_key(&state, "T1");
    h.engine
        .store()
        .append(
            run_id,
            vec![
                EventKind::RunStatusChanged {
                    status: RunStatus::Running,
                    stage: Some("implement".into()),
                    reason: None,
                },
                EventKind::TaskStatusChanged {
                    task_id,
                    status: TaskStatus::Running,
                    reason: None,
                },
                EventKind::AttemptStarted {
                    attempt_id: bodega_core::AttemptId::new(),
                    task_id,
                    number: 1,
                    agent: bodega_core::AgentRef {
                        runtime: "mock".into(),
                        model: None,
                    },
                    workspace_id: bodega_core::WorkspaceId::new(),
                    branch: "bodega/dead/T1-1".into(),
                },
            ],
            None,
        )
        .await
        .unwrap();

    let state = h.engine.drive(run_id).await.unwrap();
    assert_eq!(state.status, RunStatus::Succeeded);
    let task = state.task_by_key("T1").unwrap();
    assert_eq!(task.attempts.len(), 2);
    assert!(matches!(
        &state.attempts[&task.attempts[0]].outcome,
        Some(AttemptOutcome::Failed { kind: FailureKind::Infrastructure, message }) if message.starts_with("interrupted")
    ));
    // Driving a finished run is a no-op.
    let again = h.engine.drive(run_id).await.unwrap();
    assert_eq!(again.last_seq, state.last_seq);
}

#[tokio::test]
async fn rejects_plans_for_unknown_runtimes() {
    let h = Harness::new(
        &CONFIG.replace("runtime = \"mock\"", "runtime = \"nope\""),
        MockAgent::demo(),
    )
    .await;
    let err = h
        .engine
        .create_run(NewRun {
            title: "x".into(),
            request: "x".into(),
            source: WorkSource::Manual,
            base_ref: None,
            plan: Plan::single("x", "x"),
            max_cost_usd: None,
            max_tokens: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no agent runtime named `nope`"),
        "{err}"
    );
    let _: &Path = h.repo.root();
}

#[tokio::test]
async fn parallel_attempts_share_the_budget_without_overcommitting_it() {
    /// (task key, spend cap) for every session the mock agent started.
    type Caps = Arc<std::sync::Mutex<Vec<(String, Option<f64>)>>>;
    let caps: Caps = Arc::default();
    let seen = Arc::clone(&caps);
    let agent = MockAgent::new(move |call| {
        let key = task_key(&call.input).unwrap_or("?").to_owned();
        seen.lock()
            .unwrap()
            .push((key.clone(), call.max_budget_usd));
        let mut turn = MockTurn::writes([(format!("{key}.txt"), key)]);
        turn.delay = Duration::from_millis(100);
        turn
    });
    let h = Harness::new(CONFIG, agent).await;
    let run_id = h
        .start_with_budget(
            r#"
            [[tasks]]
            key = "A"
            title = "a"
            [[tasks]]
            key = "B"
            title = "b"
            [[tasks]]
            key = "C"
            title = "c"
            depends_on = ["A", "B"]
            "#,
            Some(1.0),
        )
        .await;
    let state = h.engine.drive(run_id).await.unwrap();
    assert_eq!(
        state.status,
        RunStatus::Succeeded,
        "{:?}",
        state.status_reason
    );

    let caps = caps.lock().unwrap().clone();
    let cap = |key: &str| {
        caps.iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, cap)| *cap)
            .unwrap_or_else(|| panic!("{key} was started without a spend cap: {caps:?}"))
    };
    // A and B ran together and split the whole budget; C got what was left
    // after their actual spend ($0.01 each).
    assert!((cap("A") - 0.5).abs() < 1e-9, "{caps:?}");
    assert!((cap("B") - 0.5).abs() < 1e-9, "{caps:?}");
    assert!((cap("C") - 0.98).abs() < 1e-9, "{caps:?}");
}

#[tokio::test]
async fn a_run_keeps_the_config_it_was_created_with() {
    // Created under a config whose check always fails...
    let strict = CONFIG
        .replace("max_attempts_per_task = 2", "max_attempts_per_task = 1")
        .replace("command = \"sh check.sh\"", "command = \"false\"");
    let agent = MockAgent::new(|_| MockTurn::writes([("x.txt", "x")]));
    let h = Harness::new(&strict, agent.clone()).await;
    let run_id = h.start("[[tasks]]\nkey = \"T1\"\ntitle = \"t\"\n").await;

    // ...and resumed by an engine configured with no checks at all (as if
    // another branch were checked out). The run's own rules still apply.
    let lax = Config::parse("[agents.default]\nruntime = \"mock\"\n").unwrap();
    assert!(lax.checks.is_empty());
    let resumed = Engine::new(
        h.engine.store().clone(),
        h.repo.clone(),
        lax,
        h.state_dir.clone(),
    )
    .with_runtime(Arc::new(agent));
    let state = resumed.drive(run_id).await.unwrap();
    assert_eq!(state.status, RunStatus::Failed);
    let attempt = state.attempts.values().next().unwrap();
    assert_eq!(attempt.checks[0].command, "false");
    assert!(!attempt.checks[0].passed);
}
