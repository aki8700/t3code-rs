use crate::protocol::StreamEvent;
use crate::types::{
    Attachment, ContextHandoff, DbStats, InteractionMode, McpInjectionSpec, ModelSelection,
    OrchModelStat, PendingResponse, Project, ProviderHealth, ProviderId, Run, RunStatus,
    RuntimeMode, SearchHit, Thread, ThreadStatus, TurnItem, TurnItemKind,
};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("auth error: {0}")]
    Auth(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub thread_id: Uuid,
    pub run_id: Uuid,
    pub workspace_dir: PathBuf,
    pub prompt: String,
    /// When set, keep the Claude slash invocation in `prompt` as its own text block.
    pub claude_slash_context: Option<String>,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub resume_session_id: Option<String>,
    pub attachments: Vec<Attachment>,
    pub mcp_injection: Option<McpInjectionSpec>,
}

#[async_trait]
pub trait ProviderDriver: Send + Sync {
    fn provider_id(&self) -> ProviderId;

    /// Returns true if this driver injects `TurnRequest::mcp_injection` into the provider CLI.
    fn supports_mcp(&self) -> bool {
        false
    }

    async fn health_check(&self) -> ProviderHealth;

    /// Executes a turn, streaming `StreamEvent`s into `tx` until completion or cancellation.
    async fn run_turn(
        &self,
        req: TurnRequest,
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> CoreResult<()>;

    /// Injects a mid-turn follow-up message ("steer") if the active turn supports streaming stdin.
    /// Returns `Ok(true)` if delivered inline, `Ok(false)` if caller should queue or interrupt+resume.
    async fn send_steer(&self, _run_id: Uuid, _message: String) -> CoreResult<bool> {
        Ok(false)
    }

    /// Responds to a pending approval / question request from an active run.
    async fn respond_request(&self, _run_id: Uuid, _resp: PendingResponse) -> CoreResult<bool> {
        Ok(false)
    }

    /// Interrupts / kills the process group for `run_id`.
    async fn interrupt(&self, run_id: Uuid) -> CoreResult<()>;
}

#[derive(Debug, Clone)]
pub struct NewTurnItem {
    pub thread_id: Uuid,
    pub run_id: Uuid,
    pub kind: TurnItemKind,
    pub summary: Option<String>,
    pub payload: Value,
    /// Optional plain text indexed into SQLite FTS5 trigram search.
    pub searchable_text: Option<String>,
}

#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait Store: Send + Sync {
    async fn create_project(&self, title: String, workspace_root: PathBuf) -> CoreResult<Project>;
    async fn list_projects(&self) -> CoreResult<Vec<Project>>;
    async fn get_project(&self, id: Uuid) -> CoreResult<Project>;
    async fn delete_project(&self, id: Uuid) -> CoreResult<()>;

    async fn create_thread(&self, thread: Thread) -> CoreResult<Thread>;
    async fn list_threads(
        &self,
        project_id: Option<Uuid>,
        include_archived: bool,
        limit: usize,
    ) -> CoreResult<Vec<Thread>>;
    async fn list_child_threads(&self, parent_thread_id: Uuid) -> CoreResult<Vec<Thread>>;
    async fn get_thread(&self, id: Uuid) -> CoreResult<Thread>;
    async fn update_thread_status(&self, id: Uuid, status: ThreadStatus) -> CoreResult<()>;
    async fn update_thread_session_id(&self, id: Uuid, session_id: String) -> CoreResult<()>;
    async fn update_thread_model(&self, id: Uuid, selection: ModelSelection) -> CoreResult<()>;
    async fn update_thread_modes(
        &self,
        id: Uuid,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
    ) -> CoreResult<()>;
    async fn update_thread_title(&self, id: Uuid, title: String) -> CoreResult<()>;
    async fn archive_thread(&self, id: Uuid, archived: bool) -> CoreResult<()>;

    async fn create_run(&self, run: Run) -> CoreResult<Run>;
    async fn list_runs(&self, thread_id: Uuid) -> CoreResult<Vec<Run>>;
    async fn finish_run(
        &self,
        run_id: Uuid,
        status: RunStatus,
        provider_session_id: Option<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        duration_ms: u64,
        error_message: Option<String>,
    ) -> CoreResult<()>;

    async fn append_turn_item(&self, item: NewTurnItem) -> CoreResult<TurnItem>;
    async fn list_turn_items(
        &self,
        thread_id: Uuid,
        limit: usize,
        offset: usize,
    ) -> CoreResult<Vec<TurnItem>>;

    /// Return the newest items before the exclusive cursor, in ascending ordinal order.
    /// The boolean indicates whether older items exist. The count covers the whole thread.
    async fn list_turn_items_page(
        &self,
        thread_id: Uuid,
        limit: usize,
        before_ordinal: Option<i64>,
    ) -> CoreResult<(Vec<TurnItem>, bool, usize)>;

    async fn record_handoff(&self, handoff: ContextHandoff) -> CoreResult<ContextHandoff>;
    async fn list_handoffs(&self, thread_id: Uuid) -> CoreResult<Vec<ContextHandoff>>;

    async fn save_attachment(&self, attachment: Attachment) -> CoreResult<Attachment>;
    async fn get_attachment(&self, id: Uuid) -> CoreResult<Attachment>;
    async fn list_attachments(&self, thread_id: Uuid) -> CoreResult<Vec<Attachment>>;

    async fn search_fts(&self, query: &str, limit: usize) -> CoreResult<Vec<SearchHit>>;
    async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>>;
    async fn db_stats(&self) -> CoreResult<DbStats>;
}
