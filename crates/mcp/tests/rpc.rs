use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};
use t3_core::*;
use t3_mcp::*;
use uuid::Uuid;

struct Backend {
    id: Uuid,
    caller: Option<Thread>,
    project_root: Option<PathBuf>,
}

impl Backend {
    fn new(id: Uuid) -> Self {
        Self {
            id,
            caller: None,
            project_root: None,
        }
    }
}

#[async_trait]
impl OrchestratorBackend for Backend {
    async fn get_thread(&self, id: Uuid) -> CoreResult<Thread> {
        self.caller
            .clone()
            .filter(|thread| thread.id == id)
            .ok_or_else(|| CoreError::NotFound("thread".into()))
    }

    async fn workspace_root(&self, thread: &Thread) -> CoreResult<PathBuf> {
        thread
            .worktree_path
            .clone()
            .or_else(|| self.project_root.clone())
            .ok_or_else(|| CoreError::NotFound("caller workspace".into()))
    }

    async fn capabilities(&self) -> CoreResult<Capabilities> {
        Ok(Capabilities {
            providers: vec![],
            model_tiers: vec![],
            routing_hints: t3_mcp::default_routing_hints(),
        })
    }

    async fn delegate(
        &self,
        spec: DelegationSpec,
        mode: DelegateMode,
    ) -> CoreResult<DelegatedTask> {
        assert_eq!(spec.parent_thread_id, self.id);
        assert_eq!(spec.model_selection.provider, ProviderId::OpenCode);
        assert_eq!(spec.isolation, IsolationMode::Worktree);
        assert_eq!(spec.base_ref.as_deref(), Some("HEAD"));
        assert!(matches!(mode, DelegateMode::Wait));
        self.task_status(self.id).await
    }

    async fn task_status(&self, task_id: Uuid) -> CoreResult<DelegatedTask> {
        Ok(DelegatedTask {
            task_id,
            child_thread_id: task_id,
            run_id: self.id,
            status: ThreadStatus::Completed,
            worktree_path: None,
            summary: Some("complete".into()),
            worktree: None,
            overlaps: vec![],
        })
    }

    async fn task_cancel(&self, task_id: Uuid) -> CoreResult<DelegatedTask> {
        let mut task = self.task_status(task_id).await?;
        task.status = ThreadStatus::Interrupted;
        Ok(task)
    }

    async fn thread_list(
        &self,
        project_id: Option<Uuid>,
        archived: bool,
        limit: usize,
    ) -> CoreResult<Vec<Thread>> {
        assert_eq!(project_id, Some(self.id));
        assert!(archived);
        assert_eq!(limit, 5);
        Ok(vec![])
    }

    async fn thread_read(&self, thread_id: Uuid, limit: usize) -> CoreResult<ThreadRead> {
        assert_eq!(thread_id, self.id);
        assert_eq!(limit, 3);
        Err(CoreError::NotFound("thread".into()))
    }

    async fn thread_send(
        &self,
        thread_id: Uuid,
        prompt: String,
        behavior: Option<FollowUpBehavior>,
    ) -> CoreResult<Uuid> {
        assert_eq!(thread_id, self.id);
        assert_eq!(prompt, "follow up");
        assert_eq!(behavior, Some(FollowUpBehavior::Queue));
        Ok(self.id)
    }

    async fn thread_fork(&self, thread_id: Uuid, options: ForkOptions) -> CoreResult<Thread> {
        assert_eq!(thread_id, self.id);
        assert_eq!(options.handoff_summary.as_deref(), Some("handoff"));
        let target = options.target_model.unwrap();
        assert_eq!(target.provider, ProviderId::Codex);
        assert_eq!(target.model, "codex");
        Err(CoreError::NotFound("thread".into()))
    }

    async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>> {
        Ok(vec![])
    }

    async fn swarm_post_message(
        &self,
        _thread_id: Uuid,
        _text: String,
    ) -> CoreResult<SwarmPostResult> {
        let board = large_swarm_board();
        Ok(SwarmPostResult {
            seq: board.head_seq + 1,
            board_id: board.board_id,
            woken: vec!["worker-1".into()],
        })
    }
}

