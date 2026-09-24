//! Agent runtimes.
//!
//! Bodega drives existing coding agents (Claude Code, Codex, ACP agents…)
//! through their machine protocols and normalizes everything they do into
//! [`AgentEvent`]s. The engine never sees a runtime's native format.
//!
//! A session is one agent process working in one workspace. The engine sends
//! the task prompt, reads events until the turn ends with
//! [`AgentEvent::Finished`], and may then send follow-up input (for example,
//! failing check output) to start another turn in the same session.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
pub use bodega_core::AgentEvent;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub mod claude;
pub mod mock;
mod process;

pub use claude::ClaudeCode;
pub use mock::{MockAgent, MockTurn};

/// Capacity of a session's event channel. The engine drains it continuously;
/// a full channel applies backpressure to the agent's output reader.
pub const EVENT_BUFFER: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("could not start `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("agent I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the agent session has ended")]
    SessionEnded,
    #[error("{0}")]
    Unsupported(&'static str),
}

pub type Result<T, E = AgentError> = std::result::Result<T, E>;

/// How much the agent may do without asking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Ask (through [`AgentEvent::PermissionRequested`]) for anything not in
    /// the allow list.
    #[default]
    Ask,
    /// File edits inside the workspace are allowed; other tools ask.
    AcceptEdits,
    /// Read-only planning: the agent may not modify anything.
    Plan,
}

/// Everything needed to start a session.
#[derive(Debug, Clone, Default)]
pub struct StartRequest {
    /// The workspace the agent works in (its current directory).
    pub workdir: PathBuf,
    /// The first user message.
    pub prompt: String,
    /// Extra instructions appended to the runtime's own system prompt.
    pub append_system_prompt: Option<String>,
    pub model: Option<String>,
    /// Session id chosen by Bodega, so it never has to be discovered.
    pub session_id: Option<String>,
    /// Resume an earlier session instead of starting fresh.
    pub resume: Option<String>,
    pub permission_mode: PermissionMode,
    /// Tools the agent may use without asking (runtime-specific names).
    pub allowed_tools: Vec<String>,
    /// Tools the agent may never use.
    pub disallowed_tools: Vec<String>,
    /// Extra environment variables for the agent process.
    pub env: BTreeMap<String, String>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
}

/// What a runtime supports, so the engine and UI only offer what works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    /// Follow-up input continues the same session.
    pub multi_turn: bool,
    /// Sessions can be resumed after the process exits.
    pub resume: bool,
    pub interrupt: bool,
    /// Tool permission prompts are routed to Bodega.
    pub permission_prompts: bool,
    /// Usage reports include a dollar cost.
    pub reports_cost: bool,
}

/// Result of checking that a runtime is installed and usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub runtime: String,
    pub available: bool,
    pub version: Option<String>,
    pub detail: Option<String>,
}

/// An answer to [`AgentEvent::PermissionRequested`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow {
        /// Replacement tool input, to run a safer variant of the call.
        updated_input: Option<serde_json::Value>,
    },
    Deny {
        message: String,
        /// Also stop the agent's current turn.
        interrupt: bool,
    },
}

/// A runtime that can start agent sessions (one per adapter).
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Adapter name, e.g. `claude-code`.
    fn name(&self) -> &str;
    fn capabilities(&self) -> AgentCapabilities;
    /// Checks the agent is installed and reports its version.
    async fn probe(&self) -> ProbeReport;
    async fn start(&self, request: StartRequest) -> Result<AgentSession>;
}

/// Control half of a running session.
#[async_trait]
pub trait AgentControl: Send + Sync {
    /// Sends follow-up input, starting a new turn.
    async fn send(&self, text: String) -> Result<()>;
    /// Answers a pending permission request.
    async fn respond_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<()>;
    /// Asks the agent to stop its current turn.
    async fn interrupt(&self) -> Result<()>;
    /// Ends the session: closes input, waits briefly, then kills the process.
    async fn shutdown(&self) -> Result<()>;
}

/// A running agent session: a stream of normalized events plus controls.
pub struct AgentSession {
    pub events: mpsc::Receiver<AgentEvent>,
    pub control: Box<dyn AgentControl>,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession").finish_non_exhaustive()
    }
}
