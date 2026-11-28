use super::*;

impl RunCoordinator {
    /// Stores the reasoning and reply text still buffered when a turn ends.
    pub(super) async fn flush_output(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        outcome: &mut Outcome,
    ) -> CoreResult<()> {
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

    /// Persists the run's last context usage and drops its live stream.
    pub(super) async fn settle_live_state(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        session: Option<String>,
    ) {
        if let Some(mut snap) = self.live_context(thread_id).await
            && snap.run_id == run_id
        {
            if snap.session_id.is_none() {
                snap.session_id = session;
                self.record_live_context(thread_id, snap.clone()).await;
            }
            self.persist_context(thread_id, &snap).await;
        }
        self.0.live_streams.lock().await.remove(&thread_id);
    }

    pub(super) async fn event(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        event: StreamEvent,
        out: &mut Outcome,
    ) -> CoreResult<()> {
        match event {
            #[cfg(unix)]
            StreamEvent::ProcessStarted {
                process_group_id,
                start_time,
            } => {
                self.0
                    .store
                    .record_orphan_process(run_id, process_group_id, start_time)
                    .await?;
            }
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
                let _thread = self.lock_thread(thread_id).await;
                let current = self.0.store.get_thread(thread_id).await?.model_selection;
                if self.active().get(&thread_id).is_some_and(|run| {
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
                if let Some(run) = self.active().get_mut(&thread_id) {
                    run.pending.insert(request.id, request.clone());
                }
                self.emit(WsServerMessage::PendingRequest { request });
            }
            StreamEvent::PendingResolved { request_id } => {
                if let Some(run) = self.active().get_mut(&thread_id) {
                    run.pending.remove(&request_id);
                }
                self.emit(WsServerMessage::PendingResolved {
                    thread_id,
                    request_id,
                });
            }
            StreamEvent::RateLimited {
                retry_after_secs,
                message,
                reset_at,
                error_code,
            } => {
                out.reset_at = reset_at.clone();
                out.limited_until =
                    limited_until(reset_at.as_deref(), retry_after_secs, Utc::now());
                let class = classify_rate_limit(out.provider, &message, error_code.as_deref());
                out.failure_class = Some(class);
                out.hard_rate_limited = class == FailureClass::UsageLimit;
                if let Some(provider) = self.active().get(&thread_id).map(|run| run.model.provider)
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
                    followup: false,
                    error: Some(message.clone()),
                });
                out.terminal = Some((RunStatus::RateLimited, Some(message)));
            }
            StreamEvent::Usage {
                input_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                output_tokens,
                reasoning_tokens,
            } => {
                accumulate_usage(&mut out.input, input_tokens);
                accumulate_usage(&mut out.cache_read, cache_read_tokens);
                accumulate_usage(&mut out.cache_creation, cache_creation_tokens);
                accumulate_usage(&mut out.output, output_tokens);
                accumulate_usage(&mut out.reasoning_tokens, reasoning_tokens);
                // Codex turn usage sums every request in the turn; its context size comes from
                // the session file instead (see `build_thread_context`).
                if !out.has_context_usage && out.provider != ProviderId::Codex {
                    let input_tokens = input_tokens.unwrap_or(0);
                    let output_tokens = output_tokens.unwrap_or(0);
                    let cache_read_tokens = cache_read_tokens.unwrap_or(0);
                    let cache_write_tokens = cache_creation_tokens.unwrap_or(0);
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
                        if out.provider == ProviderId::GeminiApp {
                            out.assistant = text;
                        } else {
                            push_capped(&mut out.assistant, &text, out.reasoning.len());
                        }
                    }
                }
                out.terminal = Some((RunStatus::Completed, None));
            }
            StreamEvent::Failed {
                message,
                reset_at,
                error_code,
            } => {
                out.limited_until = limited_until(reset_at.as_deref(), 0, Utc::now());
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

    pub(super) async fn thread_status(
        &self,
        thread_id: Uuid,
        status: ThreadStatus,
    ) -> CoreResult<()> {
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

    pub(super) async fn flush_text(
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
    pub(super) async fn update_live_stream(
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

    /// Holds new runs for a limited provider and dispatches them again once the limit resets.
    pub(super) fn hold_provider(&self, hold: ProviderHold) {
        let until = hold.until;
        tracing::warn!(provider = hold.provider.as_str(), model = ?hold.model, %until, "Holding queued runs until the provider limit resets");
        if !self.0.holds.record(hold) {
            return;
        }
        let coordinator = Arc::downgrade(&self.0);
        let wait =
            (until - Utc::now()).to_std().unwrap_or_default() + std::time::Duration::from_secs(1);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(coordinator) = coordinator.upgrade()
                && let Err(error) = RunCoordinator(coordinator).resume_queued_runs().await
            {
                tracing::warn!(%error, "Could not resume runs held for a provider limit");
            }
        });
    }

    /// Releases a provider hold early, e.g. after its quota was reset by a credit.
    pub(crate) async fn release_provider_hold(&self, provider: ProviderId) {
        self.0.holds.release(provider);
        if let Err(error) = self.resume_queued_runs().await {
            tracing::warn!(%error, "Could not resume runs after releasing a provider hold");
        }
    }

    pub(super) async fn announce_hold(&self, thread_id: Uuid, run_id: Uuid, hold: &ProviderHold) {
        let target = hold.model.as_deref().map_or_else(
            || hold.provider.as_str().to_owned(),
            |model| format!("{}/{model}", hold.provider.as_str()),
        );
        let until = hold.until.to_rfc3339();
        let notice =
            format!("{target} の利用上限が {until} に解除されるまで、この run は待機します");
        self.announce_queued_reason(
            thread_id,
            run_id,
            notice.clone(),
            json!({
                "message": notice,
                "held_until": until,
                "provider": hold.provider,
                "model": hold.model,
                "reason": hold.reason,
            }),
            Some(ThreadStatus::RateLimited),
        )
        .await;
    }

    pub(super) async fn announce_pause(&self, thread_id: Uuid, run_id: Uuid) {
        let notice = "自動実行は一時停止中です。この実行はキューで待機します。";
        self.announce_queued_reason(
            thread_id,
            run_id,
            notice.into(),
            json!({"message": notice, "reason": "pause_all"}),
            None,
        )
        .await;
    }

    pub(super) async fn announce_queued_reason(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        notice: String,
        payload: Value,
        status: Option<ThreadStatus>,
    ) {
        let result = async {
            self.append(NewTurnItem {
                thread_id,
                run_id,
                kind: TurnItemKind::SystemNotice,
                summary: Some(notice),
                payload,
                searchable_text: None,
            })
            .await?;
            if let Some(status) = status {
                self.thread_status(thread_id, status).await?;
            }
            CoreResult::Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, %run_id, "Could not report a held queued run");
        }
    }

    pub(super) fn pick_fallback_model(
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
