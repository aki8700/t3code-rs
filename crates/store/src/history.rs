use std::{collections::HashMap, io::Read};

use serde_json::json;
use t3_core::{InteractionMode, ProviderId, RuntimeMode, TurnItemKind};

use super::*;

const PROJECTS: &str = "projection_projects";
const THREADS: &str = "orchestration_v2_projection_threads";
const RUNS: &str = "orchestration_v2_projection_runs";
const ITEMS: &str = "orchestration_v2_projection_turn_items";
const SESSIONS: &str = "orchestration_v2_projection_provider_threads";
const SUBAGENTS: &str = "orchestration_v2_projection_subagents";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ImportHistoryReport {
    pub projects_imported: usize,
    pub threads_imported: usize,
    pub runs_imported: usize,
    pub items_imported: usize,
    pub db_bytes_after: u64,
}

impl SqliteStore {
    /// Import a consistent snapshot of the legacy history projections.
    pub async fn import_history_from_t3_sqlite(
        &self,
        legacy_db_path: &Path,
    ) -> CoreResult<ImportHistoryReport> {
        let path = legacy_db_path.to_owned();
        let cfg = self.storage_cfg.clone();
        self.with_writer(move |connection| {
            let mut legacy = open_legacy_source(&path)?;
            let source = legacy.transaction().map_err(storage_error)?;
            for table in [PROJECTS, THREADS, RUNS, ITEMS] {
                if !has_table(&source, table)? {
                    return Err(storage_error(format!(
                        "missing legacy history table {table}"
                    )));
                }
            }
            let destination = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            let mut report = ImportHistoryReport::default();
            let projects = import_projects(&source, &destination, &mut report)?;
            let sessions = read_sessions(&source)?;
            let threads = import_threads(&source, &destination, projects, &sessions, &mut report)?;
            let runs = import_runs(&source, &destination, &threads, &sessions, &mut report)?;
            import_items(&source, &destination, &threads, &runs, &cfg, &mut report)?;
            destination.commit().map_err(storage_error)?;
            report.db_bytes_after = connection
                .query_row(
                    "SELECT (SELECT page_count FROM pragma_page_count) *
                        (SELECT page_size FROM pragma_page_size)",
                    [],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            Ok(report)
        })
        .await
    }
}

fn open_legacy_source(path: &Path) -> CoreResult<Connection> {
    let path = fs::canonicalize(path).map_err(storage_error)?;
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    if Path::new(&wal).try_exists().map_err(storage_error)? {
        return open_gc_read_only(&path, false);
    }

    // SQLite can create WAL sidecars even for a read-only connection. Use the
    // immutable reader for a checkpointed WAL database with no existing WAL.
    let mut header = [0u8; 20];
    File::open(&path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(storage_error)?;
    if header[18..20] == [2, 2] {
        open_gc_read_only(&path, true)
    } else {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(storage_error)
    }
}

fn import_projects(
    source: &Connection,
    destination: &Connection,
    report: &mut ImportHistoryReport,
) -> CoreResult<HashMap<String, Uuid>> {
    let id_column = if has_column(source, PROJECTS, "project_id")? {
        "project_id"
    } else {
        "id"
    };
    let projects = legacy_query_all(
        source,
        PROJECTS,
        &format!(
            "SELECT {id_column}, title, workspace_root, created_at, updated_at
         FROM {PROJECTS} WHERE deleted_at IS NULL ORDER BY {id_column}"
        ),
        [],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                parse::<DateTime<Utc>>(row, 3)?,
                parse::<DateTime<Utc>>(row, 4)?,
            ))
        },
    )?;
    let mut mapping = HashMap::new();
    let mut find = destination
        .prepare_cached(
            "SELECT id FROM projects WHERE workspace_root = ?1 ORDER BY created_at, id LIMIT 1",
        )
        .map_err(storage_error)?;
    for (legacy_id, title, root, created, updated) in projects {
        let existing = find
            .query_row([&root], |row| parse(row, 0))
            .optional()
            .map_err(storage_error)?;
        let project_id = if let Some(existing) = existing {
            existing
        } else {
            let id = Uuid::new_v4();
            report.projects_imported += destination
                .execute(
                    "INSERT INTO projects (id, title, workspace_root, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        id.to_string(),
                        title,
                        root,
                        timestamp(created),
                        timestamp(updated)
                    ],
                )
                .map_err(storage_error)?;
            id
        };
        mapping.insert(legacy_id, project_id);
    }
    Ok(mapping)
}

