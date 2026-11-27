use super::{
    handoff::{
        HANDOFF_SUMMARY_MAX_BYTES, cap_handoff_summary, format_handoff, handoff_excerpt,
        handoff_item_issue,
    },
    worktree::workspace_merge_lock,
    *,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use t3_mcp::OrchestratorBackend;

#[test]
fn redacts_supported_credentials_before_truncating() {
    let credentials = [
        "sk-1234567890abcdef_-",
        "ghp_1234567890abcdef",
        "gho_1234567890abcdef",
        "ghu_1234567890abcdef",
        "xoxb-12345-fixture",
        "Bearer abcdefgh012345._~+/-",
        "t3mcp_0123456789abcdef",
        "eyJ1234567890.ABCDEFGHIJ.KLMNOPQRST",
    ];
    let text = format!("日本語 ({}) sk-short", credentials.join(", "));
    let expected = format!("日本語 ({}) sk-short", ["[REDACTED]"; 8].join(", "));
    assert_eq!(redact_credentials(&text), expected);
    assert_eq!(
        handoff_excerpt(&format!("error {}", credentials[0]), 10),
        "error [RED…"
    );
}

#[test]
fn preserves_an_unfinished_item_with_its_summary() {
    let item = TurnItem {
        id: Uuid::nil(),
        thread_id: Uuid::nil(),
        run_id: Uuid::nil(),
        ordinal: 7,
        kind: TurnItemKind::Subagent,
        summary: Some("Apply the outstanding migration".into()),
        payload: json!({"status": "running"}),
        raw_bytes: 0,
        compressed_bytes: None,
        created_at: Utc::now(),
    };
    let summary = format_handoff("Source", &[item], &[]);
    let (_, errors) = summary.split_once("## Unresolved / Recent Errors").unwrap();
    assert!(errors.contains("- Item 7 (unfinished): Apply the outstanding migration"));
}

#[test]
fn cap_handoff_summary_enforces_per_section_and_total_utf8_budgets() {
    let raw = format!(
        "## Changed Files\n{}\n\n## Recent Commands\n{}\n\n## Conversation Excerpt\n{}",
        "crates/server/src/日本語ファイル名テスト.rs\n".repeat(120),
        "cargo test -p t3-server --lib # テスト実行ログ\n".repeat(120),
        "ユーザー: ハンドオフ要約の日本語境界テストです。\n".repeat(160),
    );
    let result = cap_handoff_summary(&raw, HANDOFF_SUMMARY_MAX_BYTES);
    assert!(result.len() <= HANDOFF_SUMMARY_MAX_BYTES);
    assert!(result.contains("## Changed Files"));
    assert!(result.contains("## Recent Commands"));
    assert!(result.contains("## Conversation Excerpt"));
    assert_eq!(result.matches("[truncated]").count(), 3);

    let short = "## Changed Files\nsrc/lib.rs\n\n## Recent Commands\ncargo check";
    assert_eq!(cap_handoff_summary(short, HANDOFF_SUMMARY_MAX_BYTES), short);
}

#[test]
fn handoff_includes_modified_files_and_recent_commands() {
    let item = |ordinal, kind, summary: Option<&str>, payload| TurnItem {
        id: Uuid::new_v4(),
        thread_id: Uuid::nil(),
        run_id: Uuid::nil(),
        ordinal,
        kind,
        summary: summary.map(str::to_owned),
        payload,
        raw_bytes: 0,
        compressed_bytes: None,
        created_at: Utc::now(),
    };
    let summary = format_handoff(
        "Source",
        &[
            item(
                1,
                TurnItemKind::FileChange,
                None,
                json!({"changes":[{"path":"src/main.rs","kind":"update"}]}),
            ),
            item(
                2,
                TurnItemKind::CommandExecution,
                None,
                json!({"command":"cargo check", "exit_code":0}),
            ),
        ],
        &[],
    );
    assert!(summary.contains("## Modified Files\n- src/main.rs"));
    assert!(summary.contains("## Recent Commands\n- cargo check (exit code 0)"));
}

#[test]
fn preserves_provider_tool_errors_from_nested_payloads() {
    for payload in [
        json!({"is_error": true, "content": [{"type": "text", "text": "File missing"}]}),
        json!({"state": {"status": "error", "error": "File missing"}}),
    ] {
        let item = TurnItem {
            id: Uuid::nil(),
            thread_id: Uuid::nil(),
            run_id: Uuid::nil(),
            ordinal: 1,
            kind: TurnItemKind::ToolResult,
            summary: None,
            payload,
            raw_bytes: 0,
            compressed_bytes: None,
            created_at: Utc::now(),
        };
        assert!(handoff_item_issue(&item).unwrap().ends_with("File missing"));
    }
}

async fn run_git_cmd(dir: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn test_coordinator() -> (PathBuf, PathBuf, RunCoordinator, Project) {
    let root = std::env::temp_dir().join(format!("t3-threads-merge-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let data = root.join("data");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&data).unwrap();

    run_git_cmd(&repo, &["init", "--initial-branch=main"]).await;
    run_git_cmd(&repo, &["config", "user.name", "Test User"]).await;
    run_git_cmd(&repo, &["config", "user.email", "test@example.invalid"]).await;
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    run_git_cmd(&repo, &["add", "--", "base.txt"]).await;
    run_git_cmd(&repo, &["commit", "-m", "initial"]).await;

    let cfg = ServerConfig {
        data_dir: data,
        default_model: ModelSelection {
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            agent: None,
        },
        enabled_providers: vec![ProviderId::Mock],
        ..ServerConfig::default()
    };
    let store = t3_store::SqliteStore::open(cfg.db_path(), cfg.storage.clone()).unwrap();
    let project = store
        .create_project("Test".into(), repo.clone())
        .await
        .unwrap();
    let coordinator = RunCoordinator::new(
        cfg,
        store,
        McpInjectionSpec {
            binary_path: PathBuf::from("/bin/true"),
            server_url: "http://127.0.0.1:0".into(),
            internal_token: "token".into(),
            thread_id: Uuid::nil(),
            cua_binary_path: None,
            tool_profile: McpToolProfile::Full,
        },
    );
    (root, repo, coordinator, project)
}

#[tokio::test]
async fn child_worktree_diff_and_merge_enforce_safety_invariants() {
    let (root, repo, coordinator, project) = test_coordinator().await;
    let parent = coordinator
        .create_thread(CreateThread {
            project_id: project.id,
            title: "Parent".into(),
            model_selection: None,
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            isolation: IsolationMode::Shared,
            base_ref: None,
        })
        .await
        .unwrap();

    // Shared-workspace child must be rejected by both diff and merge.
    let shared_child = coordinator
        .new_thread(
            CreateThread {
                project_id: project.id,
                title: "Shared child".into(),
                model_selection: None,
                runtime_mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Default,
                isolation: IsolationMode::Shared,
                base_ref: None,
            },
            Some((&parent, "delegated")),
        )
        .await
        .unwrap();
    assert!(
        coordinator
            .child_worktree_diff(parent.id, shared_child.id, false)
            .await
            .is_err()
    );
    assert!(
        coordinator
            .merge_child_worktree(parent.id, shared_child.id, None, true)
            .await
            .is_err()
    );

    // Unrelated thread (not a child of parent) must be rejected.
    let unrelated = coordinator
        .create_thread(CreateThread {
            project_id: project.id,
            title: "Unrelated".into(),
            model_selection: None,
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            isolation: IsolationMode::Worktree,
            base_ref: None,
        })
        .await
        .unwrap();
    assert!(
        coordinator
            .child_worktree_diff(parent.id, unrelated.id, false)
            .await
            .is_err()
    );

    // Create isolated worktree child.
    let child = coordinator
        .new_thread(
            CreateThread {
                project_id: project.id,
                title: "Worker".into(),
                model_selection: None,
                runtime_mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Default,
                isolation: IsolationMode::Worktree,
                base_ref: None,
            },
            Some((&parent, "delegated")),
        )
        .await
        .unwrap();
    let child_wt = child.worktree_path.clone().unwrap();
    let child_branch = child.branch.clone().unwrap();
    assert!(child_branch.starts_with("t3rs/"));

    // Uncommitted changes in child worktree must reject merge.
    std::fs::write(child_wt.join("worker.txt"), "hello from worker\n").unwrap();
    let uncommitted_err = coordinator
        .merge_child_worktree(parent.id, child.id, None, true)
        .await
        .unwrap_err();
    assert!(uncommitted_err.to_string().contains("uncommitted"));

    // Commit changes in child worktree.
    run_git_cmd(&child_wt, &["add", "--", "worker.txt"]).await;
    run_git_cmd(&child_wt, &["commit", "-m", "worker change"]).await;

    // child_worktree_diff reports the committed branch diff.
    let diff = coordinator
        .child_worktree_diff(parent.id, child.id, false)
        .await
        .unwrap();
    assert!(!diff.clean);
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "worker.txt");
    assert!(diff.files[0].patch.contains("+hello from worker"));

    // Plan mode parent must reject merge.
    coordinator
        .0
        .store
        .update_thread_modes(parent.id, RuntimeMode::FullAccess, InteractionMode::Plan)
        .await
        .unwrap();
    let plan_err = coordinator
        .merge_child_worktree(parent.id, child.id, None, true)
        .await
        .unwrap_err();
    assert!(plan_err.to_string().contains("plan"));
    coordinator
        .0
        .store
        .update_thread_modes(parent.id, RuntimeMode::FullAccess, InteractionMode::Default)
        .await
        .unwrap();

    // Merge with cleanup=true succeeds, merges file, removes worktree, deletes t3rs/* branch.
    let merged = coordinator
        .merge_child_worktree(parent.id, child.id, Some("Merge worker".into()), true)
        .await
        .unwrap();
    assert!(merged.merged);
    assert!(merged.cleaned_up);
    assert!(merged.conflict_files.is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.join("worker.txt")).unwrap(),
        "hello from worker\n"
    );
    assert!(!child_wt.exists());
    assert!(
        !GitManager::list_branches(&repo)
            .await
            .unwrap()
            .iter()
            .any(|branch| branch == &child_branch)
    );

    // Verify per-workspace lock identity.
    let lock_a = workspace_merge_lock(&repo);
    let lock_b = workspace_merge_lock(&repo);
    assert!(Arc::ptr_eq(&lock_a, &lock_b));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn merge_child_worktree_reports_conflict_and_preserves_user_merge() {
    let (root, repo, coordinator, project) = test_coordinator().await;
    let parent = coordinator
        .create_thread(CreateThread {
            project_id: project.id,
            title: "Parent".into(),
            model_selection: None,
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            isolation: IsolationMode::Shared,
            base_ref: None,
        })
        .await
        .unwrap();
    let child = coordinator
        .new_thread(
            CreateThread {
                project_id: project.id,
                title: "Conflicting worker".into(),
                model_selection: None,
                runtime_mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Default,
                isolation: IsolationMode::Worktree,
                base_ref: None,
            },
            Some((&parent, "delegated")),
        )
        .await
        .unwrap();
    let child_wt = child.worktree_path.clone().unwrap();

    std::fs::write(child_wt.join("base.txt"), "child edit\n").unwrap();
    run_git_cmd(&child_wt, &["commit", "-am", "child edit"]).await;

    std::fs::write(repo.join("base.txt"), "parent edit\n").unwrap();
    run_git_cmd(&repo, &["commit", "-am", "parent edit"]).await;

    // Conflicting merge returns merged=false with conflict_files, does not clean up.
    let result = coordinator
        .merge_child_worktree(parent.id, child.id, None, true)
        .await
        .unwrap();
    assert!(!result.merged);
    assert!(!result.cleaned_up);
    assert_eq!(result.conflict_files, vec!["base.txt".to_owned()]);
    assert!(child_wt.exists());
    assert!(!GitManager::has_merge_in_progress(&repo).await.unwrap());

    // Start a user merge in parent repo and ensure merge_child_worktree rejects without aborting it.
    let manual = tokio::process::Command::new("git")
        .current_dir(&repo)
        .args(["merge", "--no-edit", child.branch.as_deref().unwrap()])
        .output()
        .await
        .unwrap();
    assert!(!manual.status.success());
    assert!(GitManager::has_merge_in_progress(&repo).await.unwrap());

    let err = coordinator
        .merge_child_worktree(parent.id, child.id, None, true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("merge in progress"));
    assert!(GitManager::has_merge_in_progress(&repo).await.unwrap());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn task_merge_blocks_no_undo_for_agents_while_human_merge_succeeds() {
    let (root, _repo, coordinator, project) = test_coordinator().await;
    let parent = coordinator
        .create_thread(CreateThread {
            project_id: project.id,
            title: "Parent".into(),
            model_selection: None,
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            isolation: IsolationMode::Shared,
            base_ref: None,
        })
        .await
        .unwrap();
    let child = coordinator
        .new_thread(
            CreateThread {
                project_id: project.id,
                title: "Migration worker".into(),
                model_selection: None,
                runtime_mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Default,
                isolation: IsolationMode::Worktree,
                base_ref: None,
            },
            Some((&parent, "delegated")),
        )
        .await
        .unwrap();
    let child_wt = child.worktree_path.clone().unwrap();

    std::fs::create_dir_all(child_wt.join("migrations")).unwrap();
    std::fs::write(
        child_wt.join("migrations/001_init.sql"),
        "CREATE TABLE t(id INTEGER);\n",
    )
    .unwrap();
    run_git_cmd(&child_wt, &["add", "migrations/001_init.sql"]).await;
    run_git_cmd(&child_wt, &["commit", "-m", "add migration"]).await;

    // Diff reports NoUndo tier.
    let diff = coordinator
        .child_worktree_diff(parent.id, child.id, false)
        .await
        .unwrap();
    let risk = diff.risk.expect("risk report populated");
    assert_eq!(risk.tier, UndoTier::NoUndo);
    assert_eq!(risk.flagged_paths, vec!["migrations/001_init.sql"]);

    // Autonomous agent MCP `task_merge` is blocked by the no-undo gate.
    let err = OrchestratorBackend::task_merge(&coordinator, parent.id, child.id, None, true)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Auth(_)), "got: {err:?}");
    assert!(err.to_string().contains("no-undo"));
    assert!(err.to_string().contains("migrations/001_init.sql"));

    // Explicit human merge via `merge_child_worktree` still succeeds.
    let merged = coordinator
        .merge_child_worktree(parent.id, child.id, Some("Human approved".into()), true)
        .await
        .unwrap();
    assert!(merged.merged);
    assert!(merged.cleaned_up);

    let _ = std::fs::remove_dir_all(&root);
}
