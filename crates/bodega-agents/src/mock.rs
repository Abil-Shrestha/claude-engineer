//! A scripted agent for tests and demos.
//!
//! `MockAgent` behaves like a real runtime (a session, turns, events,
//! permission prompts, follow-up input) but its "work" is decided by a closure
//! that returns which files to write. This lets the engine, the store and the
//! UI be exercised end to end without any model or CLI installed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bodega_core::{AgentEvent, Usage};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::{
    AgentCapabilities, AgentControl, AgentError, AgentRuntime, AgentSession, EVENT_BUFFER,
    PermissionDecision, ProbeReport, Result, StartRequest,
};

/// What the mock agent is asked to do on one turn.
#[derive(Debug, Clone)]
pub struct MockCall {
    /// 1-based turn number within the session.
    pub turn: u32,
    /// The prompt (first turn) or follow-up input (later turns).
    pub input: String,
    pub workdir: PathBuf,
    pub session_id: String,
}

/// What the mock agent does on one turn.
#[derive(Debug, Clone, Default)]
pub struct MockTurn {
    /// Files to write, relative to the workspace.
    pub writes: Vec<(String, String)>,
    /// Final assistant message for the turn.
    pub message: String,
    /// Whether the turn ends successfully.
    pub success: bool,
    /// Ask permission for this tool (with this input) before writing.
    pub ask_permission: Option<(String, serde_json::Value)>,
    /// Simulated work time.
    pub delay: Duration,
    /// Usage added by this turn.
    pub usage: Usage,
}

impl MockTurn {
    pub fn writes(files: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>) -> Self {
        Self {
            writes: files
                .into_iter()
                .map(|(p, c)| (p.into(), c.into()))
                .collect(),
            message: "Done.".into(),
            success: true,
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 200,
                cost_usd: 0.01,
                ..Usage::default()
            },
            ..Self::default()
        }
    }
}

type Behavior = dyn Fn(&MockCall) -> MockTurn + Send + Sync;

/// A runtime whose sessions follow a script.
#[derive(Clone)]
pub struct MockAgent {
    behavior: Arc<Behavior>,
}

impl MockAgent {
    pub fn new(behavior: impl Fn(&MockCall) -> MockTurn + Send + Sync + 'static) -> Self {
        Self {
            behavior: Arc::new(behavior),
        }
    }

    /// A demo agent: records each prompt it receives in
    /// `.bodega-demo/<session>-turn<n>.md` and succeeds.
    pub fn demo() -> Self {
        Self::new(|call| {
            let mut turn = MockTurn::writes([(
                format!(".bodega-demo/{}-turn{}.md", call.session_id, call.turn),
                format!("# Mock agent notes\n\n{}\n", call.input),
            )]);
            turn.delay = Duration::from_millis(300);
            turn.message = format!("Recorded the request (turn {}).", call.turn);
            turn
        })
    }
}

impl std::fmt::Debug for MockAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockAgent").finish_non_exhaustive()
    }
}

#[async_trait]
impl AgentRuntime for MockAgent {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            multi_turn: true,
            resume: false,
            interrupt: false,
            permission_prompts: true,
            reports_cost: true,
        }
    }

    async fn probe(&self) -> ProbeReport {
        ProbeReport {
            runtime: "mock".into(),
            available: true,
            version: Some(env!("CARGO_PKG_VERSION").into()),
            detail: None,
        }
    }

    async fn start(&self, request: StartRequest) -> Result<AgentSession> {
        let (events_tx, events) = mpsc::channel(EVENT_BUFFER);
        let (input_tx, input_rx) = mpsc::channel(16);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

        tokio::spawn(run_session(
            Arc::clone(&self.behavior),
            request,
            session_id,
            events_tx,
            input_rx,
            Arc::clone(&pending),
        ));

        Ok(AgentSession {
            events,
            control: Box::new(MockControl {
                input: Mutex::new(Some(input_tx)),
                pending,
            }),
        })
    }
}

type PendingPermissions = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>>;

async fn run_session(
    behavior: Arc<Behavior>,
    request: StartRequest,
    session_id: String,
    events: mpsc::Sender<AgentEvent>,
    mut input: mpsc::Receiver<String>,
    pending: PendingPermissions,
) {
    let emit = |event: AgentEvent| {
        let events = events.clone();
        async move { events.send(event).await.is_ok() }
    };
    if !emit(AgentEvent::SessionStarted {
        session_id: Some(session_id.clone()),
        model: Some(request.model.clone().unwrap_or_else(|| "mock".into())),
    })
    .await
    {
        return;
    }

    let mut total = Usage::default();
    let mut next_input = Some(request.prompt.clone());
    let mut turn_number = 0;
    while let Some(text) = next_input.take() {
        turn_number += 1;
        let call = MockCall {
            turn: turn_number,
            input: text,
            workdir: request.workdir.clone(),
            session_id: session_id.clone(),
        };
        let turn = behavior(&call);
        if !run_turn(&call, turn, &mut total, &events, &pending).await {
            return;
        }
        next_input = input.recv().await;
    }
}

