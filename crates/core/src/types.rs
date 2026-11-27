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
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
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
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_reasoning_tokens: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UndoTier {
    /// Docs, tests, or localized changes that can ship on green.
    OneClick,
    /// Shared helpers, auth, schema/config, or wide diffs — green + trace + second opinion recommended.
    Cleanup,
    /// Migrations, secrets, deploy/infra, or destructive SQL — requires explicit human sign-off.
    NoUndo,
}

impl UndoTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OneClick => "one-click",
            Self::Cleanup => "cleanup",
            Self::NoUndo => "no-undo",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRiskReport {
    pub tier: UndoTier,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub flagged_paths: Vec<String>,
}

/// Deterministic Undo-price classification for a set of changed files.
pub fn classify_diff_risk(files: &[DiffFile]) -> DiffRiskReport {
    if files.is_empty() {
        return DiffRiskReport {
            tier: UndoTier::OneClick,
            reasons: Vec::new(),
            flagged_paths: Vec::new(),
        };
    }

    let mut tier = UndoTier::OneClick;
    let mut reasons = Vec::new();
    let mut flagged_paths = Vec::new();
    let mut push_reason = |reason: &str| {
        if !reasons.iter().any(|existing: &String| existing == reason) {
            reasons.push(reason.to_owned());
        }
    };
    let mut push_path = |path: &str| {
        if !flagged_paths
            .iter()
            .any(|existing: &String| existing == path)
        {
            flagged_paths.push(path.to_owned());
        }
    };

    let mut non_doc_lines: usize = 0;
    for file in files {
        let norm = file.path.replace('\\', "/");
        let lower = norm.to_ascii_lowercase();
        let file_name = lower.rsplit('/').next().unwrap_or(&lower);
        let is_test_or_doc = is_doc_or_test_path(&lower, file_name);

        if is_no_undo_path(&lower, file_name) {
            tier = UndoTier::NoUndo;
            push_reason(
                "migration / secrets / deploy-infra path touched (human sign-off required)",
            );
            push_path(&norm);
            continue;
        }
        if !is_test_or_doc && lower.ends_with(".sql") && has_destructive_sql_addition(&file.patch) {
            tier = UndoTier::NoUndo;
            push_reason("destructive SQL statement added (human sign-off required)");
            push_path(&norm);
            continue;
        }
        if is_cleanup_path(&lower, file_name) {
            if tier < UndoTier::Cleanup {
                tier = UndoTier::Cleanup;
            }
            push_reason(
                "auth / schema / manifest / store boundary changed (second-opinion review recommended)",
            );
            push_path(&norm);
        } else if !is_test_or_doc && file.status.trim().eq_ignore_ascii_case("D")
            || (!is_test_or_doc && file.status.trim().eq_ignore_ascii_case("deleted"))
        {
            if tier < UndoTier::Cleanup {
                tier = UndoTier::Cleanup;
            }
            push_reason("non-test source file deleted");
            push_path(&norm);
        }

        if !is_test_or_doc {
            non_doc_lines = non_doc_lines
                .saturating_add(file.additions)
                .saturating_add(file.deletions);
        }
    }

    if tier < UndoTier::Cleanup && (files.len() >= 8 || non_doc_lines >= 400) {
        tier = UndoTier::Cleanup;
        push_reason("broad multi-file or large diff (second-opinion review recommended)");
    }

    DiffRiskReport {
        tier,
        reasons,
        flagged_paths,
    }
}

fn has_path_segment(lower: &str, needle: &str) -> bool {
    lower.split('/').any(|seg| seg == needle)
}

fn is_doc_or_test_path(lower: &str, file_name: &str) -> bool {
    lower.ends_with(".md")
        || lower.ends_with(".txt")
        || has_path_segment(lower, "docs")
        || has_path_segment(lower, "tests")
        || has_path_segment(lower, "__tests__")
        || file_name.starts_with("test_")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with(".test.ts")
        || file_name.ends_with(".test.js")
        || file_name.ends_with(".spec.ts")
        || file_name.ends_with(".spec.js")
        || file_name == "tests.rs"
}

