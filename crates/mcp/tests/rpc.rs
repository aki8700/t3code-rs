use async_trait::async_trait;
use serde_json::{Value, json};
use t3_core::*;
use t3_mcp::*;
use uuid::Uuid;

struct Backend {
    id: Uuid,
}

#[async_trait]
impl OrchestratorBackend for Backend {
    async fn get_thread(&self, _id: Uuid) -> CoreResult<Thread> {
        Err(CoreError::NotFound("thread".into()))
    }

    async fn capabilities(&self) -> CoreResult<Capabilities> {
        Ok(Capabilities {
            providers: vec![],
            model_tiers: vec![],
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
    let server = McpServer::new(Backend { id: server_id() });
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
    assert_eq!(tools.len(), 9);
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(tool["name"].is_string());
    }
}

#[tokio::test]
async fn dispatches_tools_with_thread_context_and_validates_arguments() {
    let id = server_id();
    let server = McpServer::new(Backend { id });
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
    assert_eq!(
        payload(call(&server, "task_status", json!({"task_id":id})).await)["summary"],
        "complete"
    );
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
