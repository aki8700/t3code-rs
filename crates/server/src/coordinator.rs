use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use t3_core::*;
use t3_git::GitManager;
use t3_providers::{ProviderRegistry, run_turn_with_retry};
use t3_store::SqliteStore;
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
}

pub(crate) struct ActiveRun {
    pub(crate) project_id: Uuid,
    pub(crate) id: Uuid,
    model: ModelSelection,
    driver: Arc<dyn ProviderDriver>,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
    queue: VecDeque<Submission>,
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

struct Submission {
    id: Uuid,
    message: Message,
    attachments: Vec<Attachment>,
    notify_parent: bool,
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
        let (events, _) = broadcast::channel(512);
        Self(Arc::new(Coordinator {
            git: GitManager::new(cfg.worktrees_dir()),
            providers: ProviderRegistry::from_config(&cfg),
            cfg,
            store,
            injection,
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
        self.ensure_accepting_runs()?;
        let thread = self.0.store.get_thread(id).await?;
        let mut submission = self.submission(&thread, message, false).await?;
        if let Some(run) = active.get_mut(&id) {
            let message = &submission.message;
            if message.behavior.unwrap_or(self.0.cfg.follow_up_behavior) == FollowUpBehavior::Steer
                && submission.attachments.is_empty()
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
                submission.id = run.id;
                self.persist_prompt(id, &submission).await?;
                return Ok(run.id);
            }
            let run_id = submission.id;
            run.queue.push_back(submission);
            return Ok(run_id);
        }
        let (ticket, turn) = self.start(&mut active, thread, submission).await?;
        self.spawn(turn);
        Ok(ticket.id)
    }

    pub(crate) async fn start_delegated(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        thread: Thread,
        message: Message,
    ) -> CoreResult<RunTicket> {
        let submission = self.submission(&thread, message, true).await?;
        let (ticket, turn) = self.start(active, thread, submission).await?;
        self.spawn(turn);
        Ok(ticket)
    }

    async fn submission(
        &self,
        thread: &Thread,
        message: Message,
        notify_parent: bool,
    ) -> CoreResult<Submission> {
        if thread.archived {
            return Err(CoreError::InvalidArgument("thread is archived".into()));
        }
        if message.prompt.trim().is_empty() && message.attachment_ids.is_empty() {
            return Err(CoreError::InvalidArgument(
                "prompt or attachment is required".into(),
            ));
        }
        if let Some(model) = &message.model_selection {
            self.driver(model)?;
        }
        let mut attachments = Vec::new();
        for &id in &message.attachment_ids {
            let attachment = self.0.store.get_attachment(id).await?;
            if attachment.thread_id.is_some_and(|owner| owner != thread.id) {
                return Err(CoreError::InvalidArgument(
                    "attachment belongs to another thread".into(),
                ));
            }
            attachments.push(attachment);
        }
        Ok(Submission {
            id: Uuid::new_v4(),
            message,
            attachments,
            notify_parent,
        })
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
        thread: Thread,
        submission: Submission,
    ) -> CoreResult<(RunTicket, RunningTurn)> {
        self.ensure_accepting_runs()?;
        let store = &self.0.store;
        let selection = submission
            .message
            .model_selection
            .as_ref()
            .unwrap_or(&thread.model_selection);
        let driver = self.driver(selection)?;
        let runs = store.list_runs(thread.id).await?;
        let previous_run = runs.iter().rev().find(|run| run.ordinal > 0);
        let resume_session_id = previous_run
            .filter(|run| run.provider == selection.provider && run.model == selection.model)
            .and_then(|run| run.provider_session_id.clone());
        let mut prompt = submission.message.prompt.clone();
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
        let workspace_dir = match &thread.worktree_path {
            Some(path) => path.clone(),
            None => store.get_project(thread.project_id).await?.workspace_root,
        };
        let mut injection = self.0.injection.clone();
        injection.thread_id = thread.id;
        injection.internal_token =
            crate::auth::mcp_thread_token(&injection.internal_token, thread.id);
        let request = TurnRequest {
            thread_id: thread.id,
            run_id: submission.id,
            workspace_dir,
            prompt,
            model_selection: selection.clone(),
            runtime_mode: thread.runtime_mode,
            interaction_mode: thread.interaction_mode,
            resume_session_id: resume_session_id.clone(),
            attachments: submission.attachments.clone(),
            mcp_injection: Some(injection),
        };
        let run = store
            .create_run(Run {
                id: submission.id,
                thread_id: thread.id,
                ordinal: previous_run.map_or(1, |previous| previous.ordinal + 1),
                provider: selection.provider,
                model: selection.model.clone(),
                effort: selection.effort.clone(),
                status: RunStatus::Running,
                prompt: submission.message.prompt.clone(),
                provider_session_id: resume_session_id,
                input_tokens: None,
                output_tokens: None,
                duration_ms: None,
                error_message: None,
                started_at: Utc::now(),
                completed_at: None,
            })
            .await?;
        let setup = async {
            if *selection != thread.model_selection {
                self.select_model(&thread, selection).await?;
            }
            let item = store
                .append_turn_item(prompt_item(thread.id, &submission))
                .await?;
            store
                .update_thread_status(thread.id, ThreadStatus::Running)
                .await?;
            Ok(item)
        }
        .await;
        let item = match setup {
            Ok(item) => item,
            Err(error) => {
                let _ = store
                    .finish_run(
                        run.id,
                        RunStatus::Failed,
                        None,
                        None,
                        None,
                        0,
                        Some("run setup failed".into()),
                    )
                    .await;
                return Err(error);
            }
        };

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
                queue: VecDeque::new(),
                pending: HashMap::new(),
            },
        );
        self.emit(WsServerMessage::RunStarted {
            thread_id: thread.id,
            run,
        });
        self.emit(WsServerMessage::TurnItemAdded {
            thread_id: thread.id,
            item,
        });
        self.emit(WsServerMessage::ThreadStatusChanged {
            thread_id: thread.id,
            status: ThreadStatus::Running,
        });
        let ticket = RunTicket {
            id: submission.id,
            done: completion,
        };
        let turn = RunningTurn {
            thread,
            request,
            cancellation,
            done,
            driver,
            notify_parent: submission.notify_parent,
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

    async fn persist_prompt(&self, id: Uuid, submission: &Submission) -> CoreResult<()> {
        let item = self
            .0
            .store
            .append_turn_item(prompt_item(id, submission))
            .await?;
        self.emit(WsServerMessage::TurnItemAdded {
            thread_id: id,
            item,
        });
        Ok(())
    }

    pub async fn interrupt(&self, id: Uuid) -> CoreResult<()> {
        self.0.store.get_thread(id).await?;
        let mut done = {
            let mut active = self.0.active.lock().await;
            let Some(run) = active.get_mut(&id) else {
                return Ok(());
            };
            run.queue.clear();
            run.cancel.send_replace(true);
            run.done.clone()
        };
        let _ = done.wait_for(|completed| *completed).await;
        Ok(())
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
                .map(|run| {
                    run.queue.clear();
                    run.cancel.send_replace(true);
                    run.done.clone()
                })
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
        let mut queue = active
            .remove(&thread_id)
            .map(|run| run.queue)
            .unwrap_or_default();
        done.send_replace(true);
        while let Some(submission) = queue.pop_front() {
            let submission_id = submission.id;
            match store.get_thread(thread_id).await {
                Ok(thread) => match self.start(&mut active, thread, submission).await {
                    Ok((_, turn)) => {
                        active.get_mut(&thread_id).expect("new active run").queue = queue;
                        return Some(turn);
                    }
                    Err(error) => self.emit(WsServerMessage::RunStatusChanged {
                        thread_id,
                        run_id: submission_id,
                        status: RunStatus::Failed,
                        error: Some(error.to_string()),
                    }),
                },
                Err(error) => {
                    self.emit(WsServerMessage::Error {
                        message: error.to_string(),
                    });
                    break;
                }
            }
        }
        None
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
                out.assistant.push_str(&text);
                self.emit(WsServerMessage::Delta {
                    thread_id,
                    run_id,
                    channel: "assistant".into(),
                    text,
                });
            }
            StreamEvent::ReasoningDelta { text } => {
                out.reasoning.push_str(&text);
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
                        out.assistant = text;
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
}

fn prompt_item(thread_id: Uuid, submission: &Submission) -> NewTurnItem {
    let prompt = &submission.message.prompt;
    NewTurnItem {
        thread_id,
        run_id: submission.id,
        kind: TurnItemKind::UserMessage,
        summary: Some(compact(prompt, 240)),
        payload: json!({"text": prompt, "attachment_ids": submission.message.attachment_ids}),
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
