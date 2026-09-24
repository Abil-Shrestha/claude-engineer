//! Claude Code, driven through its stream-json protocol.
//!
//! One `claude` process per session, full duplex over stdio:
//!
//! * stdin: `{"type":"user",…}` messages (the prompt and follow-ups) and
//!   `control_request`s (`initialize`, `interrupt`), plus our
//!   `control_response`s to the CLI's `can_use_tool` permission requests.
//! * stdout: `system/init`, `assistant`, `user` (tool results), `result` (end
//!   of each turn, with cumulative cost and usage) and `control_request`s.
//!
//! Forgeline chooses the session id (`--session-id`), passes untrusted values
//! as `--flag=value`, and never auto-approves permission prompts: they become
//! [`AgentEvent::PermissionRequested`] for the engine to decide.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use forgeline_core::{AgentEvent, LogLevel, Usage};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, mpsc, watch};

use crate::process;
use crate::{
    AgentCapabilities, AgentControl, AgentError, AgentRuntime, AgentSession, EVENT_BUFFER,
    PermissionDecision, PermissionMode, ProbeReport, Result, StartRequest,
};

/// Longest tool output kept in an event; the rest is elided.
const MAX_TOOL_OUTPUT: usize = 8 * 1024;
/// Stderr lines forwarded per session before we stop forwarding.
const MAX_STDERR_LINES: usize = 50;

/// The Claude Code runtime.
#[derive(Debug, Clone)]
pub struct ClaudeCode {
    program: PathBuf,
}

impl Default for ClaudeCode {
    fn default() -> Self {
        Self {
            program: PathBuf::from("claude"),
        }
    }
}

impl ClaudeCode {
    pub fn new() -> Self {
        Self::default()
    }

    /// Uses a specific `claude` binary (or a stand-in for tests).
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

/// Builds the `claude` command line for a session.
pub fn build_args(request: &StartRequest) -> Vec<String> {
    let mut args: Vec<String> = [
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--permission-prompt-tool",
        "stdio",
    ]
    .map(String::from)
    .to_vec();
    let mode = match request.permission_mode {
        PermissionMode::Ask => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
    };
    args.push(format!("--permission-mode={mode}"));
    if let Some(model) = &request.model {
        args.push(format!("--model={model}"));
    }
    if let Some(extra) = &request.append_system_prompt {
        args.push(format!("--append-system-prompt={extra}"));
    }
    match (&request.resume, &request.session_id) {
        (Some(resume), _) => args.push(format!("--resume={resume}")),
        (None, Some(id)) => args.push(format!("--session-id={id}")),
        (None, None) => {}
    }
    if !request.allowed_tools.is_empty() {
        args.push(format!(
            "--allowedTools={}",
            request.allowed_tools.join(",")
        ));
    }
    if !request.disallowed_tools.is_empty() {
        args.push(format!(
            "--disallowedTools={}",
            request.disallowed_tools.join(",")
        ));
    }
    if let Some(turns) = request.max_turns {
        args.push(format!("--max-turns={turns}"));
    }
    if let Some(budget) = request.max_budget_usd {
        args.push(format!("--max-budget-usd={budget}"));
    }
    args
}

/// One decoded stdout line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Incoming {
    Event(AgentEvent),
    /// The CLI asks whether it may run a tool.
    CanUseTool {
        request_id: String,
        tool: String,
        input: Value,
    },
    /// A CLI request we do not implement; answer with an error so it does
    /// not wait forever.
    UnsupportedRequest {
        request_id: String,
        subtype: String,
    },
    /// The CLI withdrew an earlier request.
    Cancelled {
        request_id: String,
    },
}

