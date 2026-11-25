use crate::types::{
    PendingRequest, Run, RunStatus, SwarmBoardSnapshot, ThreadStatus, TurnItem, TurnItemKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Streaming event emitted by a `ProviderDriver` during a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    SessionStarted {
        provider_session_id: String,
    },
    AssistantDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    Item {
        kind: TurnItemKind,
        summary: Option<String>,
        payload: Value,
    },
    PendingApproval {
        request: PendingRequest,
    },
    RateLimited {
        retry_after_secs: u64,
        message: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    ContextUsage {
        context_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    },
    Completed {
        final_text: Option<String>,
    },
    Failed {
        message: String,
    },
}

/// Messages sent from the WebSocket server to connected web/mobile clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsServerMessage {
    Hello {
        server_version: String,
        telemetry_enabled: bool,
        auth_method: String,
    },
    ThreadStatusChanged {
        thread_id: Uuid,
        status: ThreadStatus,
    },
    RunStarted {
        thread_id: Uuid,
        run: Run,
    },
    RunStatusChanged {
        thread_id: Uuid,
        run_id: Uuid,
        status: RunStatus,
        error: Option<String>,
    },
    Delta {
        thread_id: Uuid,
        run_id: Uuid,
        channel: String, // "assistant" | "reasoning"
        text: String,
    },
    TurnItemAdded {
        thread_id: Uuid,
        item: TurnItem,
    },
    RunUsageUpdated {
        thread_id: Uuid,
        run_id: Uuid,
        context_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    },
    PendingRequest {
        request: PendingRequest,
    },
    SubagentUpdated {
        parent_thread_id: Uuid,
        child_thread_id: Uuid,
        status: ThreadStatus,
    },
    SwarmUpdated {
        thread_id: Uuid,
        board: SwarmBoardSnapshot,
    },
    Resync {
        thread_id: Option<Uuid>,
    },
    Error {
        message: String,
    },
}

/// Messages sent from WebSocket clients to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMessage {
    Subscribe { thread_id: Uuid },
    Unsubscribe { thread_id: Uuid },
    Ping,
}
