use super::*;
use serde_json::json;
use std::fs;
use t3_core::{InteractionMode, ProviderId, RuntimeMode, TurnItemKind};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("t3-store-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn thread(project_id: Uuid, title: &str) -> Thread {
    let now = Utc::now();
    Thread {
        id: Uuid::new_v4(),
        project_id,
        parent_thread_id: None,
        relationship: None,
        title: title.into(),
        model_selection: ModelSelection::default(),
        runtime_mode: RuntimeMode::AutoAcceptEdits,
        interaction_mode: InteractionMode::Plan,
        status: ThreadStatus::Idle,
        worktree_path: Some("/work/tree".into()),
        branch: Some("topic".into()),
        provider_session_id: None,
        archived: false,
        pinned: true,
        created_at: now,
        updated_at: now,
    }
}

fn run(thread_id: Uuid, ordinal: i64, provider: ProviderId) -> Run {
    Run {
        id: Uuid::new_v4(),
        thread_id,
        ordinal,
        provider,
        model: "model".into(),
        effort: Some("high".into()),
        status: RunStatus::Running,
        prompt: "Test prompt".into(),
        provider_session_id: Some("resumed".into()),
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

async fn seed(store: &SqliteStore, title: &str) -> Run {
    let project = store
        .create_project("Workspace".into(), "/work/project".into())
        .await
        .unwrap();
    let thread = store
        .create_thread(thread(project.id, title))
        .await
        .unwrap();
    store
        .create_run(run(thread.id, 1, ProviderId::Mock))
        .await
        .unwrap()
}

fn item(run: &Run, payload: Value) -> NewTurnItem {
    NewTurnItem {
        thread_id: run.thread_id,
        run_id: run.id,
        kind: TurnItemKind::ToolResult,
        summary: None,
        payload,
        searchable_text: None,
    }
}

#[tokio::test]
async fn gc_removes_only_unreferenced_blobs_and_regular_attachment_files() {
    let dir = TempDir::new();
    let db_path = dir.0.join("state.sqlite");
    let attachments = dir.0.join("attachments");
    fs::create_dir(&attachments).unwrap();
    let store = SqliteStore::open(&db_path, StorageConfig::default()).unwrap();
    let live = seed(&store, "Live thread").await;
    let removed = seed(&store, "Deleted thread").await;
    let live_payload = json!({"text": "live text ".repeat(12_000)});
    for (run, payload) in [
        (&live, live_payload.clone()),
        (&removed, live_payload.clone()),
        (&removed, json!({"text": "orphan text ".repeat(12_000)})),
    ] {
        assert!(
            store
                .append_turn_item(item(run, payload))
                .await
                .unwrap()
                .compressed_bytes
                .is_some()
        );
    }
    for (run, bytes) in [(&live, b"keep".as_slice()), (&removed, b"orphan")] {
        let hash = blake3::hash(bytes).to_hex().to_string();
        let storage_path = attachments.join(&hash);
        fs::write(&storage_path, bytes).unwrap();
        store
            .save_attachment(Attachment {
                id: Uuid::new_v4(),
                thread_id: Some(run.thread_id),
                filename: "fixture".into(),
                mime_type: "text/plain".into(),
                size_bytes: bytes.len() as u64,
                blake3_hash: hash,
                storage_path,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
    }
    // A discarded thread can leave unreferenced blobs after the item cascade.
    store
        .with_writer(move |connection| {
            connection
                .execute(
                    "DELETE FROM threads WHERE id = ?1",
                    [removed.thread_id.to_string()],
                )
                .map_err(storage_error)
        })
        .await
        .unwrap();
    let orphan_file = attachments.join(blake3::hash(b"orphan").to_hex().as_str());
    let keep_file = attachments.join(blake3::hash(b"keep").to_hex().as_str());
    let subdir = attachments.join("subdirectory");
    fs::create_dir(&subdir).unwrap();
    fs::write(subdir.join("keep"), b"nested").unwrap();
    #[cfg(unix)]
    let link = {
        let target = dir.0.join("external");
        fs::write(&target, b"external").unwrap();
        let link = attachments.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        (link, target)
    };

    let unrelated = ["notes.txt".to_owned(), "A".repeat(64), "f".repeat(63)];
    for name in &unrelated {
        fs::write(attachments.join(name), b"user data").unwrap();
    }
    let temporary = attachments.join(".tmp-fixture");
    fs::write(&temporary, b"temporary").unwrap();

    let dry = store.gc_storage(&attachments, false).await.unwrap();
    assert!(!dry.applied);
    assert_eq!(dry.orphan_blobs_count, 1);
    assert!(dry.orphan_blobs_bytes > 0);
    assert_eq!(dry.orphan_attachments_count, 2);
    assert_eq!(dry.orphan_attachments_bytes, 15);
    assert_eq!(dry.db_bytes_before, dry.db_bytes_after);
    assert_eq!(store.db_stats().await.unwrap().dedup_blobs_count, 2);
    assert_eq!(fs::read(&orphan_file).unwrap(), b"orphan");

    let applied = store.gc_storage(&attachments, true).await.unwrap();
    assert!(applied.applied);
    assert_eq!(applied.orphan_blobs_count, 1);
    assert_eq!(applied.orphan_blobs_bytes, dry.orphan_blobs_bytes);
    assert_eq!(applied.orphan_attachments_count, 2);
    assert_eq!(applied.orphan_attachments_bytes, 15);
    assert_eq!(
        applied.db_bytes_after,
        fs::metadata(&db_path).unwrap().len()
    );
    assert_eq!(store.db_stats().await.unwrap().dedup_blobs_count, 1);
    assert_eq!(
        store.list_turn_items(live.thread_id, 10, 0).await.unwrap()[0].payload,
        live_payload
    );
    assert_eq!(
        store.list_attachments(live.thread_id).await.unwrap().len(),
        1
    );
    assert_eq!(fs::read(keep_file).unwrap(), b"keep");
    assert!(!orphan_file.exists());
    assert!(!temporary.exists());
    for name in unrelated {
        assert_eq!(fs::read(attachments.join(name)).unwrap(), b"user data");
    }
    assert_eq!(fs::read(subdir.join("keep")).unwrap(), b"nested");
    #[cfg(unix)]
    {
        assert!(link.0.is_symlink());
        assert_eq!(fs::read(link.1).unwrap(), b"external");
    }
    let repeat = store.gc_storage(&attachments, false).await.unwrap();
    assert_eq!(repeat.orphan_blobs_count, 0);
    assert_eq!(repeat.orphan_attachments_count, 0);
}

#[tokio::test]
async fn project_metadata_migrates_and_updates() {
    let dir = TempDir::new();
    let db_path = dir.0.join("state.sqlite");
    let id = Uuid::new_v4();
    let now = timestamp(Utc::now());
    let legacy = Connection::open(&db_path).unwrap();
    legacy
        .execute_batch(
            r#"CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL, title TEXT NOT NULL, workspace_root TEXT NOT NULL,
                default_model TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE runs (
            id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
            provider TEXT NOT NULL, model TEXT NOT NULL, effort TEXT, status TEXT NOT NULL,
            prompt TEXT NOT NULL, provider_session_id TEXT, input_tokens INTEGER, output_tokens INTEGER,
            duration_ms INTEGER, error_message TEXT, started_at TEXT NOT NULL, completed_at TEXT,
            UNIQUE (id, thread_id)
            );"#,
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO projects (id, title, workspace_root, created_at, updated_at)
         VALUES (?1, 'Legacy', '/workspace', ?2, ?2)",
            params![id.to_string(), now],
        )
        .unwrap();
    drop(legacy);

    let store = SqliteStore::open(&db_path, StorageConfig::default()).unwrap();
    let migrated = store.get_project(id).await.unwrap();
    assert_eq!(migrated.instructions, None);
    assert_eq!(migrated.verify_command, None);
    let columns = {
        let connection = Connection::open(&db_path).unwrap();
        let mut statement = connection.prepare("PRAGMA table_info(runs)").unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    for column in [
        "cache_read_tokens",
        "cache_creation_tokens",
        "reasoning_tokens",
    ] {
        assert!(
            columns.iter().any(|name| name == column),
            "missing {column}"
        );
    }

    let updated = store
        .update_project(
            id,
            Some("Updated title".into()),
            Some(Some(ModelSelection::default())),
            Some(Some("  cargo test  ".into())),
            Some(Some("  ".into())),
        )
        .await
        .unwrap();
    assert_eq!(updated.title, "Updated title");
    assert_eq!(updated.default_model, Some(ModelSelection::default()));
    assert_eq!(updated.instructions.as_deref(), Some("cargo test"));
    assert_eq!(updated.verify_command, None);
    let listed = store.list_projects().await.unwrap();
    assert_eq!(listed[0].instructions.as_deref(), Some("cargo test"));
    drop(store);

    let reopened = SqliteStore::open(&db_path, StorageConfig::default()).unwrap();
    let persisted = reopened.get_project(id).await.unwrap();
    assert_eq!(persisted.instructions.as_deref(), Some("cargo test"));
}

#[tokio::test]
async fn entity_lifecycle_and_cascade() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    let project = store
        .create_project("Main".into(), "/main".into())
        .await
        .unwrap();
    let other = store
        .create_project("Other".into(), "/other".into())
        .await
        .unwrap();
    assert_eq!(store.list_projects().await.unwrap().len(), 2);
    assert_eq!(
        serde_json::to_value(store.get_project(project.id).await.unwrap()).unwrap(),
        serde_json::to_value(&project).unwrap()
    );
    let parent = store
        .create_thread(thread(project.id, "Parent title"))
        .await
        .unwrap();
    let mut child = thread(other.id, "Child title");
    child.parent_thread_id = Some(parent.id);
    child.relationship = Some("delegation".into());
    child.archived = true;
    let child = store.create_thread(child).await.unwrap();
    assert_eq!(
        serde_json::to_value(store.get_thread(child.id).await.unwrap()).unwrap(),
        serde_json::to_value(&child).unwrap()
    );
    assert_eq!(
        store.list_child_threads(parent.id).await.unwrap()[0].id,
        child.id
    );
    assert!(
        store
            .list_threads(Some(other.id), false, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.list_threads(Some(other.id), true, 10).await.unwrap()[0].id,
        child.id
    );
    assert!(store.list_threads(None, true, 0).await.unwrap().is_empty());
    store.archive_thread(child.id, false).await.unwrap();
    let selection = ModelSelection {
        provider: ProviderId::Codex,
        ..ModelSelection::default()
    };
    store
        .update_thread_status(child.id, ThreadStatus::Running)
        .await
        .unwrap();
    store
        .update_thread_session_id(child.id, "session".into())
        .await
        .unwrap();
    store
        .update_thread_model(child.id, selection.clone())
        .await
        .unwrap();
    store
        .update_thread_title(child.id, "New title".into())
        .await
        .unwrap();
    let updated = store.get_thread(child.id).await.unwrap();
    assert_eq!(updated.status, ThreadStatus::Running);
    assert_eq!(updated.provider_session_id.as_deref(), Some("session"));
    assert_eq!(updated.model_selection, selection);
    assert_eq!(updated.title, "New title");
    assert!(!updated.archived);
    assert!(updated.updated_at >= child.updated_at);
    assert!(matches!(
        store.archive_thread(Uuid::new_v4(), true).await,
        Err(CoreError::NotFound(_))
    ));

    let run = store
        .create_run(run(parent.id, 1, ProviderId::Mock))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&store.list_runs(parent.id).await.unwrap()[0]).unwrap(),
        serde_json::to_value(&run).unwrap()
    );
    store
        .finish_run(
            run.id,
            RunStatus::Completed,
            Some("continued".into()),
            Some(13),
            Some(21),
            Some(4),
            Some(6),
            Some(3),
            40,
            None,
        )
        .await
        .unwrap();
    let finished = &store.list_runs(parent.id).await.unwrap()[0];
    assert_eq!(finished.status, RunStatus::Completed);
    assert_eq!(finished.provider_session_id.as_deref(), Some("continued"));
    assert_eq!(
        (
            finished.input_tokens,
            finished.output_tokens,
            finished.cache_read_tokens,
            finished.cache_creation_tokens,
            finished.reasoning_tokens,
            finished.duration_ms
        ),
        (Some(13), Some(21), Some(4), Some(6), Some(3), Some(40))
    );
    assert!(finished.completed_at.unwrap() >= finished.started_at);
    let saved = store
        .append_turn_item(item(
            &run,
            json!({"text": "Indexed result", "data": "x".repeat(5000)}),
        ))
        .await
        .unwrap();
    assert_eq!(
        store.search_fts("Indexed result", 10).await.unwrap()[0].item_id,
        Some(saved.id)
    );

    let handoff = store
        .record_handoff(ContextHandoff {
            id: Uuid::new_v4(),
            source_thread_id: parent.id,
            target_thread_id: child.id,
            from_model: parent.model_selection.clone(),
            to_model: selection,
            summary: "Summary".into(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    for thread_id in [parent.id, child.id] {
        let rows = store.list_handoffs(thread_id).await.unwrap();
        assert_eq!(
            serde_json::to_value(&rows[0]).unwrap(),
            serde_json::to_value(&handoff).unwrap()
        );
    }

    let attachment = store
        .save_attachment(Attachment {
            id: Uuid::new_v4(),
            thread_id: Some(parent.id),
            filename: "資料.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 10,
            blake3_hash: blake3::hash(b"attachment").to_hex().to_string(),
            storage_path: "/data/attachment".into(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    let fetched = store.get_attachment(attachment.id).await.unwrap();
    assert_eq!(
        serde_json::to_value(fetched).unwrap(),
        serde_json::to_value(&attachment).unwrap()
    );
    assert_eq!(
        store.list_attachments(parent.id).await.unwrap()[0].id,
        attachment.id
    );
    let unattached = store
        .save_attachment(Attachment {
            id: Uuid::new_v4(),
            thread_id: None,
            ..attachment.clone()
        })
        .await
        .unwrap();
    assert_eq!(store.db_stats().await.unwrap().attachments_count, 2);

    store.delete_project(project.id).await.unwrap();
    assert!(matches!(
        store.get_project(project.id).await,
        Err(CoreError::NotFound(_))
    ));
    assert!(matches!(
        store.get_thread(parent.id).await,
        Err(CoreError::NotFound(_))
    ));
    assert!(matches!(
        store.get_attachment(attachment.id).await,
        Err(CoreError::NotFound(_))
    ));
    assert!(
        store
            .get_attachment(unattached.id)
            .await
            .unwrap()
            .thread_id
            .is_none()
    );
    assert!(
        store
            .get_thread(child.id)
            .await
            .unwrap()
            .parent_thread_id
            .is_none()
    );
    assert!(store.list_handoffs(child.id).await.unwrap().is_empty());
    assert!(store.list_runs(parent.id).await.unwrap().is_empty());
    assert!(
        store
            .list_turn_items(parent.id, 10, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search_fts("Indexed result", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search_fts("Parent title", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.db_stats().await.unwrap().dedup_blobs_count, 0);
}

#[tokio::test]
async fn compression_deduplication_and_db_statistics() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    let run = seed(&store, "Compression").await;
    let payload = json!({"output": "x".repeat(100 * 1024)});
    let mut input = item(&run, payload.clone());
    input.summary = Some("Repetitive tool output".into());
    let (first, second) = tokio::join!(
        store.append_turn_item(input.clone()),
        store.append_turn_item(input)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(first.ordinal, second.ordinal);
    let raw = serde_json::to_vec(&payload).unwrap();
    assert_eq!(first.raw_bytes, raw.len());
    let stored = first.compressed_bytes.unwrap();
    assert!(
        stored * 10 < first.raw_bytes,
        "compressed {stored} of {} bytes",
        first.raw_bytes
    );
    assert_eq!(second.compressed_bytes, Some(stored));

    let inline = json!({"text": "短い"});
    let inline_item = store
        .append_turn_item(item(&run, inline.clone()))
        .await
        .unwrap();
    assert_eq!(inline_item.compressed_bytes, None);
    let rows = store.list_turn_items(run.thread_id, 10, 0).await.unwrap();
    assert_eq!(
        rows.iter().map(|item| item.ordinal).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(rows[0].payload, payload);
    assert_eq!(rows[1].payload, payload);
    assert_eq!(rows[2].payload, inline);
    assert_eq!(
        store.list_turn_items(run.thread_id, 1, 2).await.unwrap()[0].id,
        inline_item.id
    );

    let stats = store.db_stats().await.unwrap();
    assert!(stats.db_size_bytes > 0);
    assert_eq!(
        (
            stats.threads_count,
            stats.runs_count,
            stats.turn_items_count
        ),
        (1, 1, 3)
    );
    assert_eq!(
        stats.raw_payload_bytes as usize,
        raw.len() * 2 + inline_item.raw_bytes
    );
    assert_eq!(
        stats.stored_payload_bytes as usize,
        stored + inline_item.raw_bytes
    );
    assert_eq!(stats.dedup_blobs_count, 1);
    assert!(stats.compression_ratio < 0.1);

    store
        .with_writer(move |connection| {
            let (hash, raw_bytes, compressed_bytes, blob): (String, usize, usize, Vec<u8>) =
                connection
                    .query_row(
                        "SELECT hash, raw_bytes, compressed_bytes, zstd_data FROM blob_store",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .unwrap();
            assert_eq!(hash, blake3::hash(&raw).to_hex().to_string());
            assert_eq!(
                (raw_bytes, compressed_bytes, blob.len()),
                (raw.len(), stored, stored)
            );
            let matching_refs: usize = connection
                .query_row(
                    "SELECT count(*) FROM turn_items WHERE blob_hash = ?1 AND payload_text IS NULL",
                    [&hash],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(matching_refs, 2);
            Ok(())
        })
        .await
        .unwrap();
    eprintln!(
        "Repetitive tool payload: {} → {stored} bytes",
        first.raw_bytes
    );
}

#[tokio::test]
async fn oversized_payload_is_valid_utf8_and_transaction_rolls_back_on_wrong_run() {
    let max_payload_bytes = 1024;
    let store = SqliteStore::open_in_memory(StorageConfig {
        max_payload_bytes,
        ..StorageConfig::default()
    })
    .unwrap();
    let run = seed(&store, "Oversized").await;
    let payload = json!({
        "exitCode": 1,
        "tool": "shell",
        "content": [{"output": format!("starting build\n{}\nerror: ビルド失敗", "録\n\"\0".repeat(10_000))}],
    });
    let raw = serde_json::to_string(&payload).unwrap();
    let mut invalid = item(&run, payload.clone());
    invalid.thread_id = Uuid::new_v4();
    assert!(store.append_turn_item(invalid).await.is_err());
    assert_eq!(store.db_stats().await.unwrap().dedup_blobs_count, 0);
    let truncated = store.append_turn_item(item(&run, payload)).await.unwrap();
    assert_eq!(truncated.raw_bytes, raw.len());
    assert!(serde_json::to_vec(&truncated.payload).unwrap().len() <= max_payload_bytes);
    assert_eq!(truncated.payload["exitCode"], 1);
    assert_eq!(truncated.payload["tool"], "shell");
    let output = truncated.payload["content"][0]["output"].as_str().unwrap();
    assert!(output.starts_with("starting build"));
    assert!(output.contains("...[pruned "));
    assert!(output.ends_with("error: ビルド失敗"));
    let read = store.list_turn_items(run.thread_id, 10, 0).await.unwrap();
    assert_eq!(read[0].payload, truncated.payload);
}

#[tokio::test]
async fn oversized_json_structure_uses_a_bounded_utf8_preview() {
    let max_payload_bytes = 144;
    let store = SqliteStore::open_in_memory(StorageConfig {
        max_payload_bytes,
        ..StorageConfig::default()
    })
    .unwrap();
    let run = seed(&store, "Oversized structure").await;
    let payload = json!(["開始", vec![12345; 1000], "失敗"]);
    let raw_bytes = serde_json::to_vec(&payload).unwrap().len();

    let saved = store.append_turn_item(item(&run, payload)).await.unwrap();
    assert!(serde_json::to_vec(&saved.payload).unwrap().len() <= max_payload_bytes);
    assert_eq!(saved.payload["original_bytes"], raw_bytes);
    let preview = saved.payload["preview"].as_str().unwrap();
    assert!(preview.starts_with("[\"開始\""));
    assert!(preview.ends_with("\"失敗\"]"));
}

#[tokio::test]
async fn japanese_fts_trigram_short_query_and_title_updates() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    let run = seed(&store, "文字起こし会議").await;
    let mut explicit = item(&run, json!({"value": "not indexed"}));
    explicit.searchable_text = Some("「録画」の文字起こしを完了 ABC\"DEF".into());
    let explicit = store.append_turn_item(explicit).await.unwrap();
    let fallback = store
        .append_turn_item(item(&run, json!({"text": "次回も録画"})))
        .await
        .unwrap();
    let long = store.search_fts("文字起こし", 10).await.unwrap();
    assert_eq!(long.len(), 2);
    assert!(
        long.iter()
            .all(|hit| hit.thread_id == run.thread_id && hit.snippet.contains("文字起こし"))
    );
    assert!(long.iter().any(|hit| hit.item_id.is_none()));
    assert!(long.iter().any(|hit| hit.item_id == Some(explicit.id)));
    let short = store.search_fts("録画", 10).await.unwrap();
    assert_eq!(short.len(), 2);
    assert!(short.iter().any(|hit| hit.item_id == Some(fallback.id)));
    assert!(short.iter().all(|hit| hit.snippet.contains("録画")));
    assert_eq!(store.search_fts("録画", 1).await.unwrap().len(), 1);
    assert_eq!(
        store.search_fts("ABC\"DEF", 10).await.unwrap()[0].item_id,
        Some(explicit.id)
    );
    assert!(store.search_fts("%", 10).await.unwrap().is_empty());
    assert!(store.search_fts("_", 10).await.unwrap().is_empty());
    assert!(store.search_fts("", 10).await.unwrap().is_empty());

    store
        .update_thread_title(run.thread_id, "取材の議事録".into())
        .await
        .unwrap();
    let long = store.search_fts("文字起こし", 10).await.unwrap();
    assert_eq!(long.len(), 1);
    assert_eq!(long[0].thread_title, "取材の議事録");
    assert_eq!(
        store.search_fts("議事録", 10).await.unwrap()[0].item_id,
        None
    );
    assert_eq!(store.search_fts("取材", 10).await.unwrap()[0].item_id, None);
}

#[tokio::test]
async fn finished_runs_keep_session_and_error() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    let succeeded = seed(&store, "Statistics").await;
    let failed = store
        .create_run(run(succeeded.thread_id, 2, ProviderId::Mock))
        .await
        .unwrap();
    store
        .create_run(run(succeeded.thread_id, 3, ProviderId::Mock))
        .await
        .unwrap();
    store
        .create_run(run(succeeded.thread_id, 4, ProviderId::OpenCode))
        .await
        .unwrap();
    store
        .create_run(Run {
            model: "other-model".into(),
            ..run(succeeded.thread_id, 5, ProviderId::Mock)
        })
        .await
        .unwrap();
    store
        .finish_run(
            succeeded.id,
            RunStatus::Completed,
            None,
            Some(80),
            Some(15),
            Some(8),
            Some(2),
            Some(3),
            100,
            None,
        )
        .await
        .unwrap();
    store
        .finish_run(
            failed.id,
            RunStatus::Failed,
            None,
            Some(7),
            None,
            Some(1),
            None,
            Some(1),
            300,
            Some("failure".into()),
        )
        .await
        .unwrap();
    let runs = store.list_runs(succeeded.thread_id).await.unwrap();
    assert_eq!(runs[0].provider_session_id.as_deref(), Some("resumed"));
    assert_eq!(runs[1].error_message.as_deref(), Some("failure"));
}

#[tokio::test]
async fn file_readers_query_committed_data_while_the_writer_is_busy() {
    let dir = TempDir::new();
    let store = SqliteStore::open(dir.0.join("state.sqlite"), StorageConfig::default()).unwrap();
    let project = store
        .create_project("Committed".into(), "/work/project".into())
        .await
        .unwrap();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let writer_store = store.clone();
    let writer = tokio::spawn(async move {
        writer_store
            .with_writer(move |connection| {
                let transaction = connection.transaction().unwrap();
                transaction
                    .execute("UPDATE projects SET title = 'Updated'", [])
                    .unwrap();
                started.send(()).unwrap();
                let _ = release_rx.blocking_recv();
                transaction.commit().map_err(storage_error)
            })
            .await
    });
    started_rx.await.unwrap();
    let reads = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(
            store.list_projects(),
            store.get_project(project.id),
            store.db_stats(),
            store.list_threads(None, true, 10),
        )
    })
    .await;
    release.send(()).unwrap();
    writer.await.unwrap().unwrap();

    let (projects, snapshot, stats, threads) = reads.expect("reads waited for the writer").unwrap();
    assert_eq!(projects[0].title, "Committed");
    assert_eq!(snapshot.title, "Committed");
    assert_eq!(stats.threads_count, 0);
    assert!(threads.is_empty());
    assert_eq!(
        store.get_project(project.id).await.unwrap().title,
        "Updated"
    );
}

#[tokio::test]
async fn file_pragmas_and_reopen() {
    let dir = TempDir::new();
    let path = dir.0.join("state.sqlite");
    let store = SqliteStore::open(&path, StorageConfig::default()).unwrap();
    assert_eq!(store.connections.readers.len(), 4);
    for reader in &store.connections.readers {
        let reader = reader.lock().unwrap();
        assert!(reader.is_readonly(rusqlite::DatabaseName::Main).unwrap());
        for (pragma, value) in [
            ("query_only", 1),
            ("busy_timeout", 5000),
            ("cache_size", -8000),
            ("temp_store", 2),
        ] {
            let actual: i64 = reader
                .pragma_query_value(None, pragma, |row| row.get(0))
                .unwrap();
            assert_eq!(actual, value, "{pragma}");
        }
    }
    store
        .with_writer(|connection| {
            let mode: String = connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            for (pragma, value) in [
                ("synchronous", 1),
                ("foreign_keys", 1),
                ("busy_timeout", 5000),
                ("temp_store", 2),
            ] {
                let actual: i64 = connection
                    .pragma_query_value(None, pragma, |row| row.get(0))
                    .unwrap();
                assert_eq!(actual, value, "{pragma}");
            }
            Ok(())
        })
        .await
        .unwrap();
    let run = seed(&store, "永続化の確認").await;
    let payload = json!({"value": "x".repeat(10000)});
    store
        .append_turn_item(item(&run, payload.clone()))
        .await
        .unwrap();
    drop(store);

    let reopened = SqliteStore::open(
        path,
        StorageConfig {
            max_payload_bytes: 10,
            ..StorageConfig::default()
        },
    )
    .unwrap();
    assert_eq!(
        reopened.search_fts("永続化", 10).await.unwrap()[0].thread_id,
        run.thread_id
    );
    assert_eq!(
        reopened
            .list_turn_items(run.thread_id, 10, 0)
            .await
            .unwrap()[0]
            .payload,
        payload
    );
}

#[test]
fn checkpoint_worker_exits_on_last_store_drop() {
    let dir = TempDir::new();
    let store = SqliteStore::open(dir.0.join("state.sqlite"), StorageConfig::default()).unwrap();
    let worker = store.connections._checkpoint.as_ref().unwrap();
    let lifetime = Arc::downgrade(&worker.stop);
    assert_eq!(lifetime.strong_count(), 2);

    let clone = store.clone();
    drop(store);
    assert_eq!(lifetime.strong_count(), 2);
    let (done, completed) = std::sync::mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(clone);
        done.send(()).unwrap();
    });
    completed
        .recv_timeout(Duration::from_secs(2))
        .expect("store drop did not wake and join the checkpoint worker");
    dropper.join().unwrap();
    assert_eq!(lifetime.strong_count(), 0);
}

#[tokio::test]
async fn legacy_import_reads_active_projects_and_leaves_source_unchanged() {
    let dir = TempDir::new();
    let path = dir.0.join("legacy.sqlite");
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE projection_projects (title TEXT, workspace_root TEXT, deleted_at TEXT);
         INSERT INTO projection_projects VALUES
            ('既存を上書きしない', '/work/existing', NULL),
            ('録画の新規案件', '/work/new', NULL),
            ('重複', '/work/new', NULL),
            ('削除済み', '/work/deleted', '2026-01-01');
         CREATE TABLE auth (value TEXT);
         CREATE TABLE clerk (value TEXT);
         CREATE TABLE secrets (value TEXT);
         CREATE TABLE telemetry (value TEXT);",
        )
        .unwrap();
    drop(legacy);
    let before = fs::read(&path).unwrap();

    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    store
        .create_project("Existing".into(), "/work/existing".into())
        .await
        .unwrap();
    assert_eq!(
        store.import_projects_from_t3_sqlite(&path).await.unwrap(),
        1
    );
    assert_eq!(
        store.import_projects_from_t3_sqlite(&path).await.unwrap(),
        0
    );
    let projects = store.list_projects().await.unwrap();
    assert_eq!(projects.len(), 2);
    assert!(
        projects
            .iter()
            .any(|p| p.title == "Existing" && p.workspace_root == Path::new("/work/existing"))
    );
    assert!(
        projects
            .iter()
            .any(|p| p.title == "録画の新規案件" && p.workspace_root == Path::new("/work/new"))
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    let missing = dir.0.join("missing.sqlite");
    assert!(
        store
            .import_projects_from_t3_sqlite(&missing)
            .await
            .is_err()
    );
    assert!(!missing.exists());
}

#[tokio::test]
async fn imports_legacy_history_with_lineage_runs_and_search() {
    let dir = TempDir::new();
    let path = dir.0.join("legacy.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE projection_projects (project_id TEXT, title TEXT, workspace_root TEXT,
            created_at TEXT, updated_at TEXT, deleted_at TEXT);
         INSERT INTO projection_projects VALUES
            ('legacy-project', 'Legacy project', '/legacy', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL),
            ('deleted', 'Deleted project', '/deleted', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'yes');
         CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT, project_id TEXT,
            title TEXT, default_provider TEXT, runtime_mode TEXT, interaction_mode TEXT,
            active_provider_thread_id TEXT, created_at TEXT, updated_at TEXT, archived_at TEXT,
            deleted_at TEXT, payload_json TEXT);
         INSERT INTO orchestration_v2_projection_threads VALUES
            ('parent', 'legacy-project', '日本語タイトル', 'codex', 'full-access', 'plan',
             'native', '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z', NULL, NULL,
             '{\"modelSelection\": {\"model\": \"sol\", \"options\": [{\"id\": \"effort\", \"value\": \"high\"}]}, \"status\": \"running\"}'),
            ('child', 'legacy-project', 'Child', 'claudeAgent', 'approval-required', 'default',
             NULL, '2026-01-01T00:00:00Z', '2026-01-01T00:02:00Z', NULL, NULL,
             '{\"pinnedAt\": \"2026-01-01T00:01:00Z\", \"status\": \"interrupted\"}');
         CREATE TABLE orchestration_v2_projection_provider_threads (provider_thread_id TEXT,
            thread_id TEXT, updated_at TEXT, payload_json TEXT);
         INSERT INTO orchestration_v2_projection_provider_threads VALUES
            ('native', 'parent', '2026-01-01T00:00:00Z', '{\"providerSessionId\": \"session\"}'),
            ('older', 'parent', '2025-12-31T00:00:00Z', '{\"nativeThreadRef\": \"old\"}');
         CREATE TABLE orchestration_v2_projection_subagents (thread_id TEXT, child_thread_id TEXT);
         INSERT INTO orchestration_v2_projection_subagents VALUES ('parent', 'child');
         CREATE TABLE orchestration_v2_projection_runs (run_id TEXT, thread_id TEXT,
            ordinal INTEGER, provider TEXT, provider_thread_id TEXT, status TEXT,
            requested_at TEXT, completed_at TEXT, payload_json TEXT);
         INSERT INTO orchestration_v2_projection_runs VALUES
            ('legacy-run', 'parent', 1, 'codex', 'native', 'completed', '2026-01-01T00:00:00Z',
             '2026-01-01T00:01:00Z', '{\"userMessageId\": \"message-1\", \"modelSelection\": {\"model\": \"astra\"}}');
         CREATE TABLE orchestration_v2_projection_turn_items (turn_item_id TEXT, thread_id TEXT,
            run_id TEXT, ordinal INTEGER, type TEXT, updated_at TEXT, payload_json TEXT);
         INSERT INTO orchestration_v2_projection_turn_items VALUES
            ('notice', 'parent', NULL, 3, 'assistant_message', '2026-01-01T00:00:00Z',
             '{\"text\": \"履歴検索できます\"}'),
            ('prompt', 'parent', 'legacy-run', 4, 'user_message', '2026-01-01T00:00:00Z',
             '{\"messageId\": \"message-1\", \"text\": \"テスト依頼\"}');",
    )
    .unwrap();
    db.execute_batch(
        r#"UPDATE orchestration_v2_projection_runs
           SET completed_at = 'invalid fallback',
               payload_json = json_set(payload_json, '$.completedAt', '2026-01-01T00:01:00Z');
           INSERT INTO orchestration_v2_projection_turn_items VALUES
             ('bad-item', 'parent', 'legacy-run', 5, 'user_message',
              '2026-01-01T00:00:00Z', 'invalid json'),
             ('notice-link', 'parent', 'legacy-run', 6, 'notification',
              '2026-01-01T00:00:00Z', '{"source":{"childThreadId":"child"}}'),
             ('card', 'parent', 'legacy-run', 7, 'subagent',
              '2026-01-01T00:00:00Z', '{"title":"Subagent", "childThreadId":"child"}'),
             ('cross-thread', 'child', 'legacy-run', 1, 'assistant_message',
              '2026-01-01T00:00:00Z', '{"text":"child result"}');
           INSERT INTO orchestration_v2_projection_runs VALUES
             ('bad-run', 'child', 1, 'codex', NULL, 'completed', 'bad timestamp', NULL, '{}');"#,
    )
    .unwrap();
    let output = format!("HEAD{}TAIL", "出力".repeat(20_000));
    for (id, ordinal) in [("command-1", 8), ("command-2", 9)] {
        db.execute(
            "INSERT INTO orchestration_v2_projection_turn_items VALUES
                 (?1, 'parent', 'legacy-run', ?2, 'command_execution', '2026-01-01T00:00:00Z', ?3)",
            params![
                id,
                ordinal,
                json!({"input":"example\nrest", "output":output, "exitCode":0}).to_string()
            ],
        )
        .unwrap();
    }
    drop(db);
    let source = fs::read(&path).unwrap();
    let store = SqliteStore::open(dir.0.join("imported.sqlite"), StorageConfig::default()).unwrap();
    let report = store.import_history_from_t3_sqlite(&path).await.unwrap();
    assert_eq!(
        (
            report.projects_imported,
            report.threads_imported,
            report.runs_imported,
            report.items_imported
        ),
        (1, 2, 3, 7)
    );
    assert_eq!(
        report.db_bytes_after,
        store.db_stats().await.unwrap().db_size_bytes
    );
    let project = store.list_projects().await.unwrap().pop().unwrap();
    let threads = store
        .list_threads(Some(project.id), true, 10)
        .await
        .unwrap();
    let parent = threads
        .iter()
        .find(|t| t.title == "日本語タイトル")
        .unwrap();
    let child = threads.iter().find(|t| t.title == "Child").unwrap();
    assert_eq!(
        parent.id.to_string(),
        "7ec8b607-55e7-574d-bbd4-9ccf1b1521a8"
    );
    assert_eq!(child.parent_thread_id, Some(parent.id));
    assert_eq!(child.relationship.as_deref(), Some("subagent"));
    assert_eq!(child.status, ThreadStatus::Interrupted);
    assert!(child.pinned);
    assert_eq!(parent.status, ThreadStatus::Completed);
    assert_eq!(parent.provider_session_id.as_deref(), Some("session"));
    assert_eq!(parent.model_selection.effort.as_deref(), Some("high"));
    let runs = store.list_runs(parent.id).await.unwrap();
    assert_eq!(
        runs[0].id.to_string(),
        "521f35c6-b1cf-5b2c-bed4-571b0c532844"
    );
    assert_eq!(
        runs[1].id.to_string(),
        "f53f850e-5c80-5e30-8f62-ec48d86ab009"
    );
    assert_eq!(runs[0].model, "astra");
    assert_eq!(runs[0].prompt, "テスト依頼");
    let items = store.list_turn_items(parent.id, 10, 0).await.unwrap();
    assert_eq!(
        items.iter().map(|i| i.ordinal).collect::<Vec<_>>(),
        [3, 4, 6, 7, 8, 9]
    );
    assert_eq!(items[0].run_id, runs[1].id);
    assert_eq!(runs[0].completed_at, "2026-01-01T00:01:00Z".parse().ok());
    let child_runs = store.list_runs(child.id).await.unwrap();
    let child_items = store.list_turn_items(child.id, 10, 0).await.unwrap();
    assert_eq!(child_runs.len(), 1);
    assert_eq!(child_items[0].run_id, child_runs[0].id);
    assert_eq!(
        items[2].payload["source"]["childThreadId"],
        child.id.to_string()
    );
    assert_eq!(items[3].payload["childThreadId"], child.id.to_string());
    assert_eq!(items[4].kind, TurnItemKind::CommandExecution);
    assert_eq!(items[4].summary.as_deref(), Some("example"));
    assert!(items[4].compressed_bytes.is_some());
    let output = items[4].payload["output"].as_str().unwrap();
    assert!(output.starts_with("HEAD") && output.ends_with("TAIL"));
    assert_eq!(store.db_stats().await.unwrap().dedup_blobs_count, 1);
    assert_eq!(
        store.search_fts("履歴検索", 10).await.unwrap()[0].item_id,
        Some(items[0].id)
    );
    assert_eq!(
        store.search_fts("日本語タイトル", 10).await.unwrap()[0].thread_id,
        parent.id
    );
    let child_id = child.id;
    let card_id = items[3].id;
    store
        .with_writer(move |connection| {
            connection
                .execute(
                    "UPDATE threads SET parent_thread_id = NULL WHERE id = ?1",
                    [child_id.to_string()],
                )
                .map_err(storage_error)?;
            connection.execute(
            "UPDATE turn_items SET payload_text = json_set(payload_text, '$.childThreadId', 'child')
             WHERE id = ?1", [card_id.to_string()]).map_err(storage_error)?;
            Ok(())
        })
        .await
        .unwrap();
    let repeated = store.import_history_from_t3_sqlite(&path).await.unwrap();
    assert_eq!(
        (
            repeated.projects_imported,
            repeated.threads_imported,
            repeated.runs_imported,
            repeated.items_imported
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        store.get_thread(child_id).await.unwrap().parent_thread_id,
        Some(parent.id)
    );
    let page = store.list_turn_items(parent.id, 10, 0).await.unwrap();
    assert_eq!(page[3].payload["childThreadId"], child_id.to_string());
    assert!(source == fs::read(&path).unwrap());
    assert!(!dir.0.join("legacy.sqlite-wal").exists());
    assert!(!dir.0.join("legacy.sqlite-shm").exists());

    let writer = Connection::open(&path).unwrap();
    writer
        .execute(
            "INSERT INTO orchestration_v2_projection_turn_items VALUES
         ('late', 'parent', 'legacy-run', 10, 'assistant_message', '2026-01-01T00:03:00Z', '{}')",
            [],
        )
        .unwrap();
    let wal = dir.0.join("legacy.sqlite-wal");
    let wal_before = fs::read(&wal).unwrap();
    let incremental = store.import_history_from_t3_sqlite(&path).await.unwrap();
    assert_eq!(incremental.items_imported, 1);
    assert_eq!(incremental.runs_imported, 0);
    assert!(source == fs::read(&path).unwrap());
    assert!(wal_before == fs::read(&wal).unwrap());
}

#[tokio::test]
async fn fts_body_is_capped_at_a_utf8_boundary() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    let run = seed(&store, "FTS limit").await;
    let mut item = item(&run, json!({"text": "payload"}));
    item.searchable_text = Some(format!("indexprefix{}indexsuffix", "あ".repeat(24_000)));
    store.append_turn_item(item).await.unwrap();
    assert_eq!(store.search_fts("indexprefix", 10).await.unwrap().len(), 1);
    assert!(
        store
            .search_fts("indexsuffix", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn update_thread_modes_persists_both_modes() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    let run = seed(&store, "Modes").await;
    let before = store.get_thread(run.thread_id).await.unwrap();
    store
        .update_thread_modes(
            run.thread_id,
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        )
        .await
        .unwrap();
    let after = store.get_thread(run.thread_id).await.unwrap();
    assert_eq!(after.runtime_mode, RuntimeMode::FullAccess);
    assert_eq!(after.interaction_mode, InteractionMode::Default);
    assert!(after.updated_at >= before.updated_at);
    assert!(matches!(
        store
            .update_thread_modes(
                Uuid::new_v4(),
                RuntimeMode::FullAccess,
                InteractionMode::Default
            )
            .await,
        Err(CoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn list_turn_items_page_walks_backwards_with_an_exclusive_cursor() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    let run = seed(&store, "Pagination").await;
    let other = seed(&store, "Other thread").await;
    store
        .append_turn_item(item(&other, json!("other")))
        .await
        .unwrap();
    let mut ordinals = Vec::new();
    for i in 0..5 {
        let stored = store.append_turn_item(item(&run, json!(i))).await.unwrap();
        ordinals.push(stored.ordinal);
    }

    let mut cursor = None;
    for (expected, expected_more) in [(3..5, true), (1..3, true), (0..1, false)] {
        let (page, more, total) = store
            .list_turn_items_page(run.thread_id, 2, cursor)
            .await
            .unwrap();
        assert_eq!(
            page.iter().map(|item| item.ordinal).collect::<Vec<_>>(),
            ordinals[expected]
        );
        assert_eq!((more, total), (expected_more, 5));
        cursor = page.first().map(|item| item.ordinal);
    }
    let (page, more, total) = store
        .list_turn_items_page(run.thread_id, 2, cursor)
        .await
        .unwrap();
    assert!(page.is_empty());
    assert_eq!((more, total), (false, 5));
    let (page, more, total) = store
        .list_turn_items_page(run.thread_id, 0, None)
        .await
        .unwrap();
    assert!(page.is_empty());
    assert_eq!((more, total), (true, 5));
}

#[cfg(unix)]
#[test]
fn daemon_lock_is_exclusive_until_the_holder_is_dropped() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    let first = acquire_daemon_lock(&dir.0).unwrap();
    assert!(acquire_daemon_lock(&dir.0).is_err());
    let mode = fs::metadata(dir.0.join("t3d.lock"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // A child process holds a duplicate descriptor between fork and exec.
    let inherited = first.0.try_clone().unwrap();
    drop(first);
    assert!(acquire_daemon_lock(&dir.0).is_ok());
    drop(inherited);
}

#[test]
fn opening_a_database_from_before_restart_resumes_adds_the_column() {
    let dir = TempDir::new();
    let db_path = dir.0.join("state.sqlite");
    drop(SqliteStore::open(&db_path, StorageConfig::default()).unwrap());
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute_batch("ALTER TABLE runs DROP COLUMN restart_resumes")
        .unwrap();
    let store = SqliteStore::open(&db_path, StorageConfig::default()).unwrap();
    store.recover_interrupted_runs().unwrap();
}

#[test]
fn every_added_column_in_projects_and_runs_has_a_migration_and_matches_fresh_schema() {
    use std::collections::{BTreeMap, BTreeSet};

    fn table_columns(conn: &rusqlite::Connection, table: &str) -> BTreeMap<String, (String, i64, Option<String>)> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                (
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ),
            ))
        })
        .unwrap()
        .collect::<Result<BTreeMap<_, _>, _>>()
        .unwrap()
    }

    let dir = TempDir::new();
    let fresh_path = dir.0.join("fresh.sqlite");
    drop(SqliteStore::open(&fresh_path, StorageConfig::default()).unwrap());
    let fresh_conn = rusqlite::Connection::open(&fresh_path).unwrap();

    // Baseline v0 columns that existed in the original schema before ALTER TABLE migrations.
    let baseline: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::from([
        (
            "projects",
            BTreeSet::from([
                "id",
                "title",
                "workspace_root",
                "default_model",
                "created_at",
                "updated_at",
            ]),
        ),
        (
            "runs",
            BTreeSet::from([
                "id",
                "thread_id",
                "ordinal",
                "provider",
                "model",
                "effort",
                "status",
                "prompt",
                "provider_session_id",
                "input_tokens",
                "output_tokens",
                "duration_ms",
                "error_message",
                "started_at",
                "completed_at",
            ]),
        ),
    ]);

    let migrated_pairs: BTreeSet<(String, String)> = MIGRATIONS
        .iter()
        .filter_map(|sql| {
            let tokens: Vec<&str> = sql.split_whitespace().collect();
            if tokens.len() >= 6
                && tokens[0].eq_ignore_ascii_case("ALTER")
                && tokens[1].eq_ignore_ascii_case("TABLE")
                && tokens[3].eq_ignore_ascii_case("ADD")
                && tokens[4].eq_ignore_ascii_case("COLUMN")
            {
                Some((tokens[2].to_owned(), tokens[5].to_owned()))
            } else {
                None
            }
        })
        .collect();

    // Write-It-Twice guard: any column added to `projects` or `runs` in schema.sql must also
    // appear in `MIGRATIONS` so existing databases upgrade cleanly on startup.
    for (table, base_cols) in &baseline {
        let fresh_cols = table_columns(&fresh_conn, table);
        for col in fresh_cols.keys() {
            if !base_cols.contains(col.as_str()) {
                assert!(
                    migrated_pairs.contains(&((*table).to_owned(), col.clone())),
                    "column `{table}.{col}` exists in schema.sql but is missing from MIGRATIONS"
                );
            }
        }
    }

    // Drop every migrated column from a copy of the database, reopen via SqliteStore::open,
    // and assert full column parity (name, type, NOT NULL, default) with the fresh schema.
    let legacy_path = dir.0.join("legacy.sqlite");
    drop(SqliteStore::open(&legacy_path, StorageConfig::default()).unwrap());
    {
        let conn = rusqlite::Connection::open(&legacy_path).unwrap();
        for (table, col) in &migrated_pairs {
            conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {col}"))
                .unwrap();
        }
    }
    let upgraded = SqliteStore::open(&legacy_path, StorageConfig::default()).unwrap();
    upgraded.recover_interrupted_runs().unwrap();
    drop(upgraded);

    let legacy_conn = rusqlite::Connection::open(&legacy_path).unwrap();
    for table in baseline.keys() {
        assert_eq!(
            table_columns(&legacy_conn, table),
            table_columns(&fresh_conn, table),
            "upgraded table `{table}` schema diverged from fresh schema.sql"
        );
    }
}