fn large_swarm_board() -> SwarmBoardSnapshot {
    let board_id = Uuid::new_v4();
    let created_at = caller(None).created_at;
    let members = (0..20)
        .map(|index| SwarmMember {
            board_id,
            name: if index == 0 {
                "coordinator".into()
            } else {
                format!("worker-{index}")
            },
            thread_id: Uuid::new_v4(),
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            state: "working".into(),
            role: if index == 0 {
                "coordinator".into()
            } else {
                "worker".into()
            },
            last_read_seq: 0,
            created_at,
        })
        .collect();
    let messages = (0..20)
        .map(|index| SwarmMessage {
            seq: index + 1,
            board_id,
            sender: format!("worker-{index}"),
            text: format!("brief {index}: {}", "x".repeat(5 * 1024)),
            mentions: vec![],
            created_at,
        })
        .collect();
    SwarmBoardSnapshot {
        board_id,
        coordinator_thread_id: server_id(),
        title: "large swarm".into(),
        members,
        messages,
        head_seq: 20,
    }
}

fn request(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": "request", "method": method, "params":params})
}

async fn call(server: &McpServer<Backend>, name: &str, arguments: Value) -> Value {
    let response = server
        .handle(
            request("tools/call", json!({"name":name,"arguments":arguments})),
            Some(server_id()),
        )
        .await
        .unwrap();
    response["result"].clone()
}

fn server_id() -> Uuid {
    Uuid::from_u128(7)
}

fn payload(result: Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn handshake_discovery_batches_and_protocol_errors() {
    let server = McpServer::new(Backend::new(server_id()));
    let initialized = server
        .handle(
            request("initialize", json!({"protocolVersion":"2025-03-26"})),
            None,
        )
        .await
        .unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(initialized["result"]["serverInfo"]["name"], "t3code-rs");
    assert_eq!(initialized["id"], "request");
    let notification = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    assert!(server.handle(notification.clone(), None).await.is_none());
    let batch = server
        .handle(
            json!([
                notification,
                request("ping", json!({})),
                request("unknown", json!({}))
            ]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(batch[0]["result"], json!({}));
    assert_eq!(batch[1]["error"]["code"], -32601);
    assert_eq!(
        server.handle_bytes(b"{", None).await.unwrap()["error"]["code"],
        -32700
    );
    assert_eq!(
        server.handle(json!(7), None).await.unwrap()["error"]["code"],
        -32600
    );
    let list = server
        .handle(request("tools/list", json!({})), None)
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 22);
    for name in [
        "pause_all",
        "task_diff",
        "task_merge",
        "swarm_status",
        "swarm_add_worker",
        "swarm_post_message",
        "swarm_read_board",
        "swarm_kill_worker",
        "code_index",
        "code_slice",
        "code_grep",
        "code_diff",
        "compaction_econ",
    ] {
        assert!(tools.iter().any(|tool| tool["name"] == name), "{name}");
    }
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(tool["name"].is_string());
    }
    let status = tools
        .iter()
        .find(|tool| tool["name"] == "swarm_status")
        .unwrap();
    assert!(
        status["description"]
            .as_str()
            .unwrap()
            .contains("killed_count")
    );
    let read = tools
        .iter()
        .find(|tool| tool["name"] == "swarm_read_board")
        .unwrap();
    let properties = &read["inputSchema"]["properties"];
    assert!(properties.get("since_seq").is_some());
    assert!(properties.get("since").is_none());
    assert_eq!(properties["limit"]["default"], 20);
    assert!(properties.get("seq").is_some());
    let description = |name: &str| {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        tool["description"].as_str().unwrap()
    };
    for field in [
        "objective",
        "output",
        "boundaries",
        "effort budget",
        "stop condition",
    ] {
        assert!(description("swarm_add_worker").contains(field), "{field}");
    }
    assert!(description("swarm_post_message").contains("4096 bytes"));
}

#[tokio::test]
async fn dispatches_tools_with_thread_context_and_validates_arguments() {
    let id = server_id();
    let mut parent = caller(None);
    parent.id = id;
    let server = McpServer::new(Backend {
        id,
        caller: Some(parent),
        project_root: None,
    });
    assert_eq!(
        payload(call(&server, "orchestrator_capabilities", json!({})).await)["model_tiers"],
        json!([])
    );
    assert_eq!(
        payload(call(&server, "orch_stats", json!({})).await),
        json!([])
    );
    let task = payload(
        call(
            &server,
            "delegate_task",
            json!({
                "title":"child","task":"task","provider":"opencode","model":"local/model",
                "isolation":"worktree","base_ref":"HEAD","mode":"wait"
            }),
        )
        .await,
    );
    assert_eq!(task["run_id"], json!(id));
    let status = payload(call(&server, "task_status", json!({"task_id":id})).await);
    assert_eq!(status["summary"], "complete");
    assert!(status["worktree"].is_null());
    assert_eq!(status["overlaps"], json!([]));
    assert_eq!(
        payload(call(&server, "task_cancel", json!({"task_id":id})).await)["status"],
        "interrupted"
    );
    assert_eq!(
        payload(
            call(
                &server,
                "thread_list",
                json!({"project_id":id,"include_archived":true,"limit":5})
            )
            .await
        ),
        json!([])
    );
    assert_eq!(
        payload(
            call(
                &server,
                "thread_send",
                json!({"prompt":"follow up","behavior":"queue"})
            )
            .await
        )["run_id"],
        json!(id)
    );
    for (name, args) in [
        ("thread_read", json!({"limit":3})),
        (
            "thread_fork",
            json!({"provider":"codex","model":"codex","handoff_summary":"handoff"}),
        ),
        ("task_status", json!({"task_id":"invalid"})),
        ("unknown", json!({})),
    ] {
        assert_eq!(call(&server, name, args).await["isError"], true);
    }
}

#[tokio::test]
async fn swarm_post_response_stays_small_on_a_large_board() {
    let server = McpServer::new(Backend::new(server_id()));
    let result = call(
        &server,
        "swarm_post_message",
        json!({"text":"@worker-1 check this"}),
    )
    .await;
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.len() < 500, "post response was {} bytes", text.len());
    let response: Value = serde_json::from_str(text).unwrap();
    assert_eq!(response.as_object().unwrap().len(), 3);
    assert!(response.get("seq").is_some());
    assert!(response.get("board_id").is_some());
    assert_eq!(response["woken"], json!(["worker-1"]));
}

