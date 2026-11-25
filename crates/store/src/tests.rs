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

    let dry = store.gc_storage(&attachments, false).await.unwrap();
    assert!(!dry.applied);
    assert_eq!(dry.orphan_blobs_count, 1);
    assert!(dry.orphan_blobs_bytes > 0);
    assert_eq!(dry.orphan_attachments_count, 1);
    assert_eq!(dry.orphan_attachments_bytes, 6);
    assert_eq!(dry.db_bytes_before, dry.db_bytes_after);
    assert_eq!(store.db_stats().await.unwrap().dedup_blobs_count, 2);
    assert_eq!(fs::read(&orphan_file).unwrap(), b"orphan");

    let applied = store.gc_storage(&attachments, true).await.unwrap();
    assert!(applied.applied);
    assert_eq!(applied.orphan_blobs_count, 1);
    assert_eq!(applied.orphan_blobs_bytes, dry.orphan_blobs_bytes);
    assert_eq!(applied.orphan_attachments_count, 1);
    assert_eq!(applied.orphan_attachments_bytes, 6);
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
            finished.duration_ms
        ),
        (Some(13), Some(21), Some(40))
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
async fn orchestration_aggregates_by_provider_and_model() {
    let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
    assert!(store.orch_stats().await.unwrap().is_empty());
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
            300,
            Some("failure".into()),
        )
        .await
        .unwrap();
    let rows = store.orch_stats().await.unwrap();
    assert_eq!(rows.len(), 3);
    let mock = &rows[0];
    assert_eq!(
        (mock.provider, mock.model.as_str()),
        (ProviderId::Mock, "model")
    );
    assert_eq!(
        (mock.total_runs, mock.completed_runs, mock.failed_runs),
        (3, 1, 1)
    );
    assert!((mock.success_rate - 1.0 / 3.0).abs() < f64::EPSILON);
    assert_eq!(mock.avg_duration_ms, 200.0);
    assert_eq!(
        (mock.total_input_tokens, mock.total_output_tokens),
        (87, 15)
    );
    assert!(rows.iter().any(|row| row.provider == ProviderId::OpenCode));
    assert!(rows.iter().any(|row| row.model == "other-model"));
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