#[derive(Default)]
struct Sessions {
    by_thread: HashMap<String, String>,
    by_provider_thread: HashMap<String, String>,
}

fn read_sessions(source: &Connection) -> CoreResult<Sessions> {
    let mut sessions = Sessions::default();
    if has_table(source, SESSIONS)? {
        let id = if has_column(source, SESSIONS, "provider_thread_id")? {
            "provider_thread_id"
        } else {
            "CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.id') END"
        };
        let updated = if has_column(source, SESSIONS, "updated_at")? {
            "updated_at"
        } else {
            "CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.updatedAt') END"
        };
        for (id, thread_id, payload) in legacy_query_all(
            source,
            SESSIONS,
            &format!(
                "SELECT {id}, thread_id, payload_json FROM {SESSIONS}
             ORDER BY {updated} DESC, 1"
            ),
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    decode::<Value>(row, 2)?,
                ))
            },
        )? {
            if let Some(session_id) = str_field(&payload, "providerSessionId")
                .or_else(|| payload["nativeThreadRef"].as_str())
                .or_else(|| str_field(&payload["nativeThreadRef"], "nativeId"))
                .filter(|id| !id.is_empty())
            {
                if let Some(id) = id {
                    sessions
                        .by_provider_thread
                        .insert(id, session_id.to_owned());
                }
                if let Some(thread_id) = thread_id {
                    sessions
                        .by_thread
                        .entry(thread_id)
                        .or_insert_with(|| session_id.to_owned());
                }
            }
        }
    }
    Ok(sessions)
}

