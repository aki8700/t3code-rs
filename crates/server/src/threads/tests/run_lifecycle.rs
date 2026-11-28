use super::*;

fn shared_thread(project_id: Uuid, title: &str) -> CreateThread {
    CreateThread {
        project_id,
        title: title.into(),
        model_selection: None,
        runtime_mode: RuntimeMode::FullAccess,
        interaction_mode: InteractionMode::Default,
        isolation: IsolationMode::Shared,
        base_ref: None,
    }
}

// The caller holds the project's write lock. Wait for the prompt to reach
// SQLite before allowing the competing operation to proceed.
async fn enqueue_at_gate(
    coordinator: &RunCoordinator,
    thread_id: Uuid,
    events: &mut tokio::sync::broadcast::Receiver<WsServerMessage>,
) -> tokio::task::JoinHandle<CoreResult<Uuid>> {
    let sender = coordinator.clone();
    let send = tokio::spawn(async move {
        sender
            .send(
                thread_id,
                Message {
                    prompt: "hello".into(),
                    ..Default::default()
                },
                true,
            )
            .await
    });
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, WsServerMessage::TurnItemAdded {thread_id: id, ..} if id == thread_id));
    send
}

#[tokio::test]
async fn project_deletion_overtaking_start_rejects_the_deleted_thread() {
    let (root, _, coordinator, project) = test_coordinator().await;
    let thread = coordinator
        .create_thread(shared_thread(project.id, "About to run"))
        .await
        .unwrap();
    let mut events = coordinator.subscribe();

    // Put the real deletion first in the lock's wait queue, then drive send
    // until its prompt is persisted. Neither operation can yet start a run.
    let gate = coordinator
        .0
        .project_locks
        .get(project.id)
        .write_owned()
        .await;
    let deletion = coordinator.delete_project(project.id);
    tokio::pin!(deletion);
    assert!(futures_util::poll!(&mut deletion).is_pending());
    let send = enqueue_at_gate(&coordinator, thread.id, &mut events).await;
    drop(gate);
    deletion.await.unwrap();
    let result = send.await.unwrap();
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, WsServerMessage::RunStarted { .. }));
    }
    assert!(
        coordinator
            .monitor_snapshot(0)
            .await
            .unwrap()
            .active_runs
            .is_empty()
    );
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        matches!(&result, Err(CoreError::NotFound(entity)) if entity == &format!("thread {}", thread.id)),
        "{result:?}"
    );
}

