//! Transport-independent MCP orchestration tools.

mod code_tools;

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_core::*;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub providers: Vec<ProviderHealth>,
    pub model_tiers: Vec<ModelSpec>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelegateMode {
    #[default]
    Async,
    Wait,
}

#[derive(Debug, Serialize)]
pub struct DelegatedTask {
    pub task_id: Uuid,
    pub child_thread_id: Uuid,
    pub run_id: Uuid,
    pub status: ThreadStatus,
    pub worktree_path: Option<std::path::PathBuf>,
    pub summary: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ThreadRead {
    pub thread: Thread,
    pub items: Vec<TurnItem>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ForkOptions {
    pub title: Option<String>,
    pub target_model: Option<ModelSelection>,
    pub handoff_summary: Option<String>,
}

#[async_trait]
pub trait OrchestratorBackend: Send + Sync {
    async fn get_thread(&self, id: Uuid) -> CoreResult<Thread>;
    /// Resolve the workspace for a scoped code tool. Backends with shared threads
    /// should also resolve the caller's project root when there is no worktree.
    async fn workspace_root(&self, thread: &Thread) -> CoreResult<PathBuf> {
        thread
            .worktree_path
            .clone()
            .ok_or_else(|| CoreError::NotFound("caller workspace".into()))
    }
    async fn capabilities(&self) -> CoreResult<Capabilities>;
    async fn delegate(&self, spec: DelegationSpec, mode: DelegateMode)
    -> CoreResult<DelegatedTask>;
    async fn task_status(&self, task_id: Uuid) -> CoreResult<DelegatedTask>;
    async fn task_cancel(&self, task_id: Uuid) -> CoreResult<DelegatedTask>;
    async fn thread_list(
        &self,
        project_id: Option<Uuid>,
        include_archived: bool,
        limit: usize,
    ) -> CoreResult<Vec<Thread>>;
    async fn thread_read(&self, thread_id: Uuid, limit: usize) -> CoreResult<ThreadRead>;
    async fn thread_send(
        &self,
        thread_id: Uuid,
        prompt: String,
        behavior: Option<FollowUpBehavior>,
    ) -> CoreResult<Uuid>;
    async fn thread_fork(&self, thread_id: Uuid, options: ForkOptions) -> CoreResult<Thread>;
    async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>>;
}

pub struct McpServer<B> {
    backend: B,
}

#[derive(Clone, Copy)]
struct CallContext<'a> {
    thread_id: Option<Uuid>,
    caller: Option<&'a Thread>,
}

impl<B: OrchestratorBackend> McpServer<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub async fn handle_bytes(&self, request: &[u8], thread_id: Option<Uuid>) -> Option<Value> {
        self.parse(
            request,
            CallContext {
                thread_id,
                caller: None,
            },
        )
        .await
    }

    pub async fn handle_scoped_bytes(&self, request: &[u8], caller: &Thread) -> Option<Value> {
        self.parse(
            request,
            CallContext {
                thread_id: Some(caller.id),
                caller: Some(caller),
            },
        )
        .await
    }

    async fn parse(&self, request: &[u8], context: CallContext<'_>) -> Option<Value> {
        match serde_json::from_slice(request) {
            Ok(value) => self.dispatch(value, context).await,
            Err(_) => Some(rpc_error(Value::Null, -32700, "Parse error")),
        }
    }

    pub async fn handle(&self, request: Value, thread_id: Option<Uuid>) -> Option<Value> {
        self.dispatch(
            request,
            CallContext {
                thread_id,
                caller: None,
            },
        )
        .await
    }

    async fn dispatch(&self, request: Value, context: CallContext<'_>) -> Option<Value> {
        if let Value::Array(requests) = request {
            if requests.is_empty() {
                return Some(rpc_error(Value::Null, -32600, "Invalid request"));
            }
            let mut responses = Vec::new();
            for request in requests {
                responses.extend(self.handle_one(request, context).await);
            }
            return (!responses.is_empty()).then_some(Value::Array(responses));
        }
        self.handle_one(request, context).await
    }

