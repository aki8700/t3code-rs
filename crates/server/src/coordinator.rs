mod execute;
mod locks;
mod queue;
mod quota_resume;
mod start;
mod state;
mod stream;
use crate::auth::{ActiveMcpToken, McpTokens, RunMcpToken};
use crate::provider_hold::{ProviderHold, ProviderHolds, limited_until, model_scoped};
use crate::swarm::{SwarmTurn, is_swarm_command};
use chrono::Utc;
use locks::KeyedLocks;
pub(crate) use locks::ThreadGuard;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use t3_core::*;
use t3_git::GitManager;
use t3_providers::{
    FailureClass, ProviderRegistry, classify_harness_failure, classify_provider_failure,
    classify_rate_limit, format_delegated_error, format_run_error, format_worker_error,
    run_turn_with_retry,
};
use t3_store::{INTERRUPTED_BY_RESTART, QueuedRun, QueuedRunInput, SqliteStore, VerifyDeadEnd};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use uuid::Uuid;

#[derive(Clone)]
pub struct RunCoordinator(pub(crate) Arc<Coordinator>);

pub(crate) struct Coordinator {
    pub(crate) cfg: ServerConfig,
    pub(crate) settings: crate::settings::SettingsStore,
    pub(crate) store: SqliteStore,
    pub(crate) git: GitManager,
    pub(crate) providers: ProviderRegistry,
    pub(crate) events: broadcast::Sender<WsServerMessage>,
    thread_locks: KeyedLocks,
    pub(crate) project_locks: KeyedLocks,
    pub(crate) board_locks: KeyedLocks,
    // Only hold this map to inspect or update in-memory run metadata.
    active: StdMutex<HashMap<Uuid, ActiveRun>>,
    pub(crate) latest_context: Mutex<HashMap<Uuid, LiveContextUsage>>,
    pub(crate) live_streams: Mutex<HashMap<Uuid, LiveStreamSnapshot>>,
    shutting_down: AtomicBool,
    /// Home directory holding the provider CLIs' session files.
    session_home: std::sync::RwLock<Option<std::path::PathBuf>>,
    injection: McpInjectionSpec,
    tokens: McpTokens,
    limits: Arc<crate::limits::UsageLimitsCache>,
    pub(crate) holds: ProviderHolds,
    quota_timers: quota_resume::Timers,
    user_runs: StdMutex<HashSet<Uuid>>,
    queued_tickets: StdMutex<HashMap<Uuid, watch::Sender<bool>>>,
    queued_chained_dones: StdMutex<HashMap<Uuid, Vec<watch::Sender<bool>>>>,
    pub(crate) wake_deliveries: crate::swarm::WakeDeliveries,
    thread_status_deduper: ThreadStatusDeduper,
}

#[derive(Debug, Default)]
pub(crate) struct ThreadStatusDeduper {
    last: StdMutex<HashMap<Uuid, ThreadStatus>>,
}

impl ThreadStatusDeduper {
    pub(crate) fn should_emit(&self, thread_id: Uuid, status: ThreadStatus) -> bool {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.get(&thread_id) == Some(&status) {
            return false;
        }
        last.insert(thread_id, status);
        true
    }
}

const MAX_LIVE_CONTEXT_ENTRIES: usize = 256;