#[tokio::test]
async fn archived_thread_is_rechecked_before_start() {
    let (root, _, coordinator, project) = test_coordinator().await;
    let thread = coordinator
        .create_thread(shared_thread(project.id, "Archive me"))
        .await
        .unwrap();
    let gate = coordinator
        .0
        .project_locks
        .get(project.id)
        .write_owned()
        .await;
    let mut events = coordinator.subscribe();
    let send = enqueue_at_gate(&coordinator, thread.id, &mut events).await;
    coordinator
        .0
        .store
        .archive_thread(thread.id, true)
        .await
        .unwrap();
    drop(gate);

    let result = send.await.unwrap();
    assert!(
        matches!(&result, Err(CoreError::InvalidArgument(reason)) if reason == "thread is archived"),
        "{result:?}"
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, WsServerMessage::RunStarted { .. }));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn silent_worker_uses_membership_registered_after_start() {
    let (root, _, coordinator, project) = test_coordinator().await;
    let parent = coordinator
        .create_thread(shared_thread(project.id, "Parent"))
        .await
        .unwrap();
    let worker = coordinator
        .new_thread(
            shared_thread(project.id, "Worker"),
            Some((&parent, "swarm-worker")),
        )
        .await
        .unwrap();
    let board = coordinator
        .0
        .store
        .ensure_swarm_board(parent)
        .await
        .unwrap();
    // Hold the eventual coordinator wake in the queue.
    coordinator.set_pause_all(true).await.unwrap();
    coordinator
        .send(
            worker.id,
            Message {
                prompt: "[slow]".into(),
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
    let gate = coordinator.lock_thread(worker.id).await;
    let interrupt = coordinator.interrupt(worker.id);
    tokio::pin!(interrupt);
    assert!(futures_util::poll!(&mut interrupt).is_pending());
    coordinator
        .0
        .store
        .upsert_swarm_member(SwarmMember {
            name: "worker".into(),
            thread_id: worker.id,
            role: "worker".into(),
            ..board.members[0].clone()
        })
        .await
        .unwrap();
    drop(gate);
    interrupt.await.unwrap();
    let posts = coordinator
        .0
        .store
        .list_swarm_messages(board.board_id, 0, 10)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        posts.iter().any(|post| post.sender == "system"
            && post.text.contains("@coordinator")
            && post.text.contains("worker")),
        "{posts:?}"
    );
}

#[tokio::test]
async fn startup_resumes_latest_limited_member_once_and_repairs_its_notice() {
    let (root, _, coordinator, project) = test_coordinator().await;
    let store = &coordinator.0.store;
    let parent = coordinator
        .create_thread(shared_thread(project.id, "Coordinator"))
        .await
        .unwrap();
    let board = store.ensure_swarm_board(parent.clone()).await.unwrap();
    let worker = coordinator
        .new_thread(
            shared_thread(project.id, "Worker"),
            Some((&parent, "swarm-worker")),
        )
        .await
        .unwrap();
    store
        .upsert_swarm_member(SwarmMember {
            name: "review".into(),
            role: "worker".into(),
            thread_id: worker.id,
            ..board.members[0].clone()
        })
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let ended = Utc::now() - chrono::Duration::hours(1);
    store
        .create_run(Run {
            id,
            thread_id: worker.id,
            ordinal: 1,
            provider: ProviderId::Mock,
            model: "override-model".into(),
            effort: None,
            status: RunStatus::RateLimited,
            prompt: "Review the pending changes".into(),
            provider_session_id: Some("interrupted-session".into()),
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            duration_ms: Some(1),
            error_message: Some("usage_limit: no reset supplied".into()),
            started_at: ended,
            completed_at: Some(ended),
        })
        .await
        .unwrap();
    // Pending orphan cleanup must hold back the run while still repairing its board notice.
    let cfg = coordinator.0.cfg.clone();
    drop(coordinator);
    let state = crate::ServerState::new(cfg.clone(), "http://127.0.0.1:0".into()).unwrap();
    state
        .coordinator
        .restore_quota_resumes(&std::collections::HashSet::from([worker.id]))
        .await
        .unwrap();
    let posts = state
        .store
        .list_swarm_messages(board.board_id, 0, 100)
        .await
        .unwrap();
    assert_eq!(
        posts.len(),
        1,
        "startup must repair the notice even while cleanup is pending"
    );
    assert_eq!(state.store.list_runs(worker.id).await.unwrap().len(), 1);
    drop(state);

    let state = crate::ServerState::new(cfg, "http://127.0.0.1:0".into()).unwrap();
    let coordinator = &state.coordinator;
    let store = &state.store;
    let mut events = coordinator.subscribe();
    for _ in 0..2 {
        crate::resume_queued_runs_after_orphan_cleanup(coordinator, store)
            .await
            .unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if matches!(events.recv().await.unwrap(), WsServerMessage::RunStatusChanged {
                thread_id, status:RunStatus::Completed, ..} if thread_id == worker.id)
            {
                break;
            }
        }
    })
    .await
    .expect("overdue worker must resume after restart");
    crate::resume_queued_runs_after_orphan_cleanup(coordinator, store)
        .await
        .unwrap();
    let runs = store.list_runs(worker.id).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].model, "override-model");
    assert_eq!(
        runs[1].provider_session_id.as_deref(),
        Some("interrupted-session")
    );
    assert!(
        runs[1].prompt.contains("[system]")
            && runs[1].prompt.contains("Review the pending changes")
    );
    let posts = store
        .list_swarm_messages(board.board_id, 0, 100)
        .await
        .unwrap();
    assert_eq!(
        posts
            .iter()
            .filter(|post| post.sender == "system"
                && post.text.contains("review")
                && post.text.contains("RateLimited"))
            .count(),
        1
    );
    coordinator.interrupt_all().await;
    std::fs::remove_dir_all(root).unwrap();
}
