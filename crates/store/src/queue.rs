use super::*;
use serde::Deserialize;

#[derive(Serialize, Deserialize)]
pub struct QueuedRunInput {
    pub model_selection: ModelSelection,
    pub attachment_ids: Vec<Uuid>,
    pub notify_parent: bool,
    pub wake_depth: u8,
    #[serde(default)]
    pub mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
}

pub struct QueuedRun {
    pub id: Uuid,
    pub prompt: String,
    pub input: QueuedRunInput,
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
                    "SELECT runs.id, runs.prompt, inputs.input_json FROM runs
             JOIN queued_run_inputs AS inputs ON inputs.run_id = runs.id
             WHERE runs.thread_id = ?1 AND runs.status = ?2 ORDER BY runs.ordinal LIMIT 1",
                    params![thread_id.to_string(), encode(&RunStatus::Queued)?],
                    |row| {
                        Ok(QueuedRun {
                            id: parse(row, 0)?,
                            prompt: row.get(1)?,
                            input: decode(row, 2)?,
                        })
                    },
                )
                .optional()
                .map_err(storage_error)
        })
        .await
    }

    pub async fn previous_run(&self, thread_id: Uuid, before: Uuid) -> CoreResult<Option<Run>> {
        self.with_reader(move |connection| connection.query_row(
            "SELECT id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
                input_tokens, output_tokens, duration_ms, error_message, started_at, completed_at
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
                    "UPDATE runs SET status = ?2, provider_session_id = ?3, started_at = ?4
                 WHERE id = ?1 AND status = ?5
                 RETURNING id, thread_id, ordinal, provider, model, effort, status, prompt,
                    provider_session_id, input_tokens, output_tokens, duration_ms, error_message,
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
            tx.execute(
                "DELETE FROM queued_run_inputs WHERE run_id = ?1",
                [id.to_string()],
            )
            .map_err(storage_error)?;
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