/// Plays one scripted turn. Returns false if the engine stopped listening.
async fn run_turn(
    call: &MockCall,
    turn: MockTurn,
    total: &mut Usage,
    events: &mpsc::Sender<AgentEvent>,
    pending: &PendingPermissions,
) -> bool {
    macro_rules! emit {
        ($event:expr) => {
            if events.send($event).await.is_err() {
                return false;
            }
        };
    }

    if !turn.delay.is_zero() {
        tokio::time::sleep(turn.delay).await;
    }

    if let Some((tool, input)) = turn.ask_permission.clone() {
        let request_id = format!("perm-{}-{}", call.session_id, call.turn);
        let (tx, rx) = oneshot::channel();
        pending
            .lock()
            .expect("pending lock")
            .insert(request_id.clone(), tx);
        emit!(AgentEvent::PermissionRequested {
            request_id,
            tool,
            input,
        });
        match rx.await {
            Ok(PermissionDecision::Allow { .. }) => {}
            Ok(PermissionDecision::Deny { message, .. }) => {
                emit!(AgentEvent::Message {
                    text: format!("Permission denied: {message}"),
                });
                emit!(AgentEvent::Finished {
                    success: false,
                    summary: Some("permission denied".into()),
                    usage: *total,
                });
                return true;
            }
            Err(_) => return false,
        }
    }

    for (index, (path, content)) in turn.writes.iter().enumerate() {
        let call_id = format!("call-{}-{}", call.turn, index);
        emit!(AgentEvent::ToolCall {
            call_id: call_id.clone(),
            tool: "write_file".into(),
            input: json!({ "path": path, "bytes": content.len() }),
        });
        let result = write_file(&call.workdir, path, content).await;
        emit!(AgentEvent::ToolResult {
            call_id,
            output: match &result {
                Ok(()) => format!("wrote {path}"),
                Err(e) => format!("failed to write {path}: {e}"),
            },
            is_error: result.is_err(),
        });
    }

    *total += turn.usage;
    emit!(AgentEvent::Message {
        text: turn.message.clone(),
    });
    emit!(AgentEvent::Usage { usage: *total });
    emit!(AgentEvent::Finished {
        success: turn.success,
        summary: Some(turn.message),
        usage: *total,
    });
    true
}

async fn write_file(root: &Path, relative: &str, content: &str) -> std::io::Result<()> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await
}

struct MockControl {
    input: Mutex<Option<mpsc::Sender<String>>>,
    pending: PendingPermissions,
}

#[async_trait]
impl AgentControl for MockControl {
    async fn send(&self, text: String) -> Result<()> {
        let sender = self
            .input
            .lock()
            .expect("input lock")
            .clone()
            .ok_or(AgentError::SessionEnded)?;
        sender
            .send(text)
            .await
            .map_err(|_| AgentError::SessionEnded)
    }

    async fn respond_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        let sender = self
            .pending
            .lock()
            .expect("pending lock")
            .remove(request_id)
            .ok_or(AgentError::SessionEnded)?;
        sender.send(decision).map_err(|_| AgentError::SessionEnded)
    }

    async fn interrupt(&self) -> Result<()> {
        Err(AgentError::Unsupported(
            "the mock agent cannot be interrupted",
        ))
    }

    async fn shutdown(&self) -> Result<()> {
        // Dropping the input sender ends the session loop after its turn.
        self.input.lock().expect("input lock").take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn drain_turn(session: &mut AgentSession) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Some(event) = session.events.recv().await {
            let done = matches!(event, AgentEvent::Finished { .. });
            out.push(event);
            if done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn writes_files_and_supports_follow_up_turns() {
        let dir = tempfile::tempdir().unwrap();
        let agent = MockAgent::new(|call| {
            MockTurn::writes([(format!("out/{}.txt", call.turn), call.input.clone())])
        });
        let mut session = agent
            .start(StartRequest {
                workdir: dir.path().to_owned(),
                prompt: "first".into(),
                session_id: Some("s1".into()),
                ..StartRequest::default()
            })
            .await
            .unwrap();

        let events = drain_turn(&mut session).await;
        assert!(matches!(
            &events[0],
            AgentEvent::SessionStarted { session_id: Some(id), .. } if id == "s1"
        ));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Finished { success: true, .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/1.txt")).unwrap(),
            "first"
        );

        session.control.send("second".into()).await.unwrap();
        let events = drain_turn(&mut session).await;
        let Some(AgentEvent::Finished { usage, .. }) = events.last() else {
            panic!("turn did not finish");
        };
        assert_eq!(usage.input_tokens, 2_000, "usage is cumulative");
        assert!(dir.path().join("out/2.txt").exists());

        session.control.shutdown().await.unwrap();
        assert!(session.events.recv().await.is_none());
    }

    #[tokio::test]
    async fn permission_requests_block_until_answered() {
        let dir = tempfile::tempdir().unwrap();
        let agent = MockAgent::new(|_| MockTurn {
            ask_permission: Some(("Bash".into(), json!({"command": "rm -rf /"}))),
            ..MockTurn::writes([("x.txt", "x")])
        });
        let mut session = agent
            .start(StartRequest {
                workdir: dir.path().to_owned(),
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .await
            .unwrap();
        let _started = session.events.recv().await.unwrap();
        let AgentEvent::PermissionRequested { request_id, .. } =
            session.events.recv().await.unwrap()
        else {
            panic!("expected a permission request");
        };
        session
            .control
            .respond_permission(
                &request_id,
                PermissionDecision::Deny {
                    message: "not in policy".into(),
                    interrupt: false,
                },
            )
            .await
            .unwrap();
        let events = drain_turn(&mut session).await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Finished { success: false, .. })
        ));
        assert!(!dir.path().join("x.txt").exists());
    }
}
