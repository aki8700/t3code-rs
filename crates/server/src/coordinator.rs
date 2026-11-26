use crate::auth::{ActiveMcpToken, McpTokens, RunMcpToken};
use crate::swarm::{SwarmTurn, is_swarm_command};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use t3_core::*;
use t3_git::GitManager;
use t3_providers::{
    FailureClass, ProviderRegistry, classify_harness_failure, classify_provider_failure,
    classify_rate_limit, format_delegated_error, format_run_error, run_turn_with_retry,
};
use t3_store::{QueuedRun, QueuedRunInput, SqliteStore};
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
    pub(crate) active: Mutex<HashMap<Uuid, ActiveRun>>,
    pub(crate) latest_context: Mutex<HashMap<Uuid, LiveContextUsage>>,
    pub(crate) live_streams: Mutex<HashMap<Uuid, LiveStreamSnapshot>>,
    shutting_down: AtomicBool,
    injection: McpInjectionSpec,
    tokens: McpTokens,
    limits: Arc<crate::limits::UsageLimitsCache>,
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
    has_context_usage: bool,
    had_file_changes: bool,
    had_side_effects: bool,
    hard_rate_limited: bool,
    reset_at: Option<String>,
    failure_class: Option<FailureClass>,
    assistant: String,
    reasoning: String,
    terminal: Option<(RunStatus, Option<String>)>,
}

