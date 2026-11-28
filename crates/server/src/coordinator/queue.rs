use super::*;

impl RunCoordinator {
    pub(crate) async fn send(
        &self,
        id: Uuid,
        message: Message,
        user_requested: bool,
    ) -> CoreResult<Uuid> {
        let guard = self.lock_thread(id).await;
        let thread = self.0.store.get_thread(id).await?;
        self.send_locked(&guard, thread, message, 0, user_requested)
            .await
    }

    pub(crate) async fn send_locked(
        &self,
        guard: &ThreadGuard,
        thread: Thread,
        message: Message,
        wake_depth: u8,
        user_requested: bool,
    ) -> CoreResult<Uuid> {
        self.ensure_accepting_runs()?;
        self.validate_message(&thread, &message).await?;
        let id = guard.id;
        debug_assert_eq!(id, thread.id);
        let swarm_command = is_swarm_command(&message.prompt);
        if swarm_command {
            self.ensure_board(&thread).await?;
        }
        let (busy, steer) = {
            let active = self.active();
            let run = active.get(&id);
            let steer = run
                .filter(|run| {
                    !swarm_command
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
                })
                .map(|run| (run.id, run.driver.clone()));
            (run.is_some(), steer)
        };
        if let Some((run_id, driver)) = steer
            && driver.send_steer(run_id, message.prompt.clone()).await?
        {
            self.append(prompt_item(id, run_id, &message)).await?;
            return Ok(run_id);
        }
        let run_id = self
            .enqueue(&thread, message, false, wake_depth, busy)
            .await?;
        if user_requested {
            self.0
                .user_runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(run_id);
        }
        if !busy
            && let Some((_, turn)) = self
                .start_next(guard, thread.project_id, Some(run_id))
                .await?
        {
            self.spawn(turn);
        }
        Ok(run_id)
    }

