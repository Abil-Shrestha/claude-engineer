//! API tests against a real server, engine, git repository and mock agent.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use forgeline_agents::{MockAgent, MockTurn};
use forgeline_config::Config;
use forgeline_core::{Event, EventKind, RunId, RunState, RunStatus, TaskStatus};
use forgeline_engine::Engine;
use forgeline_server::{Created, PendingApproval, ServerOptions, router};
use forgeline_store::{EventStore, RunSummary};
use forgeline_workspace::{GitRepo, Worktree};
use futures_util::StreamExt;
use serde_json::json;

struct TestServer {
    _dir: tempfile::TempDir,
    base: String,
    client: reqwest::Client,
}

async fn start(agent: MockAgent, token: Option<&str>) -> TestServer {
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
    std::fs::write(repo_dir.join("README.md"), "# api test\n").unwrap();
    Worktree {
        path: repo_dir.clone(),
        branch: "main".into(),
    }
    .commit_all("initial")
    .await
    .unwrap();
    let repo = GitRepo::open(&repo_dir).await.unwrap();
    let config = Config::parse(
        "[agents.default]\nruntime = \"mock\"\n[permissions]\non_unknown = \"escalate\"\n",
    )
    .unwrap();
    let store = EventStore::open(dir.path().join("db.sqlite")).unwrap();
    let engine =
        Engine::new(store, repo, config, dir.path().join("state")).with_runtime(Arc::new(agent));
    let options = ServerOptions {
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: token.map(str::to_owned),
        allowed_origins: Vec::new(),
        ui_dir: None,
        resume_unfinished: false,
    };
    let app = router(engine, &options);
    let listener = tokio::net::TcpListener::bind(options.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    TestServer {
        _dir: dir,
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
    }
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn create(&self, body: serde_json::Value) -> RunId {
        let res = self
            .client
            .post(self.url("/api/runs"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 201, "{}", res.text().await.unwrap());
        res.json::<Created>().await.unwrap().run_id
    }

    async fn run(&self, run_id: RunId) -> RunState {
        self.client
            .get(self.url(&format!("/api/runs/{run_id}")))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Reads SSE events until `stop` returns true.
    async fn read_stream(
        &self,
        path: &str,
        last_event_id: Option<u64>,
        stop: impl Fn(&Event) -> bool,
    ) -> Vec<(u64, Event)> {
        let mut request = self.client.get(self.url(path));
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id.to_string());
        }
        let res = request.send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers()["content-type"].to_str().unwrap(),
            "text/event-stream"
        );
        let mut body = res.bytes_stream();
        let mut buffer = String::new();
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let chunk = tokio::time::timeout_at(deadline, body.next())
                .await
                .expect("stream produced the expected events in time")
                .expect("stream ended early")
                .unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..end + 2).collect();
                let mut id = None;
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("id:") {
                        id = v.trim().parse().ok();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    }
                }
                let (Some(id), false) = (id, data.is_empty()) else {
                    continue; // keep-alive comment
                };
                let event: Event = serde_json::from_str(&data).unwrap();
                let done = stop(&event);
                out.push((id, event));
                if done {
                    return out;
                }
            }
        }
    }
}

fn finished(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::RunStatusChanged { status, .. } if status.is_terminal()
    )
}