impl RunCoordinator {
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
        Self(Arc::new(Coordinator {
            git: GitManager::new(cfg.worktrees_dir()),
            providers: ProviderRegistry::from_config(&cfg),
            cfg,
            settings,
            store,
            injection,
            tokens,
            limits,
            events,
            active: Mutex::new(HashMap::new()),
            latest_context: Mutex::new(HashMap::new()),
            live_streams: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        }))
    }

    pub async fn live_context(&self, thread_id: Uuid) -> Option<LiveContextUsage> {
        self.0.latest_context.lock().await.get(&thread_id).cloned()
    }

    pub async fn live_stream(&self, thread_id: Uuid) -> Option<LiveStreamSnapshot> {
        self.0.live_streams.lock().await.get(&thread_id).cloned()
    }

    pub async fn rollback_run(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<()> {
        let active = self.0.active.lock().await;
        if active.contains_key(&thread_id) {
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

    pub async fn git_stage(&self, thread_id: Uuid, rel_path: &str) -> CoreResult<()> {
        let thread = self.0.store.get_thread(thread_id).await?;
        let workspace = self.workspace(&thread).await?;
        GitManager::stage_file(&workspace, rel_path).await
    }

    pub async fn git_discard(&self, thread_id: Uuid, rel_path: &str) -> CoreResult<()> {
        let active = self.0.active.lock().await;
        if active.contains_key(&thread_id) {
            return Err(CoreError::InvalidArgument(
                "cannot discard files while thread has an active run".into(),
            ));
        }
        let thread = self.0.store.get_thread(thread_id).await?;
        let workspace = self.workspace(&thread).await?;
        GitManager::discard_file(&workspace, rel_path).await
    }

    pub async fn git_commit(&self, thread_id: Uuid, message: &str) -> CoreResult<String> {
        let active = self.0.active.lock().await;
        if active.contains_key(&thread_id) {
            return Err(CoreError::InvalidArgument(
                "cannot commit while thread has an active run".into(),
            ));
        }
        let thread = self.0.store.get_thread(thread_id).await?;
        let workspace = self.workspace(&thread).await?;
        GitManager::commit(&workspace, message).await
    }

    pub async fn cancel_queued_run(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<bool> {
        let _active = self.0.active.lock().await;
        let cancelled = self.0.store.cancel_queued_run(thread_id, run_id).await?;
        if cancelled {
            self.emit(WsServerMessage::RunStatusChanged {
                thread_id,
                run_id,
                status: RunStatus::Cancelled,
                error: Some("Cancelled".into()),
            });
        }
        Ok(cancelled)
    }

    pub async fn stored_context(&self, thread_id: Uuid) -> Option<LiveContextUsage> {
        let id = thread_id.to_string();
        self.sql(move |db| {
            use rusqlite::OptionalExtension;
            db.query_row(
                "SELECT run_id, provider, model, session_id, context_tokens,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
                 FROM thread_context WHERE thread_id = ?1",
                [id],
                |row| {
                    let run_id_s: String = row.get(0)?;
                    let prov_s: String = row.get(1)?;
                    let run_id = Uuid::parse_str(&run_id_s).unwrap_or_default();
                    let provider =
                        serde_json::from_str::<ProviderId>(&prov_s).unwrap_or(ProviderId::Mock);
                    Ok(LiveContextUsage {
                        run_id,
                        provider,
                        model: row.get(2)?,
                        session_id: row.get(3)?,
                        context_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                        input_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                        output_tokens: row.get::<_, i64>(6)?.max(0) as u64,
                        cache_read_tokens: row.get::<_, i64>(7)?.max(0) as u64,
                        cache_write_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                    })
                },
            )
            .optional()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn persist_context(&self, thread_id: Uuid, snap: &LiveContextUsage) {
        let Ok(provider) = serde_json::to_string(&snap.provider) else {
            return;
        };
        let id = thread_id.to_string();
        let run_id = snap.run_id.to_string();
        let model = snap.model.clone();
        let session_id = snap.session_id.clone();
        let (ctx, inp, out, cr, cw) = (
            snap.context_tokens.min(i64::MAX as u64) as i64,
            snap.input_tokens.min(i64::MAX as u64) as i64,
            snap.output_tokens.min(i64::MAX as u64) as i64,
            snap.cache_read_tokens.min(i64::MAX as u64) as i64,
            snap.cache_write_tokens.min(i64::MAX as u64) as i64,
        );
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        if let Err(error) = self
            .sql(move |db| {
                db.execute(
                    "INSERT INTO thread_context (
                        thread_id, run_id, provider, model, session_id,
                        context_tokens, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT(thread_id) DO UPDATE SET
                        run_id = excluded.run_id,
                        provider = excluded.provider,
                        model = excluded.model,
                        session_id = excluded.session_id,
                        context_tokens = excluded.context_tokens,
                        input_tokens = excluded.input_tokens,
                        output_tokens = excluded.output_tokens,
                        cache_read_tokens = excluded.cache_read_tokens,
                        cache_write_tokens = excluded.cache_write_tokens,
                        updated_at = excluded.updated_at",
                    rusqlite::params![
                        id, run_id, provider, model, session_id, ctx, inp, out, cr, cw, now
                    ],
                )
            })
            .await
        {
            tracing::warn!(%error, %thread_id, "Could not persist thread context");
        }
    }

    pub async fn clear_live_context(&self, thread_id: Uuid) {
        self.0.latest_context.lock().await.remove(&thread_id);
        let id = thread_id.to_string();
        let _ = self
            .sql(move |db| db.execute("DELETE FROM thread_context WHERE thread_id = ?1", [id]))
            .await;
    }

    async fn record_live_context(&self, thread_id: Uuid, snap: LiveContextUsage) {
        let mut map = self.0.latest_context.lock().await;
        if map.len() >= MAX_LIVE_CONTEXT_ENTRIES
            && !map.contains_key(&thread_id)
            && let Some(evict) = map.keys().next().copied()
        {
            map.remove(&evict);
        }
        map.insert(thread_id, snap);
    }

    pub async fn items_breakdown(&self, thread_id: Uuid) -> CoreResult<Vec<ItemBreakdown>> {
        let id = thread_id.to_string();
        self.sql(move |db| {
            let mut stmt = db.prepare(
                "SELECT kind, count(*), coalesce(sum(raw_bytes), 0)
                 FROM turn_items WHERE thread_id = ?1 GROUP BY kind",
            )?;
            let rows = stmt
                .query_map([id], |row| {
                    let raw_kind: String = row.get(0)?;
                    let kind = serde_json::from_str::<String>(&raw_kind).unwrap_or(raw_kind);
                    Ok(ItemBreakdown {
                        kind,
                        count: row.get::<_, i64>(1)?.max(0) as u64,
                        raw_bytes: row.get::<_, i64>(2)?.max(0) as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn monitor_snapshot(&self, active_shells: usize) -> CoreResult<MonitorSnapshot> {
        let (entries, pending_requests) = {
            let active = self.0.active.lock().await;
            let mut entries = Vec::with_capacity(active.len());
            let mut pending = Vec::new();
            for (&thread_id, run) in active.iter() {
                entries.push((
                    thread_id,
                    run.project_id,
                    run.id,
                    run.model.clone(),
                    run.pending.len(),
                ));
                pending.extend(run.pending.values().cloned());
            }
            (entries, pending)
        };
        let mut active_runs = Vec::with_capacity(entries.len());
        for (thread_id, project_id, run_id, model_selection, pending_count) in entries {
            if let Ok(thread) = self.0.store.get_thread(thread_id).await {
                active_runs.push(MonitorActiveRun {
                    thread_id,
                    project_id,
                    run_id,
                    title: thread.title,
                    model_selection,
                    status: thread.status,
                    pending_count,
                });
            }
        }
        let queued_thread_ids = self.0.store.queued_threads().await?;
        Ok(MonitorSnapshot {
            active_runs,
            pending_requests,
            queued_thread_ids,
            active_shells,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WsServerMessage> {
        self.0.events.subscribe()
    }

    pub(crate) fn emit(&self, event: WsServerMessage) {
        let _ = self.0.events.send(event);
    }

    pub(crate) async fn pending(&self, id: Uuid) -> Vec<PendingRequest> {
        self.0
            .active
            .lock()
            .await
            .get(&id)
            .map(|run| run.pending.values().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn send(&self, id: Uuid, message: Message) -> CoreResult<Uuid> {
        let mut active = self.0.active.lock().await;
        let thread = self.0.store.get_thread(id).await?;
        self.send_locked(&mut active, thread, message, 0).await
    }

    pub(crate) async fn send_locked(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        thread: Thread,
        message: Message,
        wake_depth: u8,
    ) -> CoreResult<Uuid> {
        self.ensure_accepting_runs()?;
        self.validate_message(&thread, &message).await?;
        let id = thread.id;
        let swarm_command = is_swarm_command(&message.prompt);
        if swarm_command {
            self.ensure_board(&thread).await?;
        }
        if !swarm_command
            && let Some(run) = active.get(&id)
            && message
                .behavior
                .unwrap_or(self.0.settings.get().follow_up_behavior)
                == FollowUpBehavior::Steer
            && message.attachment_ids.is_empty()
            && message
                .model_selection
                .as_ref()
                .is_none_or(|model| *model == run.model)
            && !*run.cancel.borrow()
            && run
                .driver
                .send_steer(run.id, message.prompt.clone())
                .await?
        {
            self.append(prompt_item(id, run.id, &message)).await?;
            return Ok(run.id);
        }

        let busy = active.contains_key(&id);
        let run_id = self
            .enqueue(&thread, message, false, wake_depth, busy)
            .await?;
        if !busy && let Some((_, turn)) = self.start_next(active, &thread, Some(run_id)).await? {
            self.spawn(turn);
        }
        Ok(run_id)
    }

    pub(crate) async fn start_delegated(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        thread: Thread,
        message: Message,
    ) -> CoreResult<RunTicket> {
        self.ensure_accepting_runs()?;
        self.validate_message(&thread, &message).await?;
        let id = self.enqueue(&thread, message, true, 0, false).await?;
        let (ticket, turn) = self
            .start_next(active, &thread, Some(id))
            .await?
            .ok_or_else(|| CoreError::Internal("delegated run was not queued".into()))?;
        self.spawn(turn);
        Ok(ticket)
    }

    async fn validate_message(&self, thread: &Thread, message: &Message) -> CoreResult<()> {
        if thread.archived {
            return Err(CoreError::InvalidArgument("thread is archived".into()));
        }
        if message.prompt.trim().is_empty() && message.attachment_ids.is_empty() {
            return Err(CoreError::InvalidArgument(
                "prompt or attachment is required".into(),
            ));
        }
        self.driver(
            message
                .model_selection
                .as_ref()
                .unwrap_or(&thread.model_selection),
        )?;
        self.attachments(thread.id, &message.attachment_ids).await?;
        Ok(())
    }

    async fn attachments(&self, thread_id: Uuid, ids: &[Uuid]) -> CoreResult<Vec<Attachment>> {
        let mut attachments = Vec::new();
        for &id in ids {
            let attachment = self.0.store.get_attachment(id).await?;
            if attachment.thread_id.is_some_and(|owner| owner != thread_id) {
                return Err(CoreError::InvalidArgument(
                    "attachment belongs to another thread".into(),
                ));
            }
            attachments.push(attachment);
        }
        Ok(attachments)
    }

    async fn enqueue(
        &self,
        thread: &Thread,
        message: Message,
        notify_parent: bool,
        wake_depth: u8,
        busy: bool,
    ) -> CoreResult<Uuid> {
        self.enqueue_with_meta(
            thread,
            message,
            QueuedMeta {
                notify_parent,
                wake_depth,
                mode_ceiling: (wake_depth != 0)
                    .then_some((thread.runtime_mode, thread.interaction_mode)),
                verify_fix_depth: 0,
                last_verify_signature: None,
                fallback_attempts: 0,
            },
            busy,
        )
        .await
    }

    async fn enqueue_with_meta(
        &self,
        thread: &Thread,
        message: Message,
        meta: QueuedMeta,
        busy: bool,
    ) -> CoreResult<Uuid> {
        let id = Uuid::new_v4();
        let item = prompt_item(thread.id, id, &message);
        let selection = message
            .model_selection
            .unwrap_or_else(|| thread.model_selection.clone());
        let (run, item) = self
            .0
            .store
            .enqueue_run(
                Run {
                    id,
                    thread_id: thread.id,
                    ordinal: 0,
                    provider: selection.provider,
                    model: selection.model.clone(),
                    effort: selection.effort.clone(),
                    status: RunStatus::Queued,
                    prompt: message.prompt,
                    provider_session_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    duration_ms: None,
                    error_message: None,
                    started_at: Utc::now(),
                    completed_at: None,
                },
                item,
                QueuedRunInput {
                    model_selection: selection,
                    attachment_ids: message.attachment_ids,
                    notify_parent: meta.notify_parent,
                    wake_depth: meta.wake_depth,
                    mode_ceiling: meta.mode_ceiling,
                    verify_fix_depth: meta.verify_fix_depth,
                    last_verify_signature: meta.last_verify_signature,
                    fallback_attempts: meta.fallback_attempts,
                },
            )
            .await?;
        if busy {
            self.emit(WsServerMessage::RunStatusChanged {
                thread_id: thread.id,
                run_id: run.id,
                status: RunStatus::Queued,
                error: None,
            });
        }
        self.emit(WsServerMessage::TurnItemAdded {
            thread_id: thread.id,
            item,
        });
        Ok(id)
    }

    pub async fn resume_queued_runs(&self) -> CoreResult<()> {
        let mut active = self.0.active.lock().await;
        self.ensure_accepting_runs()?;
        for id in self.0.store.queued_threads().await? {
            if !active.contains_key(&id) {
                let thread = self.0.store.get_thread(id).await?;
                if let Some((_, turn)) = self.start_next(&mut active, &thread, None).await? {
                    self.spawn(turn);
                }
            }
        }
        Ok(())
    }

    async fn start_next(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        thread: &Thread,
        submitted_id: Option<Uuid>,
    ) -> CoreResult<Option<(RunTicket, RunningTurn)>> {
        if self.0.shutting_down.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let store = &self.0.store;
        while let Some(queued) = store.next_queued_run(thread.id).await? {
            let run_id = queued.id;
            match self.start(active, thread.clone(), queued).await {
                Ok(started) => return Ok(Some(started)),
                Err(error) => {
                    let message =
                        format_run_error(FailureClass::HarnessError, &error.to_string(), None);
                    store
                        .finish_run(
                            run_id,
                            RunStatus::Failed,
                            None,
                            None,
                            None,
                            0,
                            Some(message.clone()),
                        )
                        .await?;
                    self.emit(WsServerMessage::RunStatusChanged {
                        thread_id: thread.id,
                        run_id,
                        status: RunStatus::Failed,
                        error: Some(message),
                    });
                    if submitted_id == Some(run_id) {
                        return Err(error);
                    }
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn driver(&self, model: &ModelSelection) -> CoreResult<Arc<dyn ProviderDriver>> {
        if model.model.trim().is_empty() {
            return Err(CoreError::InvalidArgument("model is required".into()));
        }
        self.0.providers.get(model.provider).ok_or_else(|| {
            CoreError::InvalidArgument(format!("provider {} is disabled", model.provider.as_str()))
        })
    }

    async fn start(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        mut thread: Thread,
        queued: QueuedRun,
    ) -> CoreResult<(RunTicket, RunningTurn)> {
        self.ensure_accepting_runs()?;
        if thread.archived {
            return Err(CoreError::InvalidArgument("thread is archived".into()));
        }
        if let Some((runtime, interaction)) = queued.input.mode_ceiling {
            thread.runtime_mode = thread.runtime_mode.min(runtime);
            if interaction == InteractionMode::Plan {
                thread.interaction_mode = InteractionMode::Plan;
            }
        }
        let store = &self.0.store;
        let run_id = queued.id;
        let selection = queued.input.model_selection;
        let driver = self.driver(&selection)?;
        let previous_run = store.previous_run(thread.id, run_id).await?;
        let resume_session_id = previous_run
            .as_ref()
            .filter(|run| run.provider == selection.provider && run.model == selection.model)
            .and_then(|run| run.provider_session_id.clone());
        let project = store.get_project(thread.project_id).await?;
        let original_prompt = queued.prompt.clone();
        let mut prompt = queued.prompt;
        if previous_run.is_some() && resume_session_id.is_none() {
            let handoff = self.handoff_summary(&thread).await?;
            prompt =
                format!("Context from the previous run:\n{handoff}\n\nCurrent request:\n{prompt}");
        } else if previous_run.is_none()
            && let Some(handoff) = store
                .list_handoffs(thread.id)
                .await?
                .into_iter()
                .find(|handoff| handoff.target_thread_id == thread.id)
        {
            prompt = format!(
                "Context from the previous thread:\n{}\n\nCurrent request:\n{prompt}",
                handoff.summary
            );
        }
        if previous_run.is_none()
            && let Some(instructions) = project
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            prompt = format!("Project instructions:\n{instructions}\n\n{prompt}");
        }
        let workspace_dir = self.workspace(&thread).await?;
        let attachment_ids = queued.input.attachment_ids.clone();
        let attachments = self.attachments(thread.id, &attachment_ids).await?;
        let swarm = self
            .get_board(thread.id)
            .await?
            .map(|board| SwarmTurn::prepare(&board, thread.id, selection.provider, &mut prompt))
            .transpose()?;
        let live = self.0.settings.get();
        let enable_mcp = if selection.provider == ProviderId::Antigravity {
            live.antigravity_mcp_enabled
        } else {
            driver.supports_mcp()
        };
        let (mcp_token, mcp_injection) = if enable_mcp {
            let token = self.0.tokens.register(ActiveMcpToken {
                thread_id: thread.id,
                run_id,
                runtime_mode: thread.runtime_mode,
                interaction_mode: thread.interaction_mode,
            })?;
            let mut injection = self.0.injection.clone();
            injection.thread_id = thread.id;
            injection.internal_token = token.token.clone();
            (Some(token), Some(injection))
        } else {
            (None, None)
        };
        let request = TurnRequest {
            thread_id: thread.id,
            run_id,
            workspace_dir: workspace_dir.clone(),
            prompt,
            model_selection: selection.clone(),
            runtime_mode: thread.runtime_mode,
            interaction_mode: thread.interaction_mode,
            resume_session_id: resume_session_id.clone(),
            attachments,
            mcp_injection,
        };
        let run_started_at = Utc::now();
        let run = store
            .start_queued_run(run_id, selection.clone(), resume_session_id)
            .await?;
        let (cancel, cancellation) = watch::channel(false);
        let (done, completion) = watch::channel(false);
        active.insert(
            thread.id,
            ActiveRun {
                project_id: thread.project_id,
                id: run.id,
                model: selection.clone(),
                driver: driver.clone(),
                cancel,
                done: completion.clone(),
                wake_depth: queued.input.wake_depth,
                runtime_mode: thread.runtime_mode,
                interaction_mode: thread.interaction_mode,
                pending: HashMap::new(),
            },
        );
        self.emit(WsServerMessage::RunStarted {
            thread_id: thread.id,
            run,
        });
        self.emit(WsServerMessage::ThreadStatusChanged {
            thread_id: thread.id,
            status: ThreadStatus::Running,
        });
        if swarm.is_some()
            && let Err(error) = self.swarm_state(thread.id, "working").await
        {
            tracing::error!(%error, %run_id, "Could not persist swarm member state");
        }
        let ticket = RunTicket {
            id: run_id,
            done: completion,
        };
        let turn = RunningTurn {
            thread,
            request,
            original_prompt,
            attachment_ids,
            cancellation,
            done,
            chained_dones: Vec::new(),
            driver,
            notify_parent: queued.input.notify_parent,
            wake_depth: queued.input.wake_depth,
            mode_ceiling: queued.input.mode_ceiling,
            verify_fix_depth: queued.input.verify_fix_depth,
            last_verify_signature: queued.input.last_verify_signature,
            fallback_attempts: queued.input.fallback_attempts,
            mcp_token,
            swarm,
            started_at: run_started_at,
            workspace_dir,
            verify_command: project.verify_command,
        };
        Ok((ticket, turn))
    }

    fn spawn(&self, turn: RunningTurn) {
        let coordinator = self.clone();
        tokio::spawn(async move {
            let mut next = Some(turn);
            while let Some(turn) = next {
                next = coordinator.execute(turn).await;
            }
        });
    }

    pub async fn interrupt(&self, id: Uuid) -> CoreResult<()> {
        let done = {
            let mut active = self.0.active.lock().await;
            self.0.store.get_thread(id).await?;
            self.interrupt_locked(&mut active, id).await?
        };
        if let Some(mut done) = done {
            let _ = done.wait_for(|completed| *completed).await;
        }
        Ok(())
    }

    pub(crate) async fn interrupt_locked(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        id: Uuid,
    ) -> CoreResult<Option<watch::Receiver<bool>>> {
        let done = active.get_mut(&id).map(|run| self.cancel(run));
        for run_id in self.0.store.cancel_queued_runs(id).await? {
            self.emit(WsServerMessage::RunStatusChanged {
                thread_id: id,
                run_id,
                status: RunStatus::Cancelled,
                error: Some("Interrupted".into()),
            });
        }
        Ok(done)
    }

    fn cancel(&self, run: &mut ActiveRun) -> watch::Receiver<bool> {
        self.0.tokens.revoke_run(run.id);
        run.cancel.send_replace(true);
        run.done.clone()
    }

    pub(crate) fn ensure_accepting_runs(&self) -> CoreResult<()> {
        if self.0.shutting_down.load(Ordering::Relaxed) {
            return Err(CoreError::Provider("server is shutting down".into()));
        }
        Ok(())
    }

    pub async fn interrupt_all(&self) {
        let completions = {
            let mut active = self.0.active.lock().await;
            self.0.shutting_down.store(true, Ordering::Relaxed);
            active
                .values_mut()
                .map(|run| self.cancel(run))
                .collect::<Vec<_>>()
        };
        for mut done in completions {
            let _ = done.wait_for(|completed| *completed).await;
        }
    }

    pub async fn respond_request(&self, id: Uuid, response: PendingResponse) -> CoreResult<()> {
        let mut active = self.0.active.lock().await;
        let run = active
            .get_mut(&id)
            .filter(|run| run.pending.contains_key(&response.request_id))
            .ok_or_else(|| CoreError::NotFound("pending request".into()))?;
        let request_id = response.request_id;
        if !run.driver.respond_request(run.id, response).await? {
            return Err(CoreError::Provider(
                "provider did not accept the response".into(),
            ));
        }
        run.pending.remove(&request_id);
        Ok(())
    }

    async fn execute(&self, turn: RunningTurn) -> Option<RunningTurn> {
        let RunningTurn {
            thread,
            request,
            original_prompt,
            attachment_ids,
            mut cancellation,
            done,
            mut chained_dones,
            driver,
            notify_parent,
            wake_depth,
            mode_ceiling,
            verify_fix_depth,
            last_verify_signature,
            fallback_attempts,
            mcp_token,
            swarm,
            started_at: run_started_at,
            workspace_dir,
            verify_command,
        } = turn;
        let started = Instant::now();
        let thread_id = thread.id;
        let run_id = request.run_id;
        let current_selection = request.model_selection.clone();

        // Capture pre-run Git tree snapshot outside the global `active` lock (Astra B1)
        // so slow git subprocesses never block unrelated threads.
        let pre_tree = if thread.interaction_mode != InteractionMode::Plan {
            match GitManager::snapshot_tree(&workspace_dir).await {
                Ok(Some(tree)) => {
                    if let Err(error) = self
                        .0
                        .store
                        .save_run_checkpoint(run_id, thread_id, tree.clone())
                        .await
                    {
                        tracing::debug!(%error, %run_id, "Could not save pre-run git checkpoint");
                    }
                    Some(tree)
                }
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(%error, %run_id, "Could not snapshot workspace before run");
                    None
                }
            }
        } else {
            None
        };

        let mut outcome = Outcome {
            provider: request.model_selection.provider,
            model: request.model_selection.model.clone(),
            session: request.resume_session_id.clone(),
            input: None,
            output: None,
            has_context_usage: false,
            had_file_changes: false,
            had_side_effects: false,
            hard_rate_limited: false,
            reset_at: None,
            failure_class: None,
            assistant: String::new(),
            reasoning: String::new(),
            terminal: None,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = {
            let live = self.0.settings.get();
            let worker = run_turn_with_retry(
                driver.clone(),
                request,
                tx,
                live.auto_resume_limited_threads,
                live.rate_limit_max_retries,
            );
            tokio::pin!(worker);
            loop {
                tokio::select! {
                    biased;
                    _ = async { let _ = cancellation.wait_for(|cancelled| *cancelled).await; } => {
                        let interrupt = driver.interrupt(run_id);
                        tokio::pin!(interrupt);
                        let result = tokio::select! {
                            result = &mut interrupt => result,
                            _ = &mut worker => interrupt.await,
                        };
                        if let Err(error) = result {
                            tracing::warn!(%error, %run_id, "Provider interrupt failed");
                        }
                        break Ok(());
                    }
                    Some(event) = rx.recv() => {
                        if let Err(error) = self.event(thread_id, run_id, event, &mut outcome).await {
                            let _ = driver.interrupt(run_id).await;
                            break Err(error);
                        }
                    }
                    result = &mut worker => {
                        let mut result = result;
                        while let Ok(event) = rx.try_recv() {
                            if let Err(error) = self.event(thread_id, run_id, event, &mut outcome).await {
                                result = Err(error);
                                break;
                            }
                        }
                        break result;
                    }
                }
            }
        };
        drop(mcp_token);

        let mut verify_failure: Option<(String, String)> = None;
        let mut verify_cancelled = false;
        let mut verify_failed_error: Option<String> = None;
        if !*cancellation.borrow()
            && result.is_ok()
            && matches!(outcome.terminal, Some((RunStatus::Completed, _)))
            && thread.interaction_mode != InteractionMode::Plan
            && let Some(cmd) = verify_command
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            let post_tree = GitManager::snapshot_tree(&workspace_dir)
                .await
                .ok()
                .flatten();
            let tree_changed = pre_tree.is_some() && post_tree != pre_tree;
            if outcome.had_file_changes || verify_fix_depth > 0 || tree_changed {
                match crate::shell::run_shell_cancellable(
                    &workspace_dir,
                    crate::shell::ShellExecRequest {
                        command: cmd.to_owned(),
                        timeout_secs: Some(60),
                    },
                    cancellation.clone(),
                )
                .await
                {
                    Ok(out) => {
                        let mut combined = String::new();
                        if !out.stdout.is_empty() {
                            combined.push_str(&out.stdout);
                        }
                        if !out.stderr.is_empty() {
                            if !combined.is_empty() && !combined.ends_with('\n') {
                                combined.push('\n');
                            }
                            combined.push_str(&out.stderr);
                        }
                        if out.cancelled {
                            if !combined.is_empty() && !combined.ends_with('\n') {
                                combined.push('\n');
                            }
                            combined.push_str("[キャンセルされました]");
                        } else if out.timed_out {
                            if !combined.is_empty() && !combined.ends_with('\n') {
                                combined.push('\n');
                            }
                            combined.push_str("[タイムアウトにより強制終了しました]");
                        }
                        let summary = out.command.chars().take(240).collect::<String>();
                        let _ = self
                            .append(NewTurnItem {
                                thread_id,
                                run_id,
                                kind: TurnItemKind::CommandExecution,
                                summary: Some(summary),
                                payload: json!({
                                    "command": out.command,
                                    "cwd": out.cwd,
                                    "exit_code": out.exit_code,
                                    "stdout": out.stdout,
                                    "stderr": out.stderr,
                                    "output": combined,
                                    "duration_ms": out.duration_ms,
                                    "timed_out": out.timed_out,
                                    "cancelled": out.cancelled,
                                    "truncated": out.truncated,
                                    "source": "verify_hook",
                                }),
                                searchable_text: Some(format!("{}\n{}", out.command, combined)),
                            })
                            .await;
                        if out.cancelled {
                            verify_cancelled = true;
                        } else if out.timed_out || out.exit_code != Some(0) {
                            let tail = redact_and_tail(&combined, 4_000);
                            let sig_input = format!(
                                "{:?}|{}|{}",
                                out.exit_code,
                                post_tree.as_deref().unwrap_or(""),
                                tail
                            );
                            let sig = blake3::hash(sig_input.as_bytes()).to_hex().to_string();
                            let prompt = format!(
                                "The project verify command (`{}`) failed (exit_code={:?}, timed_out={}). Fix the reported issues so the verify command passes cleanly.\n\nDiagnostic output (redacted tail):\n```\n{}\n```",
                                out.command, out.exit_code, out.timed_out, tail
                            );
                            verify_failure = Some((prompt, sig));
                            verify_failed_error = Some(if out.timed_out {
                                format!("verify_command timed out: {}", out.command)
                            } else {
                                format!(
                                    "verify_command failed (exit {:?}): {}",
                                    out.exit_code, out.command
                                )
                            });
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, %run_id, "Post-turn verify_command failed to launch");
                    }
                }
            }
        }

        let mut active = self.0.active.lock().await;
        let cancelled = *cancellation.borrow() || verify_cancelled;
        let mut error_class = None;
        let (mut status, mut error) = if cancelled {
            (RunStatus::Cancelled, Some("Interrupted".into()))
        } else if let Err(error) = result {
            let message = error.to_string();
            error_class = Some(classify_harness_failure(
                current_selection.provider,
                &message,
                None,
            ));
            (RunStatus::Failed, Some(message))
        } else {
            match outcome.terminal.take() {
                Some(terminal) => {
                    error_class = outcome.failure_class;
                    terminal
                }
                None => {
                    error_class = Some(FailureClass::HarnessError);
                    (
                        RunStatus::Failed,
                        Some("provider ended without a completion event".into()),
                    )
                }
            }
        };

        let mut scheduled_auto_followup: Option<(Message, QueuedMeta)> = None;
        let mut verify_passed = true;
        if status == RunStatus::Completed
            && let Some((fix_prompt, sig)) = verify_failure
        {
            verify_passed = false;
            let max_fixes = self.0.settings.get().verify_max_auto_fixes.min(10) as u8;
            let made_progress = last_verify_signature.as_deref() != Some(sig.as_str());
            if verify_fix_depth < max_fixes && made_progress {
                let next_depth = verify_fix_depth + 1;
                let notice = format!(
                    "verify_command が失敗したため自動修正を試行します ({next_depth}/{max_fixes})"
                );
                let _ = self
                    .append(NewTurnItem {
                        thread_id,
                        run_id,
                        kind: TurnItemKind::SystemNotice,
                        summary: Some(notice.clone()),
                        payload: json!({
                            "message": notice,
                            "verify_fix_depth": next_depth,
                            "verify_max_auto_fixes": max_fixes,
                        }),
                        searchable_text: None,
                    })
                    .await;
                scheduled_auto_followup = Some((
                    Message {
                        prompt: fix_prompt,
                        behavior: None,
                        attachment_ids: Vec::new(),
                        model_selection: Some(current_selection.clone()),
                    },
                    QueuedMeta {
                        notify_parent,
                        wake_depth,
                        mode_ceiling: Some(
                            mode_ceiling.unwrap_or((thread.runtime_mode, thread.interaction_mode)),
                        ),
                        verify_fix_depth: next_depth,
                        last_verify_signature: Some(sig),
                        fallback_attempts,
                    },
                ));
            } else {
                status = RunStatus::Failed;
                error = verify_failed_error.or_else(|| Some("verify_command failed".into()));
                error_class = Some(FailureClass::HarnessError);
            }
        } else if status == RunStatus::RateLimited
            && outcome.hard_rate_limited
            && !outcome.had_side_effects
            && let Some((fallback_model, next_fallback_idx)) =
                self.pick_fallback_model(&current_selection, fallback_attempts)
        {
            let notice = format!(
                "クォータ上限を検知したためフォールバックモデル {}/{} へ自動切替します",
                fallback_model.provider.as_str(),
                fallback_model.model
            );
            let _ = self
                .append(NewTurnItem {
                    thread_id,
                    run_id,
                    kind: TurnItemKind::SystemNotice,
                    summary: Some(notice.clone()),
                    payload: json!({
                        "message": notice,
                        "fallback_provider": fallback_model.provider,
                        "fallback_model": fallback_model.model,
                    }),
                    searchable_text: None,
                })
                .await;
            scheduled_auto_followup = Some((
                Message {
                    prompt: original_prompt,
                    behavior: None,
                    attachment_ids,
                    model_selection: Some(fallback_model),
                },
                QueuedMeta {
                    notify_parent,
                    wake_depth,
                    mode_ceiling: Some(
                        mode_ceiling.unwrap_or((thread.runtime_mode, thread.interaction_mode)),
                    ),
                    verify_fix_depth,
                    last_verify_signature,
                    fallback_attempts: next_fallback_idx,
                },
            ));
        }

        if status == RunStatus::Completed
            && verify_passed
            && let Some(swarm) = &swarm
            && let Err(failure) = self
                .apply_swarm_actions(&mut active, thread_id, swarm.actions(&outcome.assistant))
                .await
        {
            status = RunStatus::Failed;
            error = Some(failure.to_string());
            error_class = Some(FailureClass::HarnessError);
        }
        let flush = async {
            self.flush_text(
                thread_id,
                run_id,
                TurnItemKind::Reasoning,
                &mut outcome.reasoning,
            )
            .await?;
            self.flush_text(
                thread_id,
                run_id,
                TurnItemKind::AssistantMessage,
                &mut outcome.assistant,
            )
            .await
        }
        .await;
        if let Err(failure) = flush {
            status = RunStatus::Failed;
            error = Some(failure.to_string());
            error_class = Some(FailureClass::HarnessError);
        }
        if matches!(status, RunStatus::Failed | RunStatus::RateLimited)
            && let Some(message) = error.as_deref()
        {
            let class = error_class.unwrap_or_else(|| {
                if status == RunStatus::RateLimited {
                    classify_rate_limit(outcome.provider, message, None)
                } else {
                    classify_provider_failure(outcome.provider, message, None)
                }
            });
            error = Some(format_run_error(
                class,
                message,
                outcome.reset_at.as_deref(),
            ));
        }
        let thread_status = match status {
            RunStatus::Completed => ThreadStatus::Completed,
            RunStatus::Cancelled => ThreadStatus::Interrupted,
            RunStatus::RateLimited => ThreadStatus::RateLimited,
            _ => ThreadStatus::Failed,
        };
        let store = &self.0.store;
        if let Err(error) = store
            .finish_run(
                run_id,
                status,
                outcome.session.clone(),
                outcome.input,
                outcome.output,
                started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                error.clone(),
            )
            .await
        {
            tracing::error!(%error, %run_id, "Could not persist run completion");
        }
        if let Some(mut snap) = self.live_context(thread_id).await
            && snap.run_id == run_id
        {
            if snap.session_id.is_none() {
                snap.session_id = outcome.session;
                self.record_live_context(thread_id, snap.clone()).await;
            }
            self.persist_context(thread_id, &snap).await;
        }
        let mut enqueued_followup = false;
        if let Some((followup_msg, followup_meta)) = scheduled_auto_followup {
            match self
                .enqueue_with_meta(&thread, followup_msg, followup_meta, true)
                .await
            {
                Ok(_) => {
                    enqueued_followup = true;
                }
                Err(error) => {
                    tracing::error!(%error, %thread_id, "Could not enqueue automatic follow-up run");
                }
            }
        }
        let defer_completion = enqueued_followup && !self.0.shutting_down.load(Ordering::Relaxed);
        if notify_parent && !defer_completion {
            let child_error = error
                .as_deref()
                .map(|error| format_delegated_error(outcome.provider, &outcome.model, error));
            if let Err(error) = self
                .record_child_completion(&thread, thread_status, child_error.as_deref())
                .await
            {
                tracing::error!(%error, %run_id, "Could not record delegated task completion");
            }
        }
        if let Err(error) = store.update_thread_status(thread_id, thread_status).await {
            tracing::error!(%error, %run_id, "Could not persist thread status");
        }
        if !defer_completion
            && (swarm.is_some() || thread.relationship.as_deref() == Some("swarm-worker"))
            && let Err(error) = self.swarm_state(thread_id, "waiting").await
        {
            tracing::error!(%error, %run_id, "Could not persist swarm member state");
        }
        if !defer_completion
            && (swarm.is_some() || thread.relationship.as_deref() == Some("swarm-worker"))
            && !(thread.model_selection.provider == ProviderId::Mock
                && status == RunStatus::Completed)
            && let Err(error) = self
                .notify_worker_ended_silently(
                    &mut active,
                    thread_id,
                    run_started_at,
                    status,
                    error.as_deref(),
                )
                .await
        {
            tracing::error!(%error, %run_id, "Could not notify coordinator of silent worker exit");
        }
        self.0.live_streams.lock().await.remove(&thread_id);
        self.emit(WsServerMessage::RunStatusChanged {
            thread_id,
            run_id,
            status,
            error,
        });
        self.emit(WsServerMessage::ThreadStatusChanged {
            thread_id,
            status: thread_status,
        });
        if notify_parent
            && !defer_completion
            && let Some(parent_thread_id) = thread.parent_thread_id
        {
            self.emit(WsServerMessage::SubagentUpdated {
                parent_thread_id,
                child_thread_id: thread_id,
                status: thread_status,
            });
        }
        active.remove(&thread_id);
        let next = async {
            let thread = store.get_thread(thread_id).await?;
            self.start_next(&mut active, &thread, None).await
        }
        .await;
        match next {
            Ok(Some((_, mut next_turn))) => {
                if defer_completion {
                    chained_dones.push(done);
                    next_turn.chained_dones.extend(chained_dones);
                } else {
                    done.send_replace(true);
                    for waiter in chained_dones {
                        waiter.send_replace(true);
                    }
                }
                Some(next_turn)
            }
            Ok(None) => {
                done.send_replace(true);
                for waiter in chained_dones {
                    waiter.send_replace(true);
                }
                None
            }
            Err(error) => {
                done.send_replace(true);
                for waiter in chained_dones {
                    waiter.send_replace(true);
                }
                tracing::error!(%error, %thread_id, "Could not start next queued run");
                self.emit(WsServerMessage::Error {
                    message: error.to_string(),
                });
                None
            }
        }
    }

    async fn event(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        event: StreamEvent,
        out: &mut Outcome,
    ) -> CoreResult<()> {
        match event {
            StreamEvent::SessionStarted {
                provider_session_id,
            } => {
                if out
                    .terminal
                    .as_ref()
                    .is_some_and(|(status, _)| *status == RunStatus::RateLimited)
                {
                    out.terminal = None;
                    self.thread_status(thread_id, ThreadStatus::Running).await?;
                }
                let active = self.0.active.lock().await;
                let current = self.0.store.get_thread(thread_id).await?.model_selection;
                if active.get(&thread_id).is_some_and(|run| {
                    run.model.provider == current.provider && run.model.model == current.model
                }) {
                    self.0
                        .store
                        .update_thread_session_id(thread_id, provider_session_id.clone())
                        .await?;
                }
                out.session = Some(provider_session_id);
            }
            StreamEvent::AssistantDelta { text } => {
                if !text.trim().is_empty() {
                    out.had_side_effects = true;
                }
                push_capped(&mut out.assistant, &text, out.reasoning.len());
                self.update_live_stream(thread_id, run_id, &out.assistant, &out.reasoning)
                    .await;
                self.emit(WsServerMessage::Delta {
                    thread_id,
                    run_id,
                    channel: "assistant".into(),
                    text,
                });
            }
            StreamEvent::ReasoningDelta { text } => {
                push_capped(&mut out.reasoning, &text, out.assistant.len());
                self.update_live_stream(thread_id, run_id, &out.assistant, &out.reasoning)
                    .await;
                self.emit(WsServerMessage::Delta {
                    thread_id,
                    run_id,
                    channel: "reasoning".into(),
                    text,
                });
            }
            StreamEvent::Item {
                kind,
                summary,
                payload,
            } => {
                if kind == TurnItemKind::FileChange {
                    out.had_file_changes = true;
                }
                if matches!(
                    kind,
                    TurnItemKind::FileChange
                        | TurnItemKind::ToolCall
                        | TurnItemKind::CommandExecution
                        | TurnItemKind::AssistantMessage
                ) {
                    out.had_side_effects = true;
                }
                if kind == TurnItemKind::Reasoning {
                    out.reasoning.clear();
                } else {
                    self.flush_text(
                        thread_id,
                        run_id,
                        TurnItemKind::Reasoning,
                        &mut out.reasoning,
                    )
                    .await?;
                }
                if kind == TurnItemKind::AssistantMessage {
                    out.assistant.clear();
                } else {
                    self.flush_text(
                        thread_id,
                        run_id,
                        TurnItemKind::AssistantMessage,
                        &mut out.assistant,
                    )
                    .await?;
                }
                self.update_live_stream(thread_id, run_id, &out.assistant, &out.reasoning)
                    .await;
                let searchable = (kind == TurnItemKind::AssistantMessage
                    || kind == TurnItemKind::Reasoning)
                    .then(|| text_value(&payload).map(str::to_owned))
                    .flatten();
                self.append(NewTurnItem {
                    thread_id,
                    run_id,
                    kind,
                    summary,
                    payload,
                    searchable_text: searchable,
                })
                .await?;
            }
            StreamEvent::PendingApproval { mut request } => {
                request.thread_id = thread_id;
                request.run_id = run_id;
                if let Some(run) = self.0.active.lock().await.get_mut(&thread_id) {
                    run.pending.insert(request.id, request.clone());
                }
                self.emit(WsServerMessage::PendingRequest { request });
            }
            StreamEvent::RateLimited {
                retry_after_secs,
                message,
                reset_at,
                error_code,
            } => {
                out.reset_at = reset_at.clone();
                let class = classify_rate_limit(out.provider, &message, error_code.as_deref());
                out.failure_class = Some(class);
                out.hard_rate_limited = class == FailureClass::UsageLimit;
                if let Some(provider) = self
                    .0
                    .active
                    .lock()
                    .await
                    .get(&thread_id)
                    .map(|run| run.model.provider)
                {
                    self.0
                        .limits
                        .record_runtime_limit(provider, Some(retry_after_secs), &message);
                }
                self.append(NewTurnItem {
                    thread_id,
                    run_id,
                    kind: TurnItemKind::SystemNotice,
                    summary: Some(compact(&message, 240)),
                    payload: json!({"message": message, "retry_after_secs": retry_after_secs}),
                    searchable_text: None,
                })
                .await?;
                self.thread_status(thread_id, ThreadStatus::RateLimited)
                    .await?;
                self.emit(WsServerMessage::RunStatusChanged {
                    thread_id,
                    run_id,
                    status: RunStatus::RateLimited,
                    error: Some(message.clone()),
                });
                out.terminal = Some((RunStatus::RateLimited, Some(message)));
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                out.input = Some(out.input.unwrap_or(0).saturating_add(input_tokens));
                out.output = Some(out.output.unwrap_or(0).saturating_add(output_tokens));
                // Codex turn usage sums every request in the turn; its context size comes from
                // the session file instead (see `build_thread_context`).
                if !out.has_context_usage && out.provider != ProviderId::Codex {
                    let context_tokens = input_tokens.saturating_add(output_tokens);
                    if context_tokens > 0 {
                        let snap = LiveContextUsage {
                            run_id,
                            provider: out.provider,
                            model: out.model.clone(),
                            session_id: out.session.clone(),
                            context_tokens,
                            input_tokens,
                            output_tokens,
                            cache_read_tokens: 0,
                            cache_write_tokens: 0,
                        };
                        self.record_live_context(thread_id, snap).await;
                        self.emit(WsServerMessage::RunUsageUpdated {
                            thread_id,
                            run_id,
                            context_tokens,
                            input_tokens,
                            output_tokens,
                            cache_read_tokens: 0,
                            cache_write_tokens: 0,
                        });
                    }
                }
            }
            StreamEvent::ContextUsage {
                context_tokens,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => {
                out.has_context_usage = true;
                let snap = LiveContextUsage {
                    run_id,
                    provider: out.provider,
                    model: out.model.clone(),
                    session_id: out.session.clone(),
                    context_tokens,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                };
                self.record_live_context(thread_id, snap).await;
                self.emit(WsServerMessage::RunUsageUpdated {
                    thread_id,
                    run_id,
                    context_tokens,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                });
            }
            StreamEvent::Completed { final_text } => {
                if out.assistant.is_empty()
                    && let Some(text) = final_text
                {
                    let latest = self.recent_items(thread_id, 20).await?;
                    if !latest.iter().any(|item| {
                        item.run_id == run_id
                            && item.kind == TurnItemKind::AssistantMessage
                            && text_value(&item.payload) == Some(&text)
                    }) {
                        push_capped(&mut out.assistant, &text, out.reasoning.len());
                    }
                }
                out.terminal = Some((RunStatus::Completed, None));
            }
            StreamEvent::Failed {
                message,
                reset_at,
                error_code,
            } => {
                out.reset_at = reset_at;
                out.failure_class = Some(classify_provider_failure(
                    out.provider,
                    &message,
                    error_code.as_deref(),
                ));
                out.terminal = Some((RunStatus::Failed, Some(message)));
            }
        }
        Ok(())
    }

    async fn thread_status(&self, thread_id: Uuid, status: ThreadStatus) -> CoreResult<()> {
        self.0.store.update_thread_status(thread_id, status).await?;
        self.emit(WsServerMessage::ThreadStatusChanged { thread_id, status });
        Ok(())
    }

    pub(crate) async fn append(&self, new: NewTurnItem) -> CoreResult<TurnItem> {
        let item = self.0.store.append_turn_item(new).await?;
        self.emit(WsServerMessage::TurnItemAdded {
            thread_id: item.thread_id,
            item: item.clone(),
        });
        Ok(item)
    }

    async fn flush_text(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        kind: TurnItemKind,
        buf: &mut String,
    ) -> CoreResult<()> {
        if !buf.is_empty() {
            let text = std::mem::take(buf);
            self.append(NewTurnItem {
                thread_id,
                run_id,
                kind,
                summary: Some(compact(&text, 240)),
                payload: json!({"text": text}),
                searchable_text: Some(text),
            })
            .await?;
        }
        Ok(())
    }
    async fn update_live_stream(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        assistant: &str,
        reasoning: &str,
    ) {
        const MAX_PREVIEW: usize = 200_000;
        let tail = |s: &str| {
            if s.len() <= MAX_PREVIEW {
                s.to_owned()
            } else {
                let start = s.ceil_char_boundary(s.len() - MAX_PREVIEW);
                s[start..].to_owned()
            }
        };
        self.0.live_streams.lock().await.insert(
            thread_id,
            LiveStreamSnapshot {
                run_id,
                assistant: tail(assistant),
                reasoning: tail(reasoning),
            },
        );
    }
    fn pick_fallback_model(
        &self,
        current: &ModelSelection,
        start_index: u8,
    ) -> Option<(ModelSelection, u8)> {
        self.0
            .settings
            .get()
            .fallback_models
            .into_iter()
            .enumerate()
            .skip(start_index as usize)
            .find(|(_, m)| {
                !(m.provider == current.provider && m.model == current.model)
                    && self.0.providers.get(m.provider).is_some()
            })
            .map(|(idx, m)| (m, (idx + 1).min(u8::MAX as usize) as u8))
    }
}

struct QueuedMeta {
    notify_parent: bool,
    wake_depth: u8,
    mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
    verify_fix_depth: u8,
    last_verify_signature: Option<String>,
    fallback_attempts: u8,
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
