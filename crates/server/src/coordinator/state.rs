use super::*;

impl RunCoordinator {
    pub async fn git_stage(&self, thread_id: Uuid, rel_path: &str) -> CoreResult<()> {
        let thread = self.0.store.get_thread(thread_id).await?;
        let workspace = self.workspace(&thread).await?;
        GitManager::stage_file(&workspace, rel_path).await
    }

    pub async fn git_discard(&self, thread_id: Uuid, rel_path: &str) -> CoreResult<()> {
        let _thread = self.lock_thread(thread_id).await;
        if self.active().contains_key(&thread_id) {
            return Err(CoreError::InvalidArgument(
                "cannot discard files while thread has an active run".into(),
            ));
        }
        let thread = self.0.store.get_thread(thread_id).await?;
        let workspace = self.workspace(&thread).await?;
        GitManager::discard_file(&workspace, rel_path).await
    }

    pub async fn git_commit(&self, thread_id: Uuid, message: &str) -> CoreResult<String> {
        let _thread = self.lock_thread(thread_id).await;
        if self.active().contains_key(&thread_id) {
            return Err(CoreError::InvalidArgument(
                "cannot commit while thread has an active run".into(),
            ));
        }
        let thread = self.0.store.get_thread(thread_id).await?;
        let workspace = self.workspace(&thread).await?;
        GitManager::commit(&workspace, message).await
    }

    pub async fn cancel_queued_run(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<bool> {
        let guard = self.lock_thread(thread_id).await;
        self.cancel_queued_locked(&guard, run_id).await
    }

    /// Cancels a queued run. The caller holds the thread lock, so the run cannot start meanwhile.
    pub(crate) async fn cancel_queued_locked(
        &self,
        guard: &ThreadGuard,
        run_id: Uuid,
    ) -> CoreResult<bool> {
        let thread_id = guard.id;
        let cancelled = self.0.store.cancel_queued_run(thread_id, run_id).await?;
        if cancelled {
            self.clear_user_run(run_id);
            self.complete_queued_ticket(run_id);
            self.emit(WsServerMessage::RunStatusChanged {
                thread_id,
                run_id,
                status: RunStatus::Cancelled,
                followup: false,
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

    pub(super) async fn record_live_context(&self, thread_id: Uuid, snap: LiveContextUsage) {
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
            let active = self.active();
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
        if let WsServerMessage::ThreadStatusChanged { thread_id, status } = &event
            && !self
                .0
                .thread_status_deduper
                .should_emit(*thread_id, *status)
        {
            return;
        }
        let _ = self.0.events.send(event);
    }

    pub(super) fn user_run_pending(&self, run_id: Uuid) -> bool {
        self.0
            .user_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&run_id)
    }

    pub(crate) fn clear_user_run(&self, run_id: Uuid) {
        self.0
            .user_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id);
    }

    pub(crate) fn complete_queued_ticket(&self, run_id: Uuid) {
        if let Some(done) = self
            .0
            .queued_tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id)
        {
            done.send_replace(true);
        }
        self.complete_deferred_dones(run_id);
    }

    pub(super) fn defer_dones_for_queued_run(&self, run_id: Uuid, dones: Vec<watch::Sender<bool>>) {
        self.0
            .queued_chained_dones
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run_id)
            .or_default()
            .extend(dones);
    }

    pub(super) fn take_deferred_dones(&self, run_id: Uuid) -> Vec<watch::Sender<bool>> {
        self.0
            .queued_chained_dones
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id)
            .unwrap_or_default()
    }

    pub(super) fn complete_deferred_dones(&self, run_id: Uuid) {
        for done in self.take_deferred_dones(run_id) {
            done.send_replace(true);
        }
    }

    pub(crate) async fn pending(&self, id: Uuid) -> Vec<PendingRequest> {
        self.active()
            .get(&id)
            .map(|run| run.pending.values().cloned().collect())
            .unwrap_or_default()
    }
}
