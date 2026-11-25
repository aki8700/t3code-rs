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
use t3_providers::{ProviderRegistry, run_turn_with_retry};
use t3_store::{QueuedRun, QueuedRunInput, SqliteStore};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use uuid::Uuid;

#[derive(Clone)]
pub struct RunCoordinator(pub(crate) Arc<Coordinator>);

pub(crate) struct Coordinator {
    pub(crate) cfg: ServerConfig,
    pub(crate) store: SqliteStore,
    pub(crate) git: GitManager,
    pub(crate) providers: ProviderRegistry,
    pub(crate) events: broadcast::Sender<WsServerMessage>,
    pub(crate) active: Mutex<HashMap<Uuid, ActiveRun>>,
    shutting_down: AtomicBool,
    injection: McpInjectionSpec,
    tokens: McpTokens,
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

#[derive(Default)]
struct Outcome {
    session: Option<String>,
    input: Option<u64>,
    output: Option<u64>,
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
        let (events, _) = broadcast::channel(512);
        Self(Arc::new(Coordinator {
            git: GitManager::new(cfg.worktrees_dir()),
            providers: ProviderRegistry::from_config(&cfg),
            cfg,
            store,
            injection,
            tokens,
            events,
            active: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        }))
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
            && message.behavior.unwrap_or(self.0.cfg.follow_up_behavior) == FollowUpBehavior::Steer
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
                    notify_parent,
                    wake_depth,
                    mode_ceiling: (wake_depth != 0)
                        .then_some((thread.runtime_mode, thread.interaction_mode)),
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
                    store
                        .finish_run(
                            run_id,
                            RunStatus::Failed,
                            None,
                            None,
                            None,
                            0,
                            Some(error.to_string()),
                        )
                        .await?;
                    self.emit(WsServerMessage::RunStatusChanged {
                        thread_id: thread.id,
                        run_id,
                        status: RunStatus::Failed,
                        error: Some(error.to_string()),
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
        let mut prompt = queued.prompt;
        if previous_run.is_none()
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
        let workspace_dir = self.workspace(&thread).await?;
        let attachments = self
            .attachments(thread.id, &queued.input.attachment_ids)
            .await?;
        let swarm = self
            .get_board(thread.id)
            .await?
            .map(|board| SwarmTurn::prepare(&board, thread.id, selection.provider, &mut prompt))
            .transpose()?;
        let mcp_token = self.0.tokens.register(ActiveMcpToken {
            thread_id: thread.id,
            run_id,
            runtime_mode: thread.runtime_mode,
            interaction_mode: thread.interaction_mode,
        })?;
        let mut injection = self.0.injection.clone();
        injection.thread_id = thread.id;
        injection.internal_token = mcp_token.token.clone();
        let request = TurnRequest {
            thread_id: thread.id,
            run_id,
            workspace_dir,
            prompt,
            model_selection: selection.clone(),
            runtime_mode: thread.runtime_mode,
            interaction_mode: thread.interaction_mode,
            resume_session_id: resume_session_id.clone(),
            attachments,
            mcp_injection: Some(injection),
        };
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
            cancellation,
            done,
            driver,
            notify_parent: queued.input.notify_parent,
            mcp_token,
            swarm,
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
            mut cancellation,
            done,
            driver,
            notify_parent,
            mcp_token,
            swarm,
        } = turn;
        let started = Instant::now();
        let thread_id = thread.id;
        let run_id = request.run_id;
        let mut outcome = Outcome {
            session: request.resume_session_id.clone(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = {
            let worker = run_turn_with_retry(
                driver.clone(),
                request,
                tx,
                self.0.cfg.auto_resume_limited_threads,
                self.0.cfg.rate_limit_max_retries,
                self.0.cfg.rate_limit_initial_backoff_secs,
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

        let mut active = self.0.active.lock().await;
        let cancelled = *cancellation.borrow();
        let (mut status, mut error) = if cancelled {
            (RunStatus::Cancelled, Some("Interrupted".into()))
        } else if let Err(error) = result {
            (RunStatus::Failed, Some(error.to_string()))
        } else {
            match outcome.terminal.take() {
                Some((RunStatus::RateLimited, error)) => (RunStatus::Failed, error),
                Some(terminal) => terminal,
                None => (
                    RunStatus::Failed,
                    Some("provider ended without a completion event".into()),
                ),
            }
        };
        if status == RunStatus::Completed
            && let Some(swarm) = &swarm
            && let Err(failure) = self
                .apply_swarm_actions(&mut active, thread_id, swarm.actions(&outcome.assistant))
                .await
        {
            status = RunStatus::Failed;
            error = Some(failure.to_string());
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
        }
        let thread_status = match status {
            RunStatus::Completed => ThreadStatus::Completed,
            RunStatus::Cancelled => ThreadStatus::Interrupted,
            _ => ThreadStatus::Failed,
        };
        let store = &self.0.store;
        if let Err(error) = store
            .finish_run(
                run_id,
                status,
                outcome.session,
                outcome.input,
                outcome.output,
                started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                error.clone(),
            )
            .await
        {
            tracing::error!(%error, %run_id, "Could not persist run completion");
        }
        if notify_parent
            && let Err(error) = self
                .record_child_completion(&thread, thread_status, error.as_deref())
                .await
        {
            tracing::error!(%error, %run_id, "Could not record delegated task completion");
        }
        if let Err(error) = store.update_thread_status(thread_id, thread_status).await {
            tracing::error!(%error, %run_id, "Could not persist thread status");
        }
        if (swarm.is_some() || thread.relationship.as_deref() == Some("swarm-worker"))
            && let Err(error) = self.swarm_state(thread_id, "waiting").await
        {
            tracing::error!(%error, %run_id, "Could not persist swarm member state");
        }
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
        if notify_parent && let Some(parent_thread_id) = thread.parent_thread_id {
            self.emit(WsServerMessage::SubagentUpdated {
                parent_thread_id,
                child_thread_id: thread_id,
                status: thread_status,
            });
        }
        active.remove(&thread_id);
        done.send_replace(true);
        let next = async {
            let thread = store.get_thread(thread_id).await?;
            self.start_next(&mut active, &thread, None).await
        }
        .await;
        match next {
            Ok(next) => next.map(|(_, turn)| turn),
            Err(error) => {
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
                push_capped(&mut out.assistant, &text, out.reasoning.len());
                self.emit(WsServerMessage::Delta {
                    thread_id,
                    run_id,
                    channel: "assistant".into(),
                    text,
                });
            }
            StreamEvent::ReasoningDelta { text } => {
                push_capped(&mut out.reasoning, &text, out.assistant.len());
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
            } => {
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
            StreamEvent::Failed { message } => {
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
}

struct RunningTurn {
    thread: Thread,
    request: TurnRequest,
    cancellation: watch::Receiver<bool>,
    done: watch::Sender<bool>,
    driver: Arc<dyn ProviderDriver>,
    notify_parent: bool,
    mcp_token: RunMcpToken,
    swarm: Option<SwarmTurn>,
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