/// Decodes one line of Claude Code's stream-json output. Unknown message
/// types are skipped so newer CLIs do not break older Forgeline versions.
pub(crate) fn parse_line(line: &str) -> Vec<Incoming> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return vec![Incoming::Event(AgentEvent::Log {
            level: LogLevel::Warn,
            message: format!("unparseable output: {}", truncate(line, 500)),
        })];
    };
    let str_field = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);

    match msg.get("type").and_then(Value::as_str).unwrap_or("") {
        "system" if msg.get("subtype").and_then(Value::as_str) == Some("init") => {
            vec![Incoming::Event(AgentEvent::SessionStarted {
                session_id: str_field(&msg, "session_id"),
                model: str_field(&msg, "model"),
            })]
        }
        "assistant" => content_blocks(&msg)
            .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
                "text" => Some(AgentEvent::Message {
                    text: str_field(block, "text")?,
                }),
                "thinking" => {
                    let text = str_field(block, "thinking")?;
                    (!text.is_empty()).then_some(AgentEvent::Thinking { text })
                }
                "tool_use" => Some(AgentEvent::ToolCall {
                    call_id: str_field(block, "id")?,
                    tool: str_field(block, "name")?,
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                }),
                _ => None,
            })
            .map(Incoming::Event)
            .collect(),
        "user" => content_blocks(&msg)
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|block| {
                Some(Incoming::Event(AgentEvent::ToolResult {
                    call_id: str_field(block, "tool_use_id")?,
                    output: truncate(&tool_result_text(block.get("content")), MAX_TOOL_OUTPUT),
                    is_error: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }))
            })
            .collect(),
        "result" => {
            let usage = result_usage(&msg);
            let is_error = msg
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let subtype = str_field(&msg, "subtype").unwrap_or_default();
            let summary = str_field(&msg, "result")
                .filter(|s| !s.is_empty())
                .or_else(|| (!subtype.is_empty()).then(|| subtype.clone()));
            vec![
                Incoming::Event(AgentEvent::Usage { usage }),
                Incoming::Event(AgentEvent::Finished {
                    success: !is_error && subtype == "success",
                    summary,
                    usage,
                }),
            ]
        }
        "control_request" => {
            let Some(request_id) = str_field(&msg, "request_id") else {
                return Vec::new();
            };
            let request = msg.get("request").cloned().unwrap_or(Value::Null);
            match request.get("subtype").and_then(Value::as_str) {
                Some("can_use_tool") => vec![Incoming::CanUseTool {
                    request_id,
                    tool: str_field(&request, "tool_name").unwrap_or_default(),
                    input: request.get("input").cloned().unwrap_or(Value::Null),
                }],
                other => vec![Incoming::UnsupportedRequest {
                    request_id,
                    subtype: other.unwrap_or("unknown").to_owned(),
                }],
            }
        }
        "control_cancel_request" => str_field(&msg, "request_id")
            .map(|request_id| vec![Incoming::Cancelled { request_id }])
            .unwrap_or_default(),
        "rate_limit_event" => {
            let status = msg
                .pointer("/rate_limit_info/status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if status == "allowed" {
                Vec::new()
            } else {
                vec![Incoming::Event(AgentEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("rate limit status: {status}"),
                })]
            }
        }
        _ => Vec::new(),
    }
}

fn content_blocks(msg: &Value) -> impl Iterator<Item = &Value> {
    msg.pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Cumulative usage from a `result` message. `modelUsage` and
/// `total_cost_usd` are running totals for the session, so the latest result
/// is the whole story (they must not be summed across turns).
fn result_usage(msg: &Value) -> Usage {
    let mut usage = Usage::default();
    let tokens = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
    if let Some(models) = msg.get("modelUsage").and_then(Value::as_object) {
        for model in models.values() {
            usage.input_tokens += tokens(model, "inputTokens");
            usage.output_tokens += tokens(model, "outputTokens");
            usage.cache_read_tokens += tokens(model, "cacheReadInputTokens");
            usage.cache_write_tokens += tokens(model, "cacheCreationInputTokens");
            usage.cost_usd += model.get("costUSD").and_then(Value::as_f64).unwrap_or(0.0);
        }
    } else if let Some(u) = msg.get("usage") {
        usage.input_tokens = tokens(u, "input_tokens");
        usage.output_tokens = tokens(u, "output_tokens");
        usage.cache_read_tokens = tokens(u, "cache_read_input_tokens");
        usage.cache_write_tokens = tokens(u, "cache_creation_input_tokens");
    }
    if let Some(total) = msg.get("total_cost_usd").and_then(Value::as_f64) {
        usage.cost_usd = total;
    }
    usage
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} more bytes]", &s[..end], s.len() - end)
}

fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
        "session_id": "default",
    })
}

type SharedStdin = Arc<Mutex<Option<ChildStdin>>>;

async fn write_json(stdin: &SharedStdin, value: &Value) -> Result<()> {
    let mut guard = stdin.lock().await;
    let pipe = guard.as_mut().ok_or(AgentError::SessionEnded)?;
    let mut line = serde_json::to_vec(value).expect("json values serialize");
    line.push(b'\n');
    pipe.write_all(&line).await?;
    pipe.flush().await?;
    Ok(())
}

