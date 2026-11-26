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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reset_at: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reset_at: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
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
    ShellOutput {
        exec_id: Uuid,
        thread_id: Option<Uuid>,
        stream: String,
        chunk: String,
    },
    ShellFinished {
        exec_id: Uuid,
        thread_id: Option<Uuid>,
        exit_code: Option<i32>,
        duration_ms: u64,
        timed_out: bool,
        cancelled: bool,
        truncated: bool,
        item_id: Option<Uuid>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_ws_messages_serde_roundtrip() {
        let exec_id = Uuid::new_v4();
        let thread_id = Some(Uuid::new_v4());
        let out = WsServerMessage::ShellOutput {
            exec_id,
            thread_id,
            stream: "stdout".into(),
            chunk: "hello\n".into(),
        };
        let encoded = serde_json::to_value(&out).unwrap();
        assert_eq!(encoded["type"], "shell_output");
        assert_eq!(encoded["stream"], "stdout");
        assert_eq!(encoded["chunk"], "hello\n");
        let decoded: WsServerMessage = serde_json::from_value(encoded).unwrap();
        assert!(matches!(
            decoded,
            WsServerMessage::ShellOutput {
                exec_id: id,
                thread_id: tid,
                ..
            } if id == exec_id && tid == thread_id
        ));

        let item_id = Some(Uuid::new_v4());
        let fin = WsServerMessage::ShellFinished {
            exec_id,
            thread_id,
            exit_code: Some(0),
            duration_ms: 42,
            timed_out: false,
            cancelled: true,
            truncated: false,
            item_id,
        };
        let encoded = serde_json::to_value(&fin).unwrap();
        assert_eq!(encoded["type"], "shell_finished");
        assert_eq!(encoded["cancelled"], true);
        let decoded: WsServerMessage = serde_json::from_value(encoded).unwrap();
        assert!(matches!(
            decoded,
            WsServerMessage::ShellFinished {
                exec_id: id,
                cancelled: true,
                item_id: iid,
                ..
            } if id == exec_id && iid == item_id
        ));
    }
}
