use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    Claude,
    Codex,
    Antigravity,
    #[serde(alias = "opencode")]
    OpenCode,
    Mock,
}

impl ProviderId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
            Self::Mock => "mock",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity (Gemini)",
            Self::OpenCode => "OpenCode",
            Self::Mock => "Mock (Test)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelTier {
    Tier0FreeLocal,
    Tier1Fast,
    Tier2Review,
    Tier3Arch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub provider: ProviderId,
    pub id: String,
    pub label: String,
    pub tier: ModelTier,
    pub effort_levels: Vec<String>,
    pub default_effort: Option<String>,
    pub is_local: bool,
    pub is_free: bool,
    #[serde(default)]
    pub context_window: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider: ProviderId,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

impl Default for ModelSelection {
    fn default() -> Self {
        Self {
            provider: ProviderId::Claude,
            model: "claude-sonnet-5-5".to_string(),
            effort: Some("high".to_string()),
            agent: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    #[default]
    ApprovalRequired,
    AutoAcceptEdits,
    FullAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InteractionMode {
    #[default]
    Default,
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FollowUpBehavior {
    #[default]
    Steer,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationMode {
    #[default]
    Shared,
    Worktree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub title: String,
    pub workspace_root: PathBuf,
    pub default_model: Option<ModelSelection>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub verify_command: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadStatus {
    #[default]
    Idle,
    Running,
    WaitingForChildren,
    RateLimited,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: Uuid,
    pub project_id: Uuid,
    pub parent_thread_id: Option<Uuid>,
    pub relationship: Option<String>,
    pub title: String,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub status: ThreadStatus,
    pub worktree_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub provider_session_id: Option<String>,
    pub archived: bool,
    pub pinned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    Queued,
    Running,
    RateLimited,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub ordinal: i64,
    pub provider: ProviderId,
    pub model: String,
    pub effort: Option<String>,
    pub status: RunStatus,
    pub prompt: String,
    pub provider_session_id: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub error_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnItemKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    ToolCall,
    ToolResult,
    CommandExecution,
    FileChange,
    Subagent,
    Handoff,
    SystemNotice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnItem {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub run_id: Uuid,
    pub ordinal: i64,
    pub kind: TurnItemKind,
    pub summary: Option<String>,
    pub payload: Value,
    pub raw_bytes: usize,
    pub compressed_bytes: Option<usize>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Uuid,
    pub thread_id: Option<Uuid>,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub blake3_hash: String,
    pub storage_path: PathBuf,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextHandoff {
    pub id: Uuid,
    pub source_thread_id: Uuid,
    pub target_thread_id: Uuid,
    pub from_model: ModelSelection,
    pub to_model: ModelSelection,
    pub summary: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationSpec {
    pub parent_thread_id: Uuid,
    pub title: String,
    pub task: String,
    pub model_selection: ModelSelection,
    pub isolation: IsolationMode,
    pub base_ref: Option<String>,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
}

impl DelegationSpec {
    pub fn validate_permissions(&self, parent: &Thread) -> crate::CoreResult<()> {
        if self.runtime_mode > parent.runtime_mode {
            return Err(crate::CoreError::InvalidArgument(
                "delegated runtime_mode exceeds the parent thread's permissions".into(),
            ));
        }
        if parent.interaction_mode == InteractionMode::Plan
            && self.interaction_mode == InteractionMode::Default
        {
            return Err(crate::CoreError::InvalidArgument(
                "a plan thread can only delegate in plan mode".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub run_id: Uuid,
    pub kind: String,
    pub title: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingResponse {
    pub request_id: Uuid,
    pub approved: bool,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchModelStat {
    pub provider: ProviderId,
    pub model: String,
    pub total_runs: u64,
    pub completed_runs: u64,
    pub failed_runs: u64,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStats {
    pub db_size_bytes: u64,
    pub threads_count: u64,
    pub runs_count: u64,
    pub turn_items_count: u64,
    pub raw_payload_bytes: u64,
    pub stored_payload_bytes: u64,
    pub compression_ratio: f64,
    pub dedup_blobs_count: u64,
    pub attachments_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub thread_id: Uuid,
    pub thread_title: String,
    pub item_id: Option<Uuid>,
    pub snippet: String,
    pub rank: f64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
    pub patch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub workspace_path: PathBuf,
    pub branch: Option<String>,
    pub clean: bool,
    pub total_additions: usize,
    pub total_deletions: usize,
    pub files: Vec<DiffFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthIdentity {
    pub subject: String,
    pub email: Option<String>,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub provider: ProviderId,
    pub available: bool,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub models: Vec<ModelSpec>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpInjectionSpec {
    pub binary_path: PathBuf,
    pub server_url: String,
    pub internal_token: String,
    pub thread_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_selection_roundtrip() {
        let sel = ModelSelection {
            provider: ProviderId::Codex,
            model: "gpt-6-luna".into(),
            effort: Some("high".into()),
            agent: None,
        };
        let json = serde_json::to_string(&sel).unwrap();
        let back: ModelSelection = serde_json::from_str(&json).unwrap();
        assert_eq!(sel, back);
    }

    #[test]
    fn provider_id_serialization() {
        assert_eq!(
            serde_json::to_string(&ProviderId::OpenCode).unwrap(),
            "\"open-code\""
        );
        assert_eq!(ProviderId::OpenCode.as_str(), "opencode");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmMember {
    pub board_id: Uuid,
    pub name: String,
    pub thread_id: Uuid,
    pub provider: ProviderId,
    pub model: String,
    pub effort: Option<String>,
    pub state: String,
    pub role: String,
    pub last_read_seq: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmMessage {
    pub seq: i64,
    pub board_id: Uuid,
    pub sender: String,
    pub text: String,
    pub mentions: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmBoardSnapshot {
    pub board_id: Uuid,
    pub coordinator_thread_id: Uuid,
    pub title: String,
    pub members: Vec<SwarmMember>,
    /// Recent messages. Use the board reader to retrieve earlier messages.
    pub messages: Vec<SwarmMessage>,
    pub head_seq: i64,
}

#[derive(Debug, Deserialize)]
pub struct SwarmWorkerSpec {
    pub name: String,
    pub brief: String,
    pub provider: Option<ProviderId>,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub isolation: IsolationMode,
}
