use super::*;

pub(super) const THREAD_SELECT: &str =
    "SELECT id, project_id, parent_thread_id, relationship, title, model_selection, runtime_mode,
        interaction_mode, status, worktree_path, branch, provider_session_id, archived, pinned,
        created_at, updated_at FROM threads";

pub(super) const ATTACHMENT_SELECT: &str =
    "SELECT id, thread_id, filename, mime_type, size_bytes, blake3_hash, storage_path, created_at
     FROM attachments";

pub(super) fn project_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: parse(row, 0)?,
        title: row.get(1)?,
        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
        default_model: row
            .get::<_, Option<String>>(3)?
            .map(|text| serde_json::from_str(&text).map_err(|e| conversion_error(3, Type::Text, e)))
            .transpose()?,
        instructions: row.get(4)?,
        verify_command: row.get(5)?,
        created_at: parse(row, 6)?,
        updated_at: parse(row, 7)?,
    })
}

pub(super) fn thread_row(row: &Row<'_>) -> rusqlite::Result<Thread> {
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

pub(super) fn run_row(row: &Row<'_>) -> rusqlite::Result<Run> {
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
        cache_read_tokens: row.get(11)?,
        cache_creation_tokens: row.get(12)?,
        reasoning_tokens: row.get(13)?,
        duration_ms: row.get(14)?,
        error_message: row.get(15)?,
        started_at: parse(row, 16)?,
        completed_at: parse_optional(row, 17)?,
    })
}

pub(super) fn item_row(row: &Row<'_>) -> rusqlite::Result<TurnItem> {
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

pub(super) fn handoff_row(row: &Row<'_>) -> rusqlite::Result<ContextHandoff> {
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

pub(super) fn attachment_row(row: &Row<'_>) -> rusqlite::Result<Attachment> {
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

pub(super) fn search_row(row: &Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        thread_id: parse(row, 0)?,
        thread_title: row.get(1)?,
        item_id: parse_optional(row, 2)?,
        snippet: row.get(3)?,
        rank: row.get(4)?,
        updated_at: parse(row, 5)?,
    })
}

pub(super) fn query_all<T>(
    connection: &Connection,
    sql: &str,
    params: impl Params,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> CoreResult<Vec<T>> {
    let mut stmt = connection.prepare_cached(sql).map_err(storage_error)?;
    let rows = stmt.query_map(params, map).map_err(storage_error)?;
    rows.collect::<rusqlite::Result<_>>().map_err(storage_error)
}

pub(super) fn query_required<T>(
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

pub(super) fn require_change(
    changed: rusqlite::Result<usize>,
    entity: &str,
    id: Uuid,
) -> CoreResult<()> {
    if changed.map_err(storage_error)? == 0 {
        return Err(not_found(entity, id));
    }
    Ok(())
}

pub(super) fn storage_error(error: impl Display) -> CoreError {
    CoreError::Storage(error.to_string())
}

pub(super) fn lock_connection(
    connection: &Mutex<Connection>,
) -> CoreResult<MutexGuard<'_, Connection>> {
    connection
        .lock()
        .map_err(|_| storage_error("SQLite connection lock poisoned"))
}

pub(super) fn not_found(entity: &str, id: Uuid) -> CoreError {
    CoreError::NotFound(format!("{entity} {id}"))
}

pub(super) fn encode(value: &impl Serialize) -> CoreResult<String> {
    serde_json::to_string(value).map_err(storage_error)
}

pub(super) fn decode<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| conversion_error(index, Type::Text, error))
}

pub(super) fn parse<T: FromStr>(row: &Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T::Err: Error + Send + Sync + 'static,
{
    let text: String = row.get(index)?;
    text.parse()
        .map_err(|error| conversion_error(index, Type::Text, error))
}

pub(super) fn parse_optional<T: FromStr>(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<T>>
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

pub(super) fn conversion_error(
    index: usize,
    kind: Type,
    error: impl Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, kind, Box::new(error))
}

pub(super) fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

pub(super) fn sql_limit(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn path_text(path: &Path) -> CoreResult<&str> {
    path.to_str()
        .ok_or_else(|| CoreError::InvalidArgument("path is not valid UTF-8".into()))
}

pub(super) fn trimmed_optional(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}