fn caller(worktree: Option<&Path>) -> Thread {
    let now = "2026-11-25T00:00:00Z".parse().unwrap();
    Thread {
        id: server_id(),
        project_id: Uuid::new_v4(),
        parent_thread_id: None,
        relationship: None,
        title: "Code tools".into(),
        model_selection: ModelSelection {
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            agent: None,
        },
        runtime_mode: RuntimeMode::default(),
        interaction_mode: InteractionMode::default(),
        status: ThreadStatus::Idle,
        worktree_path: worktree.map(Path::to_owned),
        branch: None,
        provider_session_id: None,
        archived: false,
        pinned: false,
        created_at: now,
        updated_at: now,
    }
}

async fn scoped_call(
    server: &McpServer<Backend>,
    caller: &Thread,
    name: &str,
    arguments: Value,
) -> Value {
    let request = request("tools/call", json!({"name": name, "arguments": arguments}));
    let response = server
        .handle_scoped_bytes(request.to_string().as_bytes(), caller)
        .await
        .unwrap();
    response["result"].clone()
}

#[tokio::test]
async fn code_tools_execute_in_the_caller_worktree() {
    let root = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("example.rs"),
        "pub fn ping() {\n    let token = 1;\n}\n",
    )
    .unwrap();
    let caller = caller(Some(root.path()));
    let server = McpServer::new(Backend {
        id: caller.id,
        caller: Some(caller.clone()),
        project_root: Some(project.path().to_owned()),
    });
    let index = payload(call(&server, "code_index", json!({"path": "example.rs"})).await);
    assert_eq!(index["files"][0]["symbols"][0]["name"], "ping");
    let result = payload(
        scoped_call(
            &server,
            &caller,
            "code_slice",
            json!({"path": "example.rs", "symbol": "ping"}),
        )
        .await,
    );
    assert_eq!(result["range"], "L1-L3");
    assert_eq!(
        result["text"],
        "L1: pub fn ping() {\nL2:     let token = 1;\nL3: }"
    );
    let result = payload(
        scoped_call(
            &server,
            &caller,
            "code_grep",
            json!({"pattern": "tok.n", "path": "."}),
        )
        .await,
    );
    assert_eq!(result["files"][0]["matches"][0]["symbol"], "fn ping");
    let econ = payload(call(&server, "compaction_econ", json!({
        "prefix": 10_000, "replaced": 130_000, "summary": 4_000, "tail": 26_000, "instruction": 500,
        "summarizer_reuses_prefix": true,
        "prices": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25},
        "remaining_turns": 6
    })).await);
    assert_eq!(econ["one_time_cost"], 69_000.0);
    assert_eq!(econ["worth_compacting"], true);
}

