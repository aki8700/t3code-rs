use super::*;

pub(super) fn insert_thread(transaction: &Connection, thread: &Thread) -> CoreResult<()> {
    let id = thread.id.to_string();
    transaction
        .prepare_cached(
            "INSERT INTO threads (
            id, project_id, parent_thread_id, relationship, title, model_selection,
            runtime_mode, interaction_mode, status, worktree_path, branch, provider_session_id,
            archived, pinned, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .map_err(storage_error)?
        .execute(params![
            id,
            thread.project_id.to_string(),
            thread.parent_thread_id.map(|id| id.to_string()),
            thread.relationship,
            thread.title,
            encode(&thread.model_selection)?,
            encode(&thread.runtime_mode)?,
            encode(&thread.interaction_mode)?,
            encode(&thread.status)?,
            thread.worktree_path.as_deref().map(path_text).transpose()?,
            thread.branch,
            thread.provider_session_id,
            thread.archived,
            thread.pinned,
            timestamp(thread.created_at),
            timestamp(thread.updated_at),
        ])
        .map_err(storage_error)?;
    transaction
        .prepare_cached("INSERT INTO fts_entries (thread_id, title) VALUES (?1, ?2)")
        .map_err(storage_error)?
        .execute(params![id, thread.title])
        .map_err(storage_error)?;
    Ok(())
}

pub(super) fn insert_run(connection: &Connection, run: &Run) -> CoreResult<()> {
    connection
        .prepare_cached(
            "INSERT INTO runs (
            id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, reasoning_tokens,
            duration_ms, error_message, started_at, completed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .map_err(storage_error)?
        .execute(params![
            run.id.to_string(),
            run.thread_id.to_string(),
            run.ordinal,
            encode(&run.provider)?,
            run.model,
            run.effort,
            encode(&run.status)?,
            run.prompt,
            run.provider_session_id,
            run.input_tokens,
            run.output_tokens,
            run.cache_read_tokens,
            run.cache_creation_tokens,
            run.reasoning_tokens,
            run.duration_ms,
            run.error_message,
            timestamp(run.started_at),
            run.completed_at.map(timestamp),
        ])
        .map_err(storage_error)?;
    Ok(())
}

pub(super) fn append_item(
    connection: &mut Connection,
    item: NewTurnItem,
    cfg: &StorageConfig,
) -> CoreResult<TurnItem> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let saved = insert_item(
        &transaction,
        item,
        cfg,
        ItemPosition {
            id: Uuid::new_v4(),
            ordinal: None,
            created_at: Utc::now(),
        },
        true,
    )?;
    transaction.commit().map_err(storage_error)?;
    Ok(saved)
}

pub(super) struct ItemPosition {
    pub(super) id: Uuid,
    pub(super) ordinal: Option<i64>,
    pub(super) created_at: DateTime<Utc>,
}

pub(super) fn insert_item(
    connection: &Connection,
    item: NewTurnItem,
    cfg: &StorageConfig,
    position: ItemPosition,
    index_payload: bool,
) -> CoreResult<TurnItem> {
    let ItemPosition {
        id,
        ordinal,
        created_at,
    } = position;
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
        (payload, text) = prune::prune_payload(payload, text, cfg.max_payload_bytes)?;
    }

    let body = searchable_text
        .as_deref()
        .filter(|text| !text.is_empty())
        .or_else(|| summary.as_deref().filter(|text| !text.is_empty()))
        .or_else(|| {
            if !index_payload {
                return None;
            }
            payload.as_str().or_else(|| {
                ["text", "content", "message", "summary", "output", "preview"]
                    .into_iter()
                    .find_map(|field| payload.get(field).and_then(Value::as_str))
            })
        })
        .map(|text| &text[..text.floor_char_boundary(PREVIEW_BYTES)]);
    let (blob_hash, compressed_bytes) = store_item_blob(connection, &text, raw_bytes, cfg)?;

    let thread_key = thread_id.to_string();
    let ordinal = connection
        .prepare_cached(
            "INSERT INTO turn_items (id, thread_id, run_id, ordinal, kind, summary, payload_text,
            blob_hash, raw_bytes, compressed_bytes, created_at)
         VALUES (?1, ?2, ?3,
            coalesce(?4, (SELECT coalesce(max(ordinal), 0) + 1 FROM turn_items WHERE thread_id = ?2)),
            ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         RETURNING ordinal",
        ).map_err(storage_error)?.query_row(
            params![
                id.to_string(),
                thread_key,
                run_id.to_string(),
                ordinal,
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
        connection
            .prepare_cached(
                "INSERT INTO fts_entries (thread_id, item_id, body) VALUES (?1, ?2, ?3)",
            )
            .map_err(storage_error)?
            .execute(params![thread_key, id.to_string(), body])
            .map_err(storage_error)?;
        let fts_rowid: i64 = connection
            .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
            .map_err(storage_error)?;
        connection
            .prepare_cached(
                "INSERT INTO fts_item_rows (item_id, thread_id, fts_rowid) VALUES (?1, ?2, ?3)",
            )
            .map_err(storage_error)?
            .execute(params![id.to_string(), thread_key, fts_rowid])
            .map_err(storage_error)?;
    }
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

pub(super) fn store_item_blob(
    connection: &Connection,
    text: &str,
    raw_bytes: usize,
    cfg: &StorageConfig,
) -> CoreResult<(Option<String>, Option<usize>)> {
    if raw_bytes > cfg.zstd_threshold_bytes {
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        // A matching blob may have been compressed with a previous configuration.
        let stored_bytes = connection
            .prepare_cached("SELECT compressed_bytes FROM blob_store WHERE hash = ?1")
            .map_err(storage_error)?
            .query_row([&hash], |row| row.get::<_, usize>(0))
            .optional()
            .map_err(storage_error)?;
        let compressed_bytes = if let Some(stored_bytes) = stored_bytes {
            stored_bytes
        } else {
            let data =
                zstd::bulk::compress(text.as_bytes(), cfg.zstd_level).map_err(storage_error)?;
            connection
                .prepare_cached(
                    "INSERT INTO blob_store (hash, raw_bytes, compressed_bytes, zstd_data)
                 VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(storage_error)?
                .execute(params![hash, text.len(), data.len(), data])
                .map_err(storage_error)?;
            data.len()
        };
        Ok((Some(hash), Some(compressed_bytes)))
    } else {
        Ok((None, None))
    }
}