    async fn handle_one(&self, request: Value, context: CallContext<'_>) -> Option<Value> {
        let id = request.get("id");
        if request["jsonrpc"] != "2.0"
            || id.is_some_and(|id| !(id.is_null() || id.is_string() || id.is_number()))
        {
            return Some(rpc_error(Value::Null, -32600, "Invalid request"));
        }
        let Some(method) = request["method"].as_str() else {
            return Some(rpc_error(Value::Null, -32600, "Invalid request"));
        };
        let id = id?.clone();
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "t3code-rs", "version": env!("CARGO_PKG_VERSION")}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => {
                let Ok(call) = serde_json::from_value::<ToolCall>(request["params"].clone()) else {
                    return Some(rpc_error(id, -32602, "Invalid tool call parameters"));
                };
                match self.call(&call.name, call.arguments, context).await {
                    Ok(data) => json!({"content": [{"type": "text", "text": data.to_string()}]}),
                    Err(error) => json!({
                        "isError": true, "content": [{"type": "text", "text": error.to_string()}]
                    }),
                }
            }
            _ => return Some(rpc_error(id, -32601, "Method not found")),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    async fn call(&self, name: &str, args: Value, context: CallContext<'_>) -> CoreResult<Value> {
        let backend = &self.backend;
        let CallContext { thread_id, caller } = context;
        match name {
            "orchestrator_capabilities" => encode(backend.capabilities().await?),
            "delegate_task" => {
                let args: DelegateArgs = decode(args)?;
                let parent_thread_id = resolve_thread(args.parent_thread_id, thread_id)?;
                self.scoped_thread(parent_thread_id, caller).await?;
                let spec = DelegationSpec {
                    parent_thread_id,
                    title: args.title,
                    task: args.task,
                    model_selection: ModelSelection {
                        provider: args.provider,
                        model: args.model,
                        effort: args.effort,
                        agent: None,
                    },
                    isolation: args.isolation,
                    base_ref: args.base_ref,
                    runtime_mode: args.runtime_mode,
                    interaction_mode: args.interaction_mode,
                };
                if let Some(caller) = caller {
                    spec.validate_permissions(caller)?;
                }
                encode(backend.delegate(spec, args.mode).await?)
            }
            "task_status" | "task_cancel" => {
                let id = decode::<TaskArgs>(args)?.task_id;
                self.scoped_thread(id, caller).await?;
                if name == "task_status" {
                    encode(backend.task_status(id).await?)
                } else {
                    encode(backend.task_cancel(id).await?)
                }
            }
            "thread_list" => {
                let args: ListArgs = decode(args)?;
                let project_id = if let Some(caller) = caller {
                    if args.project_id.is_some_and(|id| id != caller.project_id) {
                        return Err(CoreError::NotFound("project".into()));
                    }
                    Some(caller.project_id)
                } else {
                    args.project_id
                };
                encode(
                    backend
                        .thread_list(project_id, args.include_archived, args.limit)
                        .await?,
                )
            }
            "thread_read" => {
                let args: ReadArgs = decode(args)?;
                let id = resolve_thread(args.thread_id, thread_id)?;
                self.scoped_thread(id, caller).await?;
                encode(backend.thread_read(id, args.limit).await?)
            }
            "thread_send" => {
                let args: SendArgs = decode(args)?;
                let id = resolve_thread(args.thread_id, thread_id)?;
                self.check_target_permissions(id, caller).await?;
                let run_id = backend.thread_send(id, args.prompt, args.behavior).await?;
                Ok(json!({"thread_id": id, "run_id": run_id}))
            }
            "thread_fork" => {
                let args: ForkArgs = decode(args)?;
                let id = resolve_thread(args.thread_id, thread_id)?;
                self.check_target_permissions(id, caller).await?;
                let mut options = args.options;
                if let Some(provider) = args.provider {
                    let model = args.model.ok_or_else(|| {
                        CoreError::InvalidArgument("model is required with provider".into())
                    })?;
                    options.target_model = Some(ModelSelection {
                        provider,
                        model,
                        effort: args.effort,
                        agent: None,
                    });
                } else if args.model.is_some() || args.effort.is_some() {
                    return Err(CoreError::InvalidArgument(
                        "provider is required with model or effort".into(),
                    ));
                }
                encode(backend.thread_fork(id, options).await?)
            }
            "orch_stats" => encode(backend.orch_stats().await?),
            "compaction_econ" => encode(t3_maki::compaction_econ(decode(args)?)),
            "code_index" | "code_slice" | "code_grep" | "code_diff" => {
                let workspace = match (caller, thread_id) {
                    (Some(caller), _) => Some(backend.workspace_root(caller).await?),
                    (None, Some(id)) => {
                        let caller = backend.get_thread(id).await?;
                        Some(backend.workspace_root(&caller).await?)
                    }
                    (None, None) => None,
                };
                code_tools::call(name.to_owned(), args, workspace).await
            }
            _ => Err(CoreError::InvalidArgument(format!("unknown tool: {name}"))),
        }
    }