#[async_trait]
impl AgentRuntime for ClaudeCode {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            multi_turn: true,
            resume: true,
            interrupt: true,
            permission_prompts: true,
            reports_cost: true,
        }
    }

    async fn probe(&self) -> ProbeReport {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(&self.program)
                .arg("--version")
                .stdin(Stdio::null())
                .output(),
        )
        .await;
        match output {
            Ok(Ok(out)) if out.status.success() => ProbeReport {
                runtime: self.name().into(),
                available: true,
                version: Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()),
                detail: None,
            },
            Ok(Ok(out)) => ProbeReport {
                runtime: self.name().into(),
                available: false,
                version: None,
                detail: Some(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            },
            Ok(Err(e)) => ProbeReport {
                runtime: self.name().into(),
                available: false,
                version: None,
                detail: Some(format!("could not run {}: {e}", self.program.display())),
            },
            Err(_) => ProbeReport {
                runtime: self.name().into(),
                available: false,
                version: None,
                detail: Some("`claude --version` timed out".into()),
            },
        }
    }

    async fn start(&self, request: StartRequest) -> Result<AgentSession> {
        let program: OsString = self.program.clone().into_os_string();
        let mut cmd = process::command(&program);
        cmd.args(build_args(&request))
            .current_dir(&request.workdir)
            // A CLI spawned from inside a Claude Code session must not think
            // it is nested.
            .env_remove("CLAUDECODE")
            .envs(&request.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|source| AgentError::Spawn {
            program: self.program.display().to_string(),
            source,
        })?;
        let pid = child.id();
        let stdin: SharedStdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let (events_tx, events) = mpsc::channel(EVENT_BUFFER);
        let (exited_tx, exited) = watch::channel(false);
        let pending: Arc<Mutex<HashMap<String, Value>>> = Arc::default();
        let awaiting_result = Arc::new(AtomicBool::new(true));

        write_json(
            &stdin,
            &json!({
                "type": "control_request",
                "request_id": "forgeline-init",
                "request": { "subtype": "initialize", "hooks": null },
            }),
        )
        .await?;
        write_json(&stdin, &user_message(&request.prompt)).await?;

        // Forward a bounded amount of stderr as diagnostics.
        let stderr_events = events_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut forwarded = 0;
            while let Ok(Some(line)) = lines.next_line().await {
                if forwarded < MAX_STDERR_LINES && !line.trim().is_empty() {
                    forwarded += 1;
                    let _ = stderr_events
                        .send(AgentEvent::Log {
                            level: LogLevel::Warn,
                            message: truncate(&line, 2_000),
                        })
                        .await;
                }
            }
        });

        // Read stdout until the process exits.
        let reader_stdin = Arc::clone(&stdin);
        let reader_pending = Arc::clone(&pending);
        let reader_awaiting = Arc::clone(&awaiting_result);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut last_usage = Usage::default();
            while let Ok(Some(line)) = lines.next_line().await {
                for incoming in parse_line(&line) {
                    let event = match incoming {
                        Incoming::Event(event) => {
                            if let AgentEvent::Finished { usage, .. } = &event {
                                last_usage = *usage;
                                reader_awaiting.store(false, Ordering::SeqCst);
                            }
                            event
                        }
                        Incoming::CanUseTool {
                            request_id,
                            tool,
                            input,
                        } => {
                            reader_pending
                                .lock()
                                .await
                                .insert(request_id.clone(), input.clone());
                            AgentEvent::PermissionRequested {
                                request_id,
                                tool,
                                input,
                            }
                        }
                        Incoming::UnsupportedRequest {
                            request_id,
                            subtype,
                        } => {
                            let _ = write_json(
                                &reader_stdin,
                                &json!({
                                    "type": "control_response",
                                    "response": {
                                        "subtype": "error",
                                        "request_id": request_id,
                                        "error": format!("forgeline does not support `{subtype}` requests"),
                                    },
                                }),
                            )
                            .await;
                            continue;
                        }
                        Incoming::Cancelled { request_id } => {
                            reader_pending.lock().await.remove(&request_id);
                            continue;
                        }
                    };
                    if events_tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
            let status = child.wait().await;
            if reader_awaiting.load(Ordering::SeqCst) {
                let detail = match status {
                    Ok(s) => format!("claude exited ({s}) before finishing its turn"),
                    Err(e) => format!("claude exited abnormally: {e}"),
                };
                let _ = events_tx
                    .send(AgentEvent::Finished {
                        success: false,
                        summary: Some(detail),
                        usage: last_usage,
                    })
                    .await;
            }
            let _ = exited_tx.send(true);
        });

        Ok(AgentSession {
            events,
            control: Box::new(ClaudeControl {
                stdin,
                pending,
                awaiting_result,
                pid,
                exited,
                next_request: std::sync::atomic::AtomicU64::new(1),
            }),
        })
    }
}