#[tokio::test]
async fn scoped_code_tools_reject_paths_outside_the_project() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let sibling = temp.path().join("project-other");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&sibling).unwrap();
    fs::write(root.join("inside.rs"), "fn inside() {}").unwrap();
    let outside = sibling.join("secret.rs");
    fs::write(&outside, "fn secret() {}").unwrap();
    let caller = caller(None);
    let server = McpServer::new(Backend {
        id: caller.id,
        caller: Some(caller.clone()),
        project_root: Some(root.clone()),
    });
    assert_eq!(
        payload(call(&server, "code_index", json!({"path": "inside.rs"})).await)["files"][0]["symbols"]
            [0]["name"],
        "inside"
    );
    for (tool, arguments) in [
        ("code_index", json!({"path": outside})),
        ("code_slice", json!({"path": "../project-other/secret.rs"})),
        ("code_grep", json!({"path": sibling, "pattern": "secret"})),
        ("code_diff", json!({"cwd": ".."})),
        (
            "code_diff",
            json!({"cwd": ".", "path": "../project-other/secret.rs"}),
        ),
    ] {
        let result = scoped_call(&server, &caller, tool, arguments).await;
        assert_eq!(result["isError"], true, "{tool}: {result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("outside"),
            "{result}"
        );
    }
    let result = call(&server, "code_slice", json!({"path": outside})).await;
    assert_eq!(result["isError"], true);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("link.rs")).unwrap();
        std::os::unix::fs::symlink(&sibling, root.join("link-dir")).unwrap();
        let result = scoped_call(&server, &caller, "code_slice", json!({"path": "link.rs"})).await;
        assert_eq!(result["isError"], true);
        let result = scoped_call(
            &server,
            &caller,
            "code_diff",
            json!({"cwd": ".", "path": "link-dir/secret.rs"}),
        )
        .await;
        assert_eq!(result["isError"], true);
        let result = payload(
            scoped_call(
                &server,
                &caller,
                "code_grep",
                json!({"path": ".", "pattern": "secret"}),
            )
            .await,
        );
        assert_eq!(result["hits"], 0);
    }
}

#[tokio::test]
async fn scoped_diff_blocks_git_discovery_above_the_workspace() {
    let repo = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let workspace = repo.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let nested = caller(Some(&workspace));
    let server = McpServer::new(Backend::new(nested.id));
    let result = scoped_call(&server, &nested, "code_diff", json!({"cwd": "."})).await;
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Git repository is outside")
    );
    let at_root = caller(Some(repo.path()));
    let diff = payload(scoped_call(&server, &at_root, "code_diff", json!({"cwd": "."})).await);
    assert_eq!(diff["skipped_generated"], 0);
}

#[tokio::test]
async fn plan_thread_cannot_delegate_to_a_worktree() {
    let mut parent = caller(None);
    parent.interaction_mode = InteractionMode::Plan;
    let server = McpServer::new(Backend {
        id: parent.id,
        caller: Some(parent.clone()),
        project_root: None,
    });
    let args = json!({
        "title":"child","task":"task","provider":"opencode","model":"local/model",
        "isolation":"worktree","base_ref":"HEAD","mode":"wait","interaction_mode":"plan"
    });
    for result in [
        scoped_call(&server, &parent, "delegate_task", args.clone()).await,
        call(&server, "delegate_task", args).await,
    ] {
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("plan")
        );
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn scoped_diff_rejects_external_checkout_metadata() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let external_git = external.path().join("metadata");
    run_git(
        workspace.path(),
        &[
            "init",
            "-q",
            "--separate-git-dir",
            external_git.to_str().unwrap(),
        ],
    );
    let thread = caller(Some(workspace.path()));
    let server = McpServer::new(Backend::new(thread.id));
    let result = scoped_call(&server, &thread, "code_diff", json!({"cwd":"."})).await;
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("metadata")
    );
}

#[tokio::test]
async fn scoped_diff_accepts_linked_worktree_metadata_from_its_project() {
    let project = tempfile::tempdir().unwrap();
    let worktrees = tempfile::tempdir().unwrap();
    let worktree = worktrees.path().join("task");
    run_git(project.path(), &["init", "-q"]);
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(project.path().join("src/source.txt"), "original\n").unwrap();
    run_git(project.path(), &["add", "src/source.txt"]);
    run_git(
        project.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-qm",
            "initial",
        ],
    );
    run_git(
        project.path(),
        &["worktree", "add", "--detach", worktree.to_str().unwrap()],
    );
    fs::write(worktree.join("src/source.txt"), "modified\n").unwrap();
    let thread = caller(Some(&worktree));
    let server = McpServer::new(Backend {
        id: thread.id,
        caller: Some(thread.clone()),
        project_root: Some(project.path().to_owned()),
    });
    let diff = payload(
        scoped_call(
            &server,
            &thread,
            "code_diff",
            json!({"cwd":"src","rev":"HEAD"}),
        )
        .await,
    );
    assert_eq!(diff["name_status"], "M\tsource.txt");
    assert!(diff["hunks"].as_str().unwrap().contains("+modified"));
}
