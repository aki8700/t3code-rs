use super::*;

/// Error message of a run that a server restart cancelled.
pub const INTERRUPTED_BY_RESTART: &str = "Interrupted by server restart";

/// The runs `SqliteStore::recover_interrupted_runs` found in flight.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveredRuns {
    /// Runs put back in the queue to resume their provider session.
    pub resumed: Vec<Uuid>,
    pub cancelled: Vec<Uuid>,
    /// Provider process groups still requiring restart cleanup.
    pub orphan_processes: Vec<OrphanProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanProcess {
    pub run_id: Uuid,
    pub process_group_id: i32,
    pub start_time: String,
}

impl SqliteStore {
    /// Saves the process identity as soon as a provider CLI starts.
    pub async fn record_orphan_process(
        &self,
        run_id: Uuid,
        process_group_id: i32,
        start_time: String,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "INSERT INTO run_orphan_processes (run_id, process_group_id, start_time)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (
                    SELECT 1 FROM runs WHERE id = ?1 AND status IN (?4, ?5)
                 ) ON CONFLICT(run_id, process_group_id, start_time) DO NOTHING",
                    params![
                        run_id.to_string(),
                        process_group_id,
                        start_time,
                        encode(&RunStatus::Running)?,
                        encode(&RunStatus::RateLimited)?
                    ],
                )
                .map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    /// Clears a cleanup request only after the recorded process group is confirmed gone.
    pub async fn clear_orphan_cleanup(&self, process: OrphanProcess) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "DELETE FROM run_orphan_processes
                     WHERE run_id = ?1 AND process_group_id = ?2 AND start_time = ?3
                        AND cleanup_pending = 1",
                    params![
                        process.run_id.to_string(),
                        process.process_group_id,
                        process.start_time
                    ],
                )
                .map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    /// Removes process identities after a provider run has completed and cleaned up its groups.
    pub async fn clear_run_orphan_processes(&self, run_id: Uuid) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "DELETE FROM run_orphan_processes WHERE run_id = ?1 AND cleanup_pending = 0",
                    [run_id.to_string()],
                )
                .map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    /// Returns threads whose provider processes still need restart cleanup.
    pub async fn threads_with_pending_orphan_processes(&self) -> CoreResult<Vec<Uuid>> {
        self.with_reader(|connection| {
            query_all(
                connection,
                "SELECT DISTINCT runs.thread_id
                 FROM run_orphan_processes
                 JOIN runs ON runs.id = run_orphan_processes.run_id
                 WHERE run_orphan_processes.cleanup_pending = 1
                 ORDER BY runs.thread_id",
                [],
                |row| parse(row, 0),
            )
        })
        .await
    }

    /// Reconcile runs left in flight when the previous server stopped.
    ///
    /// An in-flight run goes back to the queue, at most once, when its thread still holds the
    /// provider session of that run and its queued inputs survive; the coordinator then resumes
    /// that session. Every other in-flight run is cancelled. Runs that already ended, including
    /// those that ended rate limited, keep their status.
    pub fn recover_interrupted_runs(&self) -> CoreResult<RecoveredRuns> {
        let mut connection = lock_connection(&self.connections.writer)?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let now = timestamp(Utc::now());
        let in_flight = query_all(
            &transaction,
            "SELECT runs.id, runs.thread_id, runs.provider, runs.model, runs.restart_resumes,
                EXISTS (SELECT 1 FROM queued_run_inputs WHERE run_id = runs.id),
                threads.provider_session_id, threads.model_selection, threads.archived
             FROM runs JOIN threads ON threads.id = runs.thread_id
             WHERE runs.status IN (?1, ?2) AND runs.completed_at IS NULL",
            params![
                encode(&RunStatus::Running)?,
                encode(&RunStatus::RateLimited)?
            ],
            |row| {
                let provider: ProviderId = decode(row, 2)?;
                let model: String = row.get(3)?;
                let selection: ModelSelection = decode(row, 7)?;
                let session: Option<String> = row.get(6)?;
                let resumable = row.get::<_, i64>(4)? == 0
                    && row.get::<_, bool>(5)?
                    && !row.get::<_, bool>(8)?
                    && selection.provider == provider
                    && selection.model == model;
                Ok((
                    parse::<Uuid>(row, 0)?,
                    parse::<Uuid>(row, 1)?,
                    session.filter(|_| resumable),
                ))
            },
        )?;
        let mut recovered = RecoveredRuns::default();
        for (run_id, thread_id, session) in in_flight {
            transaction
                .execute(
                    "UPDATE run_orphan_processes SET cleanup_pending = 1 WHERE run_id = ?1",
                    [run_id.to_string()],
                )
                .map_err(storage_error)?;
            if let Some(session) = session {
                transaction
                    .execute(
                        "UPDATE runs SET status = ?2, provider_session_id = ?3,
                            restart_resumes = restart_resumes + 1, completed_at = NULL,
                            error_message = NULL WHERE id = ?1",
                        params![run_id.to_string(), encode(&RunStatus::Queued)?, session],
                    )
                    .map_err(storage_error)?;
                recovered.resumed.push(run_id);
            } else {
                transaction
                    .execute(
                        "UPDATE runs SET status = ?2, completed_at = ?3, error_message = ?4
                         WHERE id = ?1",
                        params![
                            run_id.to_string(),
                            encode(&RunStatus::Cancelled)?,
                            now,
                            INTERRUPTED_BY_RESTART
                        ],
                    )
                    .map_err(storage_error)?;
                transaction
                    .execute(
                        "DELETE FROM queued_run_inputs WHERE run_id = ?1",
                        [run_id.to_string()],
                    )
                    .map_err(storage_error)?;
                recovered.cancelled.push(run_id);
            }
            transaction
                .execute(
                    "UPDATE threads SET status = ?2, updated_at = ?3 WHERE id = ?1",
                    params![
                        thread_id.to_string(),
                        encode(&ThreadStatus::Interrupted)?,
                        now
                    ],
                )
                .map_err(storage_error)?;
        }
        transaction
            .execute(
                "UPDATE threads SET status = ?1, updated_at = ?2 WHERE status = ?3",
                params![
                    encode(&ThreadStatus::Interrupted)?,
                    now,
                    encode(&ThreadStatus::Running)?,
                ],
            )
            .map_err(storage_error)?;
        transaction.execute(
            "UPDATE swarm_members SET state = 'waiting' WHERE role = 'worker' AND state = 'working'",
            [],
        ).map_err(storage_error)?;
        recovered.orphan_processes = query_all(
            &transaction,
            "SELECT run_id, process_group_id, start_time FROM run_orphan_processes
             WHERE cleanup_pending = 1 ORDER BY run_id, process_group_id",
            [],
            |row| {
                Ok(OrphanProcess {
                    run_id: parse(row, 0)?,
                    process_group_id: row.get(1)?,
                    start_time: row.get(2)?,
                })
            },
        )?;
        transaction.commit().map_err(storage_error)?;
        if !recovered.resumed.is_empty() || !recovered.cancelled.is_empty() {
            tracing::info!(
                resumed = recovered.resumed.len(),
                cancelled = recovered.cancelled.len(),
                "Recovered runs interrupted by server restart"
            );
        }
        Ok(recovered)
    }
}