#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveContextUsage {
    pub run_id: Uuid,
    pub provider: ProviderId,
    pub model: String,
    pub session_id: Option<String>,
    pub context_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveStreamSnapshot {
    pub run_id: Uuid,
    pub assistant: String,
    pub reasoning: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ItemBreakdown {
    pub kind: String,
    pub count: u64,
    pub raw_bytes: u64,
}

pub(crate) struct ActiveRun {
    pub(crate) project_id: Uuid,
    pub(crate) id: Uuid,
    model: ModelSelection,
    driver: Arc<dyn ProviderDriver>,
    cancel: watch::Sender<bool>,
    /// Set when someone asked to stop this run, as opposed to the server shutting down.
    stop_requested: bool,
    done: watch::Receiver<bool>,
    pub(crate) wake_depth: u8,
    pub(crate) runtime_mode: RuntimeMode,
    pub(crate) interaction_mode: InteractionMode,
    pending: HashMap<Uuid, PendingRequest>,
}

#[derive(Default, Deserialize)]
pub struct Message {
    pub prompt: String,
    pub behavior: Option<FollowUpBehavior>,
    #[serde(default)]
    pub attachment_ids: Vec<Uuid>,
    pub model_selection: Option<ModelSelection>,
    /// Internal queue marker: automated swarm deliveries retain their enqueue-time mode cap
    /// even when their wake depth is zero.
    #[serde(skip)]
    pub(crate) auto_wake: bool,
    #[serde(skip)]
    pub(crate) mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
}

pub(crate) struct RunTicket {
    pub(crate) id: Uuid,
    pub(crate) done: watch::Receiver<bool>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitorActiveRun {
    pub thread_id: Uuid,
    pub project_id: Uuid,
    pub run_id: Uuid,
    pub title: String,
    pub model_selection: ModelSelection,
    pub status: ThreadStatus,
    pub pending_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitorSnapshot {
    pub active_runs: Vec<MonitorActiveRun>,
    pub pending_requests: Vec<PendingRequest>,
    pub queued_thread_ids: Vec<Uuid>,
    pub active_shells: usize,
}

struct Outcome {
    provider: ProviderId,
    model: String,
    session: Option<String>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    reasoning_tokens: Option<u64>,
    has_context_usage: bool,
    had_file_changes: bool,
    had_side_effects: bool,
    hard_rate_limited: bool,
    reset_at: Option<String>,
    limited_until: Option<chrono::DateTime<Utc>>,
    failure_class: Option<FailureClass>,
    assistant: String,
    reasoning: String,
    terminal: Option<(RunStatus, Option<String>)>,
}

impl RunCoordinator {
    pub(crate) fn active(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, ActiveRun>> {
        self.0.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn new(cfg: ServerConfig, store: SqliteStore, injection: McpInjectionSpec) -> Self {
        Self::with_tokens(cfg, store, injection, McpTokens::default())
    }

    pub(crate) fn with_tokens(
        cfg: ServerConfig,
        store: SqliteStore,
        injection: McpInjectionSpec,
        tokens: McpTokens,
    ) -> Self {
        let settings = crate::settings::SettingsStore::in_memory(cfg.runtime_settings());
        Self::with_tokens_and_limits(
            cfg,
            store,
            injection,
            tokens,
            Arc::new(crate::limits::UsageLimitsCache::default()),
            settings,
        )
    }

    pub(crate) fn with_tokens_and_limits(
        cfg: ServerConfig,
        store: SqliteStore,
        injection: McpInjectionSpec,
        tokens: McpTokens,
        limits: Arc<crate::limits::UsageLimitsCache>,
        settings: crate::settings::SettingsStore,
    ) -> Self {
        let (events, _) = broadcast::channel(512);
        let mut providers = ProviderRegistry::from_config(&cfg);
        if cfg.computer_use_enabled {
            providers.register(Arc::new(crate::gemini_app::Driver::new(
                &cfg,
                store.clone(),
            )));
        }
        Self(Arc::new(Coordinator {
            git: GitManager::new(cfg.worktrees_dir()),
            providers,
            cfg,
            settings,
            store,
            injection,
            tokens,
            limits,
            holds: ProviderHolds::default(),
            quota_timers: Default::default(),
            user_runs: StdMutex::default(),
            queued_tickets: StdMutex::default(),
            queued_chained_dones: StdMutex::default(),
            wake_deliveries: Default::default(),
            thread_status_deduper: ThreadStatusDeduper::default(),
            events,
            thread_locks: KeyedLocks::default(),
            project_locks: KeyedLocks::default(),
            board_locks: KeyedLocks::default(),
            active: StdMutex::new(HashMap::new()),
            latest_context: Mutex::new(HashMap::new()),
            live_streams: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            session_home: std::sync::RwLock::new(std::env::home_dir()),
        }))
    }

    /// Points the pre-resume session check and the Codex usage reads at another home directory.
    pub fn set_session_home(&self, home: Option<std::path::PathBuf>) {
        *self
            .0
            .session_home
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = home;
    }

    async fn session_resumable(
        &self,
        provider: ProviderId,
        workspace: &std::path::Path,
        session_id: &str,
    ) -> bool {
        let home = self
            .0
            .session_home
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        crate::resume::session_resumable(provider, home, workspace, session_id).await
    }

    pub async fn live_context(&self, thread_id: Uuid) -> Option<LiveContextUsage> {
        self.0.latest_context.lock().await.get(&thread_id).cloned()
    }

    pub async fn live_stream(&self, thread_id: Uuid) -> Option<LiveStreamSnapshot> {
        self.0.live_streams.lock().await.get(&thread_id).cloned()
    }

    pub async fn rollback_run(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<()> {
        let _thread = self.lock_thread(thread_id).await;
        if self.active().contains_key(&thread_id) {
            return Err(CoreError::InvalidArgument(
                "cannot rollback while thread has an active run".into(),
            ));
        }
        let thread = self.0.store.get_thread(thread_id).await?;
        if thread.archived {
            return Err(CoreError::InvalidArgument("thread is archived".into()));
        }
        let pre_tree = self
            .0
            .store
            .get_run_checkpoint(thread_id, run_id)
            .await?
            .ok_or_else(|| CoreError::NotFound("run checkpoint".into()))?;
        let workspace = self.workspace(&thread).await?;
        GitManager::restore_tree(&workspace, &pre_tree).await
    }
}

struct QueuedMeta {
    notify_parent: bool,
    wake_depth: u8,
    mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
    verify_fix_depth: u8,
    last_verify_signature: Option<String>,
    verify_dead_ends: Vec<VerifyDeadEnd>,
    fallback_attempts: u8,
}

fn enqueue_mode_ceiling(
    current: (RuntimeMode, InteractionMode),
    message: &Message,
    wake_depth: u8,
) -> Option<(RuntimeMode, InteractionMode)> {
    message
        .mode_ceiling
        .or_else(|| (wake_depth != 0 || message.auto_wake).then_some(current))
}

struct RunningTurn {
    thread: Thread,
    request: TurnRequest,
    original_prompt: String,
    attachment_ids: Vec<Uuid>,
    cancellation: watch::Receiver<bool>,
    done: watch::Sender<bool>,
    chained_dones: Vec<watch::Sender<bool>>,
    driver: Arc<dyn ProviderDriver>,
    notify_parent: bool,
    wake_depth: u8,
    mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
    verify_fix_depth: u8,
    last_verify_signature: Option<String>,
    verify_dead_ends: Vec<VerifyDeadEnd>,
    fallback_attempts: u8,
    mcp_token: Option<RunMcpToken>,
    swarm: Option<SwarmTurn>,
    started_at: chrono::DateTime<Utc>,
    workspace_dir: std::path::PathBuf,
    verify_command: Option<String>,
}

fn redact_and_tail(text: &str, max_chars: usize) -> String {
    let cred_scrubbed = crate::threads::redact_credentials(text);
    let mut cleaned = Vec::new();
    for line in cred_scrubbed.lines() {
        let mut out_tokens = Vec::new();
        for token in line.split_whitespace() {
            if let Some(eq) = token.find('=') {
                let lhs = token[..eq]
                    .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .to_ascii_uppercase();
                if lhs.ends_with("_KEY")
                    || lhs.ends_with("_TOKEN")
                    || lhs.ends_with("_SECRET")
                    || lhs.ends_with("_PASSWORD")
                {
                    out_tokens.push(format!("{}=[REDACTED]", &token[..eq]));
                    continue;
                }
            }
            out_tokens.push(token.to_owned());
        }
        cleaned.push(out_tokens.join(" "));
    }
    let joined = cleaned.join("\n");
    let count = joined.chars().count();
    if count <= max_chars {
        joined
    } else {
        joined.chars().skip(count - max_chars).collect()
    }
}

fn accumulate_usage(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some((*total).unwrap_or_default().saturating_add(value));
    }
}

fn push_capped(buffer: &mut String, text: &str, other_len: usize) {
    const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
    let remaining = MAX_TEXT_BYTES.saturating_sub(buffer.len() + other_len);
    buffer.push_str(&text[..text.floor_char_boundary(remaining)]);
}

fn prompt_item(thread_id: Uuid, run_id: Uuid, message: &Message) -> NewTurnItem {
    let prompt = &message.prompt;
    NewTurnItem {
        thread_id,
        run_id,
        kind: TurnItemKind::UserMessage,
        summary: Some(compact(prompt, 240)),
        payload: json!({"text": prompt, "attachment_ids": message.attachment_ids}),
        searchable_text: Some(prompt.clone()),
    }
}

pub(crate) fn compact(text: &str, chars: usize) -> String {
    let mut result: String = text.chars().take(chars).collect();
    if result.len() < text.len() {
        result.push('…');
    }
    result
}

pub(crate) fn text_value(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| {
        ["text", "content", "message", "output"]
            .iter()
            .find_map(|key| value[*key].as_str())
    })
}

#[cfg(test)]
mod output_tail_tests {
    use super::*;

    #[test]
    fn auto_wake_at_depth_zero_keeps_enqueue_time_mode_ceiling() {
        let message = Message {
            auto_wake: true,
            ..Default::default()
        };
        assert_eq!(
            enqueue_mode_ceiling(
                (RuntimeMode::ApprovalRequired, InteractionMode::Plan),
                &message,
                0
            ),
            Some((RuntimeMode::ApprovalRequired, InteractionMode::Plan))
        );
    }

    #[test]
    fn redelivery_keeps_an_existing_mode_ceiling() {
        let message = Message {
            auto_wake: true,
            mode_ceiling: Some((RuntimeMode::ApprovalRequired, InteractionMode::Plan)),
            ..Default::default()
        };
        assert_eq!(
            enqueue_mode_ceiling(
                (RuntimeMode::FullAccess, InteractionMode::Default),
                &message,
                0
            ),
            Some((RuntimeMode::ApprovalRequired, InteractionMode::Plan))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verify_feedback_uses_the_kept_shell_output_tail() {
        let (_cancel_tx, cancellation) = tokio::sync::watch::channel(false);
        let out = crate::shell::run_shell_cancellable(
            &std::env::temp_dir(),
            crate::shell::ShellExecRequest {
                command: "head -c 300000 /dev/zero | tr '\\000' 'x'; printf '\\nVERIFY_FINAL_LINE_MARKER\\n'".into(),
                timeout_secs: Some(5),
            },
            cancellation,
        )
        .await
        .expect("verify shell should run");
        let tail = redact_and_tail(&out.stdout, 4_000);
        let summary = crate::verify::summarize_failure("verify", Some(1), false, &tail);
        let prompt =
            crate::verify::build_fix_prompt("verify", Some(1), false, &summary, &tail, 1, 3, &[]);

        assert!(prompt.contains("VERIFY_FINAL_LINE_MARKER"));
        assert!(summary.contains("VERIFY_FINAL_LINE_MARKER"));
    }

    #[test]
    fn thread_status_deduper_suppresses_consecutive_identical_statuses() {
        let deduper = ThreadStatusDeduper::default();
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();

        assert!(deduper.should_emit(t1, ThreadStatus::Running));
        assert!(!deduper.should_emit(t1, ThreadStatus::Running));
        assert!(!deduper.should_emit(t1, ThreadStatus::Running));

        assert!(deduper.should_emit(t2, ThreadStatus::Running));
        assert!(!deduper.should_emit(t2, ThreadStatus::Running));

        assert!(deduper.should_emit(t1, ThreadStatus::Idle));
        assert!(!deduper.should_emit(t1, ThreadStatus::Idle));
        assert!(deduper.should_emit(t1, ThreadStatus::Running));
    }
}
