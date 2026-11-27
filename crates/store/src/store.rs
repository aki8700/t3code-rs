use super::*;

impl SqliteStore {
    async fn update_thread(
        &self,
        id: Uuid,
        query: &'static str,
        value: impl rusqlite::ToSql + Send + 'static,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            require_change(
                connection.execute(query, params![id.to_string(), value, timestamp(Utc::now())]),
                "thread",
                id,
            )
        })
        .await
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn create_project(&self, title: String, workspace_root: PathBuf) -> CoreResult<Project> {
        self.with_writer(move |connection| {
            let now = Utc::now();
            let project = Project {
                id: Uuid::new_v4(),
                title,
                workspace_root,
                default_model: None,
                instructions: None,
                verify_command: None,
                created_at: now,
                updated_at: now,
            };
            connection
                .execute(
                    "INSERT INTO projects (id, title, workspace_root, default_model, instructions,
                        verify_command, created_at, updated_at)
                 VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, ?4)",
                    params![
                        project.id.to_string(),
                        project.title,
                        path_text(&project.workspace_root)?,
                        timestamp(now)
                    ],
                )
                .map_err(storage_error)?;
            Ok(project)
        })
        .await
    }

    async fn list_projects(&self) -> CoreResult<Vec<Project>> {
        self.with_reader(|connection| {
            query_all(
                connection,
                "SELECT id, title, workspace_root, default_model, instructions, verify_command,
                        created_at, updated_at
                 FROM projects ORDER BY updated_at DESC, id",
                [],
                project_row,
            )
        })
        .await
    }

    async fn get_project(&self, id: Uuid) -> CoreResult<Project> {
        self.with_reader(move |connection| {
            query_required(
                connection,
                "SELECT id, title, workspace_root, default_model, instructions, verify_command,
                        created_at, updated_at
                 FROM projects WHERE id = ?1",
                "project",
                id,
                project_row,
            )
        })
        .await
    }

    async fn delete_project(&self, id: Uuid) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            require_change(
                transaction.execute("DELETE FROM projects WHERE id = ?1", [id.to_string()]),
                "project",
                id,
            )?;
            transaction
                .execute(&format!("DELETE FROM blob_store WHERE {ORPHAN_BLOBS}"), [])
                .map_err(storage_error)?;
            transaction.commit().map_err(storage_error)
        })
        .await
    }

    async fn create_thread(&self, thread: Thread) -> CoreResult<Thread> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            insert_thread(&transaction, &thread)?;
            transaction.commit().map_err(storage_error)?;
            Ok(thread)
        })
        .await
    }

    async fn list_threads(
        &self,
        project_id: Option<Uuid>,
        include_archived: bool,
        limit: usize,
    ) -> CoreResult<Vec<Thread>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                &format!(
                    "{THREAD_SELECT} WHERE (?1 IS NULL OR project_id = ?1) AND (?2 OR NOT archived)
                     ORDER BY pinned DESC, updated_at DESC, id LIMIT ?3"
                ),
                params![
                    project_id.map(|id| id.to_string()),
                    include_archived,
                    sql_limit(limit)
                ],
                thread_row,
            )
        })
        .await
    }

    async fn list_child_threads(&self, parent_thread_id: Uuid) -> CoreResult<Vec<Thread>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                &format!("{THREAD_SELECT} WHERE parent_thread_id = ?1 ORDER BY created_at, id"),
                [parent_thread_id.to_string()],
                thread_row,
            )
        })
        .await
    }

    async fn get_thread(&self, id: Uuid) -> CoreResult<Thread> {
        self.with_reader(move |connection| {
            query_required(
                connection,
                &format!("{THREAD_SELECT} WHERE id = ?1"),
                "thread",
                id,
                thread_row,
            )
        })
        .await
    }

    async fn update_thread_status(&self, id: Uuid, status: ThreadStatus) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET status = ?2, updated_at = ?3 WHERE id = ?1",
            encode(&status)?,
        )
        .await
    }

    async fn update_thread_session_id(&self, id: Uuid, session_id: String) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET provider_session_id = ?2, updated_at = ?3 WHERE id = ?1",
            session_id,
        )
        .await
    }

    async fn update_thread_model(&self, id: Uuid, selection: ModelSelection) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET model_selection = ?2, updated_at = ?3 WHERE id = ?1",
            encode(&selection)?,
        )
        .await
    }

    async fn update_thread_modes(
        &self,
        id: Uuid,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            require_change(
                connection.execute(
                    "UPDATE threads SET runtime_mode = ?2, interaction_mode = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![
                        id.to_string(),
                        encode(&runtime_mode)?,
                        encode(&interaction_mode)?,
                        timestamp(Utc::now())
                    ],
                ),
                "thread",
                id,
            )
        })
        .await
    }

    async fn update_thread_title(&self, id: Uuid, title: String) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            let key = id.to_string();
            require_change(
                transaction.execute(
                    "UPDATE threads SET title = ?2, updated_at = ?3 WHERE id = ?1",
                    params![key, title, timestamp(Utc::now())],
                ),
                "thread",
                id,
            )?;
            transaction
                .execute(
                    "UPDATE fts_entries SET title = ?2 WHERE thread_id = ?1 AND item_id IS NULL",
                    params![key, title],
                )
                .map_err(storage_error)?;
            transaction.commit().map_err(storage_error)
        })
        .await
    }

    async fn archive_thread(&self, id: Uuid, archived: bool) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET archived = ?2, updated_at = ?3 WHERE id = ?1",
            archived,
        )
        .await
    }

    async fn create_run(&self, run: Run) -> CoreResult<Run> {
        self.with_writer(move |connection| {
            insert_run(connection, &run)?;
            Ok(run)
        })
        .await
    }

    async fn list_runs(&self, thread_id: Uuid) -> CoreResult<Vec<Run>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, reasoning_tokens,
                    duration_ms, error_message, started_at, completed_at
                 FROM runs WHERE thread_id = ?1 ORDER BY ordinal, started_at, id",
                [thread_id.to_string()],
                run_row,
            )
        })
        .await
    }

    async fn finish_run(
        &self,
        run_id: Uuid,
        status: RunStatus,
        provider_session_id: Option<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        duration_ms: u64,
        error_message: Option<String>,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            require_change(
                transaction.execute(
                    "UPDATE runs SET status = ?2, provider_session_id = COALESCE(?3, provider_session_id),
                        input_tokens = ?4, output_tokens = ?5, cache_read_tokens = ?6,
                        cache_creation_tokens = ?7, reasoning_tokens = ?8, duration_ms = ?9,
                        error_message = ?10, completed_at = ?11 WHERE id = ?1",
                    params![
                        run_id.to_string(), encode(&status)?, provider_session_id,
                        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                        reasoning_tokens, duration_ms, error_message, timestamp(Utc::now()),
                    ],
                ),
                "run",
                run_id,
            )?;
            transaction.execute("DELETE FROM queued_run_inputs WHERE run_id = ?1", [run_id.to_string()])
                .map_err(storage_error)?;
            transaction.commit().map_err(storage_error)
        })
        .await
    }

    async fn append_turn_item(&self, item: NewTurnItem) -> CoreResult<TurnItem> {
        let cfg = self.storage_cfg.clone();
        self.with_writer(move |connection| append_item(connection, item, &cfg))
            .await
    }

    async fn list_turn_items(
        &self,
        thread_id: Uuid,
        limit: usize,
        offset: usize,
    ) -> CoreResult<Vec<TurnItem>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT item.id, item.thread_id, item.run_id, item.ordinal, item.kind, item.summary,
                    item.raw_bytes, item.compressed_bytes, item.created_at, item.payload_text,
                    blob.raw_bytes, blob.zstd_data
                 FROM turn_items AS item LEFT JOIN blob_store AS blob ON blob.hash = item.blob_hash
                 WHERE item.thread_id = ?1 ORDER BY item.ordinal LIMIT ?2 OFFSET ?3",
                params![thread_id.to_string(), sql_limit(limit), sql_limit(offset)],
                item_row,
            )
        })
        .await
    }

    async fn list_turn_items_page(
        &self,
        thread_id: Uuid,
        limit: usize,
        before_ordinal: Option<i64>,
    ) -> CoreResult<(Vec<TurnItem>, bool, usize)> {
        self.with_reader(move |connection| {
            let snapshot = connection.unchecked_transaction().map_err(storage_error)?;
            let thread_id = thread_id.to_string();
            let total = snapshot
                .query_row(
                    "SELECT count(*) FROM turn_items WHERE thread_id = ?1",
                    [&thread_id],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            let last = before_ordinal.map_or(Some(i64::MAX), |ordinal| ordinal.checked_sub(1));
            let mut items = query_all(
                &snapshot,
                "SELECT item.id, item.thread_id, item.run_id, item.ordinal, item.kind, item.summary,
                    item.raw_bytes, item.compressed_bytes, item.created_at, item.payload_text,
                    blob.raw_bytes, blob.zstd_data
                 FROM turn_items AS item LEFT JOIN blob_store AS blob ON blob.hash = item.blob_hash
                 WHERE item.thread_id = ?1 AND item.ordinal <= ?2
                 ORDER BY item.ordinal DESC LIMIT ?3",
                params![thread_id, last, sql_limit(limit)],
                item_row,
            )?;
            let has_more = if items.len() == limit {
                let older_than_page = items
                    .last()
                    .map_or(last, |item| item.ordinal.checked_sub(1));
                snapshot
                    .query_row(
                        "SELECT EXISTS (
                            SELECT 1 FROM turn_items WHERE thread_id = ?1 AND ordinal <= ?2
                        )",
                        params![thread_id, older_than_page],
                        |row| row.get(0),
                    )
                    .map_err(storage_error)?
            } else {
                false
            };
            items.reverse();
            snapshot.commit().map_err(storage_error)?;
            Ok((items, has_more, total))
        })
        .await
    }

    async fn record_handoff(&self, handoff: ContextHandoff) -> CoreResult<ContextHandoff> {
        self.with_writer(move |connection| {
            connection.execute(
                "INSERT INTO context_handoffs (id, source_thread_id, target_thread_id, from_model,
                    to_model, summary, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    handoff.id.to_string(), handoff.source_thread_id.to_string(),
                    handoff.target_thread_id.to_string(), encode(&handoff.from_model)?,
                    encode(&handoff.to_model)?, handoff.summary, timestamp(handoff.created_at),
                ],
            ).map_err(storage_error)?;
            Ok(handoff)
        })
        .await
    }

    async fn list_handoffs(&self, thread_id: Uuid) -> CoreResult<Vec<ContextHandoff>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT id, source_thread_id, target_thread_id, from_model, to_model, summary, created_at
                 FROM context_handoffs WHERE source_thread_id = ?1 OR target_thread_id = ?1
                 ORDER BY created_at, id",
                [thread_id.to_string()],
                handoff_row,
            )
        })
        .await
    }

    async fn save_attachment(&self, attachment: Attachment) -> CoreResult<Attachment> {
        self.with_writer(move |connection| {
            connection.execute(
                "INSERT INTO attachments (id, thread_id, filename, mime_type, size_bytes, blake3_hash,
                    storage_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    attachment.id.to_string(), attachment.thread_id.map(|id| id.to_string()),
                    attachment.filename, attachment.mime_type, attachment.size_bytes, attachment.blake3_hash,
                    path_text(&attachment.storage_path)?, timestamp(attachment.created_at),
                ],
            ).map_err(storage_error)?;
            Ok(attachment)
        })
        .await
    }

    async fn get_attachment(&self, id: Uuid) -> CoreResult<Attachment> {
        self.with_reader(move |connection| {
            query_required(
                connection,
                &format!("{ATTACHMENT_SELECT} WHERE id = ?1"),
                "attachment",
                id,
                attachment_row,
            )
        })
        .await
    }

    async fn list_attachments(&self, thread_id: Uuid) -> CoreResult<Vec<Attachment>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                &format!("{ATTACHMENT_SELECT} WHERE thread_id = ?1 ORDER BY created_at, id"),
                [thread_id.to_string()],
                attachment_row,
            )
        })
        .await
    }

    async fn search_fts(&self, query: &str, limit: usize) -> CoreResult<Vec<SearchHit>> {
        let query = query.trim().to_owned();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.with_reader(move |connection| {
            let limit = sql_limit(limit);
            if query.chars().count() >= 3 {
                query_all(
                    connection,
                    "SELECT threads.id, threads.title, item_id,
                        snippet(fts_entries, -1, '', '', '…', 24), fts_entries.rank, threads.updated_at
                     FROM fts_entries JOIN threads ON threads.id = fts_entries.thread_id
                     WHERE fts_entries MATCH ?1
                     ORDER BY fts_entries.rank, threads.updated_at DESC, fts_entries.rowid LIMIT ?2",
                    params![format!("\"{}\"", query.replace('"', "\"\"")), limit],
                    search_row,
                )
            } else {
                let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                query_all(
                    connection,
                    "SELECT threads.id, threads.title, item_id,
                        CASE WHEN item_id IS NULL THEN fts_entries.title
                            ELSE substr(body, max(1, instr(lower(body), lower(?3)) - 40), 120) END,
                        0.0, threads.updated_at
                     FROM fts_entries JOIN threads ON threads.id = fts_entries.thread_id
                     WHERE fts_entries.title LIKE '%' || ?1 || '%' ESCAPE '\\'
                        OR body LIKE '%' || ?1 || '%' ESCAPE '\\'
                     ORDER BY threads.updated_at DESC, fts_entries.rowid LIMIT ?2",
                    params![escaped, limit, query],
                    search_row,
                )
            }
        })
        .await
    }

    async fn db_stats(&self) -> CoreResult<DbStats> {
        self.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT (SELECT page_count FROM pragma_page_count) *
                        (SELECT page_size FROM pragma_page_size),
                    (SELECT count(*) FROM threads), (SELECT count(*) FROM runs),
                    (SELECT count(*) FROM turn_items),
                    (SELECT coalesce(sum(raw_bytes), 0) FROM turn_items),
                    (SELECT coalesce(sum(length(CAST(payload_text AS BLOB))), 0) FROM turn_items)
                        + (SELECT coalesce(sum(compressed_bytes), 0) FROM blob_store),
                    (SELECT count(*) FROM blob_store), (SELECT count(*) FROM attachments)",
                    [],
                    |row| {
                        let raw_payload_bytes: u64 = row.get(4)?;
                        let stored_payload_bytes: u64 = row.get(5)?;
                        Ok(DbStats {
                            db_size_bytes: row.get(0)?,
                            threads_count: row.get(1)?,
                            runs_count: row.get(2)?,
                            turn_items_count: row.get(3)?,
                            raw_payload_bytes,
                            stored_payload_bytes,
                            compression_ratio: if raw_payload_bytes == 0 {
                                1.0
                            } else {
                                stored_payload_bytes as f64 / raw_payload_bytes as f64
                            },
                            dedup_blobs_count: row.get(6)?,
                            attachments_count: row.get(7)?,
                        })
                    },
                )
                .map_err(storage_error)
        })
        .await
    }
}