fn import_threads(
    source: &Connection,
    destination: &Connection,
    projects: HashMap<String, Uuid>,
    sessions: &Sessions,
    report: &mut ImportHistoryReport,
) -> CoreResult<HashMap<String, Thread>> {
    let parents: HashMap<String, String> = if has_table(source, SUBAGENTS)? {
        let parent = if has_column(source, SUBAGENTS, "parent_thread_id")? {
            "parent_thread_id"
        } else {
            "thread_id"
        };
        legacy_query_all(
            source,
            SUBAGENTS,
            &format!(
                "SELECT child_thread_id, {parent} FROM {SUBAGENTS}
             WHERE child_thread_id IS NOT NULL AND {parent} IS NOT NULL
             ORDER BY child_thread_id, {parent}"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
        .into_iter()
        .collect()
    } else {
        HashMap::new()
    };
    let mut query = source
        .prepare(&format!(
            "SELECT thread_id, project_id, title, default_provider, runtime_mode, interaction_mode,
                active_provider_thread_id, created_at, updated_at, archived_at, payload_json,
                (SELECT status FROM {RUNS} WHERE thread_id = thread.thread_id
                 ORDER BY ordinal DESC LIMIT 1),
                EXISTS(SELECT 1 FROM {ITEMS} WHERE thread_id = thread.thread_id)
         FROM {THREADS} thread WHERE deleted_at IS NULL"
        ))
        .map_err(storage_error)?;
    let mut rows = query.query([]).map_err(storage_error)?;
    let mut threads = HashMap::new();
    let mut lineage = Vec::new();
    while let Some(row) = rows.next().map_err(storage_error)? {
        let Some((id, project)) = legacy_row(THREADS, row, |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) else {
            continue;
        };
        let Some(&project_id) = projects.get(&project) else {
            continue;
        };
        let Some((thread, parent, fallback, relationship)) = legacy_row(THREADS, row, |row| {
            let payload: Value = decode(row, 10)?;
            let active_provider_thread: Option<String> = row.get(6)?;
            let run_status: Option<String> = row.get(11)?;
            let status = match str_field(&payload, "status").or(run_status.as_deref()) {
                Some("failed") => ThreadStatus::Failed,
                Some("interrupted") => ThreadStatus::Interrupted,
                _ if row.get::<_, bool>(12)? => ThreadStatus::Completed,
                _ => ThreadStatus::Idle,
            };
            let parent = str_field(&payload["lineage"], "parentThreadId");
            let fallback = parents.get(&id).map(String::as_str);
            let relationship = str_field(&payload["lineage"], "relationshipToParent");
            let thread = Thread {
                id: legacy_uuid(&id),
                project_id,
                parent_thread_id: None,
                relationship: None,
                title: row.get(2)?,
                model_selection: model_selection(
                    &payload["modelSelection"],
                    provider(&row.get::<_, String>(3)?),
                    None,
                ),
                runtime_mode: match row.get::<_, String>(4)?.as_str() {
                    "full-access" => RuntimeMode::FullAccess,
                    "auto" | "auto-accept-edits" => RuntimeMode::AutoAcceptEdits,
                    _ => RuntimeMode::ApprovalRequired,
                },
                interaction_mode: if row.get::<_, String>(5)? == "plan" {
                    InteractionMode::Plan
                } else {
                    InteractionMode::Default
                },
                status,
                worktree_path: str_field(&payload, "worktreePath").map(PathBuf::from),
                branch: str_field(&payload, "branch").map(str::to_owned),
                provider_session_id: active_provider_thread
                    .as_ref()
                    .and_then(|id| sessions.by_provider_thread.get(id))
                    .or_else(|| sessions.by_thread.get(&id))
                    .cloned(),
                archived: str_field(&payload, "archivedAt").is_some()
                    || row.get::<_, Option<String>>(9)?.is_some(),
                pinned: str_field(&payload, "pinnedAt").is_some(),
                created_at: date_field(&payload, "createdAt").map_or_else(|| parse(row, 7), Ok)?,
                updated_at: date_field(&payload, "updatedAt").map_or_else(|| parse(row, 8), Ok)?,
            };
            Ok((
                thread,
                parent.map(str::to_owned),
                fallback.map(str::to_owned),
                relationship.map(str::to_owned),
            ))
        }) else {
            continue;
        };
        if !destination_exists(destination, "threads", thread.id)? {
            insert_thread(destination, &thread)?;
            report.threads_imported += 1;
        }
        if parent.is_some() || fallback.is_some() {
            lineage.push((thread.id, parent, fallback, relationship));
        }
        threads.insert(id, thread);
    }
    // All parent rows exist before any relationship is set, regardless of import order.
    for (child, parent, fallback, relationship) in lineage {
        let linked = parent
            .as_ref()
            .and_then(|id| threads.get(id))
            .map(|thread| (thread, relationship.as_deref()))
            .or_else(|| {
                fallback
                    .as_ref()
                    .and_then(|id| threads.get(id))
                    .map(|thread| (thread, Some("subagent")))
            });
        if let Some((parent, relationship)) = linked.filter(|(parent, _)| parent.id != child) {
            destination
                .execute(
                    "UPDATE threads SET parent_thread_id = ?2, relationship = ?3
                     WHERE id = ?1 AND parent_thread_id IS NULL",
                    params![child.to_string(), parent.id.to_string(), relationship],
                )
                .map_err(storage_error)?;
        }
    }
    Ok(threads)
}

fn import_runs(
    source: &Connection,
    destination: &Connection,
    threads: &HashMap<String, Thread>,
    sessions: &Sessions,
    report: &mut ImportHistoryReport,
) -> CoreResult<HashMap<String, (Uuid, Uuid)>> {
    let mut by_id = HashMap::new();
    let mut find_prompt = source
        .prepare(&format!(
            "SELECT json_extract(payload_json, '$.text') FROM {ITEMS}
         WHERE run_id = ?1 AND thread_id = ?2 AND type = 'user_message'
            AND CASE WHEN json_valid(payload_json) THEN json_type(payload_json, '$.text') = 'text' END
         ORDER BY (json_extract(payload_json, '$.messageId') IS ?3) DESC, ordinal LIMIT 1"
        ))
        .map_err(storage_error)?;
    let mut query = source
        .prepare(&format!(
            "SELECT run_id, thread_id, ordinal, provider, provider_thread_id, status, requested_at,
                completed_at, payload_json
         FROM {RUNS} AS run ORDER BY thread_id, ordinal"
        ))
        .map_err(storage_error)?;
    let mut rows = query.query([]).map_err(storage_error)?;
    while let Some(row) = rows.next().map_err(storage_error)? {
        let Some((legacy_id, thread_id)) = legacy_row(RUNS, row, |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) else {
            continue;
        };
        let Some(thread) = threads.get(&thread_id) else {
            continue;
        };
        let id = uuid_v5(legacy_id.as_bytes());
        if let Some(owner) = run_owner(destination, id)? {
            if owner == thread.id {
                by_id.insert(legacy_id, (id, owner));
            }
            continue;
        }
        let Some((
            payload,
            provider,
            started_at,
            completed_at,
            provider_thread_id,
            ordinal,
            status,
        )) = legacy_row(RUNS, row, |row| {
            let payload = decode(row, 8)?;
            let started_at = date_field(&payload, "startedAt").map_or_else(|| parse(row, 6), Ok)?;
            let completed_at = match date_field(&payload, "completedAt") {
                Some(date) => Some(date),
                None => parse_optional(row, 7)?,
            };
            Ok((
                payload,
                provider(&row.get::<_, String>(3)?),
                started_at,
                completed_at,
                row.get::<_, Option<String>>(4)?,
                row.get(2)?,
                row.get::<_, String>(5)?,
            ))
        })
        else {
            continue;
        };
        let model = model_selection(
            &payload["modelSelection"],
            provider,
            Some(&thread.model_selection),
        );
        let prompt: Option<String> = find_prompt
            .query_row(
                params![legacy_id, thread_id, str_field(&payload, "userMessageId")],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .flatten();
        let run = Run {
            id,
            thread_id: thread.id,
            ordinal,
            provider: model.provider,
            model: model.model,
            effort: model.effort,
            status: match status.as_str() {
                "completed" => RunStatus::Completed,
                "failed" => RunStatus::Failed,
                _ => RunStatus::Cancelled,
            },
            prompt: str_field(&payload, "prompt")
                .map(str::to_owned)
                .or(prompt)
                .unwrap_or_default(),
            provider_session_id: str_field(&payload, "providerSessionId")
                .map(str::to_owned)
                .or_else(|| {
                    provider_thread_id
                        .as_ref()
                        .and_then(|id| sessions.by_provider_thread.get(id))
                        .cloned()
                }),
            input_tokens: payload["inputTokens"]
                .as_u64()
                .or_else(|| payload["usage"]["inputTokens"].as_u64()),
            output_tokens: payload["outputTokens"]
                .as_u64()
                .or_else(|| payload["usage"]["outputTokens"].as_u64()),
            cache_read_tokens: payload["cacheReadTokens"]
                .as_u64()
                .or_else(|| payload["usage"]["cacheReadTokens"].as_u64()),
            cache_creation_tokens: payload["cacheCreationTokens"]
                .as_u64()
                .or_else(|| payload["usage"]["cacheCreationTokens"].as_u64()),
            reasoning_tokens: payload["reasoningTokens"]
                .as_u64()
                .or_else(|| payload["usage"]["reasoningTokens"].as_u64()),
            duration_ms: payload["durationMs"].as_u64().or_else(|| {
                completed_at.and_then(|end| (end - started_at).num_milliseconds().try_into().ok())
            }),
            error_message: str_field(&payload, "errorMessage").map(str::to_owned),
            started_at,
            completed_at,
        };
        if [run.input_tokens, run.output_tokens, run.duration_ms]
            .into_iter()
            .flatten()
            .any(|value| i64::try_from(value).is_err())
        {
            tracing::warn!(table = RUNS, "Skipping malformed legacy history row");
            continue;
        }
        insert_run(destination, &run)?;
        by_id.insert(legacy_id, (id, thread.id));
        report.runs_imported += 1;
    }
    Ok(by_id)
}

fn import_items(
    source: &Connection,
    destination: &Connection,
    threads: &HashMap<String, Thread>,
    runs: &HashMap<String, (Uuid, Uuid)>,
    cfg: &StorageConfig,
    report: &mut ImportHistoryReport,
) -> CoreResult<()> {
    let mut query = source
        .prepare(&format!(
            "SELECT turn_item_id, thread_id, run_id, ordinal, type, updated_at, payload_json
         FROM {ITEMS} ORDER BY thread_id, ordinal, turn_item_id"
        ))
        .map_err(storage_error)?;
    let mut rows = query.query([]).map_err(storage_error)?;
    let mut synthetic = HashMap::new();
    let mut existing_ordinals = destination
        .prepare_cached("SELECT coalesce(max(ordinal), 0) FROM turn_items WHERE thread_id = ?1")
        .map_err(storage_error)?;
    let mut last_ordinal = HashMap::new();
    while let Some(row) = rows.next().map_err(storage_error)? {
        let Some((legacy_id, thread_id, kind)) = legacy_row(ITEMS, row, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(4)?,
            ))
        }) else {
            continue;
        };
        let Some(thread) = threads.get(&thread_id) else {
            continue;
        };
        let id = uuid_v5(legacy_id.as_bytes());
        let has_child_link = matches!(kind.as_str(), "subagent" | "notification");
        if destination_exists(destination, "turn_items", id)? {
            if has_child_link {
                repair_child_link(destination, id, thread.id, cfg)?;
            }
            continue;
        }
        let Some((mut payload, created_at, legacy_run_id, legacy_ordinal)) =
            legacy_row(ITEMS, row, |row| {
                let payload = decode(row, 6)?;
                let created_at = date_field(&payload, "createdAt")
                    .or_else(|| date_field(&payload, "startedAt"))
                    .map_or_else(|| parse(row, 5), Ok)?;
                Ok((
                    payload,
                    created_at,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
        else {
            continue;
        };
        if has_child_link {
            rewrite_child_links(&mut payload);
        }
        let ordinal = match last_ordinal.entry(thread.id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(
                existing_ordinals
                    .query_row([thread.id.to_string()], |row| row.get::<_, i64>(0))
                    .map_err(storage_error)?,
            ),
        };
        let Some(next) = ordinal
            .checked_add(1)
            .map(|next| next.max(legacy_ordinal))
            .filter(|&next| next < i64::MAX)
        else {
            tracing::warn!(
                table = ITEMS,
                "Skipping legacy history item with out-of-range ordinal"
            );
            continue;
        };
        let run_id = match legacy_run_id
            .as_ref()
            .and_then(|id| runs.get(id))
            .filter(|(_, owner)| *owner == thread.id)
        {
            Some((id, _)) => *id,
            None => match synthetic.entry(thread.id) {
                std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let run_id =
                        synthetic_run(destination, &thread_id, thread, created_at, report)?;
                    *entry.insert(run_id)
                }
            },
        };
        *ordinal = next;
        let item = turn_item(thread.id, run_id, &kind, payload);
        insert_item(
            destination,
            item,
            cfg,
            ItemPosition {
                id,
                ordinal: Some(*ordinal),
                created_at,
            },
            false,
        )?;
        report.items_imported += 1;
    }
    Ok(())
}

fn run_owner(connection: &Connection, run_id: Uuid) -> CoreResult<Option<Uuid>> {
    connection
        .prepare_cached("SELECT thread_id FROM runs WHERE id = ?1")
        .map_err(storage_error)?
        .query_row([run_id.to_string()], |row| parse(row, 0))
        .optional()
        .map_err(storage_error)
}

fn rewrite_child_links(payload: &mut Value) -> bool {
    let mut changed = false;
    for pointer in ["/childThreadId", "/source/childThreadId"] {
        if let Some(field) = payload.pointer_mut(pointer)
            && let Some(old) = field.as_str().filter(|id| !id.is_empty())
        {
            let id = legacy_uuid(old).to_string();
            if id != old {
                *field = Value::String(id);
                changed = true;
            }
        }
    }
    changed
}

fn repair_child_link(
    destination: &Connection,
    id: Uuid,
    thread_id: Uuid,
    cfg: &StorageConfig,
) -> CoreResult<()> {
    let item = destination
        .prepare_cached(
            "SELECT item.id, item.thread_id, item.run_id, item.ordinal, item.kind, item.summary,
                item.raw_bytes, item.compressed_bytes, item.created_at, item.payload_text,
                blob.raw_bytes, blob.zstd_data
             FROM turn_items AS item LEFT JOIN blob_store AS blob ON blob.hash = item.blob_hash
             WHERE item.id = ?1 AND item.thread_id = ?2",
        )
        .map_err(storage_error)?
        .query_row(params![id.to_string(), thread_id.to_string()], item_row)
        .optional()
        .map_err(storage_error)?;
    if let Some(mut item) = item
        && rewrite_child_links(&mut item.payload)
    {
        let mut text = encode(&item.payload)?;
        let raw_bytes = text.len();
        if raw_bytes > cfg.max_payload_bytes {
            (_, text) = prune::prune_payload(item.payload, text, cfg.max_payload_bytes)?;
        }
        let (blob_hash, compressed_bytes) = store_item_blob(destination, &text, raw_bytes, cfg)?;
        destination
            .execute(
                "UPDATE turn_items SET payload_text = ?2, blob_hash = ?3, raw_bytes = ?4,
                    compressed_bytes = ?5 WHERE id = ?1",
                params![
                    id.to_string(),
                    blob_hash.is_none().then_some(&text),
                    blob_hash,
                    raw_bytes,
                    compressed_bytes
                ],
            )
            .map_err(storage_error)?;
    }
    Ok(())
}

fn synthetic_run(
    destination: &Connection,
    legacy_thread_id: &str,
    thread: &Thread,
    created_at: DateTime<Utc>,
    report: &mut ImportHistoryReport,
) -> CoreResult<Uuid> {
    let name = format!("legacy-run:{legacy_thread_id}");
    let mut id = uuid_v5(name.as_bytes());
    for suffix in 1.. {
        match run_owner(destination, id)? {
            Some(owner) if owner == thread.id => return Ok(id),
            Some(_) => id = uuid_v5(format!("{name}:{suffix}").as_bytes()),
            None => break,
        }
    }
    let last_ordinal: i64 = destination
        .query_row(
            "SELECT coalesce(max(ordinal), 0) FROM runs WHERE thread_id = ?1",
            [thread.id.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    insert_run(
        destination,
        &Run {
            id,
            thread_id: thread.id,
            ordinal: last_ordinal.saturating_add(1),
            provider: thread.model_selection.provider,
            model: thread.model_selection.model.clone(),
            effort: thread.model_selection.effort.clone(),
            status: RunStatus::Completed,
            prompt: String::new(),
            provider_session_id: thread.provider_session_id.clone(),
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            duration_ms: None,
            error_message: None,
            started_at: created_at,
            completed_at: Some(created_at),
        },
    )?;
    report.runs_imported += 1;
    Ok(id)
}

fn turn_item(thread_id: Uuid, run_id: Uuid, kind: &str, mut payload: Value) -> NewTurnItem {
    use TurnItemKind::*;
    let mut summary = str_field(&payload, "title").map(str::to_owned);
    let mut searchable_text = None;
    let (kind, payload) = match kind {
        "user_message" | "assistant_message" | "reasoning" => {
            let text = str_field(&payload, "text").unwrap_or_default();
            let item_kind = match kind {
                "user_message" => UserMessage,
                "assistant_message" => AssistantMessage,
                _ => Reasoning,
            };
            if item_kind != Reasoning {
                let mut indexed = summary.clone().unwrap_or_default();
                if !indexed.is_empty() {
                    indexed.push('\n');
                }
                indexed.push_str(text);
                searchable_text = Some(indexed);
            }
            (item_kind, json!({"text": text}))
        }
        "command_execution" => {
            summary = summary.or_else(|| {
                str_field(&payload, "input")
                    .and_then(|text| text.lines().next())
                    .map(str::to_owned)
            });
            (
                CommandExecution,
                remap_payload(
                    payload,
                    [
                        ("input", "command"),
                        ("output", "output"),
                        ("exitCode", "exitCode"),
                    ],
                ),
            )
        }
        "dynamic_tool" => {
            summary = summary.or_else(|| str_field(&payload, "toolName").map(str::to_owned));
            (
                ToolCall,
                remap_payload(
                    payload,
                    [
                        ("toolName", "name"),
                        ("title", "title"),
                        ("input", "input"),
                        ("output", "output"),
                    ],
                ),
            )
        }
        "file_change" => {
            let path = str_field(&payload, "fileName")
                .or_else(|| str_field(&payload["changes"][0], "path"));
            summary = path.map(str::to_owned).or(summary);
            let mut change = json!({"path": path, "diff": payload["diffStr"]});
            if payload["changes"].is_array() {
                change["changes"] = payload["changes"].take();
            }
            (FileChange, change)
        }
        _ => {
            if let Some(fields) = payload.as_object_mut() {
                for key in [
                    "id",
                    "threadId",
                    "runId",
                    "nodeId",
                    "providerThreadId",
                    "providerTurnId",
                    "nativeItemRef",
                    "parentItemId",
                    "ordinal",
                    "startedAt",
                    "completedAt",
                    "updatedAt",
                ] {
                    fields.remove(key);
                }
            }
            let item_kind = match kind {
                "subagent" => Subagent,
                "handoff" => Handoff,
                _ => SystemNotice,
            };
            (item_kind, payload)
        }
    };
    NewTurnItem {
        thread_id,
        run_id,
        kind,
        summary,
        payload,
        searchable_text,
    }
}

fn remap_payload<const N: usize>(payload: Value, fields: [(&str, &str); N]) -> Value {
    let Value::Object(mut source) = payload else {
        return Value::Null;
    };
    Value::Object(
        fields
            .into_iter()
            .map(|(old, new)| (new.to_owned(), source.remove(old).unwrap_or(Value::Null)))
            .collect(),
    )
}

fn model_selection(
    payload: &Value,
    provider: ProviderId,
    fallback: Option<&ModelSelection>,
) -> ModelSelection {
    let effort = payload["options"].as_array().and_then(|options| {
        options
            .iter()
            .find_map(|option| match str_field(option, "id") {
                Some("effort" | "thought_level" | "reasoningEffort" | "variant") => {
                    str_field(option, "value")
                }
                _ => None,
            })
    });
    ModelSelection {
        provider,
        model: str_field(payload, "model")
            .or(fallback.map(|model| model.model.as_str()))
            .unwrap_or("unknown")
            .to_owned(),
        effort: effort
            .map(str::to_owned)
            .or_else(|| fallback.and_then(|model| model.effort.clone())),
        agent: fallback.and_then(|model| model.agent.clone()),
    }
}

fn provider(name: &str) -> ProviderId {
    match name {
        "codex" => ProviderId::Codex,
        "antigravity" => ProviderId::Antigravity,
        "opencode" | "open-code" => ProviderId::OpenCode,
        "mock" => ProviderId::Mock,
        _ => ProviderId::Claude,
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str()
}

fn date_field(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    str_field(value, key)?.parse().ok()
}

fn legacy_row<T>(
    table: &str,
    row: &Row<'_>,
    read: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
) -> Option<T> {
    match read(row) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            tracing::warn!(table, "Skipping malformed legacy history row");
            None
        }
    }
}

fn legacy_query_all<T>(
    source: &Connection,
    table: &str,
    sql: &str,
    params: impl Params,
    read: impl Fn(&Row<'_>) -> rusqlite::Result<T>,
) -> CoreResult<Vec<T>> {
    let mut statement = source.prepare(sql).map_err(storage_error)?;
    let mut rows = statement.query(params).map_err(storage_error)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().map_err(storage_error)? {
        result.extend(legacy_row(table, row, &read));
    }
    Ok(result)
}

fn has_table(connection: &Connection, table: &str) -> CoreResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(storage_error)
}

fn has_column(connection: &Connection, table: &str, column: &str) -> CoreResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            params![table, column],
            |row| row.get(0),
        )
        .map_err(storage_error)
}

fn destination_exists(connection: &Connection, table: &str, id: Uuid) -> CoreResult<bool> {
    connection
        .prepare_cached(&format!(
            "SELECT EXISTS(SELECT 1 FROM {table} WHERE id = ?1)"
        ))
        .map_err(storage_error)?
        .query_row([id.to_string()], |row| row.get(0))
        .map_err(storage_error)
}

fn legacy_uuid(id: &str) -> Uuid {
    Uuid::parse_str(id).unwrap_or_else(|_| uuid_v5(id.as_bytes()))
}

// The workspace enables UUIDv4 only. UUIDv5 hashes the namespace and name
// with SHA-1, then uses the UUID builder to set the version and variant.
fn uuid_v5(name: &[u8]) -> Uuid {
    let mut bytes = Vec::with_capacity(name.len() + 80);
    bytes.extend_from_slice(Uuid::NAMESPACE_URL.as_bytes());
    bytes.extend_from_slice(name);
    let bit_len = (bytes.len() as u64) * 8;
    bytes.push(0x80);
    while bytes.len() % 64 != 56 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&bit_len.to_be_bytes());
    let mut hash: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    for block in bytes.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (word, chunk) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*chunk);
        }
        for i in 16..80 {
            words[i] = (words[i - 3] ^ words[i - 8] ^ words[i - 14] ^ words[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = hash;
        for (i, word) in words.into_iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            (a, b, c, d, e) = (next, a, b.rotate_left(30), c, d);
        }
        for (state, word) in hash.iter_mut().zip([a, b, c, d, e]) {
            *state = state.wrapping_add(word);
        }
    }
    let mut uuid_bytes = [0; 16];
    for (bytes, word) in uuid_bytes.as_chunks_mut::<4>().0.iter_mut().zip(hash) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    uuid::Builder::from_sha1_bytes(uuid_bytes).into_uuid()
}
