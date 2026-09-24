//! Keeps the UI's TypeScript types in sync with the Rust types.
//!
//! `ui/src/api/types.ts` is generated from the serde types the API sends and
//! receives. This test fails when the file is stale; regenerate it with
//!
//! ```sh
//! UPDATE_TYPES=1 cargo test -p bodega-server --test typescript
//! ```

use std::path::PathBuf;

use bodega_config::{Plan, PlanTask};
use bodega_core::{
    AgentEvent, AgentRef, ApprovalId, ApprovalKind, ApprovalState, AttemptId, AttemptOutcome,
    AttemptState, Budget, BudgetExceeded, CheckResult, Decision, Event, EventKind, FailureKind,
    Finding, FindingKind, LogLevel, ReviewVerdict, RunId, RunSpec, RunState, RunStatus, TaskId,
    TaskSpec, TaskState, TaskStatus, Usage, WorkSource, WorkspaceId,
    state::{PullRequest, Resolution, Review},
};
use bodega_server::{CreateRun, Created, PendingApproval, Resolve};
use bodega_store::RunSummary;
use ts_rs::{Config, TS};

fn declarations() -> String {
    let cfg = Config::default().with_large_int("number");
    let decls = [
        // Ids
        RunId::decl(&cfg),
        TaskId::decl(&cfg),
        AttemptId::decl(&cfg),
        WorkspaceId::decl(&cfg),
        ApprovalId::decl(&cfg),
        // Events
        Event::decl(&cfg),
        EventKind::decl(&cfg),
        AgentEvent::decl(&cfg),
        LogLevel::decl(&cfg),
        // Domain model
        RunSpec::decl(&cfg),
        WorkSource::decl(&cfg),
        RunStatus::decl(&cfg),
        TaskSpec::decl(&cfg),
        TaskStatus::decl(&cfg),
        AgentRef::decl(&cfg),
        FailureKind::decl(&cfg),
        AttemptOutcome::decl(&cfg),
        CheckResult::decl(&cfg),
        ReviewVerdict::decl(&cfg),
        FindingKind::decl(&cfg),
        Finding::decl(&cfg),
        ApprovalKind::decl(&cfg),
        Decision::decl(&cfg),
        Usage::decl(&cfg),
        Budget::decl(&cfg),
        BudgetExceeded::decl(&cfg),
        // Snapshots
        RunState::decl(&cfg),
        TaskState::decl(&cfg),
        AttemptState::decl(&cfg),
        ApprovalState::decl(&cfg),
        Resolution::decl(&cfg),
        Review::decl(&cfg),
        PullRequest::decl(&cfg),
        RunSummary::decl(&cfg),
        // Requests and responses
        CreateRun::decl(&cfg),
        Created::decl(&cfg),
        PendingApproval::decl(&cfg),
        Resolve::decl(&cfg),
        Plan::decl(&cfg),
        PlanTask::decl(&cfg),
        serde_json::Value::decl(&cfg),
    ];
    let mut out = String::from(
        "// Generated from Bodega's Rust types. Do not edit by hand.\n\
         // Regenerate: UPDATE_TYPES=1 cargo test -p bodega-server --test typescript\n\n",
    );
    for decl in decls {
        out.push_str("export ");
        out.push_str(decl.trim());
        out.push_str("\n\n");
    }
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

#[test]
fn typescript_types_are_up_to_date() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/api/types.ts");
    let expected = declarations();
    if std::env::var_os("UPDATE_TYPES").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &expected).unwrap();
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "{} is out of date with the Rust types.\n\
         Regenerate it with: UPDATE_TYPES=1 cargo test -p bodega-server --test typescript",
        path.display()
    );
}
