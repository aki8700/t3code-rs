use std::{
    error::Error,
    fmt::Display,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Params, Row, params, types::Type};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use t3_core::{
    Attachment, ContextHandoff, CoreError, CoreResult, DbStats, ModelSelection, NewTurnItem,
    OrchModelStat, Project, Run, RunStatus, SearchHit, StorageConfig, Store, Thread, ThreadStatus,
    TurnItem,
};
use uuid::Uuid;

const PREVIEW_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    storage_cfg: StorageConfig,
}

impl SqliteStore {
    pub fn open(db_path: impl AsRef<Path>, storage_cfg: StorageConfig) -> CoreResult<Self> {
        let db_path = db_path.as_ref();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(db_path)
                .map_err(storage_error)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(storage_error)?;
        }
        let connection = Connection::open(db_path).map_err(storage_error)?;
        Self::initialize(connection, storage_cfg, true)
    }

    pub fn open_in_memory(storage_cfg: StorageConfig) -> CoreResult<Self> {
        let connection = Connection::open_in_memory().map_err(storage_error)?;
        Self::initialize(connection, storage_cfg, false)
    }

    /// Reconcile runs left in flight when the previous server stopped.
    pub fn recover_interrupted_runs(&self) -> CoreResult<()> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| storage_error("SQLite connection lock poisoned"))?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let now = timestamp(Utc::now());
        transaction.execute(
            "UPDATE runs SET status = ?1, completed_at = ?2, error_message = ?3 WHERE status IN (?4, ?5)",
            params![
                encode(&RunStatus::Cancelled)?, now, "Interrupted by server restart",
                encode(&RunStatus::Running)?, encode(&RunStatus::RateLimited)?,
            ],
        ).map_err(storage_error)?;
        transaction
            .execute(
                "UPDATE threads SET status = ?1, updated_at = ?2 WHERE status IN (?3, ?4)",
                params![
                    encode(&ThreadStatus::Interrupted)?,
                    now,
                    encode(&ThreadStatus::Running)?,
                    encode(&ThreadStatus::RateLimited)?,
                ],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)
    }

    fn initialize(
        mut connection: Connection,
        storage_cfg: StorageConfig,
        file_backed: bool,
    ) -> CoreResult<Self> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        if file_backed {
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(storage_error)?;
        }
        connection
            .execute_batch(
                "PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA temp_store=MEMORY;",
            )
            .map_err(storage_error)?;
        let transaction = connection.transaction().map_err(storage_error)?;
        transaction
            .execute_batch(include_str!("schema.sql"))
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            storage_cfg,
        })
    }

    async fn with_connection<T, F>(&self, operation: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> CoreResult<T> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage_error("SQLite connection lock poisoned"))?;
            operation(&mut connection)
        })
        .await
        .map_err(storage_error)?
    }

    /// Imports active project titles and workspace roots from a legacy database.
    pub async fn import_projects_from_t3_sqlite(&self, legacy_db_path: &Path) -> CoreResult<usize> {
        let legacy_path = legacy_db_path.to_owned();
        self.with_connection(move |connection| {
            let legacy = Connection::open_with_flags(legacy_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(storage_error)?;
            let projects = query_all(
                &legacy,
                "SELECT title, workspace_root FROM projection_projects WHERE deleted_at IS NULL",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?;
            let transaction = connection.transaction().map_err(storage_error)?;
            let mut inserted = 0;
            for (title, workspace_root) in projects {
                inserted += transaction
                    .execute(
                        "INSERT INTO projects (id, title, workspace_root, created_at, updated_at)
                         SELECT ?1, ?2, ?3, ?4, ?4
                         WHERE NOT EXISTS (SELECT 1 FROM projects WHERE workspace_root = ?3)",
                        params![
                            Uuid::new_v4().to_string(),
                            title,
                            workspace_root,
                            timestamp(Utc::now())
                        ],
                    )
                    .map_err(storage_error)?;
            }
            transaction.commit().map_err(storage_error)?;
            tracing::debug!(inserted, "Imported legacy project metadata");
            Ok(inserted)
        })
        .await
    }

    async fn update_thread(
        &self,
        id: Uuid,
        query: &'static str,
        value: impl rusqlite::ToSql + Send + 'static,
    ) -> CoreResult<()> {
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
            let now = Utc::now();
            let project = Project {
                id: Uuid::new_v4(),
                title,
                workspace_root,
                default_model: None,
                created_at: now,
                updated_at: now,
            };
            connection
                .execute(
                    "INSERT INTO projects (id, title, workspace_root, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
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
        self.with_connection(|connection| {
            query_all(
                connection,
                "SELECT id, title, workspace_root, default_model, created_at, updated_at
                 FROM projects ORDER BY updated_at DESC, id",
                [],
                project_row,
            )
        })
        .await
    }

    async fn get_project(&self, id: Uuid) -> CoreResult<Project> {
        self.with_connection(move |connection| {
            query_required(
                connection,
                "SELECT id, title, workspace_root, default_model, created_at, updated_at
                 FROM projects WHERE id = ?1",
                "project",
                id,
                project_row,
            )
        })
        .await
    }

    async fn delete_project(&self, id: Uuid) -> CoreResult<()> {
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            require_change(
                transaction.execute("DELETE FROM projects WHERE id = ?1", [id.to_string()]),
                "project",
                id,
            )?;
            transaction
                .execute(
                    "DELETE FROM blob_store WHERE NOT EXISTS (
                    SELECT 1 FROM turn_items WHERE blob_hash = blob_store.hash
                 )",
                    [],
                )
                .map_err(storage_error)?;
            transaction.commit().map_err(storage_error)
        })
        .await
    }

    async fn create_thread(&self, thread: Thread) -> CoreResult<Thread> {
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            let id = thread.id.to_string();
            transaction.execute(
                "INSERT INTO threads (
                    id, project_id, parent_thread_id, relationship, title, model_selection,
                    runtime_mode, interaction_mode, status, worktree_path, branch, provider_session_id,
                    archived, pinned, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    id, thread.project_id.to_string(), thread.parent_thread_id.map(|id| id.to_string()),
                    thread.relationship, thread.title, encode(&thread.model_selection)?,
                    encode(&thread.runtime_mode)?, encode(&thread.interaction_mode)?,
                    encode(&thread.status)?, thread.worktree_path.as_deref().map(path_text).transpose()?,
                    thread.branch, thread.provider_session_id, thread.archived, thread.pinned,
                    timestamp(thread.created_at), timestamp(thread.updated_at),
                ],
            ).map_err(storage_error)?;
            transaction.execute(
                "INSERT INTO fts_entries (thread_id, title) VALUES (?1, ?2)",
                params![id, thread.title],
            ).map_err(storage_error)?;
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
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
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

    async fn update_thread_title(&self, id: Uuid, title: String) -> CoreResult<()> {
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
            connection.execute(
                "INSERT INTO runs (
                    id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
                    input_tokens, output_tokens, duration_ms, error_message, started_at, completed_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    run.id.to_string(), run.thread_id.to_string(), run.ordinal, encode(&run.provider)?,
                    run.model, run.effort, encode(&run.status)?, run.prompt, run.provider_session_id,
                    run.input_tokens, run.output_tokens, run.duration_ms, run.error_message,
                    timestamp(run.started_at), run.completed_at.map(timestamp),
                ],
            ).map_err(storage_error)?;
            Ok(run)
        })
        .await
    }

    async fn list_runs(&self, thread_id: Uuid) -> CoreResult<Vec<Run>> {
        self.with_connection(move |connection| {
            query_all(
                connection,
                "SELECT id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
                    input_tokens, output_tokens, duration_ms, error_message, started_at, completed_at
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
        duration_ms: u64,
        error_message: Option<String>,
    ) -> CoreResult<()> {
        self.with_connection(move |connection| {
            require_change(
                connection.execute(
                    "UPDATE runs SET status = ?2, provider_session_id = COALESCE(?3, provider_session_id),
                        input_tokens = ?4, output_tokens = ?5, duration_ms = ?6, error_message = ?7,
                        completed_at = ?8 WHERE id = ?1",
                    params![
                        run_id.to_string(), encode(&status)?, provider_session_id,
                        input_tokens, output_tokens, duration_ms, error_message, timestamp(Utc::now()),
                    ],
                ),
                "run",
                run_id,
            )
        })
        .await
    }

    async fn append_turn_item(&self, item: NewTurnItem) -> CoreResult<TurnItem> {
        let cfg = self.storage_cfg.clone();
        self.with_connection(move |connection| append_item(connection, item, &cfg))
            .await
    }

    async fn list_turn_items(
        &self,
        thread_id: Uuid,
        limit: usize,
        offset: usize,
    ) -> CoreResult<Vec<TurnItem>> {
        self.with_connection(move |connection| {
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

    async fn record_handoff(&self, handoff: ContextHandoff) -> CoreResult<ContextHandoff> {
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
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
        self.with_connection(move |connection| {
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

    async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>> {
        self.with_connection(|connection| {
            query_all(
                connection,
                "SELECT provider, model, count(*), sum(status = ?1), sum(status = ?2),
                    avg(status = ?1), coalesce(avg(duration_ms), 0.0),
                    coalesce(sum(input_tokens), 0), coalesce(sum(output_tokens), 0)
                 FROM runs GROUP BY provider, model ORDER BY count(*) DESC, provider, model",
                params![encode(&RunStatus::Completed)?, encode(&RunStatus::Failed)?],
                |row| {
                    Ok(OrchModelStat {
                        provider: decode(row, 0)?,
                        model: row.get(1)?,
                        total_runs: row.get(2)?,
                        completed_runs: row.get(3)?,
                        failed_runs: row.get(4)?,
                        success_rate: row.get(5)?,
                        avg_duration_ms: row.get(6)?,
                        total_input_tokens: row.get(7)?,
                        total_output_tokens: row.get(8)?,
                    })
                },
            )
        })
        .await
    }

    async fn db_stats(&self) -> CoreResult<DbStats> {
        self.with_connection(|connection| {
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

fn append_item(
    connection: &mut Connection,
    item: NewTurnItem,
    cfg: &StorageConfig,
) -> CoreResult<TurnItem> {
    let NewTurnItem {
        thread_id,
        run_id,
        kind,
        summary,
        mut payload,
        searchable_text,
    } = item;
    let mut text = encode(&payload)?;
    let raw_bytes = text.len();
    if raw_bytes > cfg.max_payload_bytes {
        payload = json!({
            "truncated": true,
            "original_bytes": raw_bytes,
            "preview": &text[..text.floor_char_boundary(PREVIEW_BYTES)],
        });
        text = encode(&payload)?;
    }

    let body = searchable_text
        .as_deref()
        .filter(|text| !text.is_empty())
        .or_else(|| summary.as_deref().filter(|text| !text.is_empty()))
        .or_else(|| {
            payload
                .as_str()
                .or_else(|| {
                    ["text", "content", "message", "summary", "output", "preview"]
                        .into_iter()
                        .find_map(|field| payload.get(field).and_then(Value::as_str))
                })
                .map(|text| &text[..text.floor_char_boundary(PREVIEW_BYTES)])
        });
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let (blob_hash, compressed_bytes) = if raw_bytes > cfg.zstd_threshold_bytes {
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        let data = zstd::bulk::compress(text.as_bytes(), cfg.zstd_level).map_err(storage_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO blob_store (hash, raw_bytes, compressed_bytes, zstd_data)
             VALUES (?1, ?2, ?3, ?4)",
                params![hash, text.len(), data.len(), data],
            )
            .map_err(storage_error)?;
        // A matching blob may have been compressed with a previous configuration.
        let stored_bytes = transaction
            .query_row(
                "SELECT compressed_bytes FROM blob_store WHERE hash = ?1",
                [&hash],
                |row| row.get::<_, usize>(0),
            )
            .map_err(storage_error)?;
        (Some(hash), Some(stored_bytes))
    } else {
        (None, None)
    };

    let id = Uuid::new_v4();
    let thread_key = thread_id.to_string();
    let created_at = Utc::now();
    let ordinal = transaction
        .query_row(
            "INSERT INTO turn_items (id, thread_id, run_id, ordinal, kind, summary, payload_text,
            blob_hash, raw_bytes, compressed_bytes, created_at)
         VALUES (?1, ?2, ?3,
            (SELECT coalesce(max(ordinal), 0) + 1 FROM turn_items WHERE thread_id = ?2),
            ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         RETURNING ordinal",
            params![
                id.to_string(),
                thread_key,
                run_id.to_string(),
                encode(&kind)?,
                summary,
                blob_hash.is_none().then_some(&text),
                blob_hash,
                raw_bytes,
                compressed_bytes,
                timestamp(created_at),
            ],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    if let Some(body) = body.filter(|body| !body.is_empty()) {
        transaction
            .execute(
                "INSERT INTO fts_entries (thread_id, item_id, body) VALUES (?1, ?2, ?3)",
                params![thread_key, id.to_string(), body],
            )
            .map_err(storage_error)?;
    }
    transaction.commit().map_err(storage_error)?;
    Ok(TurnItem {
        id,
        thread_id,
        run_id,
        ordinal,
        kind,
        summary,
        payload,
        raw_bytes,
        compressed_bytes,
        created_at,
    })
}

const THREAD_SELECT: &str =
    "SELECT id, project_id, parent_thread_id, relationship, title, model_selection, runtime_mode,
        interaction_mode, status, worktree_path, branch, provider_session_id, archived, pinned,
        created_at, updated_at FROM threads";

const ATTACHMENT_SELECT: &str =
    "SELECT id, thread_id, filename, mime_type, size_bytes, blake3_hash, storage_path, created_at
     FROM attachments";

fn project_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: parse(row, 0)?,
        title: row.get(1)?,
        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
        default_model: row
            .get::<_, Option<String>>(3)?
            .map(|text| serde_json::from_str(&text).map_err(|e| conversion_error(3, Type::Text, e)))
            .transpose()?,
        created_at: parse(row, 4)?,
        updated_at: parse(row, 5)?,
    })
}

fn thread_row(row: &Row<'_>) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: parse(row, 0)?,
        project_id: parse(row, 1)?,
        parent_thread_id: parse_optional(row, 2)?,
        relationship: row.get(3)?,
        title: row.get(4)?,
        model_selection: decode(row, 5)?,
        runtime_mode: decode(row, 6)?,
        interaction_mode: decode(row, 7)?,
        status: decode(row, 8)?,
        worktree_path: row.get::<_, Option<String>>(9)?.map(PathBuf::from),
        branch: row.get(10)?,
        provider_session_id: row.get(11)?,
        archived: row.get(12)?,
        pinned: row.get(13)?,
        created_at: parse(row, 14)?,
        updated_at: parse(row, 15)?,
    })
}

fn run_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: parse(row, 0)?,
        thread_id: parse(row, 1)?,
        ordinal: row.get(2)?,
        provider: decode(row, 3)?,
        model: row.get(4)?,
        effort: row.get(5)?,
        status: decode(row, 6)?,
        prompt: row.get(7)?,
        provider_session_id: row.get(8)?,
        input_tokens: row.get(9)?,
        output_tokens: row.get(10)?,
        duration_ms: row.get(11)?,
        error_message: row.get(12)?,
        started_at: parse(row, 13)?,
        completed_at: parse_optional(row, 14)?,
    })
}

fn item_row(row: &Row<'_>) -> rusqlite::Result<TurnItem> {
    let payload = match row.get::<_, Option<String>>(9)? {
        Some(text) => {
            serde_json::from_str(&text).map_err(|e| conversion_error(9, Type::Text, e))?
        }
        None => {
            let raw_bytes = row.get(10)?;
            let data: Vec<u8> = row.get(11)?;
            let data = zstd::bulk::decompress(&data, raw_bytes)
                .map_err(|e| conversion_error(11, Type::Blob, e))?;
            serde_json::from_slice(&data).map_err(|e| conversion_error(11, Type::Blob, e))?
        }
    };
    Ok(TurnItem {
        id: parse(row, 0)?,
        thread_id: parse(row, 1)?,
        run_id: parse(row, 2)?,
        ordinal: row.get(3)?,
        kind: decode(row, 4)?,
        summary: row.get(5)?,
        payload,
        raw_bytes: row.get(6)?,
        compressed_bytes: row.get(7)?,
        created_at: parse(row, 8)?,
    })
}

fn handoff_row(row: &Row<'_>) -> rusqlite::Result<ContextHandoff> {
    Ok(ContextHandoff {
        id: parse(row, 0)?,
        source_thread_id: parse(row, 1)?,
        target_thread_id: parse(row, 2)?,
        from_model: decode(row, 3)?,
        to_model: decode(row, 4)?,
        summary: row.get(5)?,
        created_at: parse(row, 6)?,
    })
}

fn attachment_row(row: &Row<'_>) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: parse(row, 0)?,
        thread_id: parse_optional(row, 1)?,
        filename: row.get(2)?,
        mime_type: row.get(3)?,
        size_bytes: row.get(4)?,
        blake3_hash: row.get(5)?,
        storage_path: PathBuf::from(row.get::<_, String>(6)?),
        created_at: parse(row, 7)?,
    })
}

fn search_row(row: &Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        thread_id: parse(row, 0)?,
        thread_title: row.get(1)?,
        item_id: parse_optional(row, 2)?,
        snippet: row.get(3)?,
        rank: row.get(4)?,
        updated_at: parse(row, 5)?,
    })
}

fn query_all<T>(
    connection: &Connection,
    sql: &str,
    params: impl Params,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> CoreResult<Vec<T>> {
    let mut stmt = connection.prepare_cached(sql).map_err(storage_error)?;
    let rows = stmt.query_map(params, map).map_err(storage_error)?;
    rows.collect::<rusqlite::Result<_>>().map_err(storage_error)
}

fn query_required<T>(
    connection: &Connection,
    sql: &str,
    entity: &str,
    id: Uuid,
    map: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
) -> CoreResult<T> {
    connection
        .query_row(sql, [id.to_string()], map)
        .optional()
        .map_err(storage_error)?
        .ok_or_else(|| not_found(entity, id))
}

fn require_change(changed: rusqlite::Result<usize>, entity: &str, id: Uuid) -> CoreResult<()> {
    if changed.map_err(storage_error)? == 0 {
        return Err(not_found(entity, id));
    }
    Ok(())
}

fn storage_error(error: impl Display) -> CoreError {
    CoreError::Storage(error.to_string())
}

fn not_found(entity: &str, id: Uuid) -> CoreError {
    CoreError::NotFound(format!("{entity} {id}"))
}

fn encode(value: &impl Serialize) -> CoreResult<String> {
    serde_json::to_string(value).map_err(storage_error)
}

fn decode<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| conversion_error(index, Type::Text, error))
}

fn parse<T: FromStr>(row: &Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T::Err: Error + Send + Sync + 'static,
{
    let text: String = row.get(index)?;
    text.parse()
        .map_err(|error| conversion_error(index, Type::Text, error))
}

fn parse_optional<T: FromStr>(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<T>>
where
    T::Err: Error + Send + Sync + 'static,
{
    row.get::<_, Option<String>>(index)?
        .map(|text| {
            text.parse()
                .map_err(|error| conversion_error(index, Type::Text, error))
        })
        .transpose()
}

fn conversion_error(
    index: usize,
    kind: Type,
    error: impl Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, kind, Box::new(error))
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn sql_limit(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn path_text(path: &Path) -> CoreResult<&str> {
    path.to_str()
        .ok_or_else(|| CoreError::InvalidArgument("path is not valid UTF-8".into()))
}

#[cfg(test)]
mod tests;