    async fn scoped_thread(&self, id: Uuid, caller: Option<&Thread>) -> CoreResult<Option<Thread>> {
        let Some(caller) = caller else {
            return Ok(None);
        };
        let thread = self.backend.get_thread(id).await?;
        if thread.project_id != caller.project_id {
            return Err(CoreError::NotFound("thread".into()));
        }
        Ok(Some(thread))
    }

    async fn check_target_permissions(&self, id: Uuid, caller: Option<&Thread>) -> CoreResult<()> {
        if let (Some(caller), Some(target)) = (caller, self.scoped_thread(id, caller).await?)
            && (target.runtime_mode > caller.runtime_mode
                || (caller.interaction_mode == InteractionMode::Plan
                    && target.interaction_mode == InteractionMode::Default))
        {
            return Err(CoreError::InvalidArgument(
                "target thread exceeds the caller thread's permissions".into(),
            ));
        }
        Ok(())
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn resolve_thread(arg: Option<Uuid>, context: Option<Uuid>) -> CoreResult<Uuid> {
    arg.or(context)
        .ok_or_else(|| CoreError::InvalidArgument("thread ID is required".into()))
}

fn decode<T: DeserializeOwned>(args: Value) -> CoreResult<T> {
    serde_json::from_value(args).map_err(|error| CoreError::InvalidArgument(error.to_string()))
}

fn encode<T: Serialize>(data: T) -> CoreResult<Value> {
    serde_json::to_value(data).map_err(|error| CoreError::Internal(error.to_string()))
}

#[derive(Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

fn empty_object() -> Value {
    json!({})
}
fn default_limit() -> usize {
    50
}

#[derive(Deserialize)]
struct DelegateArgs {
    parent_thread_id: Option<Uuid>,
    title: String,
    task: String,
    #[serde(deserialize_with = "provider_id")]
    provider: ProviderId,
    model: String,
    effort: Option<String>,
    #[serde(default)]
    isolation: IsolationMode,
    base_ref: Option<String>,
    #[serde(default)]
    mode: DelegateMode,
    #[serde(default)]
    runtime_mode: RuntimeMode,
    #[serde(default)]
    interaction_mode: InteractionMode,
}

#[derive(Deserialize)]
struct TaskArgs {
    task_id: Uuid,
}

#[derive(Deserialize)]
struct ListArgs {
    project_id: Option<Uuid>,
    #[serde(default)]
    include_archived: bool,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct ReadArgs {
    thread_id: Option<Uuid>,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct SendArgs {
    thread_id: Option<Uuid>,
    prompt: String,
    behavior: Option<FollowUpBehavior>,
}

#[derive(Deserialize)]
struct ForkArgs {
    thread_id: Option<Uuid>,
    #[serde(flatten)]
    options: ForkOptions,
    #[serde(default, deserialize_with = "optional_provider_id")]
    provider: Option<ProviderId>,
    model: Option<String>,
    effort: Option<String>,
}

fn optional_provider_id<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ProviderId>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|id| parse_provider(&id))
        .transpose()
}

fn provider_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ProviderId, D::Error> {
    parse_provider(&String::deserialize(d)?)
}

fn parse_provider<E: serde::de::Error>(id: &str) -> Result<ProviderId, E> {
    if id == "opencode" {
        Ok(ProviderId::OpenCode)
    } else {
        serde_json::from_value(json!(id)).map_err(E::custom)
    }
}

fn tools() -> Vec<Value> {
    let string = json!({"type": "string"});
    let uuid = json!({"type": "string", "format": "uuid"});
    let limit = json!({"type": "integer", "minimum": 0});
    let provider =
        json!({"enum": ["claude", "codex", "antigravity", "open-code", "opencode", "mock"]});
    let make = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({"name": name, "description": description, "inputSchema": {
            "type": "object", "properties": properties, "required": required
        }})
    };
    vec![
        make(
            "orchestrator_capabilities",
            "List provider health and curated models by tier.",
            json!({}),
            &[],
        ),
        make(
            "delegate_task",
            "Start a child task, optionally in a separate Git worktree. The parent defaults to the current thread.",
            json!({
                "parent_thread_id": uuid, "title": string, "task": string, "provider": provider,
                "model": string, "effort": string, "isolation": {"enum": ["shared", "worktree"]},
                "base_ref": string, "mode": {"enum": ["async", "wait"]},
                "runtime_mode": {"enum": ["approval-required", "auto-accept-edits", "full-access"]},
                "interaction_mode": {"enum": ["default", "plan"]}
            }),
            &["title", "task", "provider", "model"],
        ),
        make(
            "task_status",
            "Read a delegated task's status and latest assistant summary.",
            json!({"task_id": uuid}),
            &["task_id"],
        ),
        make(
            "task_cancel",
            "Interrupt a delegated task.",
            json!({"task_id": uuid}),
            &["task_id"],
        ),
        make(
            "thread_list",
            "List recent threads.",
            json!({"project_id": uuid, "include_archived": {"type": "boolean"}, "limit": limit}),
            &[],
        ),
        make(
            "thread_read",
            "Read thread metadata and recent turn items.",
            json!({"thread_id": uuid, "limit": limit}),
            &[],
        ),
        make(
            "thread_send",
            "Send a follow-up prompt to a thread.",
            json!({"thread_id": uuid, "prompt": string, "behavior": {"enum": ["steer", "queue"]}}),
            &["prompt"],
        ),
        make(
            "thread_fork",
            "Fork a thread with a compact context handoff.",
            json!({
                "thread_id": uuid, "title": string, "provider": provider, "model": string,
                "effort": string, "handoff_summary": string, "target_model": {
                    "type": "object", "properties": {"provider": provider, "model": string, "effort": string, "agent": string},
                    "required": ["provider", "model"]
                }
            }),
            &[],
        ),
        make(
            "orch_stats",
            "Read run and token statistics per provider and model.",
            json!({}),
            &[],
        ),
        make(
            "code_index",
            "List source declarations and their line ranges for a file or directory.",
            json!({"path": string, "max_depth": limit, "max_lines": limit}),
            &["path"],
        ),
        make(
            "code_slice",
            "Read a named symbol or a 1-based inclusive line range.",
            json!({
                "path": string, "symbol": string, "max_bytes": limit,
                "start": {"type": "integer", "minimum": 1}, "end": {"type": "integer", "minimum": 1}
            }),
            &["path"],
        ),
        make(
            "code_grep",
            "Search source lines with a regex and show their enclosing symbols. A glob without a slash matches file names at any depth.",
            json!({"path": string, "pattern": string, "glob": string, "max_hits": limit, "max_bytes": limit}),
            &["pattern", "path"],
        ),
        make(
            "code_diff",
            "Show compact tracked Git changes. The optional path is literal and relative to cwd. Pass a revision such as HEAD to compare against it.",
            json!({"cwd": string, "rev": string, "path": string, "max_bytes": limit}),
            &["cwd"],
        ),
        make(
            "compaction_econ",
            "Estimate prompt-cache compaction cost, per-turn savings, and break-even turns using prices in a common unit.",
            json!({
                "prefix": limit, "replaced": limit, "summary": limit, "tail": limit,
                "instruction": limit, "summarizer_reuses_prefix": {"type": "boolean"},
                "prices": {
                    "type": "object", "properties": {
                        "input": {"type": "number"}, "output": {"type": "number"},
                        "cache_read": {"type": "number"}, "cache_write": {"type": "number"}
                    },
                    "required": ["input", "output", "cache_read", "cache_write"]
                },
                "remaining_turns": {"type": "number", "minimum": 0}
            }),
            &[
                "prefix",
                "replaced",
                "summary",
                "tail",
                "instruction",
                "summarizer_reuses_prefix",
                "prices",
            ],
        ),
    ]
}
