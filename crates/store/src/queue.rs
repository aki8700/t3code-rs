use super::*;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyDeadEnd {
    pub attempt: u8,
    pub signature: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    pub summary: String,
}

#[derive(Serialize, Deserialize)]
pub struct QueuedRunInput {
    pub model_selection: ModelSelection,
    pub attachment_ids: Vec<Uuid>,
    pub notify_parent: bool,
    pub wake_depth: u8,
    #[serde(default)]
    pub mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
    #[serde(default)]
    pub verify_fix_depth: u8,
    #[serde(default)]
    pub last_verify_signature: Option<String>,
    #[serde(default)]
    pub verify_dead_ends: Vec<VerifyDeadEnd>,
    #[serde(default)]
    pub fallback_attempts: u8,
}

pub struct QueuedRun {
    pub id: Uuid,
    pub prompt: String,
    pub input: QueuedRunInput,
    /// Session to resume when a server restart interrupted this run and put it back in the queue.
    pub restart_session: Option<String>,
}

impl SqliteStore {
    pub async fn enqueue_run(
        &self,
        mut run: Run,
        item: NewTurnItem,
        input: QueuedRunInput,
    ) -> CoreResult<(Run, TurnItem)> {
        let cfg = self.storage_cfg.clone();
        self.with_writer(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            run.ordinal = tx
                .query_row(
                    "SELECT coalesce(max(ordinal), 0) + 1 FROM runs WHERE thread_id = ?1",
                    [run.thread_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            insert_run(&tx, &run)?;
            tx.execute(
                "INSERT INTO queued_run_inputs (run_id, input_json) VALUES (?1, ?2)",
                params![run.id.to_string(), encode(&input)?],
            )
            .map_err(storage_error)?;
            let item = insert_item(
                &tx,
                item,
                &cfg,
                ItemPosition {
                    id: Uuid::new_v4(),
                    ordinal: None,
                    created_at: Utc::now(),
                },
                true,
            )?;
            tx.commit().map_err(storage_error)?;
            Ok((run, item))
        })
        .await
    }

    pub async fn queued_threads(&self) -> CoreResult<Vec<Uuid>> {
        self.with_reader(|connection| {
            query_all(
                connection,
                "SELECT DISTINCT thread_id FROM runs WHERE status = ?1",
                [encode(&RunStatus::Queued)?],
                |row| parse(row, 0),
            )
        })
        .await
    }

    pub async fn next_queued_run(&self, thread_id: Uuid) -> CoreResult<Option<QueuedRun>> {
        self.with_reader(move |connection| {
            connection
                .query_row(
                    "SELECT runs.id, runs.prompt, inputs.input_json,
                CASE WHEN runs.restart_resumes > 0 THEN runs.provider_session_id END
             FROM runs JOIN queued_run_inputs AS inputs ON inputs.run_id = runs.id
             WHERE runs.thread_id = ?1 AND runs.status = ?2 ORDER BY runs.ordinal LIMIT 1",
                    params![thread_id.to_string(), encode(&RunStatus::Queued)?],
                    |row| {
                        Ok(QueuedRun {
                            id: parse(row, 0)?,
                            prompt: row.get(1)?,
                            input: decode(row, 2)?,
                            restart_session: row.get(3)?,
                        })
                    },
                )
                .optional()
                .map_err(storage_error)
        })
        .await
    }

    pub async fn queued_run(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<Option<QueuedRun>> {
        self.with_reader(move |connection| {
            connection
                .query_row(
                    "SELECT runs.id, runs.prompt, inputs.input_json,
                CASE WHEN runs.restart_resumes > 0 THEN runs.provider_session_id END
             FROM runs JOIN queued_run_inputs AS inputs ON inputs.run_id = runs.id
             WHERE runs.thread_id = ?1 AND runs.id = ?2 AND runs.status = ?3",
                    params![
                        thread_id.to_string(),
                        run_id.to_string(),
                        encode(&RunStatus::Queued)?
                    ],
                    |row| {
                        Ok(QueuedRun {
                            id: parse(row, 0)?,
                            prompt: row.get(1)?,
                            input: decode(row, 2)?,
                            restart_session: row.get(3)?,
                        })
                    },
                )
                .optional()
                .map_err(storage_error)
        })
        .await
    }

    pub async fn list_queued_runs(&self, thread_id: Uuid) -> CoreResult<Vec<Run>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT id, thread_id, ordinal, provider, model, effort, status, prompt,
                    provider_session_id, input_tokens, output_tokens, cache_read_tokens,
                    cache_creation_tokens, reasoning_tokens, duration_ms, error_message,
                    started_at, completed_at FROM runs
                 WHERE thread_id = ?1 AND status = ?2 ORDER BY ordinal",
                params![thread_id.to_string(), encode(&RunStatus::Queued)?],
                run_row,
            )
        })
        .await
    }

    pub async fn cancel_queued_run(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<bool> {
        self.with_writer(move |connection| {
            let tx = connection.transaction().map_err(storage_error)?;
            let changed = tx
                .execute(
                    "UPDATE runs SET status = ?3, completed_at = ?4, error_message = 'Interrupted'
                 WHERE id = ?1 AND thread_id = ?2 AND status = ?5",
                    params![
                        run_id.to_string(),
                        thread_id.to_string(),
                        encode(&RunStatus::Cancelled)?,
                        timestamp(Utc::now()),
                        encode(&RunStatus::Queued)?
                    ],
                )
                .map_err(storage_error)?
                != 0;
            if changed {
                tx.execute(
                    "DELETE FROM queued_run_inputs WHERE run_id = ?1",
                    [run_id.to_string()],
                )
                .map_err(storage_error)?;
            }
            tx.commit().map_err(storage_error)?;
            Ok(changed)
        })
        .await
    }

    /// Records the workspace tree from before the run and returns the run's baseline. The first
    /// save wins: a run resumed after a restart already holds the interrupted turn's edits, so
    /// its new snapshot must not replace the tree that verify and rollback compare against.
    pub async fn save_run_checkpoint(
        &self,
        run_id: Uuid,
        thread_id: Uuid,
        pre_tree: String,
    ) -> CoreResult<String> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "INSERT INTO run_checkpoints (run_id, thread_id, pre_tree, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(run_id) DO NOTHING",
                    params![
                        run_id.to_string(),
                        thread_id.to_string(),
                        pre_tree,
                        timestamp(Utc::now())
                    ],
                )
                .map_err(storage_error)?;
            connection
                .query_row(
                    "SELECT pre_tree FROM run_checkpoints WHERE run_id = ?1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(storage_error)
        })
        .await
    }

    pub async fn get_run_checkpoint(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
    ) -> CoreResult<Option<String>> {
        self.with_reader(move |connection| {
            connection
                .query_row(
                    "SELECT pre_tree FROM run_checkpoints WHERE thread_id = ?1 AND run_id = ?2",
                    params![thread_id.to_string(), run_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage_error)
        })
        .await
    }

    pub async fn list_run_checkpoints(&self, thread_id: Uuid) -> CoreResult<Vec<Uuid>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT run_id FROM run_checkpoints WHERE thread_id = ?1 ORDER BY created_at, run_id",
                [thread_id.to_string()],
                |row| parse(row, 0),
            )
        }).await
    }

    pub async fn previous_run(&self, thread_id: Uuid, before: Uuid) -> CoreResult<Option<Run>> {
        self.with_reader(move |connection| connection.query_row(
            "SELECT id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, reasoning_tokens,
                duration_ms, error_message, started_at, completed_at
             FROM runs WHERE thread_id = ?1 AND ordinal > 0
                AND model != 'shell'
                AND ordinal < (SELECT ordinal FROM runs WHERE id = ?2)
                AND (status = ?3 OR provider_session_id IS NOT NULL)
             ORDER BY ordinal DESC LIMIT 1",
            params![thread_id.to_string(), before.to_string(), encode(&RunStatus::Completed)?],
            run_row,
        ).optional().map_err(storage_error)).await
    }

    pub async fn start_queued_run(
        &self,
        id: Uuid,
        selection: ModelSelection,
        session: Option<String>,
    ) -> CoreResult<Run> {
        self.with_writer(move |connection| {
            let tx = connection.transaction().map_err(storage_error)?;
            let now = timestamp(Utc::now());
            let run = tx
                .query_row(
                    "UPDATE runs SET status = ?2, provider_session_id = ?3,
                    started_at = CASE WHEN restart_resumes > 0 THEN started_at ELSE ?4 END
                 WHERE id = ?1 AND status = ?5
                 RETURNING id, thread_id, ordinal, provider, model, effort, status, prompt,
                    provider_session_id, input_tokens, output_tokens, cache_read_tokens,
                    cache_creation_tokens, reasoning_tokens, duration_ms, error_message,
                    started_at, completed_at",
                    params![
                        id.to_string(),
                        encode(&RunStatus::Running)?,
                        session,
                        now,
                        encode(&RunStatus::Queued)?
                    ],
                    run_row,
                )
                .map_err(storage_error)?;
            tx.execute(
                "UPDATE threads SET status = ?2, model_selection = ?3, provider_session_id = ?4,
                    updated_at = ?5 WHERE id = ?1",
                params![
                    run.thread_id.to_string(),
                    encode(&ThreadStatus::Running)?,
                    encode(&selection)?,
                    session,
                    now
                ],
            )
            .map_err(storage_error)?;
            // The inputs stay until the run finishes so a restart can queue the run again.
            tx.commit().map_err(storage_error)?;
            Ok(run)
        })
        .await
    }

    pub async fn cancel_queued_runs(&self, thread_id: Uuid) -> CoreResult<Vec<Uuid>> {
        self.with_writer(move |connection| {
            let tx = connection.transaction().map_err(storage_error)?;
            let ids: Vec<Uuid> = query_all(
                &tx,
                "UPDATE runs SET status = ?2, completed_at = ?3, error_message = 'Interrupted'
                 WHERE thread_id = ?1 AND status = ?4 RETURNING id",
                params![
                    thread_id.to_string(),
                    encode(&RunStatus::Cancelled)?,
                    timestamp(Utc::now()),
                    encode(&RunStatus::Queued)?
                ],
                |row| parse(row, 0),
            )?;
            for id in &ids {
                tx.execute(
                    "DELETE FROM queued_run_inputs WHERE run_id = ?1",
                    [id.to_string()],
                )
                .map_err(storage_error)?;
            }
            tx.commit().map_err(storage_error)?;
            Ok(ids)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use t3_core::{InteractionMode, ProviderId, RuntimeMode, Thread, ThreadStatus};

    async fn fixture() -> (SqliteStore, Uuid) {
        let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
        let project = store
            .create_project("Queue tests".into(), "/tmp/queue-tests".into())
            .await
            .unwrap();
        let now = Utc::now();
        let thread = store
            .create_thread(Thread {
                id: Uuid::new_v4(),
                project_id: project.id,
                parent_thread_id: None,
                relationship: None,
                title: "Queue".into(),
                model_selection: ModelSelection::default(),
                runtime_mode: RuntimeMode::AutoAcceptEdits,
                interaction_mode: InteractionMode::Plan,
                status: ThreadStatus::Idle,
                worktree_path: None,
                branch: None,
                provider_session_id: None,
                archived: false,
                pinned: false,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        (store, thread.id)
    }

    fn queued_run(thread_id: Uuid) -> Run {
        Run {
            id: Uuid::new_v4(),
            thread_id,
            ordinal: 1,
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            status: RunStatus::Queued,
            prompt: "queued".into(),
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
        }
    }

    #[tokio::test]
    async fn lists_and_cancels_only_matching_queued_run() {
        let (store, thread_id) = fixture().await;
        let run = store.create_run(queued_run(thread_id)).await.unwrap();
        assert_eq!(store.list_queued_runs(thread_id).await.unwrap().len(), 1);
        assert!(
            !store
                .cancel_queued_run(Uuid::new_v4(), run.id)
                .await
                .unwrap()
        );
        assert!(store.cancel_queued_run(thread_id, run.id).await.unwrap());
        assert!(store.list_queued_runs(thread_id).await.unwrap().is_empty());
        assert!(!store.cancel_queued_run(thread_id, run.id).await.unwrap());
    }

    fn mock_selection() -> ModelSelection {
        ModelSelection {
            provider: ProviderId::Mock,
            model: "mock".into(),
            ..ModelSelection::default()
        }
    }

    /// Queues and starts a run as the coordinator does, optionally recording the session the
    /// provider reported for it.
    async fn running_run(store: &SqliteStore, thread_id: Uuid, session: Option<&str>) -> Uuid {
        let run = queued_run(thread_id);
        let item = NewTurnItem {
            thread_id,
            run_id: run.id,
            kind: t3_core::TurnItemKind::UserMessage,
            summary: None,
            payload: serde_json::json!({"text": "queued"}),
            searchable_text: None,
        };
        let (run, _) = store
            .enqueue_run(
                run,
                item,
                QueuedRunInput {
                    model_selection: mock_selection(),
                    attachment_ids: Vec::new(),
                    notify_parent: true,
                    wake_depth: 2,
                    mode_ceiling: Some((RuntimeMode::ApprovalRequired, InteractionMode::Plan)),
                    verify_fix_depth: 0,
                    last_verify_signature: None,
                    verify_dead_ends: Vec::new(),
                    fallback_attempts: 0,
                },
            )
            .await
            .unwrap();
        store
            .start_queued_run(run.id, mock_selection(), None)
            .await
            .unwrap();
        if let Some(session) = session {
            store
                .update_thread_session_id(thread_id, session.into())
                .await
                .unwrap();
        }
        run.id
    }

    async fn run_status(store: &SqliteStore, thread_id: Uuid) -> (RunStatus, Option<String>) {
        let run = store.list_runs(thread_id).await.unwrap().remove(0);
        (run.status, run.error_message)
    }

    #[tokio::test]
    async fn restart_queues_a_run_with_a_session_once_with_its_inputs() {
        let (store, thread_id) = fixture().await;
        let run_id = running_run(&store, thread_id, Some("session-1")).await;

        let recovered = store.recover_interrupted_runs().unwrap();
        assert_eq!(recovered.resumed, vec![run_id]);
        assert!(recovered.cancelled.is_empty());
        assert_eq!(
            store.get_thread(thread_id).await.unwrap().status,
            ThreadStatus::Interrupted
        );
        let queued = store.next_queued_run(thread_id).await.unwrap().unwrap();
        assert_eq!(queued.id, run_id);
        assert_eq!(queued.restart_session.as_deref(), Some("session-1"));
        assert!(queued.input.notify_parent);
        assert_eq!(queued.input.wake_depth, 2);
        assert_eq!(
            queued.input.mode_ceiling,
            Some((RuntimeMode::ApprovalRequired, InteractionMode::Plan))
        );

        // The resumed run is interrupted again: the second restart cancels it.
        let started = store
            .list_runs(thread_id)
            .await
            .unwrap()
            .remove(0)
            .started_at;
        let resumed = store
            .start_queued_run(run_id, mock_selection(), Some("session-1".into()))
            .await
            .unwrap();
        assert_eq!(resumed.started_at, started);
        let recovered = store.recover_interrupted_runs().unwrap();
        assert!(recovered.resumed.is_empty());
        assert_eq!(recovered.cancelled, vec![run_id]);
        assert_eq!(
            run_status(&store, thread_id).await,
            (RunStatus::Cancelled, Some(INTERRUPTED_BY_RESTART.into()))
        );
        assert!(store.next_queued_run(thread_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn restart_cancels_runs_without_a_session_or_inputs() {
        let (store, thread_id) = fixture().await;
        let run_id = running_run(&store, thread_id, None).await;
        assert_eq!(
            store.recover_interrupted_runs().unwrap().cancelled,
            vec![run_id]
        );
        assert_eq!(
            run_status(&store, thread_id).await,
            (RunStatus::Cancelled, Some(INTERRUPTED_BY_RESTART.into()))
        );

        let (store, thread_id) = fixture().await;
        let run_id = running_run(&store, thread_id, Some("session-1")).await;
        store
            .with_writer(move |connection| {
                connection
                    .execute(
                        "DELETE FROM queued_run_inputs WHERE run_id = ?1",
                        [run_id.to_string()],
                    )
                    .map_err(storage_error)
            })
            .await
            .unwrap();
        assert_eq!(
            store.recover_interrupted_runs().unwrap().cancelled,
            vec![run_id]
        );
    }

    #[tokio::test]
    async fn restart_keeps_runs_that_ended_rate_limited() {
        let (store, thread_id) = fixture().await;
        let run_id = running_run(&store, thread_id, Some("session-1")).await;
        store
            .finish_run(
                run_id,
                RunStatus::RateLimited,
                None,
                None,
                None,
                None,
                None,
                None,
                1,
                Some("usage limit".into()),
            )
            .await
            .unwrap();
        store
            .update_thread_status(thread_id, ThreadStatus::RateLimited)
            .await
            .unwrap();
        assert_eq!(
            store.recover_interrupted_runs().unwrap(),
            RecoveredRuns::default()
        );
        assert_eq!(
            run_status(&store, thread_id).await,
            (RunStatus::RateLimited, Some("usage limit".into()))
        );
        assert_eq!(
            store.get_thread(thread_id).await.unwrap().status,
            ThreadStatus::RateLimited
        );
    }

    #[tokio::test]
    async fn stores_and_lists_run_checkpoints() {
        let (store, thread_id) = fixture().await;
        let run = store.create_run(queued_run(thread_id)).await.unwrap();
        assert_eq!(
            store.get_run_checkpoint(thread_id, run.id).await.unwrap(),
            None
        );
        store
            .save_run_checkpoint(run.id, thread_id, "tree-one".into())
            .await
            .unwrap();
        assert_eq!(
            store
                .get_run_checkpoint(thread_id, run.id)
                .await
                .unwrap()
                .as_deref(),
            Some("tree-one")
        );
        // A run resumed after a restart saves again; the pre-run baseline must survive.
        store
            .save_run_checkpoint(run.id, thread_id, "tree-two".into())
            .await
            .unwrap();
        assert_eq!(
            store
                .get_run_checkpoint(thread_id, run.id)
                .await
                .unwrap()
                .as_deref(),
            Some("tree-one")
        );
        assert_eq!(
            store.list_run_checkpoints(thread_id).await.unwrap(),
            vec![run.id]
        );
    }
}