fn is_no_undo_path(lower: &str, file_name: &str) -> bool {
    // Migrations
    if has_path_segment(lower, "migrations")
        || lower.contains("alembic/versions/")
        || lower.contains("prisma/migrations/")
        || (file_name.starts_with("migration") && file_name.ends_with(".sql"))
        || file_name.contains(".migration.")
    {
        return true;
    }
    // Secrets / keys / env files (excluding templates)
    let is_env_template = file_name.ends_with(".example")
        || file_name.ends_with(".sample")
        || file_name.ends_with(".template");
    if (!is_env_template && (file_name == ".env" || file_name.starts_with(".env.")))
        || file_name.ends_with(".pem")
        || file_name.ends_with(".key")
        || file_name.ends_with(".p12")
        || file_name.starts_with("id_rsa")
        || file_name.starts_with("id_ed25519")
        || file_name.starts_with("secrets.")
        || file_name.starts_with("credentials.")
    {
        return true;
    }
    // Deploy / infra / system install hooks
    if has_path_segment(lower, "terraform")
        || has_path_segment(lower, "k8s")
        || has_path_segment(lower, "helm")
        || has_path_segment(lower, "deploy")
        || has_path_segment(lower, "launchagents")
        || lower.starts_with(".github/workflows/")
        || lower.contains("/.github/workflows/")
        || file_name.ends_with(".tf")
        || file_name.ends_with(".plist")
        || file_name == "dockerfile"
        || file_name.starts_with("docker-compose")
        || lower.starts_with("scripts/install-")
        || lower.starts_with("scripts/deploy")
        || lower.starts_with("scripts/release")
    {
        return true;
    }
    false
}

fn is_cleanup_path(lower: &str, file_name: &str) -> bool {
    if is_doc_or_test_path(lower, file_name) {
        return false;
    }
    let sensitive_stem = [
        "auth",
        "security",
        "privacy",
        "permission",
        "oauth",
        "jwt",
        "crypto",
        "rbac",
    ];
    if sensitive_stem
        .iter()
        .any(|kw| file_name.contains(kw) || has_path_segment(lower, kw))
    {
        return true;
    }
    if file_name == "schema.sql"
        || file_name == "schema.prisma"
        || lower.starts_with("crates/store/")
        || has_path_segment(lower, "db")
    {
        return true;
    }
    matches!(
        file_name,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "pyproject.toml"
            | "go.mod"
            | "makefile"
    )
}

fn has_destructive_sql_addition(patch: &str) -> bool {
    for line in patch.lines() {
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if added.starts_with("++") {
            continue;
        }
        let upper = added.to_ascii_uppercase();
        if upper.contains("DROP TABLE")
            || upper.contains("DROP COLUMN")
            || upper.contains("TRUNCATE ")
            || upper.contains("DELETE FROM")
        {
            return true;
        }
    }
    false
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<DiffRiskReport>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cua_binary_path: Option<PathBuf>,
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

    #[test]
    fn classify_diff_risk_tiers_by_undo_price() {
        let one_click = classify_diff_risk(&[DiffFile {
            path: "README.md".into(),
            status: "M".into(),
            additions: 5,
            deletions: 2,
            patch: String::new(),
        }]);
        assert_eq!(one_click.tier, UndoTier::OneClick);
        assert!(one_click.flagged_paths.is_empty());

        let cleanup_auth = classify_diff_risk(&[DiffFile {
            path: "crates/server/src/auth.rs".into(),
            status: "M".into(),
            additions: 12,
            deletions: 4,
            patch: String::new(),
        }]);
        assert_eq!(cleanup_auth.tier, UndoTier::Cleanup);
        assert_eq!(
            cleanup_auth.flagged_paths,
            vec!["crates/server/src/auth.rs"]
        );

        let no_undo_migration = classify_diff_risk(&[
            DiffFile {
                path: "README.md".into(),
                status: "M".into(),
                additions: 1,
                deletions: 0,
                patch: String::new(),
            },
            DiffFile {
                path: "db/migrations/0012_drop_legacy.sql".into(),
                status: "A".into(),
                additions: 3,
                deletions: 0,
                patch: "+DROP TABLE legacy_runs;\n".into(),
            },
        ]);
        assert_eq!(no_undo_migration.tier, UndoTier::NoUndo);
        assert_eq!(
            no_undo_migration.flagged_paths,
            vec!["db/migrations/0012_drop_legacy.sql"]
        );

        let no_undo_sql = classify_diff_risk(&[DiffFile {
            path: "crates/store/src/schema.sql".into(),
            status: "M".into(),
            additions: 1,
            deletions: 0,
            patch: "+ALTER TABLE runs DROP COLUMN legacy;\n".into(),
        }]);
        assert_eq!(no_undo_sql.tier, UndoTier::NoUndo);

        let env_example = classify_diff_risk(&[DiffFile {
            path: ".env.example".into(),
            status: "M".into(),
            additions: 1,
            deletions: 0,
            patch: String::new(),
        }]);
        assert_eq!(env_example.tier, UndoTier::OneClick);

        let real_env = classify_diff_risk(&[DiffFile {
            path: ".env.production".into(),
            status: "M".into(),
            additions: 1,
            deletions: 0,
            patch: String::new(),
        }]);
        assert_eq!(real_env.tier, UndoTier::NoUndo);
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