#[tokio::test]
async fn create_follow_and_inspect_a_run() {
    let server = start(
        MockAgent::new(|call| MockTurn::writes([(format!("out-{}.txt", call.session_id), "x")])),
        None,
    )
    .await;
    let health: serde_json::Value = server
        .client
        .get(server.url("/api/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["name"], "forgeline");

    let run_id = server
        .create(json!({
            "title": "Two tasks",
            "request": "Do two things",
            "plan": { "summary": "two", "tasks": [
                { "key": "A", "title": "first" },
                { "key": "B", "title": "second", "depends_on": ["A"] }
            ]}
        }))
        .await;

    let events = server
        .read_stream(&format!("/api/stream?run={run_id}"), None, finished)
        .await;
    let seqs: Vec<u64> = events.iter().map(|(id, _)| *id).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "ids increase: {seqs:?}"
    );
    assert!(
        events
            .iter()
            .all(|(id, e)| *id == e.seq && e.run_id == run_id)
    );
    assert!(matches!(
        events.last().unwrap().1.kind,
        EventKind::RunStatusChanged {
            status: RunStatus::Succeeded,
            ..
        }
    ));

    let state = server.run(run_id).await;
    assert_eq!(state.status, RunStatus::Succeeded);
    assert!(state.tasks.values().all(|t| t.status == TaskStatus::Done));

    let runs: Vec<RunSummary> = server
        .client
        .get(server.url("/api/runs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].tasks_done, 2);

    // Resuming the stream from a known id replays exactly what came after.
    let resume_from = seqs[seqs.len() / 2];
    let resumed = server
        .read_stream(
            &format!("/api/stream?run={run_id}"),
            Some(resume_from),
            finished,
        )
        .await;
    assert_eq!(resumed.first().unwrap().0, resume_from + 1);
    assert_eq!(
        resumed.len(),
        events.iter().filter(|(id, _)| *id > resume_from).count()
    );

    // Paged history agrees with the stream.
    let history: Vec<Event> = server
        .client
        .get(server.url(&format!("/api/runs/{run_id}/events?after=0&limit=5000")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history.len(), events.len());

    // Errors are JSON with useful statuses.
    let missing = server
        .client
        .get(server.url(&format!("/api/runs/{}", RunId::new())))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let bad = server
        .client
        .post(server.url("/api/runs"))
        .json(&json!({ "title": "cyclic", "plan": { "tasks": [
            { "key": "A", "title": "a", "depends_on": ["B"] },
            { "key": "B", "title": "b", "depends_on": ["A"] }
        ]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let body: serde_json::Value = bad.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("cycle"));
}

#[tokio::test]
async fn approvals_are_listed_and_resolved_over_http() {
    let server = start(
        MockAgent::new(|_| MockTurn {
            ask_permission: Some(("Bash".into(), json!({ "command": "make deploy" }))),
            ..MockTurn::writes([("deployed.txt", "yes")])
        }),
        None,
    )
    .await;
    let run_id = server.create(json!({ "title": "Needs a human" })).await;

    let pending = loop {
        let list: Vec<PendingApproval> = server
            .client
            .get(server.url("/api/approvals"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(first) = list.into_iter().next() {
            break first;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(pending.run_id, run_id);
    assert!(pending.approval.details.contains("make deploy"));

    let res = server
        .client
        .post(server.url(&format!("/api/approvals/{}", pending.approval.id)))
        .json(&json!({ "decision": "approved", "by": "abil" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);

    let events = server
        .read_stream(&format!("/api/stream?run={run_id}"), None, finished)
        .await;
    assert!(events.iter().any(|(_, e)| matches!(
        &e.kind,
        EventKind::ApprovalResolved { by, .. } if by == "abil"
    )));
    assert_eq!(server.run(run_id).await.status, RunStatus::Succeeded);

    // A second decision on the same approval is a conflict.
    let again = server
        .client
        .post(server.url(&format!("/api/approvals/{}", pending.approval.id)))
        .json(&json!({ "decision": "rejected" }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);
}

#[tokio::test]
async fn without_a_token_only_local_hosts_are_served() {
    let server = start(MockAgent::demo(), None).await;
    let rebound = server
        .client
        .get(server.url("/api/runs"))
        .header("host", "evil.example:7777")
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), 403);
    for host in [
        "localhost:7777",
        "127.0.0.1:7777",
        "[::1]:7777",
        "LOCALHOST",
    ] {
        let local = server
            .client
            .get(server.url("/api/runs"))
            .header("host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(local.status(), 200, "{host}");
    }
}

#[tokio::test]
async fn a_token_protects_every_endpoint() {
    let server = start(MockAgent::demo(), Some("s3cret")).await;
    let denied = server
        .client
        .get(server.url("/api/runs"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let wrong = server
        .client
        .get(server.url("/api/runs"))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let allowed = server
        .client
        .get(server.url("/api/runs"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    // EventSource cannot send headers, so the stream accepts ?token=.
    let stream = server
        .client
        .get(server.url("/api/stream?token=s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
}