struct ClaudeControl {
    stdin: SharedStdin,
    /// Open `can_use_tool` requests and the tool input each one proposed.
    pending: Arc<Mutex<HashMap<String, Value>>>,
    awaiting_result: Arc<AtomicBool>,
    pid: Option<u32>,
    exited: watch::Receiver<bool>,
    next_request: std::sync::atomic::AtomicU64,
}

impl Drop for ClaudeControl {
    /// A session dropped without `shutdown` (an aborted attempt, a crashed
    /// engine task) must not leave the agent running unsupervised.
    fn drop(&mut self) {
        if *self.exited.borrow() {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
        }
    }
}

#[async_trait]
impl AgentControl for ClaudeControl {
    async fn send(&self, text: String) -> Result<()> {
        self.awaiting_result.store(true, Ordering::SeqCst);
        write_json(&self.stdin, &user_message(&text)).await
    }

    async fn respond_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        let original = self
            .pending
            .lock()
            .await
            .remove(request_id)
            .ok_or(AgentError::SessionEnded)?;
        let response = match decision {
            PermissionDecision::Allow { updated_input } => json!({
                "behavior": "allow",
                "updatedInput": updated_input.unwrap_or(original),
            }),
            PermissionDecision::Deny { message, interrupt } => json!({
                "behavior": "deny",
                "message": message,
                "interrupt": interrupt,
            }),
        };
        write_json(
            &self.stdin,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": response,
                },
            }),
        )
        .await
    }

    async fn interrupt(&self) -> Result<()> {
        let n = self.next_request.fetch_add(1, Ordering::SeqCst);
        write_json(
            &self.stdin,
            &json!({
                "type": "control_request",
                "request_id": format!("forgeline-interrupt-{n}"),
                "request": { "subtype": "interrupt" },
            }),
        )
        .await
    }

    async fn shutdown(&self) -> Result<()> {
        // Closing stdin tells the CLI no more input is coming.
        self.stdin.lock().await.take();
        process::stop_group(self.pid, self.exited.clone(), Duration::from_secs(5)).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_safe_command_line() {
        let args = build_args(&StartRequest {
            prompt: "ignored here".into(),
            model: Some("claude-opus-5".into()),
            session_id: Some("-rf".into()),
            permission_mode: PermissionMode::AcceptEdits,
            allowed_tools: vec!["Read".into(), "Bash(git:*)".into()],
            max_turns: Some(30),
            ..StartRequest::default()
        });
        assert_eq!(
            &args[..7],
            &[
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--permission-prompt-tool",
                "stdio"
            ]
        );
        assert!(args.contains(&"--permission-mode=acceptEdits".to_string()));
        assert!(args.contains(&"--model=claude-opus-5".to_string()));
        // Values that look like flags stay attached to their flag.
        assert!(args.contains(&"--session-id=-rf".to_string()));
        assert!(args.contains(&"--allowedTools=Read,Bash(git:*)".to_string()));
        assert!(args.contains(&"--max-turns=30".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--resume")));
    }

    #[test]
    fn resume_takes_precedence_over_a_new_session_id() {
        let args = build_args(&StartRequest {
            session_id: Some("new".into()),
            resume: Some("old".into()),
            ..StartRequest::default()
        });
        assert!(args.contains(&"--resume=old".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--session-id")));
    }

    #[test]
    fn parses_a_turn() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s-1","model":"claude-opus-5","tools":[]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"plan"},{"type":"text","text":"Running tests"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}]},"session_id":"s-1"}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"ok"}],"is_error":false}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"All green","total_cost_usd":0.42,"modelUsage":{"claude-opus-5":{"inputTokens":100,"outputTokens":50,"cacheReadInputTokens":1000,"cacheCreationInputTokens":10,"costUSD":0.40}},"session_id":"s-1"}"#,
        ];
        let events: Vec<Incoming> = lines.iter().flat_map(|l| parse_line(l)).collect();
        assert_eq!(
            events[0],
            Incoming::Event(AgentEvent::SessionStarted {
                session_id: Some("s-1".into()),
                model: Some("claude-opus-5".into()),
            })
        );
        assert!(
            matches!(&events[1], Incoming::Event(AgentEvent::Thinking { text }) if text == "plan")
        );
        assert!(
            matches!(&events[2], Incoming::Event(AgentEvent::Message { text }) if text == "Running tests")
        );
        assert!(matches!(
            &events[3],
            Incoming::Event(AgentEvent::ToolCall { call_id, tool, .. }) if call_id == "toolu_1" && tool == "Bash"
        ));
        assert!(matches!(
            &events[4],
            Incoming::Event(AgentEvent::ToolResult { output, is_error: false, .. }) if output == "ok"
        ));
        let Incoming::Event(AgentEvent::Finished {
            success,
            summary,
            usage,
        }) = &events[6]
        else {
            panic!("expected finished, got {:?}", events[6]);
        };
        assert!(success);
        assert_eq!(summary.as_deref(), Some("All green"));
        assert_eq!(usage.cache_read_tokens, 1000);
        assert!((usage.cost_usd - 0.42).abs() < 1e-9, "total_cost_usd wins");
    }

    #[test]
    fn errors_and_control_requests() {
        let finished = parse_line(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"","total_cost_usd":1.0}"#,
        );
        assert!(matches!(
            &finished[1],
            Incoming::Event(AgentEvent::Finished { success: false, summary: Some(s), .. }) if s == "error_max_turns"
        ));

        let ask = parse_line(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf target"}}}"#,
        );
        assert_eq!(
            ask,
            vec![Incoming::CanUseTool {
                request_id: "r1".into(),
                tool: "Bash".into(),
                input: json!({"command": "rm -rf target"}),
            }]
        );
        let hook = parse_line(
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"hook_callback"}}"#,
        );
        assert!(
            matches!(&hook[0], Incoming::UnsupportedRequest { subtype, .. } if subtype == "hook_callback")
        );

        assert!(parse_line(r#"{"type":"keep_alive"}"#).is_empty());
        assert!(parse_line(r#"{"type":"some_future_message"}"#).is_empty());
        assert!(matches!(
            &parse_line("not json")[0],
            Incoming::Event(AgentEvent::Log { .. })
        ));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let s = "é".repeat(10);
        let t = truncate(&s, 5);
        assert!(t.starts_with("éé"));
        assert!(t.contains("more bytes"));
    }

    async fn next_event(session: &mut AgentSession) -> Option<AgentEvent> {
        tokio::time::timeout(Duration::from_secs(10), session.events.recv())
            .await
            .expect("event in time")
    }

    /// Drives the real session plumbing against a shell script that speaks
    /// the stream-json protocol.
    #[cfg(unix)]
    #[tokio::test]
    async fn drives_a_fake_claude_process() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude");
        std::fs::write(
            &script,
            r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"fake-1","model":"fake"}'
turn=0
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      turn=$((turn+1))
      echo '{"type":"control_request","request_id":"perm-'$turn'","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"make"}}}'
      ;;
    *'"behavior":"allow"'*)
      echo '{"type":"assistant","message":{"content":[{"type":"text","text":"allowed"}]}}'
      echo '{"type":"result","subtype":"success","is_error":false,"result":"turn '$turn' done","total_cost_usd":0.'$turn'}'
      ;;
    *'"behavior":"deny"'*)
      echo '{"type":"result","subtype":"success","is_error":true,"result":"denied","total_cost_usd":0.'$turn'}'
      ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runtime = ClaudeCode::with_program(&script);
        let mut session = runtime
            .start(StartRequest {
                workdir: dir.path().to_owned(),
                prompt: "build it".into(),
                ..StartRequest::default()
            })
            .await
            .unwrap();

        assert!(matches!(
            next_event(&mut session).await,
            Some(AgentEvent::SessionStarted { .. })
        ));
        let Some(AgentEvent::PermissionRequested { request_id, .. }) =
            next_event(&mut session).await
        else {
            panic!("expected a permission request");
        };
        session
            .control
            .respond_permission(
                &request_id,
                PermissionDecision::Allow {
                    updated_input: None,
                },
            )
            .await
            .unwrap();
        let mut finished = None;
        while let Some(event) = session.events.recv().await {
            if let AgentEvent::Finished {
                success, summary, ..
            } = event
            {
                finished = Some((success, summary));
                break;
            }
        }
        assert_eq!(finished, Some((true, Some("turn 1 done".into()))));

        // A second turn in the same session, denied this time.
        session.control.send("again".into()).await.unwrap();
        let Some(AgentEvent::PermissionRequested { request_id, .. }) = session.events.recv().await
        else {
            panic!("expected a second permission request");
        };
        session
            .control
            .respond_permission(
                &request_id,
                PermissionDecision::Deny {
                    message: "no".into(),
                    interrupt: false,
                },
            )
            .await
            .unwrap();
        let Some(AgentEvent::Usage { .. }) = session.events.recv().await else {
            panic!("expected usage");
        };
        assert!(matches!(
            session.events.recv().await,
            Some(AgentEvent::Finished { success: false, .. })
        ));

        session.control.shutdown().await.unwrap();
        // The event stream ends once the process is gone.
        while session.events.recv().await.is_some() {}
    }
}
