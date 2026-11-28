use super::*;

impl RunCoordinator {
    pub(super) async fn start(
        &self,
        guard: &ThreadGuard,
        mut thread: Thread,
        queued: QueuedRun,
    ) -> CoreResult<Option<(RunTicket, RunningTurn)>> {
        debug_assert_eq!(guard.id, thread.id);
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
        let workspace_dir = self.workspace(&thread).await?;
        let restarted = queued.restart_session.is_some();
        let previous_run = store.previous_run(thread.id, run_id).await?;
        let resume_session_id = if let Some(session) = queued.restart_session {
            if !self
                .session_resumable(selection.provider, &workspace_dir, &session)
                .await
            {
                tracing::warn!(%run_id, %session, "Cancelling a run interrupted by restart: its provider session cannot be resumed");
                self.cancel_unresumable(thread.id, run_id).await?;
                return Ok(None);
            }
            Some(session)
        } else {
            match previous_run
                .as_ref()
                .filter(|run| run.provider == selection.provider && run.model == selection.model)
                .and_then(|run| run.provider_session_id.clone())
            {
                Some(session)
                    if !self
                        .session_resumable(selection.provider, &workspace_dir, &session)
                        .await =>
                {
                    tracing::warn!(%run_id, %session, "Provider session cannot be resumed; starting a new session");
                    None
                }
                session => session,
            }
        };
        let project = store.get_project(thread.project_id).await?;
        let original_prompt = queued.prompt.clone();
        let request_prompt = if restarted {
            crate::resume::RESTART_PROMPT.to_owned()
        } else {
            queued.prompt
        };
        let claude_slash = selection.provider == ProviderId::Claude
            && request_prompt
                .split_whitespace()
                .next()
                .is_some_and(|command| {
                    command == "/verify-t3code"
                        || command.strip_prefix("/pstack:").is_some_and(|skill| {
                            !skill.is_empty()
                                && skill
                                    .bytes()
                                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                        })
                });
        let desktop = selection.provider == ProviderId::GeminiApp;
        let handoff = if restarted || desktop {
            None
        } else if previous_run.is_some() && resume_session_id.is_none() {
            let handoff = self.handoff_summary(&thread).await?;
            Some(format!("Context from the previous run:\n{handoff}"))
        } else if previous_run.is_none()
            && let Some(handoff) = store
                .list_handoffs(thread.id)
                .await?
                .into_iter()
                .find(|handoff| handoff.target_thread_id == thread.id)
        {
            Some(format!(
                "Context from the previous thread:\n{}",
                handoff.summary
            ))
        } else {
            None
        };
        let mut prompt = if claude_slash {
            handoff.unwrap_or_default()
        } else if let Some(handoff) = handoff {
            format!("{handoff}\n\nCurrent request:\n{request_prompt}")
        } else {
            request_prompt.clone()
        };
        if !desktop
            && resume_session_id.is_none()
            && let Some(instructions) = project
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            prompt = format!("Project instructions:\n{instructions}\n\n{prompt}");
        }
        let attachment_ids = queued.input.attachment_ids.clone();
        // The interrupted turn already delivered its attachments to the session.
        let attachments = if restarted {
            Vec::new()
        } else {
            self.attachments(thread.id, &attachment_ids).await?
        };
        let swarm = self
            .get_board(thread.id)
            .await?
            .map(|board| {
                SwarmTurn::prepare(
                    &board,
                    thread.id,
                    selection.provider,
                    resume_session_id.is_some(),
                    &mut prompt,
                )
            })
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
            injection.tool_profile =
                t3_mcp::OrchestratorBackend::tool_profile(self, thread.id).await;
            (Some(token), Some(injection))
        } else {
            (None, None)
        };
        let (prompt, claude_slash_context) = if claude_slash {
            (request_prompt, Some(prompt.trim().to_owned()))
        } else {
            (prompt, None)
        };
        let request = TurnRequest {
            thread_id: thread.id,
            run_id,
            workspace_dir: workspace_dir.clone(),
            prompt,
            claude_slash_context,
            model_selection: selection.clone(),
            runtime_mode: thread.runtime_mode,
            interaction_mode: thread.interaction_mode,
            resume_session_id: resume_session_id.clone(),
            attachments,
            mcp_injection,
        };
        let run = store
            .start_queued_run(run_id, selection.clone(), resume_session_id)
            .await?;
        // A resumed run keeps its original start, so posts made before the restart still count
        // for the swarm's silent-exit check.
        let run_started_at = run.started_at;
        let (cancel, cancellation) = watch::channel(false);
        let (done, completion) = self
            .0
            .queued_tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id)
            .map(|done| {
                let completion = done.subscribe();
                (done, completion)
            })
            .unwrap_or_else(|| watch::channel(false));
        let chained_dones = self.take_deferred_dones(run_id);
        self.active().insert(
            thread.id,
            ActiveRun {
                project_id: thread.project_id,
                id: run.id,
                model: selection.clone(),
                driver: driver.clone(),
                cancel,
                stop_requested: false,
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
        if restarted {
            let notice = "ハーネスの再起動で中断されたため、同じセッションで再開します";
            if let Err(error) = self
                .append(NewTurnItem {
                    thread_id: thread.id,
                    run_id,
                    kind: TurnItemKind::SystemNotice,
                    summary: Some(notice.into()),
                    payload: json!({
                        "message": notice,
                        "prompt": crate::resume::RESTART_PROMPT,
                    }),
                    searchable_text: None,
                })
                .await
            {
                tracing::error!(%error, %run_id, "Could not record the restart resume notice");
            }
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
            chained_dones,
            driver,
            notify_parent: queued.input.notify_parent,
            wake_depth: queued.input.wake_depth,
            mode_ceiling: queued.input.mode_ceiling,
            verify_fix_depth: queued.input.verify_fix_depth,
            last_verify_signature: queued.input.last_verify_signature,
            verify_dead_ends: queued.input.verify_dead_ends,
            fallback_attempts: queued.input.fallback_attempts,
            mcp_token,
            swarm,
            started_at: run_started_at,
            workspace_dir,
            verify_command: project.verify_command,
        };
        Ok(Some((ticket, turn)))
    }

    /// Cancels a run that a restart queued again when its provider session turns out unusable.
    pub(super) async fn cancel_unresumable(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<()> {
        let error = format!("{INTERRUPTED_BY_RESTART} (the provider session could not be resumed)");
        self.0
            .store
            .finish_run(
                run_id,
                RunStatus::Cancelled,
                None,
                None,
                None,
                None,
                None,
                None,
                0,
                Some(error.clone()),
            )
            .await?;
        self.emit(WsServerMessage::RunStatusChanged {
            thread_id,
            run_id,
            status: RunStatus::Cancelled,
            followup: false,
            error: Some(error),
        });
        Ok(())
    }

    pub(super) fn spawn(&self, turn: RunningTurn) {
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
            let guard = self.lock_thread(id).await;
            self.0.store.get_thread(id).await?;
            self.interrupt_locked(&guard).await?
        };
        if let Some(mut done) = done {
            let _ = done.wait_for(|completed| *completed).await;
        }
        Ok(())
    }

    pub(crate) async fn interrupt_locked(
        &self,
        guard: &ThreadGuard,
    ) -> CoreResult<Option<watch::Receiver<bool>>> {
        let id = guard.id;
        let done = self.active().get_mut(&id).map(|run| {
            run.stop_requested = true;
            self.cancel(run)
        });
        for run_id in self.0.store.cancel_queued_runs(id).await? {
            self.clear_user_run(run_id);
            self.complete_queued_ticket(run_id);
            self.emit(WsServerMessage::RunStatusChanged {
                thread_id: id,
                run_id,
                status: RunStatus::Cancelled,
                followup: false,
                error: Some("Interrupted".into()),
            });
        }
        Ok(done)
    }

    pub(super) fn cancel(&self, run: &mut ActiveRun) -> watch::Receiver<bool> {
        self.0.tokens.revoke_run(run.id);
        run.cancel.send_replace(true);
        run.done.clone()
    }

    pub(crate) fn ensure_accepting_runs(&self) -> CoreResult<()> {
        if self.0.shutting_down.load(Ordering::SeqCst) {
            return Err(CoreError::Provider("server is shutting down".into()));
        }
        Ok(())
    }

    pub async fn interrupt_all(&self) {
        self.0.shutting_down.store(true, Ordering::SeqCst);
        // In-flight starts already own a thread guard, even before they enter
        // the active map. Later starts observe the shutdown flag under that guard.
        let mut ids: HashSet<_> = self.0.thread_locks.live_keys().into_iter().collect();
        ids.extend(self.active().keys().copied());
        futures_util::future::join_all(ids.into_iter().map(|id| async move {
            let done = {
                let _thread = self.lock_thread(id).await;
                self.active().get_mut(&id).map(|run| self.cancel(run))
            };
            if let Some(mut done) = done {
                let _ = done.wait_for(|completed| *completed).await;
            }
        }))
        .await;
    }

    pub async fn respond_request(&self, id: Uuid, response: PendingResponse) -> CoreResult<()> {
        let _thread = self.lock_thread(id).await;
        let (run_id, driver) = self
            .active()
            .get(&id)
            .filter(|run| run.pending.contains_key(&response.request_id))
            .map(|run| (run.id, run.driver.clone()))
            .ok_or_else(|| CoreError::NotFound("pending request".into()))?;
        let request_id = response.request_id;
        if !driver.respond_request(run_id, response).await? {
            return Err(CoreError::Provider(
                "provider did not accept the response".into(),
            ));
        }
        if let Some(run) = self.active().get_mut(&id) {
            run.pending.remove(&request_id);
        }
        Ok(())
    }
}