    pub(crate) async fn start_delegated(
        &self,
        guard: &ThreadGuard,
        thread: Thread,
        message: Message,
    ) -> CoreResult<RunTicket> {
        self.ensure_accepting_runs()?;
        self.validate_message(&thread, &message).await?;
        let selection = message
            .model_selection
            .as_ref()
            .unwrap_or(&thread.model_selection);
        if let Some((hold, _)) = self.0.holds.blocking(selection, None) {
            return Err(CoreError::Provider(format_worker_error(
                FailureClass::UsageLimit,
                selection.provider,
                &selection.model,
                Some(&hold.until.to_rfc3339()),
                &format!("provider is held until its limit resets: {}", hold.reason),
            )));
        }
        let id = self.enqueue(&thread, message, true, 0, false).await?;
        let (done, completion) = watch::channel(false);
        self.0
            .queued_tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, done);
        match self.start_next(guard, thread.project_id, Some(id)).await {
            Ok(Some((_, turn))) => self.spawn(turn),
            Ok(None) => {}
            Err(error) => {
                self.complete_queued_ticket(id);
                return Err(error);
            }
        }
        Ok(RunTicket {
            id,
            done: completion,
        })
    }

    pub(super) async fn validate_message(
        &self,
        thread: &Thread,
        message: &Message,
    ) -> CoreResult<()> {
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

    pub(super) async fn attachments(
        &self,
        thread_id: Uuid,
        ids: &[Uuid],
    ) -> CoreResult<Vec<Attachment>> {
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

    pub(super) async fn enqueue(
        &self,
        thread: &Thread,
        message: Message,
        notify_parent: bool,
        wake_depth: u8,
        busy: bool,
    ) -> CoreResult<Uuid> {
        let mode_ceiling = enqueue_mode_ceiling(
            (thread.runtime_mode, thread.interaction_mode),
            &message,
            wake_depth,
        );
        self.enqueue_with_meta(
            thread,
            message,
            QueuedMeta {
                notify_parent,
                wake_depth,
                mode_ceiling,
                verify_fix_depth: 0,
                last_verify_signature: None,
                verify_dead_ends: Vec::new(),
                fallback_attempts: 0,
            },
            busy,
        )
        .await
    }

    pub(super) async fn enqueue_with_meta(
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
                    cache_read_tokens: None,
                    cache_creation_tokens: None,
                    reasoning_tokens: None,
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
                    verify_dead_ends: meta.verify_dead_ends,
                    fallback_attempts: meta.fallback_attempts,
                },
            )
            .await?;
        self.cancel_quota_timer(thread.id);
        if busy {
            self.emit(WsServerMessage::RunStatusChanged {
                thread_id: thread.id,
                run_id: run.id,
                status: RunStatus::Queued,
                followup: false,
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
        self.resume_queued_runs_excluding(&std::collections::HashSet::new())
            .await
    }

    pub(crate) async fn resume_queued_runs_excluding(
        &self,
        excluded_threads: &std::collections::HashSet<Uuid>,
    ) -> CoreResult<()> {
        self.ensure_accepting_runs()?;
        let ids = self.0.store.queued_threads().await?;
        let resumes = ids
            .into_iter()
            .filter(|id| !excluded_threads.contains(id))
            .map(|id| async move {
                let guard = self.lock_thread(id).await;
                self.ensure_accepting_runs()?;
                if !self.active().contains_key(&id) {
                    let thread = self.0.store.get_thread(id).await?;
                    if let Some((_, turn)) =
                        self.start_next(&guard, thread.project_id, None).await?
                    {
                        self.spawn(turn);
                    }
                }
                CoreResult::Ok(())
            });
        // Finish all submitted starts even if one thread fails.
        for result in futures_util::future::join_all(resumes).await {
            result?;
        }
        Ok(())
    }

    pub async fn set_pause_all(&self, paused: bool) -> CoreResult<bool> {
        self.0.holds.set_paused(paused);
        if paused {
            for thread_id in self.0.store.queued_threads().await? {
                for run in self.0.store.list_queued_runs(thread_id).await? {
                    if !self.user_run_pending(run.id)
                        && let Some(announced) = self.0.holds.pause_blocking(run.id)
                        && !announced
                    {
                        self.announce_pause(thread_id, run.id).await;
                    }
                }
            }
        }
        self.resume_queued_runs().await?;
        Ok(paused)
    }

    pub fn pause_all(&self) -> bool {
        self.0.holds.is_paused()
    }

    pub(super) async fn start_next(
        &self,
        guard: &ThreadGuard,
        project_id: Uuid,
        submitted_id: Option<Uuid>,
    ) -> CoreResult<Option<(RunTicket, RunningTurn)>> {
        // Admission was checked under this thread guard. Shutdown waits for
        // admitted starts before collecting their completion receivers.
        // A project deletion can win after enqueue. Revalidate before even
        // selecting a queued run, and keep deletion excluded until registration.
        let _project = self.0.project_locks.get(project_id).read_owned().await;
        let store = &self.0.store;
        let thread = store.get_thread(guard.id).await.inspect_err(|error| {
            if matches!(error, CoreError::NotFound(_))
                && let Some(id) = submitted_id
            {
                self.clear_user_run(id);
            }
        })?;
        let mut next = store.next_queued_run(thread.id).await?;
        if self.0.holds.is_paused()
            && next.as_ref().is_some_and(|queued| {
                !self.user_run_pending(queued.id) && queued.restart_session.is_none()
            })
        {
            for run in store.list_queued_runs(thread.id).await? {
                if self.user_run_pending(run.id) {
                    next = store.queued_run(thread.id, run.id).await?;
                    break;
                }
            }
        }
        while let Some(queued) = next {
            let run_id = queued.id;
            if !self.user_run_pending(run_id)
                && let Some(announced) = self.0.holds.pause_blocking(run_id)
            {
                if !announced {
                    self.announce_pause(thread.id, run_id).await;
                }
                return Ok(None);
            }
            if let Some((hold, announced)) = self
                .0
                .holds
                .blocking(&queued.input.model_selection, Some(run_id))
            {
                if !announced {
                    self.announce_hold(thread.id, run_id, &hold).await;
                }
                return Ok(None);
            }
            self.clear_user_run(run_id);
            match self.start(guard, thread.clone(), queued).await {
                Ok(Some(started)) => return Ok(Some(started)),
                Ok(None) => self.complete_queued_ticket(run_id),
                Err(error) => {
                    self.complete_queued_ticket(run_id);
                    let message =
                        format_run_error(FailureClass::HarnessError, &error.to_string(), None);
                    store
                        .finish_run(
                            run_id,
                            RunStatus::Failed,
                            None,
                            None,
                            None,
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
                        followup: false,
                        error: Some(message),
                    });
                    if submitted_id == Some(run_id) {
                        return Err(error);
                    }
                }
            }
            next = store.next_queued_run(thread.id).await?;
        }
        Ok(None)
    }

    pub(crate) fn driver(&self, model: &ModelSelection) -> CoreResult<Arc<dyn ProviderDriver>> {
        if model.provider == ProviderId::GeminiApp {
            crate::gemini_app::validate_selection(model)?;
        }
        if model.model.trim().is_empty() {
            return Err(CoreError::InvalidArgument("model is required".into()));
        }
        self.0.providers.get(model.provider).ok_or_else(|| {
            CoreError::InvalidArgument(format!("provider {} is disabled", model.provider.as_str()))
        })
    }
}
